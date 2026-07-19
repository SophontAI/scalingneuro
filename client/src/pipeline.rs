use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{
    MANIFEST_SCHEMA_VERSION,
    api::{
        AlreadyReceivedSeries, CompleteUploadRequest, CompletedObject, ContributionInfo,
        CreateUploadResponse, IngestApi, MultipartObject, RegisterRequest, UploadStatus,
        VerificationProgress, has_error_code, is_not_found_api_error, is_transient_api_error,
        normalize_base_url,
    },
    archive::{
        ArchiveRequest, DICOM_METADATA_POLICY_ID, DICOM_METADATA_POLICY_VERSION,
        create_dicom_archive, metadata_policy,
    },
    bundle::{METADATA_POLICY_ID, METADATA_POLICY_VERSION},
    classify::classify_header,
    config::{AppPaths, ClientConfig},
    dicom::{
        Discovery, DiscoveryPhase, SeriesGroup, discover_with_progress,
        snapshot_source_with_progress,
    },
    model::{
        Classification, ClassificationDecision, ClassificationEvidence, ExistingArchiveBundle,
        HeldSeries, LocalManifest, ManifestBundle, ReportBundle, RunReport, ScanSidecar,
        SourceSummary,
    },
    privacy,
    progress::{Progress, ProgressUnit},
    pseudonym::Pseudonymizer,
    s3::MultipartUploader,
    state::{RunRecord, StateStore, UploadObjectRecord},
};

const MAX_LEGACY_BUNDLES_PER_UPLOAD: usize = 32;
// Keep each raw-DICOM receipt completion comfortably below Cloudflare Free's
// per-request internal/D1 subrequest ceiling. A multipart series may require a
// pre-completion HEAD, multipart completion, and a post-completion HEAD.
const MAX_DICOM_SERIES_PER_UPLOAD: usize = 8;
const MAX_LEGACY_BYTES_PER_UPLOAD: u64 = 32 * 1024 * 1024 * 1024;
const MAX_DICOM_ARCHIVE_BYTES_PER_SERIES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_DICOM_BYTES_PER_RECEIPT: u64 = 250 * 1024 * 1024 * 1024;
const SOURCE_QUIET_INTERVAL: Duration = Duration::from_secs(2);
const ARCHIVE_VERIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(5);
const ARCHIVE_VERIFICATION_NOTICE_INTERVAL: Duration = Duration::from_secs(2 * 60);

struct PrivacyPreparationProgress<'a> {
    state: &'a StateStore,
    run_id: &'a str,
    total_series: usize,
    last_report: Instant,
}

impl PrivacyPreparationProgress<'_> {
    fn checkpoint(&mut self, summary: &SourceSummary, processed_series: usize) -> Result<()> {
        self.state
            .update_run(self.run_id, "preparing", summary, None)?;
        if self.last_report.elapsed() >= Duration::from_secs(2)
            || processed_series == self.total_series
        {
            tracing::info!(
                processed_series,
                total_series = self.total_series,
                accepted = summary.accepted,
                held = summary.held,
                excluded = summary.excluded,
                "Privacy preparation progress"
            );
            self.last_report = Instant::now();
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingEnrollment {
    invite_sha256: String,
    #[serde(default)]
    api_origin: String,
    enrollment_id: String,
    device_token: String,
    device_name: String,
    client_version: String,
    platform: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContributorDetails {
    pub contact_email: String,
    pub contact_name: String,
    pub institution_name: String,
    pub institution_ror_id: Option<String>,
    pub lab_name: String,
    pub contact_opt_in: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DicomProcessingStatus {
    pub receipt_received_series: u64,
    pub reconciled_series: u64,
    pub queryable_receipts: u64,
    pub inaccessible_receipts: u64,
    pub status: String,
    pub queued_series: u64,
    pub processing_series: u64,
    pub processed_series: u64,
    pub failed_series: u64,
    pub purged_series: u64,
    pub processing_total_series: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingRegistration {
    api_origin: String,
    registration_id: String,
    device_token: String,
    device_name: String,
    client_version: String,
    platform: String,
    details: ContributorDetails,
    accepted_consent_policy_version: String,
}

#[derive(Clone)]
pub struct Runtime {
    pub paths: AppPaths,
    pub state: StateStore,
    _instance_lock: Arc<File>,
}

impl Runtime {
    pub fn initialize(state_root: Option<&Path>) -> Result<Self> {
        let paths = AppPaths::discover(state_root)?;
        paths.initialize()?;
        let instance_lock = open_private_instance_lock(&paths.lock)?;
        fs2::FileExt::try_lock_exclusive(&instance_lock)
            .context("another neuro-sync process is already using this state directory")?;
        cleanup_abandoned_workspaces(&paths.work)?;
        let state = StateStore::open(&paths.database)?;
        recover_interrupted_preparations(&paths, &state)?;
        Ok(Self {
            paths,
            state,
            _instance_lock: Arc::new(instance_lock),
        })
    }

    pub async fn enroll(
        &self,
        invite: String,
        api_url: &str,
        device_name: String,
    ) -> Result<ClientConfig> {
        let invite = invite.trim().to_owned();
        if invite.is_empty() {
            bail!("enrollment invite must not be empty");
        }
        let api_origin = normalize_base_url(api_url)?;
        let pending =
            load_or_create_pending_enrollment(&self.paths, &invite, &api_origin, device_name)?;
        let api = IngestApi::unauthenticated(&api_origin)?;
        let response = api
            .enroll(
                invite,
                pending.enrollment_id.clone(),
                pending.device_token.clone(),
                pending.device_name,
                pending.client_version,
                pending.platform,
            )
            .await?;
        if response.enrollment_id != pending.enrollment_id
            || response.device_token != pending.device_token
        {
            bail!("enrollment response did not return the client-bound enrollment identity");
        }
        let config = ClientConfig {
            api_url: api_origin,
            device_token: response.device_token,
            site_id: response.site_id,
            project_id: response.project_id,
            project_name: response.project_name,
            consent_policy_version: response.consent_policy_version,
            pseudonym_key_b64: response.pseudonym_key_b64,
        };
        config.save(&self.paths)?;
        match fs::remove_file(&self.paths.pending_enrollment) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => tracing::warn!("could not remove completed pending enrollment state"),
        }
        Ok(config)
    }

    pub async fn contribution_info(&self, api_url: &str) -> Result<ContributionInfo> {
        let api_origin = normalize_base_url(api_url)?;
        IngestApi::unauthenticated(&api_origin)?
            .contribution_info()
            .await
    }

    pub async fn register(
        &self,
        details: ContributorDetails,
        accepted_consent_policy_version: String,
        api_url: &str,
        device_name: String,
    ) -> Result<ClientConfig> {
        let details = normalize_contributor_details(details)?;
        let api_origin = normalize_base_url(api_url)?;
        let pending = load_or_create_pending_registration(
            &self.paths,
            &api_origin,
            device_name,
            details,
            accepted_consent_policy_version,
        )?;
        let request = RegisterRequest {
            registration_id: pending.registration_id.clone(),
            device_token: pending.device_token.clone(),
            device_name: pending.device_name.clone(),
            client_version: pending.client_version.clone(),
            platform: pending.platform.clone(),
            contact_email: pending.details.contact_email.clone(),
            contact_name: pending.details.contact_name.clone(),
            institution_name: pending.details.institution_name.clone(),
            institution_ror_id: pending.details.institution_ror_id.clone(),
            lab_name: pending.details.lab_name.clone(),
            contact_opt_in: pending.details.contact_opt_in,
            accepted_consent_policy_version: pending.accepted_consent_policy_version.clone(),
        };
        let response = IngestApi::unauthenticated(&api_origin)?
            .register(&request)
            .await?;
        if response.enrollment_id != pending.registration_id
            || response.device_token != pending.device_token
        {
            bail!("registration response did not return the client-bound device identity");
        }
        let config = ClientConfig {
            api_url: api_origin,
            device_token: response.device_token,
            site_id: response.site_id,
            project_id: response.project_id,
            project_name: response.project_name,
            consent_policy_version: response.consent_policy_version,
            pseudonym_key_b64: response.pseudonym_key_b64,
        };
        config.save(&self.paths)?;
        match fs::remove_file(&self.paths.pending_registration) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => tracing::warn!("could not remove completed pending registration state"),
        }
        Ok(config)
    }

    pub async fn sync_folder(&self, source: PathBuf, dry_run: bool) -> Result<String> {
        let canonical_source = source
            .canonicalize()
            .with_context(|| format!("could not open selected folder: {}", source.display()))?;
        if !canonical_source.is_dir() {
            bail!("selected source is not a folder");
        }
        let config = match ClientConfig::load(&self.paths) {
            Ok(config) => config,
            Err(_) if dry_run => {
                ClientConfig::unenrolled_local(load_or_create_dry_run_key(&self.paths)?)
            }
            Err(error) => return Err(error),
        };
        if let Some(run) = self
            .state
            .continuable_run_for_source(&canonical_source, dry_run)?
        {
            return self
                .continue_or_reprepare_folder_run(run, canonical_source, config)
                .await;
        }
        if let Some(run) = self
            .state
            .interrupted_preparation_for_source(&canonical_source, dry_run)?
        {
            tracing::info!(
                run_id = %run.id,
                "Continuing the interrupted local folder check"
            );
            self.state.restart_interrupted_preparation(&run.id)?;
            self.process_existing_run(&run.id, canonical_source, dry_run, config, None)
                .await?;
            return Ok(run.id);
        }
        if let Some(completed) = self
            .state
            .completed_run_for_source(&canonical_source, dry_run)?
        {
            if completed_run_matches_config(&completed, &config)
                && completed_result_uses_current_classifier(&completed)
            {
                if let Some(previous) = self.state.source_fingerprint(&completed.id)? {
                    tracing::info!(
                        files = previous.file_count,
                        "Checking whether this folder changed since its completed sync"
                    );
                    let mut comparison =
                        Progress::spinner("Checking completed folder", ProgressUnit::Files);
                    let current_snapshot =
                        snapshot_source_with_progress(&canonical_source, |progress| {
                            comparison.set(progress.files_seen)
                        })?;
                    comparison.finish_at(comparison.completed());
                    let current = current_snapshot.fingerprint(&canonical_source)?;
                    if current == previous {
                        tracing::info!(
                            run_id = %completed.id,
                            files = current.file_count,
                            "Folder is already fully synced; nothing will be prepared or uploaded"
                        );
                        return Ok(completed.id);
                    }
                    tracing::info!(
                        previous_files = previous.file_count,
                        current_files = current.file_count,
                        "Folder contents changed; checking the current export for new eligible scans"
                    );
                }
            } else if completed.status == "complete_no_eligible_series"
                || completed.status == "dry_run_complete"
            {
                tracing::info!(
                    previous_run_id = %completed.id,
                    "The classifier changed since this folder was last found ineligible; rechecking it automatically"
                );
            }
        }

        let run_id = Uuid::new_v4().to_string();
        self.state.create_run(&run_id, &canonical_source, dry_run)?;
        self.process_existing_run(&run_id, canonical_source, dry_run, config, None)
            .await?;
        Ok(run_id)
    }

    async fn continue_or_reprepare_folder_run(
        &self,
        run: RunRecord,
        source: PathBuf,
        config: ClientConfig,
    ) -> Result<String> {
        let manifest = load_checkpoint_manifest(&run)?;
        validate_manifest_context(&manifest, &config)?;
        let expected_bundle_ids = manifest
            .bundles
            .iter()
            .map(|bundle| bundle.bundle_id.clone())
            .collect::<HashSet<_>>();
        let saved_fingerprint = self.state.source_fingerprint(&run.id)?;
        let comparison_total = saved_fingerprint
            .as_ref()
            .map(|fingerprint| fingerprint.file_count)
            .unwrap_or(run.summary.files_seen);
        let mut comparison = Progress::bounded(
            "Checking checkpointed source",
            comparison_total,
            ProgressUnit::Files,
        );
        let current_snapshot =
            snapshot_source_with_progress(&source, |progress| comparison.set(progress.files_seen))?;
        comparison.finish_at(current_snapshot.fingerprint(&source)?.file_count);
        let current_fingerprint = current_snapshot.fingerprint(&source)?;
        let source_reason = match saved_fingerprint.as_ref() {
            None => Some("source_checkpoint_missing"),
            Some(saved) if saved != &current_fingerprint => Some("source_changed_since_checkpoint"),
            Some(_) => None,
        };
        if let Some(reason) = source_reason {
            let run_id = Uuid::new_v4().to_string();
            tracing::info!(
                old_run_id = %run.id,
                new_run_id = %run_id,
                reason,
                "The selected folder no longer matches its unfinished checkpoint; rechecking it from source"
            );
            self.state.supersede_run(&run.id, &run_id, reason)?;
            remove_bundle_cache(&self.paths, &run.id);
            self.process_existing_run(&run_id, source, run.dry_run, config, None)
                .await?;
            return Ok(run_id);
        }

        if manifest_uses_current_privacy_contract(&manifest) {
            if let Err(error) = verify_prepared_objects(&manifest).await {
                let run_id = Uuid::new_v4().to_string();
                tracing::warn!(
                    old_run_id = %run.id,
                    new_run_id = %run_id,
                    error = %error,
                    "A checkpointed archive failed its local hash check; safely rebuilding it before any upload"
                );
                self.state
                    .supersede_run(&run.id, &run_id, "prepared_archive_integrity_failed")?;
                remove_bundle_cache(&self.paths, &run.id);
                self.process_existing_run(
                    &run_id,
                    source,
                    run.dry_run,
                    config,
                    Some(expected_bundle_ids),
                )
                .await?;
                return Ok(run_id);
            }
            tracing::info!(
                run_id = %run.id,
                previous_status = %run.status,
                "Found a source-matched, hash-verified checkpoint; continuing its transfer"
            );
            self.continue_prepared_run_verified(&run, &manifest, &config)
                .await?;
            return Ok(run.id);
        }

        let run_id = Uuid::new_v4().to_string();
        tracing::info!(
            old_run_id = %run.id,
            new_run_id = %run_id,
            "The privacy contract changed; safely re-preparing the same folder"
        );
        self.state
            .supersede_run_for_repreparation(&run.id, &run_id)?;
        remove_bundle_cache(&self.paths, &run.id);
        self.process_existing_run(
            &run_id,
            source,
            run.dry_run,
            config,
            Some(expected_bundle_ids),
        )
        .await?;
        Ok(run_id)
    }

    async fn process_existing_run(
        &self,
        run_id: &str,
        source: PathBuf,
        dry_run: bool,
        config: ClientConfig,
        expected_bundle_ids: Option<HashSet<String>>,
    ) -> Result<()> {
        let state = self.state.clone();
        let paths = self.paths.clone();
        let prepare_id = run_id.to_owned();
        let prepare_config = config.clone();
        let preparation_task = tokio::task::spawn_blocking(move || {
            prepare_run(
                &paths,
                &state,
                &prepare_id,
                &source,
                dry_run,
                &prepare_config,
            )
        });
        let preparation = match preparation_task.await {
            Ok(preparation) => preparation,
            Err(error) => {
                let summary = self
                    .state
                    .run(run_id)?
                    .map(|run| run.summary)
                    .unwrap_or_default();
                self.state.update_run(
                    run_id,
                    "failed",
                    &summary,
                    Some("local_preparation_interrupted"),
                )?;
                remove_bundle_cache(&self.paths, run_id);
                return Err(error).context("local preparation task stopped unexpectedly");
            }
        };
        let (manifest, mut report) = match preparation {
            Ok(value) => value,
            Err(error) => {
                let summary = self
                    .state
                    .run(run_id)?
                    .map(|run| run.summary)
                    .unwrap_or_default();
                self.state.update_run(
                    run_id,
                    "failed",
                    &summary,
                    Some("local_preparation_failed"),
                )?;
                remove_bundle_cache(&self.paths, run_id);
                return Err(error);
            }
        };
        if let Some(expected_bundle_ids) = expected_bundle_ids {
            let prepared_bundle_ids = manifest
                .bundles
                .iter()
                .map(|bundle| bundle.bundle_id.clone())
                .collect::<HashSet<_>>();
            if prepared_bundle_ids != expected_bundle_ids {
                report.status = "failed".into();
                report.completed_at = Some(Utc::now().to_rfc3339());
                report
                    .errors
                    .push("source_changed_since_privacy_checkpoint".into());
                write_json(&self.paths.reports.join(format!("{run_id}.json")), &report)?;
                self.state.update_run(
                    run_id,
                    "failed",
                    &manifest.source_summary,
                    Some("source_changed_since_privacy_checkpoint"),
                )?;
                remove_bundle_cache(&self.paths, run_id);
                bail!("the selected source changed since the outdated privacy checkpoint");
            }
        }
        if dry_run || manifest.bundles.is_empty() {
            let status = if dry_run {
                "dry_run_complete"
            } else {
                "complete_no_eligible_series"
            };
            report.status = status.into();
            report.completed_at = Some(Utc::now().to_rfc3339());
            write_json(&self.paths.reports.join(format!("{run_id}.json")), &report)?;
            self.state
                .update_run(run_id, status, &manifest.source_summary, None)?;
            if !dry_run {
                remove_bundle_cache(&self.paths, run_id);
            }
            return Ok(());
        }
        // Archive creation has just streamed every byte through its recorded
        // SHA-256 and completed the post-write DICOM audit. Re-reading a fresh
        // multi-gigabyte archive here would add no recovery value. Checkpointed
        // runs take the separate verify_prepared_objects path before they reach
        // continue_upload_verified.
        if let Err(error) = self
            .continue_upload_verified(run_id, &manifest, &config)
            .await
        {
            report.status = "upload_failed".into();
            report.errors.push("upload_failed".into());
            write_json(&self.paths.reports.join(format!("{run_id}.json")), &report)?;
            self.state.update_run(
                run_id,
                "upload_failed",
                &manifest.source_summary,
                Some("upload_failed"),
            )?;
            return Err(error);
        }
        report.status = "complete".into();
        report.completed_at = Some(Utc::now().to_rfc3339());
        let chunks = self.state.run_uploads(run_id)?;
        report.worker_upload_ids = chunks
            .iter()
            .filter(|chunk| chunk.status == "committed")
            .filter_map(|chunk| chunk.worker_upload_id.clone())
            .collect();
        report.worker_upload_id = report.worker_upload_ids.first().cloned();
        report.existing_bundles = self.state.existing_bundles(run_id)?;
        report.archive_commit_count = chunks
            .iter()
            .filter(|chunk| chunk.status == "committed" && chunk.worker_upload_id.is_some())
            .count() as u64;
        write_json(&self.paths.reports.join(format!("{run_id}.json")), &report)?;
        self.state
            .update_run(run_id, "complete", &manifest.source_summary, None)?;
        remove_bundle_cache(&self.paths, run_id);
        Ok(())
    }

    async fn continue_upload_verified(
        &self,
        run_id: &str,
        manifest: &LocalManifest,
        config: &ClientConfig,
    ) -> Result<()> {
        let raw_bundles = manifest
            .bundles
            .iter()
            .filter(|bundle| bundle.is_dicom_archive())
            .count();
        if raw_bundles == manifest.bundles.len() {
            return self.continue_dicom_upload(run_id, manifest, config).await;
        }
        if raw_bundles != 0 {
            bail!("prepared manifest mixes legacy and DICOM archive bundle formats");
        }
        self.continue_legacy_upload(run_id, manifest, config).await
    }

    async fn continue_legacy_upload(
        &self,
        run_id: &str,
        manifest: &LocalManifest,
        config: &ClientConfig,
    ) -> Result<()> {
        validate_manifest_enrollment(manifest, config)?;
        let api = IngestApi::from_config(config)?;
        let bundle_subjects = manifest
            .bundles
            .iter()
            .map(|bundle| bundle.subject_id.clone())
            .collect::<Vec<_>>();
        let bundle_sizes = manifest
            .bundles
            .iter()
            .map(ManifestBundle::total_size)
            .collect::<Vec<_>>();
        self.state.ensure_run_uploads(
            run_id,
            &bundle_subjects,
            &bundle_sizes,
            MAX_LEGACY_BUNDLES_PER_UPLOAD,
            MAX_LEGACY_BYTES_PER_UPLOAD,
        )?;
        for chunk in self.state.run_uploads(run_id)? {
            if matches!(chunk.status.as_str(), "committed" | "reconciled") {
                continue;
            }
            let end = chunk.bundle_start + chunk.bundle_count;
            let bundles = manifest
                .bundles
                .get(chunk.bundle_start..end)
                .context("local upload chunk points outside the prepared manifest")?;
            self.continue_upload_chunk(run_id, &chunk, bundles, &manifest.client_version, &api)
                .await?;
        }
        if self
            .state
            .run_uploads(run_id)?
            .iter()
            .any(|chunk| !matches!(chunk.status.as_str(), "committed" | "reconciled"))
        {
            bail!("not every archive upload chunk committed");
        }
        Ok(())
    }

    async fn continue_dicom_upload(
        &self,
        run_id: &str,
        manifest: &LocalManifest,
        config: &ClientConfig,
    ) -> Result<()> {
        validate_manifest_enrollment(manifest, config)?;
        let api = IngestApi::from_config(config)?;
        let subjects = manifest
            .bundles
            .iter()
            .map(|bundle| bundle.subject_id.clone())
            .collect::<Vec<_>>();
        let sizes = manifest
            .bundles
            .iter()
            .map(ManifestBundle::total_size)
            .collect::<Vec<_>>();
        ensure_dicom_receipt_chunks(&self.state, run_id, &subjects, &sizes)?;
        for chunk in self.state.run_uploads(run_id)? {
            if matches!(chunk.status.as_str(), "committed" | "reconciled") {
                continue;
            }
            let end = chunk.bundle_start + chunk.bundle_count;
            let bundles = manifest
                .bundles
                .get(chunk.bundle_start..end)
                .context("local DICOM upload chunk points outside the prepared manifest")?;
            self.continue_dicom_upload_chunk(
                run_id,
                &chunk,
                bundles,
                &manifest.client_version,
                &api,
            )
            .await?;
        }
        if self
            .state
            .run_uploads(run_id)?
            .iter()
            .any(|chunk| !matches!(chunk.status.as_str(), "committed" | "reconciled"))
        {
            bail!("not every DICOM series archive was safely received");
        }
        Ok(())
    }

    async fn continue_dicom_upload_chunk(
        &self,
        run_id: &str,
        chunk: &crate::state::RunUploadRecord,
        bundles: &[ManifestBundle],
        preparation_client_version: &str,
        api: &IngestApi,
    ) -> Result<()> {
        if bundles.is_empty() || bundles.iter().any(|bundle| !bundle.is_dicom_archive()) {
            bail!("DICOM receipt chunk contains an invalid series archive");
        }
        let subject_id = &bundles[0].subject_id;
        if bundles
            .iter()
            .any(|bundle| &bundle.subject_id != subject_id)
        {
            bail!("DICOM receipt chunk must contain exactly one pseudonymous subject");
        }
        let completion_path = self.paths.reports.join(format!(
            "{run_id}.chunk-{}.complete-request.json",
            chunk.chunk_index
        ));

        if let (Some(upload_id), true) =
            (chunk.worker_upload_id.as_deref(), completion_path.is_file())
        {
            match api.dicom_status(upload_id).await {
                Ok(status) if dicom_receipt_complete(&status.status) => {
                    record_dicom_receipt(&self.state, run_id, chunk.chunk_index, bundles, &status)?;
                    return Ok(());
                }
                Ok(status) if matches!(status.status.as_str(), "created" | "uploading") => {
                    let saved: CompleteUploadRequest =
                        serde_json::from_slice(&fs::read(&completion_path)?)?;
                    let mut receipt = Progress::bounded(
                        "Confirming receipt and queueing",
                        bundles.len() as u64,
                        ProgressUnit::Series,
                    );
                    let status = api.complete_dicom_upload(upload_id, saved.objects).await?;
                    if !dicom_receipt_complete(&status.status) {
                        bail!("DICOM receipt API did not commit the transferred archives");
                    }
                    receipt.finish();
                    record_dicom_receipt(&self.state, run_id, chunk.chunk_index, bundles, &status)?;
                    tracing::info!(
                        "Durable series receipt confirmed; Sophont processing continues asynchronously"
                    );
                    return Ok(());
                }
                Ok(status) if status.status == "expired" => {
                    fs::remove_file(&completion_path)?;
                }
                Ok(status) if status.status == "withdrawn" => {
                    bail!("DICOM upload was withdrawn and cannot be continued");
                }
                Ok(status) => bail!(
                    "DICOM upload entered an unsupported server state: {}",
                    status.status
                ),
                Err(error) if is_transient_api_error(&error) => {
                    let saved: CompleteUploadRequest =
                        serde_json::from_slice(&fs::read(&completion_path)?)?;
                    let status = api.complete_dicom_upload(upload_id, saved.objects).await?;
                    if !dicom_receipt_complete(&status.status) {
                        bail!("DICOM receipt API did not commit the transferred archives");
                    }
                    record_dicom_receipt(&self.state, run_id, chunk.chunk_index, bundles, &status)?;
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }

        let (upload_id, object_prefix, descriptors, already_received, committed) =
            if let Some(upload_id) = chunk.worker_upload_id.as_deref() {
                match api.dicom_status(upload_id).await {
                    Ok(status) if dicom_receipt_complete(&status.status) => (
                        upload_id.to_owned(),
                        status.object_prefix.unwrap_or_default(),
                        Vec::new(),
                        status.already_received_series,
                        true,
                    ),
                    Ok(status) if status.status == "expired" => {
                        let created = api
                            .create_dicom_upload(
                                bundles,
                                preparation_client_version,
                                DICOM_METADATA_POLICY_ID,
                                DICOM_METADATA_POLICY_VERSION,
                            )
                            .await?;
                        (
                            created.upload_id,
                            created.object_prefix,
                            created.multipart_objects,
                            created.already_received_series,
                            dicom_receipt_complete(&created.status),
                        )
                    }
                    Ok(status) if status.status == "withdrawn" => {
                        bail!("DICOM upload was withdrawn and cannot be continued");
                    }
                    Ok(_) => {
                        let refreshed = api.refresh_dicom_credentials(upload_id).await?;
                        (
                            refreshed.upload_id,
                            refreshed.object_prefix,
                            refreshed.multipart_objects,
                            refreshed.already_received_series,
                            refreshed
                                .status
                                .as_deref()
                                .is_some_and(dicom_receipt_complete),
                        )
                    }
                    Err(error) if has_error_code(&error, "UPLOAD_NOT_WRITABLE") => {
                        let created = api
                            .create_dicom_upload(
                                bundles,
                                preparation_client_version,
                                DICOM_METADATA_POLICY_ID,
                                DICOM_METADATA_POLICY_VERSION,
                            )
                            .await?;
                        (
                            created.upload_id,
                            created.object_prefix,
                            created.multipart_objects,
                            created.already_received_series,
                            dicom_receipt_complete(&created.status),
                        )
                    }
                    Err(error) => return Err(error),
                }
            } else {
                let created = api
                    .create_dicom_upload(
                        bundles,
                        preparation_client_version,
                        DICOM_METADATA_POLICY_ID,
                        DICOM_METADATA_POLICY_VERSION,
                    )
                    .await?;
                (
                    created.upload_id,
                    created.object_prefix,
                    created.multipart_objects,
                    created.already_received_series,
                    dicom_receipt_complete(&created.status),
                )
            };
        if committed {
            validate_dicom_reconciliation(bundles, &already_received, object_prefix.is_empty())?;
            if object_prefix.is_empty() {
                self.state.set_chunk_reconciled(run_id, chunk.chunk_index)?;
                tracing::info!(
                    series = bundles.len(),
                    "Every series was already safely received from another workstation"
                );
            } else {
                self.state
                    .set_chunk_worker(run_id, chunk.chunk_index, &upload_id)?;
                self.state
                    .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
            }
            return Ok(());
        }
        let reconciled = validate_dicom_reconciliation(bundles, &already_received, false)?;
        let pending_bundles = bundles
            .iter()
            .filter(|bundle| !reconciled.contains(&bundle.bundle_id))
            .cloned()
            .collect::<Vec<_>>();
        if pending_bundles.is_empty() {
            self.state.set_chunk_reconciled(run_id, chunk.chunk_index)?;
            return Ok(());
        }
        self.state
            .set_chunk_worker(run_id, chunk.chunk_index, &upload_id)?;
        register_dicom_objects(
            &self.state,
            run_id,
            &upload_id,
            &object_prefix,
            &pending_bundles,
            &descriptors,
        )?;
        let objects = self.state.upload_objects(&upload_id)?;
        let uploader =
            MultipartUploader::new_dicom(api.clone(), self.state.clone(), upload_id.clone())?;
        let completed = uploader.upload_all(&objects, &descriptors).await?;
        let request = CompleteUploadRequest { objects: completed };
        write_json(&completion_path, &request)?;
        let mut receipt = Progress::bounded(
            "Confirming receipt and queueing",
            bundles.len() as u64,
            ProgressUnit::Series,
        );
        let status = api
            .complete_dicom_upload(&upload_id, request.objects)
            .await?;
        if !dicom_receipt_complete(&status.status) {
            bail!("DICOM receipt API did not commit the completed multipart upload");
        }
        receipt.finish();
        record_dicom_receipt(&self.state, run_id, chunk.chunk_index, bundles, &status)?;
        tracing::info!(
            "Durable series receipt confirmed; Sophont processing continues asynchronously"
        );
        Ok(())
    }

    async fn continue_upload_chunk(
        &self,
        run_id: &str,
        chunk: &crate::state::RunUploadRecord,
        bundles: &[ManifestBundle],
        preparation_client_version: &str,
        api: &IngestApi,
    ) -> Result<()> {
        let subject_id = bundles
            .first()
            .map(|bundle| bundle.subject_id.as_str())
            .context("local upload chunk contains no bundles")?;
        if bundles.iter().any(|bundle| bundle.subject_id != subject_id) {
            bail!("local upload chunk must contain exactly one pseudonymous subject");
        }
        let completion_path = self.paths.reports.join(format!(
            "{run_id}.chunk-{}.complete-request.json",
            chunk.chunk_index
        ));
        if completion_path.is_file() {
            if let Some(upload_id) = chunk.worker_upload_id.as_deref() {
                let recover_saved_completion = match api.status(upload_id).await {
                    Ok(status) if matches!(status.status.as_str(), "committed" | "complete") => {
                        self.state
                            .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
                        return Ok(());
                    }
                    Ok(status) if matches!(status.status.as_str(), "created" | "uploading") => true,
                    Ok(status) if status.status == "expired" => false,
                    Ok(status) if status.status == "withdrawn" => {
                        bail!("archive upload was withdrawn and cannot be continued");
                    }
                    Ok(status) => bail!(
                        "archive upload entered an unsupported server state: {}",
                        status.status
                    ),
                    Err(error) if has_error_code(&error, "UPLOAD_NOT_WRITABLE") => false,
                    Err(error) if is_transient_api_error(&error) => true,
                    Err(error) => return Err(error),
                };
                if recover_saved_completion {
                    let saved: CompleteUploadRequest =
                        serde_json::from_slice(&fs::read(&completion_path)?)?;
                    tracing::info!(
                        "Found the completed transfer checkpoint; continuing server verification"
                    );
                    let status = match complete_with_recovery(api, upload_id, &saved.objects).await
                    {
                        Ok(status) => status,
                        Err(error)
                            if reconcile_completion_duplicate(
                                &self.state,
                                run_id,
                                bundles,
                                &error,
                            )? =>
                        {
                            self.state
                                .set_chunk_status(run_id, chunk.chunk_index, "reconciled")?;
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    };
                    if matches!(status.status.as_str(), "committed" | "complete") {
                        self.state
                            .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
                        tracing::info!("Archive verification and commit complete");
                        return Ok(());
                    }
                    bail!("ingest API did not commit the saved completion request");
                }
            }
        }
        let plan: Option<UploadSessionPlan> = if let Some(upload_id) =
            chunk.worker_upload_id.as_deref()
        {
            match api.status(upload_id).await {
                Err(error) if has_error_code(&error, "UPLOAD_NOT_WRITABLE") => {
                    create_session_reconciling(
                        &self.state,
                        run_id,
                        api,
                        bundles,
                        preparation_client_version,
                        true,
                    )
                    .await?
                }
                Err(error) => return Err(error),
                Ok(status) if status.status == "committed" => Some((
                    upload_id.to_owned(),
                    status.object_prefix.unwrap_or_default(),
                    Vec::new(),
                    true,
                    false,
                )),
                Ok(status) if status.status == "expired" => {
                    create_session_reconciling(
                        &self.state,
                        run_id,
                        api,
                        bundles,
                        preparation_client_version,
                        true,
                    )
                    .await?
                }
                Ok(status) if status.status == "withdrawn" => {
                    bail!("archive upload was withdrawn and cannot be continued");
                }
                Ok(_) => Some(match api.refresh_credentials(upload_id).await {
                    Ok(refreshed) => (
                        refreshed.upload_id,
                        refreshed.object_prefix,
                        refreshed.multipart_objects,
                        false,
                        false,
                    ),
                    Err(error) if has_error_code(&error, "UPLOAD_NOT_WRITABLE") => {
                        let Some(recreated) = create_session_reconciling(
                            &self.state,
                            run_id,
                            api,
                            bundles,
                            preparation_client_version,
                            true,
                        )
                        .await?
                        else {
                            self.state
                                .set_chunk_status(run_id, chunk.chunk_index, "reconciled")?;
                            return Ok(());
                        };
                        recreated
                    }
                    Err(error) => return Err(error),
                }),
            }
        } else {
            create_session_reconciling(
                &self.state,
                run_id,
                api,
                bundles,
                preparation_client_version,
                false,
            )
            .await?
        };
        let Some((worker_upload_id, object_prefix, descriptors, committed, revived)) = plan else {
            self.state
                .set_chunk_status(run_id, chunk.chunk_index, "reconciled")?;
            return Ok(());
        };
        self.state
            .set_chunk_worker(run_id, chunk.chunk_index, &worker_upload_id)?;
        if committed {
            self.state
                .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
            return Ok(());
        }
        if revived && completion_path.is_file() {
            fs::remove_file(&completion_path)?;
        }
        if completion_path.is_file() {
            let saved: CompleteUploadRequest =
                serde_json::from_slice(&fs::read(&completion_path)?)?;
            tracing::info!("Asking Scaling Neuro to verify and commit the transferred files");
            let status = match complete_with_recovery(api, &worker_upload_id, &saved.objects).await
            {
                Ok(status) => status,
                Err(error)
                    if reconcile_completion_duplicate(&self.state, run_id, bundles, &error)? =>
                {
                    self.state
                        .set_chunk_status(run_id, chunk.chunk_index, "reconciled")?;
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if status.status == "committed" || status.status == "complete" {
                self.state
                    .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
                tracing::info!("Archive verification and commit complete");
                return Ok(());
            }
            bail!("ingest API did not commit the saved completion request");
        }
        register_objects(
            &self.state,
            run_id,
            &worker_upload_id,
            &object_prefix,
            bundles,
            &descriptors,
        )?;
        let objects = self.state.upload_objects(&worker_upload_id)?;
        let uploader =
            MultipartUploader::new(api.clone(), self.state.clone(), worker_upload_id.clone())?;
        let completed = uploader.upload_all(&objects, &descriptors).await?;
        let request = CompleteUploadRequest { objects: completed };
        write_json(&completion_path, &request)?;
        tracing::info!("Transfer complete; verifying hashes and committing the archive");
        let status = match complete_with_recovery(api, &worker_upload_id, &request.objects).await {
            Ok(status) => status,
            Err(error) if reconcile_completion_duplicate(&self.state, run_id, bundles, &error)? => {
                self.state
                    .set_chunk_status(run_id, chunk.chunk_index, "reconciled")?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if status.status != "committed" && status.status != "complete" {
            bail!("ingest API did not commit the completed multipart upload");
        }
        self.state
            .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
        tracing::info!("Archive verification and commit complete");
        Ok(())
    }

    async fn continue_prepared_run_verified(
        &self,
        run: &RunRecord,
        manifest: &LocalManifest,
        config: &ClientConfig,
    ) -> Result<()> {
        match self
            .continue_upload_verified(&run.id, manifest, config)
            .await
        {
            Ok(()) => {
                self.state
                    .update_run(&run.id, "complete", &manifest.source_summary, None)?;
                let chunks = self.state.run_uploads(&run.id)?;
                let worker_upload_ids = chunks
                    .iter()
                    .filter(|chunk| chunk.status == "committed")
                    .filter_map(|chunk| chunk.worker_upload_id.clone())
                    .collect();
                let committed = chunks
                    .iter()
                    .filter(|chunk| chunk.status == "committed" && chunk.worker_upload_id.is_some())
                    .count() as u64;
                let existing_bundles = self.state.existing_bundles(&run.id)?;
                update_report_status(
                    &self.paths,
                    &run.id,
                    "complete",
                    worker_upload_ids,
                    existing_bundles,
                    committed,
                )?;
                remove_bundle_cache(&self.paths, &run.id);
                Ok(())
            }
            Err(error) => {
                self.state.update_run(
                    &run.id,
                    "upload_failed",
                    &manifest.source_summary,
                    Some("upload_failed"),
                )?;
                Err(error)
            }
        }
    }

    pub fn run_record(&self, id: Option<&str>) -> Result<Option<RunRecord>> {
        match id {
            Some(id) => self.state.run(id),
            None => self.state.latest_run(),
        }
    }

    pub fn report(&self, id: Option<&str>) -> Result<RunReport> {
        let run = self.run_record(id)?.context("no matching run was found")?;
        let path = run
            .report_path
            .context("run has not produced a report yet")?;
        serde_json::from_slice(&fs::read(path)?).context("saved run report is invalid")
    }

    pub async fn dicom_processing_status(
        &self,
        run: &RunRecord,
    ) -> Result<Option<DicomProcessingStatus>> {
        if run.dry_run {
            return Ok(None);
        }
        let Ok(manifest) = load_checkpoint_manifest(run) else {
            return Ok(None);
        };
        if manifest.bundles.is_empty()
            || !manifest
                .bundles
                .iter()
                .all(ManifestBundle::is_dicom_archive)
        {
            return Ok(None);
        }
        let chunks = self.state.run_uploads(&run.id)?;
        let mut summary = DicomProcessingStatus {
            receipt_received_series: chunks
                .iter()
                .filter(|chunk| matches!(chunk.status.as_str(), "committed" | "reconciled"))
                .map(|chunk| chunk.bundle_count as u64)
                .sum(),
            reconciled_series: chunks
                .iter()
                .filter(|chunk| chunk.status == "reconciled")
                .map(|chunk| chunk.bundle_count as u64)
                .sum(),
            ..Default::default()
        };
        let queryable = chunks
            .iter()
            .filter(|chunk| chunk.status == "committed")
            .filter_map(|chunk| chunk.worker_upload_id.as_deref())
            .collect::<Vec<_>>();
        if queryable.is_empty() {
            summary.status = if summary.receipt_received_series > 0 {
                "received_processing_not_queryable_from_this_workstation".into()
            } else {
                "not_received".into()
            };
            return Ok(Some(summary));
        }
        let config = ClientConfig::load(&self.paths)?;
        let api = IngestApi::from_config(&config)?;
        for upload_id in queryable {
            match api.dicom_status(upload_id).await {
                Ok(remote) => {
                    summary.queryable_receipts += 1;
                    if let Some(processing) = remote.processing {
                        summary.queued_series += u64::from(processing.queued_series);
                        summary.processing_series += u64::from(processing.processing_series);
                        summary.processed_series += u64::from(processing.processed_series);
                        summary.failed_series += u64::from(processing.failed_series);
                        summary.purged_series += u64::from(processing.purged_series);
                        summary.processing_total_series += u64::from(processing.total_series);
                    }
                }
                Err(error) if is_not_found_api_error(&error) => {
                    // Older clients could save the winning receipt ID from a
                    // different workstation. It is durable but intentionally
                    // not queryable using this device credential.
                    summary.inaccessible_receipts += 1;
                }
                Err(error) => return Err(error),
            }
        }
        summary.status = if summary.purged_series > 0 {
            "purged_or_partially_purged".into()
        } else if summary.failed_series > 0 {
            "failed".into()
        } else if summary.processing_series > 0 {
            "processing".into()
        } else if summary.processing_total_series > 0
            && summary.processed_series == summary.processing_total_series
        {
            "processed".into()
        } else if summary.queued_series > 0 {
            "queued".into()
        } else if summary.queryable_receipts > 0 {
            "received".into()
        } else {
            "received_processing_not_queryable_from_this_workstation".into()
        };
        Ok(Some(summary))
    }
}

fn open_private_instance_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).with_context(|| {
        format!(
            "could not create private instance lock at {}",
            path.display()
        )
    })?;
    privacy::restrict_file(path)?;
    Ok(file)
}

fn load_or_create_pending_enrollment(
    paths: &AppPaths,
    invite: &str,
    api_origin: &str,
    device_name: String,
) -> Result<PendingEnrollment> {
    let invite_sha256 = hex::encode(Sha256::digest(invite.as_bytes()));
    if paths.pending_enrollment.is_file() {
        privacy::restrict_file(&paths.pending_enrollment)?;
        let mut pending: PendingEnrollment = serde_json::from_slice(
            &fs::read(&paths.pending_enrollment)
                .context("could not read pending enrollment state")?,
        )
        .context("pending enrollment state is invalid")?;
        if pending.invite_sha256 == invite_sha256 && pending.api_origin == api_origin {
            validate_pending_enrollment(&pending)?;
            let current_platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
            if pending.client_version != crate::CLIENT_VERSION
                || pending.platform != current_platform
            {
                // Preserve the replay-bound enrollment UUID and device token,
                // but let an upgraded client recover a response lost under an
                // older minimum-version contract.
                pending.client_version = crate::CLIENT_VERSION.into();
                pending.platform = current_platform;
                write_private_json_atomic(&paths.pending_enrollment, &pending)?;
            }
            return Ok(pending);
        }
    }

    let mut token_bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut token_bytes);
    let device_token = format!("sn_device_{}", URL_SAFE_NO_PAD.encode(token_bytes));
    token_bytes.zeroize();
    let pending = PendingEnrollment {
        invite_sha256,
        api_origin: api_origin.into(),
        enrollment_id: Uuid::new_v4().to_string(),
        device_token,
        device_name,
        client_version: crate::CLIENT_VERSION.into(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    };
    validate_pending_enrollment(&pending)?;
    write_private_json_atomic(&paths.pending_enrollment, &pending)?;
    Ok(pending)
}

fn normalize_contributor_details(mut details: ContributorDetails) -> Result<ContributorDetails> {
    details.contact_email = details.contact_email.trim().to_lowercase();
    details.contact_name = details.contact_name.trim().to_owned();
    details.institution_name = details.institution_name.trim().to_owned();
    details.lab_name = details.lab_name.trim().to_owned();
    details.institution_ror_id = details
        .institution_ror_id
        .take()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty());
    if !details.contact_email.contains('@')
        || details.contact_email.len() > 254
        || details.contact_email.contains(char::is_whitespace)
        || details.contact_email.contains("..")
    {
        bail!("contact email is invalid");
    }
    for (name, value, maximum) in [
        ("contact name", &details.contact_name, 96),
        ("institution", &details.institution_name, 160),
        ("lab name", &details.lab_name, 160),
    ] {
        if value.is_empty()
            || value.chars().count() > maximum
            || value.chars().any(char::is_control)
        {
            bail!("{name} is invalid");
        }
    }
    if let Some(ror) = &details.institution_ror_id {
        let suffix = ror.strip_prefix("https://ror.org/0");
        if ror.len() != 25
            || !suffix.is_some_and(|value| {
                value.len() == 8
                    && value.bytes().all(|byte| {
                        byte.is_ascii_digit()
                            || matches!(byte, b'a'..=b'h' | b'j'..=b'k' | b'm'..=b'n' | b'p'..=b't' | b'v'..=b'z')
                    })
            })
        {
            bail!("institution ROR ID is invalid");
        }
    }
    Ok(details)
}

fn load_or_create_pending_registration(
    paths: &AppPaths,
    api_origin: &str,
    device_name: String,
    details: ContributorDetails,
    accepted_consent_policy_version: String,
) -> Result<PendingRegistration> {
    if paths.pending_registration.is_file() {
        privacy::restrict_file(&paths.pending_registration)?;
        let mut pending: PendingRegistration = serde_json::from_slice(
            &fs::read(&paths.pending_registration)
                .context("could not read pending registration state")?,
        )
        .context("pending registration state is invalid")?;
        // The email identifies the human retry. Preserve the entire first
        // request if they retype another field differently after a lost
        // response, because the Worker binds replays to that exact request.
        if pending.api_origin == api_origin
            && pending.details.contact_email == details.contact_email
            && pending.accepted_consent_policy_version == accepted_consent_policy_version
        {
            validate_pending_registration(&pending)?;
            let current_platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
            if pending.client_version != crate::CLIENT_VERSION
                || pending.platform != current_platform
            {
                pending.client_version = crate::CLIENT_VERSION.into();
                pending.platform = current_platform;
                write_private_json_atomic(&paths.pending_registration, &pending)?;
            }
            return Ok(pending);
        }
    }
    let mut token_bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut token_bytes);
    let device_token = format!("sn_device_{}", URL_SAFE_NO_PAD.encode(token_bytes));
    token_bytes.zeroize();
    let pending = PendingRegistration {
        api_origin: api_origin.into(),
        registration_id: Uuid::new_v4().to_string(),
        device_token,
        device_name,
        client_version: crate::CLIENT_VERSION.into(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        details,
        accepted_consent_policy_version,
    };
    validate_pending_registration(&pending)?;
    write_private_json_atomic(&paths.pending_registration, &pending)?;
    Ok(pending)
}

fn validate_pending_registration(pending: &PendingRegistration) -> Result<()> {
    let device_token_suffix = pending.device_token.strip_prefix("sn_device_");
    if normalize_base_url(&pending.api_origin)? != pending.api_origin
        || Uuid::parse_str(&pending.registration_id)
            .ok()
            .and_then(|value| value.get_version())
            != Some(uuid::Version::Random)
        || !device_token_suffix.is_some_and(|value| {
            value.len() == 43
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        || pending.device_name.trim().is_empty()
        || pending.device_name.chars().count() > 96
        || pending.device_name.chars().any(char::is_control)
        || pending.client_version.is_empty()
        || pending.platform.is_empty()
        || pending.accepted_consent_policy_version.is_empty()
        || pending.accepted_consent_policy_version.len() > 64
    {
        bail!("pending registration state is invalid");
    }
    normalize_contributor_details(pending.details.clone())?;
    Ok(())
}

fn validate_pending_enrollment(pending: &PendingEnrollment) -> Result<()> {
    if pending.invite_sha256.len() != 64
        || !pending
            .invite_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("pending enrollment invite key is invalid");
    }
    if normalize_base_url(&pending.api_origin)? != pending.api_origin {
        bail!("pending enrollment API origin is invalid");
    }
    let enrollment_id = Uuid::parse_str(&pending.enrollment_id)
        .context("pending enrollment identifier is invalid")?;
    if enrollment_id.get_version() != Some(uuid::Version::Random) {
        bail!("pending enrollment identifier is not a UUIDv4");
    }
    let token = pending
        .device_token
        .strip_prefix("sn_device_")
        .context("pending enrollment device token is invalid")?;
    if token.len() != 43
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("pending enrollment device token is invalid");
    }
    if pending.device_name.trim().is_empty()
        || pending.device_name.chars().count() > 96
        || pending.client_version.is_empty()
        || pending.platform.is_empty()
    {
        bail!("pending enrollment metadata is invalid");
    }
    Ok(())
}

fn write_private_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("pending enrollment state has no parent directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".pending-enrollment-")
        .tempfile_in(parent)
        .context("could not create private pending enrollment state")?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    privacy::restrict_file(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|_| anyhow::anyhow!("could not commit private pending enrollment state"))?;
    privacy::restrict_file(path)
}

fn ensure_dicom_receipt_chunks(
    state: &StateStore,
    run_id: &str,
    subjects: &[String],
    sizes: &[u64],
) -> Result<()> {
    if sizes
        .iter()
        .any(|size| *size > MAX_DICOM_ARCHIVE_BYTES_PER_SERIES)
    {
        bail!("one DICOM series archive exceeds the 64 GiB object limit");
    }
    state.ensure_run_uploads(
        run_id,
        subjects,
        sizes,
        MAX_DICOM_SERIES_PER_UPLOAD,
        MAX_DICOM_BYTES_PER_RECEIPT,
    )
}

type UploadSessionPlan = (String, String, Vec<MultipartObject>, bool, bool);
const MINIMUM_PRIVACY_CLIENT_VERSION: &str = "0.1.1";

fn load_checkpoint_manifest(run: &RunRecord) -> Result<LocalManifest> {
    let path = run
        .manifest_path
        .as_deref()
        .context("prepared run has no local manifest")?;
    let manifest: LocalManifest =
        serde_json::from_slice(&fs::read(path)?).context("prepared run manifest is invalid")?;
    if manifest.run_id != run.id {
        bail!("prepared run manifest identity does not match local state");
    }
    Ok(manifest)
}

async fn verify_prepared_objects(manifest: &LocalManifest) -> Result<()> {
    let objects = manifest
        .bundles
        .iter()
        .flat_map(ManifestBundle::upload_objects)
        .cloned()
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || verify_prepared_object_files(&objects))
        .await
        .context("prepared archive verification task stopped unexpectedly")?
}

fn verify_prepared_object_files(objects: &[crate::model::ManifestObject]) -> Result<()> {
    let total = objects.iter().try_fold(0_u64, |total, object| {
        total
            .checked_add(object.size)
            .context("prepared archive byte total overflow")
    })?;
    let mut progress = Progress::bounded(
        "Verifying prepared archive hashes",
        total,
        ProgressUnit::Bytes,
    );
    let mut buffer = vec![0_u8; 1024 * 1024];
    for object in objects {
        let path = Path::new(&object.local_path);
        let metadata = fs::metadata(path)
            .with_context(|| format!("prepared archive is missing: {}", path.display()))?;
        if !metadata.is_file() || metadata.len() != object.size {
            bail!(
                "prepared archive size does not match its checkpoint: {}",
                path.display()
            );
        }
        let mut reader = BufReader::with_capacity(1024 * 1024, File::open(path)?);
        let mut digest = Sha256::new();
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
            progress.inc(read as u64);
        }
        let actual = hex::encode(digest.finalize());
        if actual != object.sha256.to_ascii_lowercase() {
            bail!(
                "prepared archive hash does not match its checkpoint: {}",
                path.display()
            );
        }
    }
    progress.finish();
    Ok(())
}

fn manifest_uses_current_privacy_contract(manifest: &LocalManifest) -> bool {
    if !privacy_client_version_supported(&manifest.client_version) {
        return false;
    }
    if manifest.metadata_policy.policy_id == DICOM_METADATA_POLICY_ID {
        return manifest.schema_version == MANIFEST_SCHEMA_VERSION
            && manifest.metadata_policy.policy_version == DICOM_METADATA_POLICY_VERSION
            && manifest.bundles.iter().all(|bundle| {
                let Some(archive) = bundle.archive.as_ref() else {
                    return false;
                };
                bundle.is_dicom_archive()
                    && archive.format == crate::archive::DICOM_ARCHIVE_FORMAT
                    && archive.deidentification_profile == DICOM_METADATA_POLICY_ID
                    && archive.deidentification_profile_version == DICOM_METADATA_POLICY_VERSION
                    && archive.dicom_instance_count == bundle.source_dicom_count
                    && Path::new(&archive.object.local_path).is_file()
            });
    }
    if manifest.metadata_policy.policy_id != METADATA_POLICY_ID
        || manifest.metadata_policy.policy_version != METADATA_POLICY_VERSION
    {
        return false;
    }
    manifest.bundles.iter().all(|bundle| {
        let Some(metadata) = bundle.metadata.as_ref() else {
            return false;
        };
        let Ok(bytes) = fs::read(&metadata.local_path) else {
            return false;
        };
        let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return false;
        };
        let Ok(sidecar) = serde_json::from_value::<ScanSidecar>(raw.clone()) else {
            return false;
        };
        let Ok(roundtrip) = serde_json::to_value(&sidecar) else {
            return false;
        };
        raw == roundtrip
            && sidecar.schema_version == crate::SIDECAR_SCHEMA_VERSION
            && sidecar.bundle_id == bundle.bundle_id
            && sidecar.series_id == bundle.series_id
            && sidecar.subject_id == bundle.subject_id
            && sidecar.session_id == bundle.session_id
            && sidecar.protocol_group_id == bundle.protocol_group_id
            && sidecar.metadata_policy.policy_id == METADATA_POLICY_ID
            && sidecar.metadata_policy.policy_version == METADATA_POLICY_VERSION
            && sidecar.conversion.client_version == manifest.client_version
            && privacy_client_version_supported(&sidecar.conversion.client_version)
    })
}

fn privacy_client_version_supported(value: &str) -> bool {
    let Ok(version) = semver::Version::parse(value) else {
        return false;
    };
    let minimum = semver::Version::parse(MINIMUM_PRIVACY_CLIENT_VERSION)
        .expect("minimum privacy client version must be valid semver");
    version >= minimum
}

fn validate_manifest_context(manifest: &LocalManifest, config: &ClientConfig) -> Result<()> {
    if manifest.site_id != config.site_id || manifest.project_id != config.project_id {
        bail!("prepared run belongs to a different enrolled site or project");
    }
    if manifest.consent_policy_version.is_empty()
        || manifest.consent_policy_version != config.consent_policy_version
    {
        bail!("prepared run requires approval under the current contribution policy");
    }
    Ok(())
}

fn validate_manifest_enrollment(manifest: &LocalManifest, config: &ClientConfig) -> Result<()> {
    validate_manifest_context(manifest, config)?;
    if !manifest_uses_current_privacy_contract(manifest) {
        bail!("prepared run requires repreparation under the current privacy contract");
    }
    Ok(())
}

fn completed_run_matches_config(run: &RunRecord, config: &ClientConfig) -> bool {
    load_checkpoint_manifest(run).is_ok_and(|manifest| {
        manifest.site_id == config.site_id
            && manifest.project_id == config.project_id
            && !manifest.consent_policy_version.is_empty()
            && manifest.consent_policy_version == config.consent_policy_version
    })
}

fn completed_result_uses_current_classifier(run: &RunRecord) -> bool {
    if run.status == "complete" {
        return true;
    }
    load_checkpoint_manifest(run).is_ok_and(|manifest| {
        manifest.client_version == crate::CLIENT_VERSION
            && manifest.schema_version == MANIFEST_SCHEMA_VERSION
            && manifest.metadata_policy.policy_id == DICOM_METADATA_POLICY_ID
            && manifest.metadata_policy.policy_version == DICOM_METADATA_POLICY_VERSION
    })
}

fn session_from_created(created: CreateUploadResponse, revived: bool) -> UploadSessionPlan {
    let committed = created.status == "committed";
    (
        created.upload_id,
        created.object_prefix,
        created.multipart_objects,
        committed,
        revived,
    )
}

async fn create_session_reconciling(
    state: &StateStore,
    run_id: &str,
    api: &IngestApi,
    bundles: &[ManifestBundle],
    client_version: &str,
    revived: bool,
) -> Result<Option<UploadSessionPlan>> {
    let mut remaining = bundles.to_vec();
    for _ in 0..=bundles.len() {
        match api.create_upload(&remaining, client_version).await {
            Ok(created) => return Ok(Some(session_from_created(created, revived))),
            Err(error) => {
                let Some(existing) = error
                    .downcast_ref::<crate::api::ApiFailure>()
                    .and_then(crate::api::ApiFailure::exact_existing_bundles)
                else {
                    return Err(error);
                };
                let reconciled_ids = validate_existing_bundles(&remaining, existing)?;
                state.record_existing_bundles(run_id, existing)?;
                remaining.retain(|bundle| !reconciled_ids.contains(&bundle.bundle_id));
                if remaining.is_empty() {
                    return Ok(None);
                }
            }
        }
    }
    bail!("ingest API duplicate reconciliation did not converge")
}

fn reconcile_completion_duplicate(
    state: &StateStore,
    run_id: &str,
    bundles: &[ManifestBundle],
    error: &anyhow::Error,
) -> Result<bool> {
    let Some(existing) = error
        .downcast_ref::<crate::api::ApiFailure>()
        .and_then(crate::api::ApiFailure::exact_existing_bundles)
    else {
        return Ok(false);
    };
    let mut reconciled_ids = validate_existing_bundles(bundles, existing)?;
    let previously_recorded = state
        .existing_bundles(run_id)?
        .into_iter()
        .filter(|item| {
            bundles
                .iter()
                .any(|bundle| bundle.bundle_id == item.bundle_id)
        })
        .collect::<Vec<_>>();
    if !previously_recorded.is_empty() {
        reconciled_ids.extend(validate_existing_bundles(bundles, &previously_recorded)?);
    }
    if reconciled_ids.len() != bundles.len() {
        bail!("completion-time duplicate reconciliation omitted a prepared bundle");
    }
    state.record_existing_bundles(run_id, existing)?;
    Ok(true)
}

async fn complete_with_recovery(
    api: &IngestApi,
    upload_id: &str,
    objects: &[CompletedObject],
) -> Result<UploadStatus> {
    complete_with_recovery_polling(api, upload_id, objects, ARCHIVE_VERIFICATION_POLL_INTERVAL)
        .await
}

async fn complete_with_recovery_polling(
    api: &IngestApi,
    upload_id: &str,
    objects: &[CompletedObject],
    poll_interval: Duration,
) -> Result<UploadStatus> {
    let mut last_progress: Option<VerificationProgress> = None;
    let mut last_notice = Instant::now() - ARCHIVE_VERIFICATION_NOTICE_INTERVAL;
    loop {
        let should_backoff = match api.complete_upload(upload_id, objects.to_vec()).await {
            Ok(status) if matches!(status.status.as_str(), "committed" | "complete") => {
                return Ok(status);
            }
            Ok(status) if matches!(status.status.as_str(), "created" | "uploading") => {
                !log_archive_verification_progress(&status, &mut last_progress, &mut last_notice)
            }
            Ok(status) => return Ok(status),
            Err(error) if is_transient_api_error(&error) => match api.status(upload_id).await {
                Ok(status) if matches!(status.status.as_str(), "committed" | "complete") => {
                    return Ok(status);
                }
                Ok(status) if matches!(status.status.as_str(), "created" | "uploading") => {
                    !log_archive_verification_progress(
                        &status,
                        &mut last_progress,
                        &mut last_notice,
                    )
                }
                Ok(status) => return Ok(status),
                Err(status_error) if is_transient_api_error(&status_error) => {
                    log_archive_verification_status_retry(&mut last_notice);
                    true
                }
                Err(status_error) => return Err(status_error),
            },
            Err(error) => return Err(error),
        };
        // A successful state transition releases its Worker lease, so drive
        // the next durable step immediately. Back off only when another
        // request is active or the control plane is temporarily unavailable.
        if should_backoff {
            tokio::time::sleep(poll_interval).await;
        }
    }
}

fn log_archive_verification_progress(
    status: &UploadStatus,
    last_progress: &mut Option<VerificationProgress>,
    last_notice: &mut Instant,
) -> bool {
    let progress = status.verification.clone();
    let changed = progress != *last_progress;
    if changed || last_notice.elapsed() >= ARCHIVE_VERIFICATION_NOTICE_INTERVAL {
        if let Some(progress) = progress.as_ref() {
            if let (Some(phase), Some(finalized_series)) =
                (progress.phase.as_deref(), progress.finalized_series)
            {
                tracing::info!(
                    phase,
                    finalized_series,
                    verified_series = progress.verified_series,
                    total_series = progress.total_series,
                    "Server archive verification progress; transferred files remain checkpointed"
                );
            } else {
                tracing::info!(
                    verified_series = progress.verified_series,
                    total_series = progress.total_series,
                    "Server archive verification progress; transferred files remain checkpointed"
                );
            }
        } else {
            tracing::info!(
                "Server archive verification is still running; transferred files remain checkpointed"
            );
        }
        *last_progress = progress;
        *last_notice = Instant::now();
    }
    changed
}

fn log_archive_verification_status_retry(last_notice: &mut Instant) {
    if last_notice.elapsed() >= ARCHIVE_VERIFICATION_NOTICE_INTERVAL {
        tracing::info!(
            "Reconnecting to server archive verification; transferred files remain checkpointed"
        );
        *last_notice = Instant::now();
    }
}

fn validate_existing_bundles(
    requested: &[ManifestBundle],
    existing: &[ExistingArchiveBundle],
) -> Result<std::collections::HashSet<String>> {
    if existing.is_empty() || existing.len() > requested.len() {
        bail!("ingest API returned an invalid existing-bundle reconciliation");
    }
    let mut ids = std::collections::HashSet::with_capacity(existing.len());
    for item in existing {
        if !ids.insert(item.bundle_id.clone()) {
            bail!("ingest API repeated an existing bundle identity");
        }
        let requested = requested
            .iter()
            .find(|bundle| bundle.bundle_id == item.bundle_id)
            .context("ingest API reconciled a bundle that was not requested")?;
        let expected_uncompressed = requested
            .nifti
            .as_ref()
            .context("legacy archive reconciliation requires a NIfTI object")?
            .uncompressed_sha256
            .as_deref()
            .context("prepared NIfTI has no scientific-content hash")?;
        if item.series_id != requested.series_id
            || item.subject_id != requested.subject_id
            || item.session_id != requested.session_id
            || item.protocol_group_id != requested.protocol_group_id
            || item.nii_uncompressed_sha256 != expected_uncompressed
            || Uuid::parse_str(&item.upload_id).is_err()
        {
            bail!("existing archive bundle identity does not match the prepared scan");
        }
    }
    Ok(ids)
}

fn prepare_run(
    paths: &AppPaths,
    state: &StateStore,
    run_id: &str,
    source: &Path,
    _dry_run: bool,
    config: &ClientConfig,
) -> Result<(LocalManifest, RunReport)> {
    let started_at = Utc::now().to_rfc3339();
    state.update_run(run_id, "discovering", &SourceSummary::default(), None)?;
    let mut discovery_phase = DiscoveryPhase::Inventory;
    let mut discovery_progress =
        Progress::spinner("Inventorying source files", ProgressUnit::Files);
    let discovery = discover_with_progress(source, |progress| {
        if progress.phase != discovery_phase {
            let total = progress
                .total_files
                .expect("header discovery always reports its inventory total");
            discovery_progress.finish_at(total);
            discovery_progress =
                Progress::bounded("Reading DICOM headers", total, ProgressUnit::Files);
            discovery_phase = progress.phase;
        }
        discovery_progress.set(progress.files_seen);
    })?;
    discovery_progress.finish();
    tracing::info!(
        files_checked = discovery.summary.files_seen,
        dicom_files = discovery.summary.dicom_files,
        series = discovery.summary.series_found,
        "DICOM discovery complete"
    );
    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    let mut summary = discovery.summary.clone();
    state.update_run(run_id, "preparing", &summary, None)?;
    let pseudonymizer = Pseudonymizer::from_base64(&config.pseudonym_key_b64)?;
    let mut stability_progress = Progress::bounded(
        "Confirming source stability",
        discovery.summary.files_seen,
        ProgressUnit::Files,
    );
    let quiet_snapshot = snapshot_source_with_progress(source, |progress| {
        stability_progress.set(progress.files_seen);
    })?;
    stability_progress.finish();
    if !discovery.source_snapshot.is_stable_with(&quiet_snapshot) {
        return finish_unstable_preparation(
            paths,
            state,
            run_id,
            config,
            started_at,
            discovery,
            &pseudonymizer,
        );
    }
    let source_snapshot = quiet_snapshot;
    if discovery.unreadable_dicom_like_files > 0 {
        tracing::warn!(
            unreadable_files = discovery.unreadable_dicom_like_files,
            "Unreadable DICOM-like files will stay local; independent readable series can continue"
        );
    }
    let bundle_root = paths.bundles.join(run_id);
    fs::create_dir_all(&bundle_root)?;
    let mut bundles = Vec::new();
    let mut held_series = Vec::new();
    let series_total = discovery.series.len();
    let mut local_progress = PrivacyPreparationProgress {
        state,
        run_id,
        total_series: series_total,
        last_report: Instant::now(),
    };
    let classifications = discovery
        .series
        .iter()
        .map(classify_header)
        .collect::<Vec<_>>();
    let preparation_bytes = discovery
        .series
        .iter()
        .zip(&classifications)
        .filter(|(_, classification)| {
            classification.decision == ClassificationDecision::Accepted
                && classification.kind == "functional_epi_candidate"
        })
        .flat_map(|(group, _)| &group.files)
        .try_fold(0_u64, |total, path| {
            Ok::<_, anyhow::Error>(total.saturating_add(fs::metadata(path)?.len()))
        })?;
    let mut archive_progress = Progress::bounded(
        "Preparing privacy-cleared EPI archives",
        preparation_bytes,
        ProgressUnit::Bytes,
    );

    for (index, (group, initial)) in discovery.series.iter().zip(classifications).enumerate() {
        match initial.decision {
            ClassificationDecision::Excluded => {
                summary.excluded += 1;
                local_progress.checkpoint(&summary, index + 1)?;
                continue;
            }
            ClassificationDecision::Held => {
                summary.held += 1;
                held_series.push(held(&pseudonymizer, group, index, &initial));
                local_progress.checkpoint(&summary, index + 1)?;
                continue;
            }
            ClassificationDecision::Accepted => {}
        }
        if initial.kind != "functional_epi_candidate" {
            let classification = coded_hold(
                "insufficient_functional_epi_header_evidence",
                "dicom_header",
            );
            summary.held += 1;
            held_series.push(held(&pseudonymizer, group, index, &classification));
            local_progress.checkpoint(&summary, index + 1)?;
            continue;
        }
        let before = archive_progress.completed();
        match create_dicom_archive(ArchiveRequest {
            group,
            classification: initial,
            pseudonymizer: &pseudonymizer,
            bundle_root: &bundle_root,
            progress: |bytes| archive_progress.inc(bytes),
        }) {
            Ok(bundle) if bundle.total_size() <= MAX_DICOM_ARCHIVE_BYTES_PER_SERIES => {
                let source_bytes = group
                    .files
                    .iter()
                    .filter_map(|path| fs::metadata(path).ok())
                    .map(|metadata| metadata.len())
                    .sum::<u64>();
                archive_progress.set(before.saturating_add(source_bytes));
                summary.accepted += 1;
                bundles.push(bundle);
            }
            Ok(bundle) => {
                if let Some(archive) = bundle.archive {
                    if let Some(directory) = Path::new(&archive.object.local_path).parent() {
                        let _ = fs::remove_dir_all(directory);
                    }
                }
                summary.held += 1;
                let classification = coded_hold("bundle_exceeds_upload_limit", "privacy_processor");
                held_series.push(held(&pseudonymizer, group, index, &classification));
            }
            Err(error) => {
                tracing::warn!(
                    series = index + 1,
                    total_series = series_total,
                    error = %error,
                    "Functional series was held by the local privacy processor"
                );
                summary.held += 1;
                let preparation_code = match error.to_string().as_str() {
                    "dicom_instance_exceeds_256_mib" => "dicom_instance_exceeds_256_mib",
                    "series_exceeds_64_gib_uncompressed_dicom_limit" => {
                        "series_exceeds_64_gib_uncompressed_dicom_limit"
                    }
                    "dicom_archive_expansion_ratio_exceeded" => {
                        "dicom_archive_expansion_ratio_exceeded"
                    }
                    _ => "dicom_privacy_preparation_failed",
                };
                let classification = coded_hold(preparation_code, "privacy_processor");
                held_series.push(held(&pseudonymizer, group, index, &classification));
            }
        }
        local_progress.checkpoint(&summary, index + 1)?;
    }
    archive_progress.finish_at(archive_progress.completed());

    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    let mut final_stability_progress = Progress::bounded(
        "Final source stability check",
        discovery.summary.files_seen,
        ProgressUnit::Files,
    );
    let final_snapshot = snapshot_source_with_progress(source, |progress| {
        final_stability_progress.set(progress.files_seen);
    })?;
    final_stability_progress.finish();
    if !source_snapshot.is_stable_with(&final_snapshot) {
        let _ = fs::remove_dir_all(&bundle_root);
        return finish_unstable_preparation(
            paths,
            state,
            run_id,
            config,
            started_at,
            discovery,
            &pseudonymizer,
        );
    }
    state.set_source_fingerprint(run_id, &final_snapshot.fingerprint(source)?)?;
    tracing::info!(
        accepted = summary.accepted,
        held = summary.held,
        excluded = summary.excluded,
        "Privacy preparation complete"
    );

    let mut errors = Vec::new();
    if discovery.unreadable_dicom_like_files > 0 {
        errors.push("unreadable_dicom_like_files".into());
    }
    bundles.sort_by(|left, right| {
        left.subject_id
            .cmp(&right.subject_id)
            .then(left.session_id.cmp(&right.session_id))
            .then(left.series_id.cmp(&right.series_id))
            .then(left.bundle_id.cmp(&right.bundle_id))
    });
    let manifest = LocalManifest {
        schema_version: MANIFEST_SCHEMA_VERSION.into(),
        run_id: run_id.into(),
        site_id: config.site_id.clone(),
        project_id: config.project_id.clone(),
        consent_policy_version: config.consent_policy_version.clone(),
        client_version: crate::CLIENT_VERSION.into(),
        metadata_policy: metadata_policy(),
        created_at: Utc::now().to_rfc3339(),
        source_summary: summary.clone(),
        bundles: bundles.clone(),
    };
    let manifest_path = paths.reports.join(format!("{run_id}.manifest.json"));
    let report_path = paths.reports.join(format!("{run_id}.json"));
    let report = RunReport {
        run_id: run_id.into(),
        status: "prepared".into(),
        site_id: config.site_id.clone(),
        project_id: config.project_id.clone(),
        project_name: config.project_name.clone(),
        consent_policy_version: config.consent_policy_version.clone(),
        started_at,
        completed_at: None,
        source_summary: summary.clone(),
        bundles: bundles.iter().map(ReportBundle::from).collect(),
        held_series,
        errors,
        worker_upload_id: None,
        worker_upload_ids: Vec::new(),
        existing_bundles: Vec::new(),
        archive_commit_count: 0,
    };
    write_json(&manifest_path, &manifest)?;
    write_json(&report_path, &report)?;
    state.set_artifacts(run_id, &manifest_path, &report_path)?;
    state.update_run(run_id, "prepared", &summary, None)?;
    Ok((manifest, report))
}

fn finish_unstable_preparation(
    paths: &AppPaths,
    state: &StateStore,
    run_id: &str,
    config: &ClientConfig,
    started_at: String,
    discovery: Discovery,
    pseudonymizer: &Pseudonymizer,
) -> Result<(LocalManifest, RunReport)> {
    let mut summary = discovery.summary;
    summary.accepted = 0;
    summary.excluded = 0;
    summary.held = summary.series_found;
    let classification = coded_hold("source_changed_or_incomplete", "derived");
    let held_series = discovery
        .series
        .iter()
        .enumerate()
        .map(|(index, group)| held(pseudonymizer, group, index, &classification))
        .collect();
    let manifest = LocalManifest {
        schema_version: MANIFEST_SCHEMA_VERSION.into(),
        run_id: run_id.into(),
        site_id: config.site_id.clone(),
        project_id: config.project_id.clone(),
        consent_policy_version: config.consent_policy_version.clone(),
        client_version: crate::CLIENT_VERSION.into(),
        metadata_policy: metadata_policy(),
        created_at: Utc::now().to_rfc3339(),
        source_summary: summary.clone(),
        bundles: Vec::new(),
    };
    let report = RunReport {
        run_id: run_id.into(),
        status: "prepared".into(),
        site_id: config.site_id.clone(),
        project_id: config.project_id.clone(),
        project_name: config.project_name.clone(),
        consent_policy_version: config.consent_policy_version.clone(),
        started_at,
        completed_at: None,
        source_summary: summary.clone(),
        bundles: Vec::new(),
        held_series,
        errors: vec!["source_changed_or_incomplete".into()],
        worker_upload_id: None,
        worker_upload_ids: Vec::new(),
        existing_bundles: Vec::new(),
        archive_commit_count: 0,
    };
    let manifest_path = paths.reports.join(format!("{run_id}.manifest.json"));
    let report_path = paths.reports.join(format!("{run_id}.json"));
    write_json(&manifest_path, &manifest)?;
    write_json(&report_path, &report)?;
    state.set_artifacts(run_id, &manifest_path, &report_path)?;
    state.update_run(run_id, "prepared", &summary, None)?;
    Ok((manifest, report))
}

#[cfg(test)]
fn multi_echo_labels(images: &[crate::convert::ConvertedImage]) -> Option<Vec<Option<String>>> {
    if images.len() == 1 {
        return Some(vec![None]);
    }
    let echo_numbers: Vec<Option<i64>> = images
        .iter()
        .map(|image| {
            image
                .metadata
                .get("EchoNumber")
                .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
                .filter(|value| (1..=100).contains(value))
        })
        .collect();
    if echo_numbers.iter().all(Option::is_some) {
        let numbers: Vec<i64> = echo_numbers.into_iter().flatten().collect();
        let unique: std::collections::HashSet<_> = numbers.iter().copied().collect();
        if unique.len() == numbers.len() {
            return Some(
                numbers
                    .into_iter()
                    .map(|number| Some(number.to_string()))
                    .collect(),
            );
        }
        return None;
    }
    let echo_times: Vec<Option<i64>> = images
        .iter()
        .map(|image| {
            image
                .metadata
                .get("EchoTime")
                .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
                .filter(|value| value.is_finite() && 0.0 < *value && *value <= 2.0)
                .map(|value| (value * 1_000_000_000.0).round() as i64)
        })
        .collect();
    if !echo_times.iter().all(Option::is_some) {
        return None;
    }
    let times: Vec<i64> = echo_times.into_iter().flatten().collect();
    let unique: std::collections::HashSet<_> = times.iter().copied().collect();
    if unique.len() != times.len() {
        return None;
    }
    let mut ranked: Vec<(usize, i64)> = times.into_iter().enumerate().collect();
    ranked.sort_by_key(|(_, time)| *time);
    let mut labels = vec![None; ranked.len()];
    for (rank, (original_index, _)) in ranked.into_iter().enumerate() {
        labels[original_index] = Some((rank + 1).to_string());
    }
    Some(labels)
}

fn register_objects(
    state: &StateStore,
    run_id: &str,
    worker_upload_id: &str,
    prefix: &str,
    bundles: &[ManifestBundle],
    descriptors: &[MultipartObject],
) -> Result<()> {
    if !prefix.ends_with('/') {
        bail!("ingest API object prefix must end with '/'");
    }
    let descriptor_keys: std::collections::HashSet<_> =
        descriptors.iter().map(|item| item.key.as_str()).collect();
    let existing_bundle_ids: std::collections::HashSet<_> = state
        .existing_bundles(run_id)?
        .into_iter()
        .map(|bundle| bundle.bundle_id)
        .collect();
    let mut expected_descriptor_keys = std::collections::HashSet::new();
    for bundle in bundles {
        if existing_bundle_ids.contains(&bundle.bundle_id) {
            for object in bundle.upload_objects() {
                let key = format!("{prefix}{}", object.relative_key);
                if descriptor_keys.contains(key.as_str()) {
                    bail!("ingest API allocated an object for an already archived bundle");
                }
            }
            continue;
        }
        for object in bundle.upload_objects() {
            let key = format!("{prefix}{}", object.relative_key);
            if !descriptor_keys.contains(key.as_str()) {
                bail!("ingest API multipart plan key does not match the requested archive key");
            }
            expected_descriptor_keys.insert(key.clone());
            state.add_upload_object(&UploadObjectRecord {
                run_id: run_id.into(),
                worker_upload_id: worker_upload_id.into(),
                key,
                local_path: object.local_path.clone(),
                size: object.size,
                sha256: object.sha256.clone(),
                multipart_id: None,
                status: "pending".into(),
                etag: None,
            })?;
        }
    }
    if expected_descriptor_keys.len() != descriptor_keys.len()
        || !descriptors
            .iter()
            .all(|descriptor| expected_descriptor_keys.contains(&descriptor.key))
    {
        bail!("ingest API multipart plan contains an unexpected archive key");
    }
    Ok(())
}

fn register_dicom_objects(
    state: &StateStore,
    run_id: &str,
    worker_upload_id: &str,
    prefix: &str,
    bundles: &[ManifestBundle],
    descriptors: &[MultipartObject],
) -> Result<()> {
    if !prefix.ends_with('/') {
        bail!("DICOM ingest API object prefix must end with '/'");
    }
    if descriptors.len() != bundles.len() {
        bail!("DICOM ingest API returned an incomplete multipart plan");
    }
    let descriptor_by_key = descriptors
        .iter()
        .map(|descriptor| (descriptor.key.as_str(), descriptor))
        .collect::<std::collections::HashMap<_, _>>();
    if descriptor_by_key.len() != descriptors.len() {
        bail!("DICOM ingest API repeated a multipart object key");
    }
    let mut expected = std::collections::HashSet::new();
    for bundle in bundles {
        let archive = bundle
            .archive
            .as_ref()
            .context("DICOM upload bundle has no archive object")?;
        let key = format!("{prefix}{}", archive.object.relative_key);
        let descriptor = descriptor_by_key
            .get(key.as_str())
            .context("DICOM ingest API omitted an expected archive key")?;
        if descriptor.kind.as_deref() != Some("dicom_archive")
            || descriptor.series_archive_id.as_deref() != Some(bundle.bundle_id.as_str())
        {
            bail!("DICOM ingest API multipart descriptor identity does not match the series");
        }
        expected.insert(key.clone());
        state.add_upload_object(&UploadObjectRecord {
            run_id: run_id.into(),
            worker_upload_id: worker_upload_id.into(),
            key,
            local_path: archive.object.local_path.clone(),
            size: archive.object.size,
            sha256: archive.object.sha256.clone(),
            multipart_id: None,
            status: "pending".into(),
            etag: None,
        })?;
    }
    if descriptors
        .iter()
        .any(|descriptor| !expected.contains(&descriptor.key))
    {
        bail!("DICOM ingest API multipart plan contains an unexpected archive key");
    }
    Ok(())
}

fn dicom_receipt_complete(status: &str) -> bool {
    matches!(status, "committed" | "already_received")
}

fn record_dicom_receipt(
    state: &StateStore,
    run_id: &str,
    chunk_index: u32,
    bundles: &[ManifestBundle],
    status: &UploadStatus,
) -> Result<()> {
    match status.status.as_str() {
        "committed" => state.set_chunk_status(run_id, chunk_index, "committed"),
        "already_received" => {
            validate_dicom_reconciliation(bundles, &status.already_received_series, true)?;
            state.set_chunk_reconciled(run_id, chunk_index)
        }
        _ => bail!("DICOM receipt API did not commit the transferred archives"),
    }
}

fn validate_dicom_reconciliation(
    bundles: &[ManifestBundle],
    already_received: &[AlreadyReceivedSeries],
    require_all: bool,
) -> Result<HashSet<String>> {
    let requested = bundles
        .iter()
        .map(|bundle| bundle.bundle_id.as_str())
        .collect::<HashSet<_>>();
    let mut reconciled = HashSet::with_capacity(already_received.len());
    for item in already_received {
        if !requested.contains(item.series_archive_id.as_str())
            || item.receipt_upload_id.is_empty()
            || item.receipt_upload_id.len() > 128
            || item
                .receipt_upload_id
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
            || !reconciled.insert(item.series_archive_id.clone())
        {
            bail!("DICOM ingest API returned an invalid already-received series receipt");
        }
    }
    if require_all && reconciled.len() != requested.len() {
        bail!("DICOM ingest API returned an incomplete already-received series receipt");
    }
    Ok(reconciled)
}

fn held(
    pseudonymizer: &Pseudonymizer,
    group: &SeriesGroup,
    index: usize,
    classification: &Classification,
) -> HeldSeries {
    let series_id = if group.series_uid.is_empty() {
        pseudonymizer.id("held-series-index", &index.to_string())
    } else {
        pseudonymizer.id("series", &group.series_uid)
    };
    HeldSeries {
        series_id,
        dicom_count: group.files.len() as u64,
        reason_code: classification.kind.clone(),
        evidence: classification
            .evidence
            .iter()
            .map(|item| item.code.clone())
            .collect(),
    }
}

fn coded_hold(code: &str, source: &str) -> Classification {
    Classification {
        decision: ClassificationDecision::Held,
        kind: code.into(),
        confidence: 1.0,
        evidence: vec![ClassificationEvidence {
            code: code.into(),
            source: source.into(),
            effect: "contradicts".into(),
        }],
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn load_or_create_dry_run_key(paths: &AppPaths) -> Result<String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let path = paths.root.join("dry-run-pseudonym.key");
    if let Ok(value) = fs::read_to_string(&path) {
        return Ok(value.trim().to_owned());
    }
    let mut key = [0_u8; 32];
    rand::rng().fill_bytes(&mut key);
    let value = STANDARD.encode(key);
    fs::write(&path, format!("{value}\n"))?;
    privacy::restrict_file(&path)?;
    Ok(value)
}

fn cleanup_abandoned_workspaces(work_root: &Path) -> Result<()> {
    for entry in fs::read_dir(work_root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("conversion-"))
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn recover_interrupted_preparations(paths: &AppPaths, state: &StateStore) -> Result<()> {
    for run in state.interrupted_preparation_runs()? {
        remove_bundle_cache(paths, &run.id);
        state.update_run(
            &run.id,
            "failed",
            &run.summary,
            Some("local_preparation_interrupted"),
        )?;
    }
    Ok(())
}

fn remove_bundle_cache(paths: &AppPaths, run_id: &str) {
    if let Err(error) = fs::remove_dir_all(paths.bundles.join(run_id)) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(run_id, "could not remove local bundle cache");
        }
    }
}

fn update_report_status(
    paths: &AppPaths,
    run_id: &str,
    status: &str,
    worker_upload_ids: Vec<String>,
    existing_bundles: Vec<ExistingArchiveBundle>,
    archive_commit_count: u64,
) -> Result<()> {
    let path = paths.reports.join(format!("{run_id}.json"));
    let mut report: RunReport = serde_json::from_slice(&fs::read(&path)?)?;
    report.status = status.into();
    report.completed_at = Some(Utc::now().to_rfc3339());
    report.worker_upload_id = worker_upload_ids.first().cloned();
    report.worker_upload_ids = worker_upload_ids;
    report.existing_bundles = existing_bundles;
    report.archive_commit_count = archive_commit_count;
    write_json(&path, &report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::ConvertedImage;

    fn image(metadata: serde_json::Value) -> ConvertedImage {
        ConvertedImage {
            nifti_path: PathBuf::from("fixture.nii"),
            metadata_path: None,
            metadata,
        }
    }

    fn upload_test_bundle(bundle_digit: char, series_digit: char) -> ManifestBundle {
        let bundle_id = bundle_digit.to_string().repeat(24);
        ManifestBundle {
            bundle_id: bundle_id.clone(),
            series_id: series_digit.to_string().repeat(24),
            subject_id: "3".repeat(24),
            session_id: "4".repeat(24),
            protocol_group_id: "5".repeat(24),
            nifti: Some(crate::model::ManifestObject {
                relative_key: format!("{bundle_id}/scan_bold.nii.gz"),
                local_path: format!("/private/{bundle_id}/scan_bold.nii.gz"),
                size: 1_024,
                sha256: "a".repeat(64),
                uncompressed_sha256: Some("b".repeat(64)),
            }),
            metadata: Some(crate::model::ManifestObject {
                relative_key: format!("{bundle_id}/scan_bold.json"),
                local_path: format!("/private/{bundle_id}/scan_bold.json"),
                size: 512,
                sha256: "c".repeat(64),
                uncompressed_sha256: None,
            }),
            archive: None,
            source_dicom_count: 20,
            classification: Classification {
                decision: ClassificationDecision::Accepted,
                kind: "functional_epi".into(),
                confidence: 0.99,
                evidence: Vec::new(),
            },
            qc: crate::model::QcResult {
                passed: true,
                checks: Vec::new(),
                warnings: Vec::new(),
            },
        }
    }

    fn dicom_upload_test_bundle(bundle_digit: char, series_digit: char) -> ManifestBundle {
        let bundle_id = bundle_digit.to_string().repeat(24);
        ManifestBundle {
            bundle_id: bundle_id.clone(),
            series_id: series_digit.to_string().repeat(24),
            subject_id: "3".repeat(24),
            session_id: "4".repeat(24),
            protocol_group_id: "5".repeat(24),
            nifti: None,
            metadata: None,
            archive: Some(crate::model::ManifestArchiveObject {
                object: crate::model::ManifestObject {
                    relative_key: format!("{bundle_id}/dicom.tar.zst"),
                    local_path: format!("/private/{bundle_id}/dicom.tar.zst"),
                    size: 2_048,
                    sha256: "d".repeat(64),
                    uncompressed_sha256: None,
                },
                format: crate::archive::DICOM_ARCHIVE_FORMAT.into(),
                dicom_instance_count: 20,
                deidentification_profile: DICOM_METADATA_POLICY_ID.into(),
                deidentification_profile_version: DICOM_METADATA_POLICY_VERSION.into(),
            }),
            source_dicom_count: 20,
            classification: Classification {
                decision: ClassificationDecision::Accepted,
                kind: "functional_epi".into(),
                confidence: 0.99,
                evidence: Vec::new(),
            },
            qc: crate::model::QcResult {
                passed: true,
                checks: Vec::new(),
                warnings: Vec::new(),
            },
        }
    }

    fn sync_test_config() -> ClientConfig {
        ClientConfig {
            api_url: crate::DEFAULT_API_URL.into(),
            device_token: "sn_device_fixture".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy".into(),
            pseudonym_key_b64: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into(),
        }
    }

    #[test]
    fn dicom_receipts_use_raw_series_count_and_byte_limits_without_changing_legacy() {
        let root = tempfile::tempdir().unwrap();
        let state = StateStore::open(&root.path().join("state.sqlite3")).unwrap();

        for (run_id, series_count, expected_layout) in [
            ("dicom-15", 15_usize, vec![(0_usize, 8_usize), (8, 7)]),
            ("dicom-32", 32, vec![(0, 8), (8, 8), (16, 8), (24, 8)]),
        ] {
            state.create_run(run_id, root.path(), false).unwrap();
            let subjects = vec!["same-subject".to_owned(); series_count];
            let sizes = vec![1_u64; series_count];
            ensure_dicom_receipt_chunks(&state, run_id, &subjects, &sizes).unwrap();
            let layout = state
                .run_uploads(run_id)
                .unwrap()
                .into_iter()
                .map(|chunk| (chunk.bundle_start, chunk.bundle_count))
                .collect::<Vec<_>>();
            assert_eq!(layout, expected_layout);
            assert!(
                layout
                    .iter()
                    .all(|(_, count)| *count <= MAX_DICOM_SERIES_PER_UPLOAD)
            );
        }

        let gib = 1024_u64 * 1024 * 1024;
        state
            .create_run("dicom-250-gib", root.path(), false)
            .unwrap();
        let subjects = vec!["same-subject".to_owned(); 5];
        let sizes = [64_u64, 64, 64, 58, 1].map(|size| size * gib);
        ensure_dicom_receipt_chunks(&state, "dicom-250-gib", &subjects, &sizes).unwrap();
        let byte_layout = state
            .run_uploads("dicom-250-gib")
            .unwrap()
            .into_iter()
            .map(|chunk| (chunk.bundle_start, chunk.bundle_count))
            .collect::<Vec<_>>();
        assert_eq!(byte_layout, vec![(0, 4), (4, 1)]);

        state
            .create_run("dicom-archive-too-large", root.path(), false)
            .unwrap();
        let error = ensure_dicom_receipt_chunks(
            &state,
            "dicom-archive-too-large",
            &["same-subject".to_owned()],
            &[MAX_DICOM_ARCHIVE_BYTES_PER_SERIES + 1],
        )
        .unwrap_err();
        assert!(error.to_string().contains("64 GiB object limit"));

        state.create_run("legacy-32", root.path(), false).unwrap();
        let subjects = vec!["same-subject".to_owned(); 32];
        let sizes = vec![1_u64; 32];
        state
            .ensure_run_uploads(
                "legacy-32",
                &subjects,
                &sizes,
                MAX_LEGACY_BUNDLES_PER_UPLOAD,
                MAX_LEGACY_BYTES_PER_UPLOAD,
            )
            .unwrap();
        let legacy = state.run_uploads("legacy-32").unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].bundle_count, 32);
    }

    fn write_empty_sync_checkpoint(runtime: &Runtime, run_id: &str, status: &str) {
        let config = sync_test_config();
        let manifest = LocalManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.into(),
            run_id: run_id.into(),
            site_id: config.site_id.clone(),
            project_id: config.project_id.clone(),
            consent_policy_version: config.consent_policy_version.clone(),
            client_version: crate::CLIENT_VERSION.into(),
            metadata_policy: crate::model::MetadataPolicy {
                policy_id: METADATA_POLICY_ID.into(),
                policy_version: METADATA_POLICY_VERSION.into(),
            },
            created_at: "2026-07-18T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: Vec::new(),
        };
        let report = RunReport {
            run_id: run_id.into(),
            status: status.into(),
            site_id: config.site_id,
            project_id: config.project_id,
            project_name: config.project_name,
            consent_policy_version: config.consent_policy_version,
            started_at: "2026-07-18T00:00:00Z".into(),
            completed_at: None,
            source_summary: SourceSummary::default(),
            bundles: Vec::new(),
            held_series: Vec::new(),
            errors: Vec::new(),
            worker_upload_id: None,
            worker_upload_ids: Vec::new(),
            existing_bundles: Vec::new(),
            archive_commit_count: 0,
        };
        let manifest_path = runtime
            .paths
            .reports
            .join(format!("{run_id}.manifest.json"));
        let report_path = runtime.paths.reports.join(format!("{run_id}.json"));
        write_json(&manifest_path, &manifest).unwrap();
        write_json(&report_path, &report).unwrap();
        runtime
            .state
            .set_artifacts(run_id, &manifest_path, &report_path)
            .unwrap();
    }

    fn write_saved_completion_checkpoint(
        runtime: &Runtime,
        run_id: &str,
        upload_id: &str,
    ) -> (ManifestBundle, crate::state::RunUploadRecord) {
        runtime
            .state
            .create_run(run_id, Path::new("/private/source"), false)
            .unwrap();
        let bundle = upload_test_bundle('1', '2');
        runtime
            .state
            .ensure_run_uploads(
                run_id,
                std::slice::from_ref(&bundle.subject_id),
                &[bundle.total_size()],
                MAX_LEGACY_BUNDLES_PER_UPLOAD,
                MAX_LEGACY_BYTES_PER_UPLOAD,
            )
            .unwrap();
        runtime
            .state
            .set_chunk_worker(run_id, 0, upload_id)
            .unwrap();
        let completion_path = runtime
            .paths
            .reports
            .join(format!("{run_id}.chunk-0.complete-request.json"));
        write_json(
            &completion_path,
            &CompleteUploadRequest {
                objects: Vec::new(),
            },
        )
        .unwrap();
        let chunk = runtime.state.run_uploads(run_id).unwrap().remove(0);
        (bundle, chunk)
    }

    fn write_saved_dicom_completion_checkpoint(
        runtime: &Runtime,
        run_id: &str,
        upload_id: &str,
    ) -> (ManifestBundle, crate::state::RunUploadRecord) {
        runtime
            .state
            .create_run(run_id, Path::new("/private/source"), false)
            .unwrap();
        let mut bundle = dicom_upload_test_bundle('1', '2');
        let archive_path = runtime.paths.work.join(format!("{run_id}-fixture.tar.zst"));
        std::fs::write(&archive_path, vec![0_u8; 2_048]).unwrap();
        bundle.archive.as_mut().unwrap().object.local_path =
            archive_path.to_string_lossy().into_owned();
        ensure_dicom_receipt_chunks(
            &runtime.state,
            run_id,
            std::slice::from_ref(&bundle.subject_id),
            &[bundle.total_size()],
        )
        .unwrap();
        runtime
            .state
            .set_chunk_worker(run_id, 0, upload_id)
            .unwrap();
        let completion_path = runtime
            .paths
            .reports
            .join(format!("{run_id}.chunk-0.complete-request.json"));
        write_json(
            &completion_path,
            &CompleteUploadRequest {
                objects: Vec::new(),
            },
        )
        .unwrap();
        let chunk = runtime.state.run_uploads(run_id).unwrap().remove(0);
        (bundle, chunk)
    }

    #[test]
    fn multi_echo_labels_prefer_unique_explicit_echo_numbers() {
        let images = [
            image(serde_json::json!({"EchoNumber": 1, "EchoTime": 0.03})),
            image(serde_json::json!({"EchoNumber": 2, "EchoTime": 0.05})),
        ];
        assert_eq!(
            multi_echo_labels(&images),
            Some(vec![Some("1".into()), Some("2".into())])
        );
    }

    #[test]
    fn mixed_cross_workstation_reconciliation_registers_only_pending_series() {
        let root = tempfile::tempdir().unwrap();
        let state = StateStore::open(&root.path().join("state.sqlite3")).unwrap();
        state.create_run("run", root.path(), false).unwrap();
        let received = dicom_upload_test_bundle('1', '2');
        let mut pending = dicom_upload_test_bundle('6', '7');
        let pending_path = root.path().join("pending.tar.zst");
        std::fs::write(&pending_path, vec![7_u8; 2_048]).unwrap();
        pending.archive.as_mut().unwrap().object.local_path =
            pending_path.to_string_lossy().into_owned();
        let bundles = vec![received.clone(), pending.clone()];
        let reconciled = validate_dicom_reconciliation(
            &bundles,
            &[AlreadyReceivedSeries {
                series_archive_id: received.bundle_id.clone(),
                receipt_upload_id: "11111111-1111-4111-8111-111111111111".into(),
            }],
            false,
        )
        .unwrap();
        assert_eq!(reconciled, HashSet::from([received.bundle_id]));

        let prefix = "dicom/site/project/upload/";
        let key = format!(
            "{prefix}{}",
            pending.archive.as_ref().unwrap().object.relative_key
        );
        let descriptors = vec![MultipartObject {
            key: key.clone(),
            upload_id: "multipart-pending".into(),
            part_size: 64 * 1024 * 1024,
            kind: Some("dicom_archive".into()),
            series_archive_id: Some(pending.bundle_id.clone()),
        }];
        register_dicom_objects(
            &state,
            "run",
            "22222222-2222-4222-8222-222222222222",
            prefix,
            std::slice::from_ref(&pending),
            &descriptors,
        )
        .unwrap();
        let objects = state
            .upload_objects("22222222-2222-4222-8222-222222222222")
            .unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].key, key);
    }

    #[test]
    fn multi_echo_labels_rank_unique_echo_times_and_reject_duplicates() {
        let ranked = [
            image(serde_json::json!({"EchoTime": 0.05})),
            image(serde_json::json!({"EchoTime": 0.03})),
        ];
        assert_eq!(
            multi_echo_labels(&ranked),
            Some(vec![Some("2".into()), Some("1".into())])
        );
        let duplicate = [
            image(serde_json::json!({"EchoNumber": 1})),
            image(serde_json::json!({"EchoNumber": 1})),
        ];
        assert!(multi_echo_labels(&duplicate).is_none());
    }

    #[tokio::test]
    async fn exact_duplicate_is_checkpointed_and_new_subset_is_initialized() {
        use axum::{Json, Router, http::StatusCode, routing::post};

        let existing_bundle = upload_test_bundle('1', '2');
        let new_bundle = upload_test_bundle('6', '7');
        let existing = ExistingArchiveBundle {
            bundle_id: existing_bundle.bundle_id.clone(),
            series_id: existing_bundle.series_id.clone(),
            subject_id: existing_bundle.subject_id.clone(),
            session_id: existing_bundle.session_id.clone(),
            protocol_group_id: existing_bundle.protocol_group_id.clone(),
            upload_id: "11111111-1111-4111-8111-111111111111".into(),
            nii_uncompressed_sha256: existing_bundle
                .nifti
                .as_ref()
                .unwrap()
                .uncompressed_sha256
                .clone()
                .unwrap(),
        };
        let existing_for_server = existing.clone();
        let app = Router::new().route(
            "/v1/uploads",
            post(move |Json(body): Json<serde_json::Value>| {
                let existing = existing_for_server.clone();
                async move {
                    let bundles = body["bundles"].as_array().unwrap();
                    if bundles.iter().any(|bundle| {
                        bundle["bundle_id"].as_str() == Some(existing.bundle_id.as_str())
                    }) {
                        return (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({
                                "error": {
                                    "code": "DUPLICATE_BUNDLE",
                                    "message": "already committed",
                                    "request_id": "request-duplicate",
                                    "details": {
                                        "reason": "active_exact_match",
                                        "existing_bundles": [existing]
                                    }
                                }
                            })),
                        );
                    }
                    let bundle = &bundles[0];
                    let prefix = "archive/v1/site/project/22222222-2222-4222-8222-222222222222/";
                    (
                        StatusCode::CREATED,
                        Json(serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "uploading",
                            "object_prefix": prefix,
                            "multipart_objects": [
                                {
                                    "key": format!("{prefix}{}", bundle["nii"]["relative_key"].as_str().unwrap()),
                                    "upload_id": "multipart-nii",
                                    "part_size": 67_108_864
                                },
                                {
                                    "key": format!("{prefix}{}", bundle["metadata"]["relative_key"].as_str().unwrap()),
                                    "upload_id": "multipart-json",
                                    "part_size": 67_108_864
                                }
                            ]
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy-1".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        state
            .create_run("run", Path::new("/private/source"), false)
            .unwrap();

        let requested = vec![existing_bundle, new_bundle];
        let plan = create_session_reconciling(&state, "run", &api, &requested, "0.2.0", false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plan.0, "22222222-2222-4222-8222-222222222222");
        assert_eq!(plan.2.len(), 2);
        assert_eq!(state.existing_bundles("run").unwrap(), vec![existing]);
        register_objects(&state, "run", &plan.0, &plan.1, &requested, &plan.2).unwrap();
        let objects = state.upload_objects(&plan.0).unwrap();
        assert_eq!(objects.len(), 2);
        assert!(
            objects
                .iter()
                .all(|object| object.key.contains(&"6".repeat(24)))
        );
        let existing_only =
            create_session_reconciling(&state, "run", &api, &requested[..1], "0.2.0", false)
                .await
                .unwrap();
        assert!(existing_only.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn completion_race_is_reconciled_without_manual_recovery() {
        use axum::{Json, Router, http::StatusCode, routing::post};

        let prior_bundle = upload_test_bundle('6', '7');
        let bundle = upload_test_bundle('1', '2');
        let existing = ExistingArchiveBundle {
            bundle_id: bundle.bundle_id.clone(),
            series_id: bundle.series_id.clone(),
            subject_id: bundle.subject_id.clone(),
            session_id: bundle.session_id.clone(),
            protocol_group_id: bundle.protocol_group_id.clone(),
            upload_id: "11111111-1111-4111-8111-111111111111".into(),
            nii_uncompressed_sha256: bundle
                .nifti
                .as_ref()
                .unwrap()
                .uncompressed_sha256
                .clone()
                .unwrap(),
        };
        let app = Router::new().route(
            "/v1/uploads/{upload_id}/complete",
            post(move || {
                let existing = existing.clone();
                async move {
                    (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": {
                                "code": "DUPLICATE_BUNDLE",
                                "message": "committed concurrently",
                                "request_id": "request-race",
                                "details": {
                                    "reason": "active_exact_match",
                                    "existing_bundles": [existing]
                                }
                            }
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy-1".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();
        let error = api
            .complete_upload("22222222-2222-4222-8222-222222222222", Vec::new())
            .await
            .unwrap_err();
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::open(&directory.path().join("state.sqlite3")).unwrap();
        state
            .create_run("run", Path::new("/private/source"), false)
            .unwrap();
        state
            .record_existing_bundles(
                "run",
                &[ExistingArchiveBundle {
                    bundle_id: prior_bundle.bundle_id.clone(),
                    series_id: prior_bundle.series_id.clone(),
                    subject_id: prior_bundle.subject_id.clone(),
                    session_id: prior_bundle.session_id.clone(),
                    protocol_group_id: prior_bundle.protocol_group_id.clone(),
                    upload_id: "33333333-3333-4333-8333-333333333333".into(),
                    nii_uncompressed_sha256: prior_bundle
                        .nifti
                        .as_ref()
                        .unwrap()
                        .uncompressed_sha256
                        .clone()
                        .unwrap(),
                }],
            )
            .unwrap();
        assert!(
            reconcile_completion_duplicate(&state, "run", &[prior_bundle, bundle], &error).unwrap()
        );
        assert_eq!(state.existing_bundles("run").unwrap().len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn completion_recovery_owns_each_busy_verifier_retry() {
        use axum::{
            Json, Router,
            http::StatusCode,
            routing::{get, post},
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let post_attempts = Arc::new(AtomicUsize::new(0));
        let post_attempts_for_handler = post_attempts.clone();
        let status_attempts = Arc::new(AtomicUsize::new(0));
        let status_attempts_for_handler = status_attempts.clone();
        let app = Router::new()
            .route(
                "/v1/uploads/{upload_id}/complete",
                post(move || {
                    let attempt = post_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if attempt < 20 {
                            (
                                StatusCode::CONFLICT,
                                Json(serde_json::json!({
                                    "error": {
                                        "code": "CONFLICT",
                                        "message": "Upload verification is already in progress",
                                        "request_id": "request-busy"
                                    }
                                })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "upload_id": "22222222-2222-4222-8222-222222222222",
                                    "status": "committed"
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1/uploads/{upload_id}",
                get(move || {
                    status_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "uploading",
                            "verification": {
                                "phase": "validating_scans",
                                "finalized_series": 8,
                                "verified_series": 4,
                                "total_series": 15
                            }
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy-1".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();
        let status = complete_with_recovery_polling(
            &api,
            "22222222-2222-4222-8222-222222222222",
            &[],
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(status.status, "committed");
        assert_eq!(post_attempts.load(Ordering::SeqCst), 21);
        assert_eq!(status_attempts.load(Ordering::SeqCst), 20);
        server.abort();
    }

    #[tokio::test]
    async fn successful_verification_phases_are_driven_without_poll_delay() {
        use axum::{Json, Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let post_attempts = Arc::new(AtomicUsize::new(0));
        let post_attempts_for_handler = post_attempts.clone();
        let app = Router::new().route(
            "/v1/uploads/{upload_id}/complete",
            post(move || {
                let attempt = post_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                async move {
                    Json(match attempt {
                        0 => serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "uploading",
                            "verification": {
                                "phase": "finalizing_objects",
                                "finalized_series": 1,
                                "verified_series": 0,
                                "total_series": 2
                            }
                        }),
                        1 => serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "uploading",
                            "verification": {
                                "phase": "validating_scans",
                                "finalized_series": 2,
                                "verified_series": 1,
                                "total_series": 2
                            }
                        }),
                        _ => serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "committed"
                        }),
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy-1".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();

        let status = tokio::time::timeout(
            Duration::from_secs(2),
            complete_with_recovery_polling(
                &api,
                "22222222-2222-4222-8222-222222222222",
                &[],
                Duration::from_secs(60 * 60),
            ),
        )
        .await
        .expect("successful verification progress must bypass the poll delay")
        .unwrap();

        assert_eq!(status.status, "committed");
        assert_eq!(post_attempts.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn saved_completion_checkpoint_bypasses_credentials_and_multipart() {
        use axum::{
            Json, Router,
            http::StatusCode,
            routing::{get, post},
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let credential_attempts = Arc::new(AtomicUsize::new(0));
        let credential_attempts_for_handler = credential_attempts.clone();
        let part_attempts = Arc::new(AtomicUsize::new(0));
        let part_attempts_for_handler = part_attempts.clone();
        let completion_attempts = Arc::new(AtomicUsize::new(0));
        let completion_attempts_for_handler = completion_attempts.clone();
        let app = Router::new()
            .route(
                "/v1/uploads/{upload_id}",
                get(|| async {
                    Json(serde_json::json!({
                        "upload_id": "22222222-2222-4222-8222-222222222222",
                        "status": "uploading",
                        "verification": {
                            "phase": "validating_scans",
                            "finalized_series": 0,
                            "verified_series": 0,
                            "total_series": 1
                        }
                    }))
                }),
            )
            .route(
                "/v1/uploads/{upload_id}/parts",
                post(move || {
                    part_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/uploads/{upload_id}/credentials",
                post(move || {
                    credential_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        (
                            StatusCode::CONFLICT,
                            Json(serde_json::json!({
                                "error": {
                                    "code": "CONFLICT",
                                    "message": "Upload is busy; retry shortly"
                                }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1/uploads/{upload_id}/complete",
                post(move || {
                    completion_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "committed"
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy-1".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        let (bundle, chunk) = write_saved_completion_checkpoint(
            &runtime,
            "run",
            "22222222-2222-4222-8222-222222222222",
        );

        runtime
            .continue_upload_chunk("run", &chunk, &[bundle], crate::CLIENT_VERSION, &api)
            .await
            .unwrap();

        assert_eq!(credential_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(part_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(completion_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.state.run_uploads("run").unwrap()[0].status,
            "committed"
        );
        server.abort();
    }

    #[tokio::test]
    async fn second_workstation_all_duplicate_receipt_skips_multipart_and_is_idempotent() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let create_attempts = Arc::new(AtomicUsize::new(0));
        let create_attempts_for_handler = Arc::clone(&create_attempts);
        let credential_attempts = Arc::new(AtomicUsize::new(0));
        let credential_attempts_for_handler = Arc::clone(&credential_attempts);
        let part_attempts = Arc::new(AtomicUsize::new(0));
        let part_attempts_for_handler = Arc::clone(&part_attempts);
        let app = Router::new()
            .route(
                "/v1/dicom-uploads",
                post(move || {
                    create_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "upload_id": "33333333-3333-4333-8333-333333333333",
                            "status": "already_received",
                            "format": "dicom-series-v1",
                            "already_received_series": [{
                                "series_archive_id": "111111111111111111111111",
                                "receipt_upload_id": "33333333-3333-4333-8333-333333333333"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/credentials",
                post(move || {
                    credential_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/parts",
                post(move || {
                    part_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = sync_test_config();
        config.api_url = format!("http://{address}");
        let state_root = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        runtime
            .state
            .create_run("run", Path::new("/private/source"), false)
            .unwrap();
        let manifest = LocalManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.into(),
            run_id: "run".into(),
            site_id: config.site_id.clone(),
            project_id: config.project_id.clone(),
            consent_policy_version: config.consent_policy_version.clone(),
            client_version: crate::CLIENT_VERSION.into(),
            metadata_policy: crate::archive::metadata_policy(),
            created_at: "2026-07-18T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: {
                let mut bundle = dicom_upload_test_bundle('1', '2');
                let archive_path = runtime.paths.work.join("duplicate-fixture.tar.zst");
                std::fs::write(&archive_path, vec![0_u8; 2_048]).unwrap();
                bundle.archive.as_mut().unwrap().object.local_path =
                    archive_path.to_string_lossy().into_owned();
                vec![bundle]
            },
        };

        runtime
            .continue_dicom_upload("run", &manifest, &config)
            .await
            .unwrap();
        runtime
            .continue_dicom_upload("run", &manifest, &config)
            .await
            .unwrap();

        assert_eq!(create_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(credential_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(part_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime.state.run_uploads("run").unwrap()[0].status,
            "reconciled"
        );
        assert!(
            runtime.state.run_uploads("run").unwrap()[0]
                .worker_upload_id
                .is_none()
        );
        server.abort();
    }

    #[tokio::test]
    async fn dicom_refresh_already_received_without_object_prefix_reconciles_checkpoint() {
        use axum::{Json, Router, http::StatusCode, routing::get, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let create_attempts = Arc::new(AtomicUsize::new(0));
        let create_attempts_for_handler = Arc::clone(&create_attempts);
        let credential_attempts = Arc::new(AtomicUsize::new(0));
        let credential_attempts_for_handler = Arc::clone(&credential_attempts);
        let part_attempts = Arc::new(AtomicUsize::new(0));
        let part_attempts_for_handler = Arc::clone(&part_attempts);
        let completion_attempts = Arc::new(AtomicUsize::new(0));
        let completion_attempts_for_handler = Arc::clone(&completion_attempts);
        let status_attempts = Arc::new(AtomicUsize::new(0));
        let status_attempts_for_handler = Arc::clone(&status_attempts);
        let app = Router::new()
            .route(
                "/v1/dicom-uploads",
                post(move || {
                    create_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}",
                get(move || {
                    status_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "uploading"
                        }))
                    }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/credentials",
                post(move || {
                    credential_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "already_received",
                            "already_received_series": [{
                                "series_archive_id": "111111111111111111111111",
                                "receipt_upload_id": "33333333-3333-4333-8333-333333333333"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/parts",
                post(move || {
                    part_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/complete",
                post(move || {
                    completion_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = sync_test_config();
        config.api_url = format!("http://{address}");
        let state_root = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        runtime
            .state
            .create_run("run", Path::new("/private/source"), false)
            .unwrap();
        let mut bundle = dicom_upload_test_bundle('1', '2');
        let archive_path = runtime.paths.work.join("refresh-fixture.tar.zst");
        std::fs::write(&archive_path, vec![0_u8; 2_048]).unwrap();
        bundle.archive.as_mut().unwrap().object.local_path =
            archive_path.to_string_lossy().into_owned();
        ensure_dicom_receipt_chunks(
            &runtime.state,
            "run",
            std::slice::from_ref(&bundle.subject_id),
            &[bundle.total_size()],
        )
        .unwrap();
        runtime
            .state
            .set_chunk_worker("run", 0, "22222222-2222-4222-8222-222222222222")
            .unwrap();
        let manifest = LocalManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.into(),
            run_id: "run".into(),
            site_id: config.site_id.clone(),
            project_id: config.project_id.clone(),
            consent_policy_version: config.consent_policy_version.clone(),
            client_version: crate::CLIENT_VERSION.into(),
            metadata_policy: crate::archive::metadata_policy(),
            created_at: "2026-07-18T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: vec![bundle],
        };

        runtime
            .continue_dicom_upload("run", &manifest, &config)
            .await
            .unwrap();
        runtime
            .continue_dicom_upload("run", &manifest, &config)
            .await
            .unwrap();

        assert_eq!(status_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(credential_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(create_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(part_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(completion_attempts.load(Ordering::SeqCst), 0);
        let chunk = &runtime.state.run_uploads("run").unwrap()[0];
        assert_eq!(chunk.status, "reconciled");
        assert!(chunk.worker_upload_id.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn dicom_checkpoint_resume_and_rerun_never_create_a_duplicate_upload() {
        use axum::{Json, Router, http::StatusCode, routing::get, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let create_attempts = Arc::new(AtomicUsize::new(0));
        let create_attempts_for_handler = Arc::clone(&create_attempts);
        let credential_attempts = Arc::new(AtomicUsize::new(0));
        let credential_attempts_for_handler = Arc::clone(&credential_attempts);
        let part_attempts = Arc::new(AtomicUsize::new(0));
        let part_attempts_for_handler = Arc::clone(&part_attempts);
        let completion_attempts = Arc::new(AtomicUsize::new(0));
        let completion_attempts_for_handler = Arc::clone(&completion_attempts);
        let status_attempts = Arc::new(AtomicUsize::new(0));
        let status_attempts_for_handler = Arc::clone(&status_attempts);
        let app = Router::new()
            .route(
                "/v1/dicom-uploads",
                post(move || {
                    create_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}",
                get(move || {
                    status_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "uploading"
                        }))
                    }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/credentials",
                post(move || {
                    credential_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/parts",
                post(move || {
                    part_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/dicom-uploads/{upload_id}/complete",
                post(move || {
                    completion_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(serde_json::json!({
                            "upload_id": "22222222-2222-4222-8222-222222222222",
                            "status": "already_received",
                            "already_received_series": [{
                                "series_archive_id": "111111111111111111111111",
                                "receipt_upload_id": "33333333-3333-4333-8333-333333333333"
                            }]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config = sync_test_config();
        config.api_url = format!("http://{address}");
        let api = IngestApi::from_config(&config).unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        let (bundle, chunk) = write_saved_dicom_completion_checkpoint(
            &runtime,
            "run",
            "22222222-2222-4222-8222-222222222222",
        );

        runtime
            .continue_dicom_upload_chunk(
                "run",
                &chunk,
                std::slice::from_ref(&bundle),
                crate::CLIENT_VERSION,
                &api,
            )
            .await
            .unwrap();

        let manifest = LocalManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.into(),
            run_id: "run".into(),
            site_id: config.site_id.clone(),
            project_id: config.project_id.clone(),
            consent_policy_version: config.consent_policy_version.clone(),
            client_version: crate::CLIENT_VERSION.into(),
            metadata_policy: crate::archive::metadata_policy(),
            created_at: "2026-07-18T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: vec![bundle],
        };
        runtime
            .continue_dicom_upload("run", &manifest, &config)
            .await
            .unwrap();
        runtime
            .continue_dicom_upload("run", &manifest, &config)
            .await
            .unwrap();

        assert_eq!(status_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(completion_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(create_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(credential_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(part_attempts.load(Ordering::SeqCst), 0);
        let saved = runtime.state.run_uploads("run").unwrap();
        assert_eq!(saved[0].status, "reconciled");
        assert!(saved[0].worker_upload_id.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn saved_completion_already_committed_is_a_network_no_op_after_status() {
        use axum::{Json, Router, http::StatusCode, routing::get, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let credential_attempts = Arc::new(AtomicUsize::new(0));
        let credential_attempts_for_handler = credential_attempts.clone();
        let part_attempts = Arc::new(AtomicUsize::new(0));
        let part_attempts_for_handler = part_attempts.clone();
        let completion_attempts = Arc::new(AtomicUsize::new(0));
        let completion_attempts_for_handler = completion_attempts.clone();
        let app = Router::new()
            .route(
                "/v1/uploads/{upload_id}",
                get(|| async {
                    Json(serde_json::json!({
                        "upload_id": "22222222-2222-4222-8222-222222222222",
                        "status": "committed"
                    }))
                }),
            )
            .route(
                "/v1/uploads/{upload_id}/credentials",
                post(move || {
                    credential_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/uploads/{upload_id}/parts",
                post(move || {
                    part_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            )
            .route(
                "/v1/uploads/{upload_id}/complete",
                post(move || {
                    completion_attempts_for_handler.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::INTERNAL_SERVER_ERROR }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy-1".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        let (bundle, chunk) = write_saved_completion_checkpoint(
            &runtime,
            "run",
            "22222222-2222-4222-8222-222222222222",
        );

        runtime
            .continue_upload_chunk("run", &chunk, &[bundle], crate::CLIENT_VERSION, &api)
            .await
            .unwrap();

        assert_eq!(credential_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(part_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(completion_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime.state.run_uploads("run").unwrap()[0].status,
            "committed"
        );
        server.abort();
    }

    #[test]
    fn existing_bundle_reconciliation_rejects_identity_mismatch() {
        let requested = upload_test_bundle('1', '2');
        let mismatched = ExistingArchiveBundle {
            bundle_id: requested.bundle_id.clone(),
            series_id: "f".repeat(24),
            subject_id: requested.subject_id.clone(),
            session_id: requested.session_id.clone(),
            protocol_group_id: requested.protocol_group_id.clone(),
            upload_id: "11111111-1111-4111-8111-111111111111".into(),
            nii_uncompressed_sha256: requested
                .nifti
                .as_ref()
                .unwrap()
                .uncompressed_sha256
                .clone()
                .unwrap(),
        };
        assert!(validate_existing_bundles(&[requested], &[mismatched]).is_err());
    }

    #[tokio::test]
    async fn same_folder_command_continues_the_existing_failed_upload_run() {
        let state_root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        sync_test_config().save(&runtime.paths).unwrap();
        let canonical_source = source.path().canonicalize().unwrap();
        runtime
            .state
            .create_run("checkpointed-run", &canonical_source, false)
            .unwrap();
        write_empty_sync_checkpoint(&runtime, "checkpointed-run", "upload_failed");
        let fingerprint = snapshot_source_with_progress(&canonical_source, |_| {})
            .unwrap()
            .fingerprint(&canonical_source)
            .unwrap();
        runtime
            .state
            .set_source_fingerprint("checkpointed-run", &fingerprint)
            .unwrap();
        runtime
            .state
            .update_run(
                "checkpointed-run",
                "upload_failed",
                &SourceSummary::default(),
                Some("upload_failed"),
            )
            .unwrap();

        let selected = runtime
            .sync_folder(source.path().to_path_buf(), false)
            .await
            .unwrap();
        assert_eq!(selected, "checkpointed-run");
        let completed = runtime.state.latest_run().unwrap().unwrap();
        assert_eq!(completed.id, "checkpointed-run");
        assert_eq!(completed.status, "complete");
    }

    #[tokio::test]
    async fn changed_source_supersedes_an_unfinished_run_before_continuation() {
        let state_root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("export.bin"), b"first export").unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        sync_test_config().save(&runtime.paths).unwrap();
        let canonical_source = source.path().canonicalize().unwrap();
        runtime
            .state
            .create_run("stale-run", &canonical_source, false)
            .unwrap();
        write_empty_sync_checkpoint(&runtime, "stale-run", "upload_failed");
        let fingerprint = snapshot_source_with_progress(&canonical_source, |_| {})
            .unwrap()
            .fingerprint(&canonical_source)
            .unwrap();
        runtime
            .state
            .set_source_fingerprint("stale-run", &fingerprint)
            .unwrap();
        runtime
            .state
            .update_run(
                "stale-run",
                "upload_failed",
                &SourceSummary::default(),
                Some("upload_failed"),
            )
            .unwrap();
        fs::write(source.path().join("export.bin"), b"second export").unwrap();

        let selected = runtime
            .sync_folder(source.path().to_path_buf(), false)
            .await
            .unwrap();
        assert_ne!(selected, "stale-run");
        let stale = runtime.state.run("stale-run").unwrap().unwrap();
        assert_eq!(stale.status, "superseded");
        assert_eq!(
            stale.error_code.as_deref(),
            Some("source_changed_since_checkpoint")
        );
    }

    #[tokio::test]
    async fn same_size_archive_tampering_blocks_network_even_with_a_saved_part() {
        use axum::{Router, routing::any};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_handler = requests.clone();
        let app = Router::new().fallback(any(move || {
            requests_for_handler.fetch_add(1, Ordering::SeqCst);
            async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let state_root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        let archive_path = state_root.path().join("dicom.tar.zst");
        fs::write(&archive_path, b"AAAA").unwrap();
        let expected_hash = hex::encode(Sha256::digest(b"AAAA"));
        let mut bundle = dicom_upload_test_bundle('1', '2');
        let object = &mut bundle.archive.as_mut().unwrap().object;
        object.local_path = archive_path.to_string_lossy().into_owned();
        object.size = 4;
        object.sha256 = expected_hash.clone();
        let config = ClientConfig {
            api_url: format!("http://{address}"),
            ..sync_test_config()
        };
        let manifest = LocalManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.into(),
            run_id: "tampered-run".into(),
            site_id: config.site_id.clone(),
            project_id: config.project_id.clone(),
            consent_policy_version: config.consent_policy_version.clone(),
            client_version: crate::CLIENT_VERSION.into(),
            metadata_policy: crate::archive::metadata_policy(),
            created_at: "2026-07-18T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: vec![bundle],
        };
        runtime
            .state
            .create_run("tampered-run", source.path(), false)
            .unwrap();
        runtime
            .state
            .ensure_run_uploads("tampered-run", &["3".repeat(24)], &[4], 8, 1024)
            .unwrap();
        let upload_id = "22222222-2222-4222-8222-222222222222";
        runtime
            .state
            .set_chunk_worker("tampered-run", 0, upload_id)
            .unwrap();
        runtime
            .state
            .add_upload_object(&UploadObjectRecord {
                run_id: "tampered-run".into(),
                worker_upload_id: upload_id.into(),
                key: "prefix/dicom.tar.zst".into(),
                local_path: archive_path.to_string_lossy().into_owned(),
                size: 4,
                sha256: expected_hash,
                multipart_id: Some("multipart".into()),
                status: "uploading".into(),
                etag: None,
            })
            .unwrap();
        runtime
            .state
            .save_part(
                upload_id,
                "prefix/dicom.tar.zst",
                &crate::state::UploadedPart {
                    part_number: 1,
                    etag: "saved-etag".into(),
                    size: 2,
                },
            )
            .unwrap();
        fs::write(&archive_path, b"BBBB").unwrap();

        let error = verify_prepared_objects(&manifest).await.unwrap_err();
        assert!(error.to_string().contains("hash does not match"));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime
                .state
                .uploaded_parts(upload_id, "prefix/dicom.tar.zst")
                .unwrap()
                .len(),
            1
        );
        server.abort();
    }

    #[tokio::test]
    async fn unchanged_completed_folder_is_a_local_no_op() {
        let state_root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("scan.dcm"), b"stable source fixture").unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        sync_test_config().save(&runtime.paths).unwrap();
        let canonical_source = source.path().canonicalize().unwrap();
        runtime
            .state
            .create_run("completed-run", &canonical_source, false)
            .unwrap();
        write_empty_sync_checkpoint(&runtime, "completed-run", "complete");
        runtime
            .state
            .update_run("completed-run", "complete", &SourceSummary::default(), None)
            .unwrap();
        let fingerprint = snapshot_source_with_progress(&canonical_source, |_| {})
            .unwrap()
            .fingerprint(&canonical_source)
            .unwrap();
        runtime
            .state
            .set_source_fingerprint("completed-run", &fingerprint)
            .unwrap();

        let selected = runtime
            .sync_folder(source.path().to_path_buf(), false)
            .await
            .unwrap();
        assert_eq!(selected, "completed-run");
        assert_eq!(
            runtime.state.latest_run().unwrap().unwrap().id,
            "completed-run"
        );
    }

    #[tokio::test]
    async fn old_no_eligible_result_is_reclassified_after_client_upgrade() {
        let state_root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(
            source.path().join("not-dicom.txt"),
            b"stable source fixture",
        )
        .unwrap();
        let runtime = Runtime::initialize(Some(state_root.path())).unwrap();
        sync_test_config().save(&runtime.paths).unwrap();
        let canonical_source = source.path().canonicalize().unwrap();
        runtime
            .state
            .create_run("old-no-eligible", &canonical_source, false)
            .unwrap();
        write_empty_sync_checkpoint(&runtime, "old-no-eligible", "complete_no_eligible_series");
        let checkpoint = runtime.state.run("old-no-eligible").unwrap().unwrap();
        let mut manifest = load_checkpoint_manifest(&checkpoint).unwrap();
        manifest.client_version = "0.3.0".into();
        manifest.metadata_policy = crate::archive::metadata_policy();
        write_json(
            Path::new(checkpoint.manifest_path.as_deref().unwrap()),
            &manifest,
        )
        .unwrap();
        runtime
            .state
            .update_run(
                "old-no-eligible",
                "complete_no_eligible_series",
                &SourceSummary::default(),
                None,
            )
            .unwrap();
        let fingerprint = snapshot_source_with_progress(&canonical_source, |_| {})
            .unwrap()
            .fingerprint(&canonical_source)
            .unwrap();
        runtime
            .state
            .set_source_fingerprint("old-no-eligible", &fingerprint)
            .unwrap();

        let selected = runtime
            .sync_folder(source.path().to_path_buf(), false)
            .await
            .unwrap();
        assert_ne!(selected, "old-no-eligible");
        assert_eq!(
            runtime.state.latest_run().unwrap().unwrap().status,
            "complete_no_eligible_series"
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_lock_and_database_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(directory.path())).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&runtime.paths.lock), 0o600);
        assert_eq!(mode(&runtime.paths.database), 0o600);
        assert_eq!(mode(&runtime.paths.root), 0o700);
        assert_eq!(mode(&runtime.paths.work), 0o700);
        assert_eq!(mode(&runtime.paths.bundles), 0o700);
        assert_eq!(mode(&runtime.paths.reports), 0o700);
    }

    #[test]
    fn automatic_continuation_requires_current_privacy_and_original_enrollment() {
        let config = ClientConfig {
            api_url: crate::DEFAULT_API_URL.into(),
            device_token: "sn_device_fixture".into(),
            site_id: "site-a".into(),
            project_id: "project-a".into(),
            project_name: "Project A".into(),
            consent_policy_version: "policy-2".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let mut manifest = LocalManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.into(),
            run_id: "run".into(),
            site_id: "site-a".into(),
            project_id: "project-a".into(),
            consent_policy_version: "policy-2".into(),
            client_version: crate::CLIENT_VERSION.into(),
            metadata_policy: crate::model::MetadataPolicy {
                policy_id: METADATA_POLICY_ID.into(),
                policy_version: METADATA_POLICY_VERSION.into(),
            },
            created_at: "2026-07-12T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: Vec::new(),
        };
        assert!(validate_manifest_enrollment(&manifest, &config).is_ok());
        manifest.client_version = "0.1.0".into();
        assert!(validate_manifest_enrollment(&manifest, &config).is_err());
        manifest.client_version = crate::CLIENT_VERSION.into();
        manifest.metadata_policy.policy_version = "1.0.0".into();
        assert!(validate_manifest_enrollment(&manifest, &config).is_err());
        manifest.metadata_policy.policy_version = METADATA_POLICY_VERSION.into();
        manifest.project_id = "project-b".into();
        assert!(validate_manifest_enrollment(&manifest, &config).is_err());
        manifest.project_id = "project-a".into();
        manifest.consent_policy_version = "policy-1".into();
        assert!(validate_manifest_enrollment(&manifest, &config).is_err());
    }

    #[test]
    fn privacy_checkpoint_rejects_old_or_tampered_sidecars_but_allows_future_patch_clients() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar_path = directory.path().join("scan.json");
        let mut bundle = upload_test_bundle('1', '2');
        bundle.metadata.as_mut().unwrap().local_path = sidecar_path.to_string_lossy().into_owned();
        let mut sidecar: ScanSidecar = serde_json::from_str(include_str!(
            "../../schemas/examples/scan-sidecar-v1.example.json"
        ))
        .unwrap();
        sidecar.bundle_id = bundle.bundle_id.clone();
        sidecar.series_id = bundle.series_id.clone();
        sidecar.subject_id = bundle.subject_id.clone();
        sidecar.session_id = bundle.session_id.clone();
        sidecar.protocol_group_id = bundle.protocol_group_id.clone();
        sidecar.conversion.client_version = crate::CLIENT_VERSION.into();
        fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
        let mut manifest = LocalManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.into(),
            run_id: "run".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            consent_policy_version: "policy".into(),
            client_version: crate::CLIENT_VERSION.into(),
            metadata_policy: crate::model::MetadataPolicy {
                policy_id: METADATA_POLICY_ID.into(),
                policy_version: METADATA_POLICY_VERSION.into(),
            },
            created_at: "2026-07-12T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: vec![bundle],
        };
        assert!(manifest_uses_current_privacy_contract(&manifest));

        let mut tampered = serde_json::to_value(&sidecar).unwrap();
        tampered
            .as_object_mut()
            .unwrap()
            .insert("patient_name".into(), serde_json::json!("private"));
        fs::write(&sidecar_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(!manifest_uses_current_privacy_contract(&manifest));

        sidecar.metadata_policy.policy_version = "1.0.0".into();
        fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
        assert!(!manifest_uses_current_privacy_contract(&manifest));

        sidecar.metadata_policy.policy_version = METADATA_POLICY_VERSION.into();
        sidecar.conversion.client_version = "0.1.0".into();
        manifest.client_version = "0.1.0".into();
        fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
        assert!(!manifest_uses_current_privacy_contract(&manifest));

        sidecar.conversion.client_version = "0.1.2".into();
        manifest.client_version = "0.1.2".into();
        fs::write(&sidecar_path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
        assert!(manifest_uses_current_privacy_contract(&manifest));
    }

    #[test]
    fn pending_enrollment_is_private_invite_and_origin_keyed() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path())).unwrap();
        paths.initialize().unwrap();
        let invite = "sn_invite_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let origin = "https://ingest.scalingneuro.org";
        let first =
            load_or_create_pending_enrollment(&paths, invite, origin, "Fixture workstation".into())
                .unwrap();
        let second =
            load_or_create_pending_enrollment(&paths, invite, origin, "Changed name".into())
                .unwrap();
        assert_eq!(first.enrollment_id, second.enrollment_id);
        assert_eq!(first.device_token, second.device_token);
        assert_eq!(second.device_name, "Fixture workstation");
        assert_eq!(second.api_origin, origin);
        let mut old_pending = second.clone();
        old_pending.client_version = "0.1.0".into();
        old_pending.platform = "old-platform".into();
        write_private_json_atomic(&paths.pending_enrollment, &old_pending).unwrap();
        let upgraded =
            load_or_create_pending_enrollment(&paths, invite, origin, "Ignored name".into())
                .unwrap();
        assert_eq!(upgraded.enrollment_id, first.enrollment_id);
        assert_eq!(upgraded.device_token, first.device_token);
        assert_eq!(upgraded.client_version, crate::CLIENT_VERSION);
        assert_eq!(
            upgraded.platform,
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
        );
        let saved = fs::read_to_string(&paths.pending_enrollment).unwrap();
        assert!(!saved.contains(invite));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&paths.pending_enrollment)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let other_origin = load_or_create_pending_enrollment(
            &paths,
            invite,
            "https://other.scalingneuro.org",
            "Other origin".into(),
        )
        .unwrap();
        assert_ne!(first.enrollment_id, other_origin.enrollment_id);
        assert_ne!(first.device_token, other_origin.device_token);

        let replacement = load_or_create_pending_enrollment(
            &paths,
            "sn_invite_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            origin,
            "Replacement".into(),
        )
        .unwrap();
        assert_ne!(first.enrollment_id, replacement.enrollment_id);
        assert_ne!(first.device_token, replacement.device_token);
    }

    #[test]
    fn pending_public_registration_is_private_and_exactly_replayable() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::discover(Some(directory.path())).unwrap();
        paths.initialize().unwrap();
        let details = ContributorDetails {
            contact_email: "Researcher@Example.edu".into(),
            contact_name: "Example Researcher".into(),
            institution_name: "Example University".into(),
            institution_ror_id: Some("https://ror.org/03yrm5c26".into()),
            lab_name: "Example Neuroimaging Lab".into(),
            contact_opt_in: true,
        };
        let normalized = normalize_contributor_details(details).unwrap();
        let first = load_or_create_pending_registration(
            &paths,
            "https://scalingneuro.com",
            "Fixture workstation".into(),
            normalized.clone(),
            "open-epi-1.0.0".into(),
        )
        .unwrap();
        let second = load_or_create_pending_registration(
            &paths,
            "https://scalingneuro.com",
            "Changed workstation".into(),
            normalized.clone(),
            "open-epi-1.0.0".into(),
        )
        .unwrap();
        assert_eq!(first.registration_id, second.registration_id);
        assert_eq!(first.device_token, second.device_token);
        assert_eq!(second.device_name, "Fixture workstation");
        assert_eq!(second.details.contact_email, "researcher@example.edu");

        let replacement = load_or_create_pending_registration(
            &paths,
            "https://scalingneuro.com",
            "Fixture workstation".into(),
            ContributorDetails {
                contact_email: "other@example.edu".into(),
                lab_name: "Different Lab".into(),
                ..normalized
            },
            "open-epi-1.0.0".into(),
        )
        .unwrap();
        assert_ne!(first.registration_id, replacement.registration_id);
        assert_ne!(first.device_token, replacement.device_token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&paths.pending_registration)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn public_registration_saves_an_upload_ready_client_configuration() {
        use axum::{Json, Router, http::StatusCode, routing::post};

        let app = Router::new().route(
            "/v1/register",
            post(|Json(body): Json<serde_json::Value>| async move {
                assert_eq!(body["client_version"], crate::CLIENT_VERSION);
                assert_eq!(body["contact_email"], "researcher@example.edu");
                assert_eq!(body["accepted_consent_policy_version"], "open-epi-1.0.0");
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({
                        "enrollment_id": body["registration_id"],
                        "device_token": body["device_token"],
                        "device_id": "11111111-1111-4111-8111-111111111111",
                        "site_id": "22222222-2222-4222-8222-222222222222",
                        "project_id": "33333333-3333-4333-8333-333333333333",
                        "project_name": "Scaling Neuro public EPI contribution",
                        "consent_policy_version": "open-epi-1.0.0",
                        "pseudonym_key_b64": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="
                    })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::initialize(Some(directory.path())).unwrap();
        let config = runtime
            .register(
                ContributorDetails {
                    contact_email: "Researcher@Example.edu".into(),
                    contact_name: "Example Researcher".into(),
                    institution_name: "Example University".into(),
                    institution_ror_id: None,
                    lab_name: "Example Lab".into(),
                    contact_opt_in: false,
                },
                "open-epi-1.0.0".into(),
                &format!("http://{address}"),
                "Fixture workstation".into(),
            )
            .await
            .unwrap();
        assert_eq!(config.project_name, "Scaling Neuro public EPI contribution");
        assert_eq!(
            ClientConfig::load(&runtime.paths).unwrap().site_id,
            config.site_id
        );
        assert!(!runtime.paths.pending_registration.exists());
        server.abort();
    }
}
