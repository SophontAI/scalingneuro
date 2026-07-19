use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
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
    MANIFEST_SCHEMA_VERSION, PINNED_DCM2NIIX_VERSION,
    api::{
        CompleteUploadRequest, CompletedObject, ContributionInfo, CreateUploadResponse, IngestApi,
        MultipartObject, RegisterRequest, UploadStatus, has_error_code, normalize_base_url,
    },
    bundle::{
        BundleRequest, METADATA_POLICY_ID, METADATA_POLICY_VERSION, analyze_converted,
        create_bundle,
    },
    classify::{ConversionSignals, classify_header, refine_after_conversion},
    config::{AppPaths, ClientConfig},
    convert::Converter,
    dicom::{Discovery, SeriesGroup, discover_with_progress, snapshot_source_with_progress},
    model::{
        Classification, ClassificationDecision, ClassificationEvidence, ExistingArchiveBundle,
        HeldSeries, LocalManifest, ManifestBundle, ReportBundle, RunReport, ScanSidecar,
        SourceSummary,
    },
    pseudonym::Pseudonymizer,
    s3::MultipartUploader,
    state::{RunRecord, StateStore, UploadObjectRecord},
};

const MAX_BUNDLES_PER_UPLOAD: usize = 32;
const MAX_BYTES_PER_UPLOAD: u64 = 32 * 1024 * 1024 * 1024;
const MAX_NIFTI_BYTES_PER_BUNDLE: u64 = 5 * 1024 * 1024 * 1024;
const SOURCE_QUIET_INTERVAL: Duration = Duration::from_secs(2);
const ARCHIVE_VERIFICATION_WAIT: Duration = Duration::from_secs(30 * 60);

struct LocalValidationProgress<'a> {
    state: &'a StateStore,
    run_id: &'a str,
    total_series: usize,
    last_report: Instant,
}

impl LocalValidationProgress<'_> {
    fn checkpoint(&mut self, summary: &SourceSummary, processed_series: usize) -> Result<()> {
        self.state
            .update_run(self.run_id, "converting", summary, None)?;
        if self.last_report.elapsed() >= Duration::from_secs(2)
            || processed_series == self.total_series
        {
            tracing::info!(
                processed_series,
                total_series = self.total_series,
                accepted = summary.accepted,
                held = summary.held,
                excluded = summary.excluded,
                "Local validation progress"
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
            && completed_run_matches_config(&completed, &config)
            && let Some(previous) = self.state.source_fingerprint(&completed.id)?
        {
            tracing::info!(
                files = previous.file_count,
                "Checking whether this folder changed since its completed sync"
            );
            let current_snapshot = snapshot_source_with_progress(&canonical_source, |progress| {
                tracing::info!(
                    files_checked = progress.files_seen,
                    "Completed folder comparison progress"
                );
            })?;
            let current = current_snapshot.fingerprint(&canonical_source)?;
            if current == previous {
                tracing::info!(
                    run_id = %completed.id,
                    files = current.file_count,
                    "Folder is already fully synced; nothing will be converted or uploaded"
                );
                return Ok(completed.id);
            }
            tracing::info!(
                previous_files = previous.file_count,
                current_files = current.file_count,
                "Folder contents changed; checking the current export for new eligible scans"
            );
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
        if manifest_uses_current_privacy_contract(&manifest) {
            tracing::info!(
                run_id = %run.id,
                previous_status = %run.status,
                "Found checkpointed work for this folder; continuing without reconversion"
            );
            self.continue_prepared_run(&run, &manifest, &config).await?;
            return Ok(run.id);
        }

        let expected_bundle_ids = manifest
            .bundles
            .iter()
            .map(|bundle| bundle.bundle_id.clone())
            .collect::<HashSet<_>>();
        if expected_bundle_ids.is_empty() {
            bail!("the outdated local checkpoint has no prepared EPI bundles");
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
        if let Err(error) = self.continue_upload(run_id, &manifest, &config).await {
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

    async fn continue_upload(
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
            .map(|bundle| bundle.nifti.size.saturating_add(bundle.metadata.size))
            .collect::<Vec<_>>();
        self.state.ensure_run_uploads(
            run_id,
            &bundle_subjects,
            &bundle_sizes,
            MAX_BUNDLES_PER_UPLOAD,
            MAX_BYTES_PER_UPLOAD,
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
        let completion_path = self.paths.reports.join(format!(
            "{run_id}.chunk-{}.complete-request.json",
            chunk.chunk_index
        ));
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

    async fn continue_prepared_run(
        &self,
        run: &RunRecord,
        manifest: &LocalManifest,
        config: &ClientConfig,
    ) -> Result<()> {
        match self.continue_upload(&run.id, manifest, config).await {
            Ok(()) => {
                self.state
                    .update_run(&run.id, "complete", &manifest.source_summary, None)?;
                let chunks = self.state.run_uploads(&run.id)?;
                let worker_upload_ids = chunks
                    .iter()
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
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
        restrict_private_file(&paths.pending_enrollment)?;
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
        restrict_private_file(&paths.pending_registration)?;
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
    restrict_private_file(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|_| anyhow::anyhow!("could not commit private pending enrollment state"))?;
    restrict_private_file(path)
}

#[cfg(unix)]
fn restrict_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_private_file(_path: &Path) -> Result<()> {
    Ok(())
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

fn manifest_uses_current_privacy_contract(manifest: &LocalManifest) -> bool {
    if !privacy_client_version_supported(&manifest.client_version)
        || manifest.metadata_policy.policy_id != METADATA_POLICY_ID
        || manifest.metadata_policy.policy_version != METADATA_POLICY_VERSION
    {
        return false;
    }
    manifest.bundles.iter().all(|bundle| {
        let Ok(bytes) = fs::read(&bundle.metadata.local_path) else {
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
    let started = Instant::now();
    let mut last_progress: Option<(u32, u32)> = None;
    let mut last_notice = Instant::now() - Duration::from_secs(60);
    loop {
        match api.complete_upload(upload_id, objects.to_vec()).await {
            Ok(status) if matches!(status.status.as_str(), "committed" | "complete") => {
                return Ok(status);
            }
            Ok(status) if matches!(status.status.as_str(), "created" | "uploading") => {
                log_archive_verification_progress(&status, &mut last_progress, &mut last_notice);
            }
            Ok(status) => return Ok(status),
            Err(error) if has_error_code(&error, "CONFLICT") => {
                let status = api.status(upload_id).await?;
                if matches!(status.status.as_str(), "committed" | "complete") {
                    return Ok(status);
                }
                if !matches!(status.status.as_str(), "created" | "uploading") {
                    return Err(error);
                }
                log_archive_verification_progress(&status, &mut last_progress, &mut last_notice);
            }
            Err(error) => return Err(error),
        }
        if started.elapsed() >= ARCHIVE_VERIFICATION_WAIT {
            bail!(
                "archive verification is still pending after 30 minutes; rerun the same `neuro-sync <folder>` command to continue without reconversion or reuploading completed files"
            );
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn log_archive_verification_progress(
    status: &UploadStatus,
    last_progress: &mut Option<(u32, u32)>,
    last_notice: &mut Instant,
) {
    let progress = status
        .verification
        .as_ref()
        .map(|value| (value.verified_series, value.total_series));
    if progress != *last_progress || last_notice.elapsed() >= Duration::from_secs(30) {
        if let Some((verified_series, total_series)) = progress {
            tracing::info!(
                verified_series,
                total_series,
                "Server archive verification progress; transferred files remain checkpointed"
            );
        } else {
            tracing::info!(
                "Server archive verification is still running; transferred files remain checkpointed"
            );
        }
        *last_progress = progress;
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
    dry_run: bool,
    config: &ClientConfig,
) -> Result<(LocalManifest, RunReport)> {
    let started_at = Utc::now().to_rfc3339();
    state.update_run(run_id, "discovering", &SourceSummary::default(), None)?;
    tracing::info!("Scanning the selected folder for DICOM headers");
    let discovery = discover_with_progress(source, |progress| {
        tracing::info!(
            files_checked = progress.files_seen,
            dicom_files = progress.dicom_files,
            series = progress.series_found,
            "DICOM discovery progress"
        );
    })?;
    tracing::info!(
        files_checked = discovery.summary.files_seen,
        dicom_files = discovery.summary.dicom_files,
        series = discovery.summary.series_found,
        "DICOM discovery complete"
    );
    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    let mut summary = discovery.summary.clone();
    state.update_run(run_id, "converting", &summary, None)?;
    let pseudonymizer = Pseudonymizer::from_base64(&config.pseudonym_key_b64)?;
    tracing::info!("Confirming that the DICOM export has stopped changing");
    let quiet_snapshot = snapshot_source_with_progress(source, |progress| {
        tracing::info!(
            files_checked = progress.files_seen,
            "Source stability check progress"
        );
    })?;
    if discovery.unreadable_dicom_like_files > 0
        || !discovery.source_snapshot.is_stable_with(&quiet_snapshot)
    {
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
    let converter = Converter::discover(&paths.work)?;
    if !dry_run && converter.version != PINNED_DCM2NIIX_VERSION {
        bail!("unvalidated converter builds are allowed only with --dry-run");
    }
    let bundle_root = paths.bundles.join(run_id);
    fs::create_dir_all(&bundle_root)?;
    let mut bundles = Vec::new();
    let mut held_series = Vec::new();
    let series_total = discovery.series.len();
    let mut local_progress = LocalValidationProgress {
        state,
        run_id,
        total_series: series_total,
        last_report: Instant::now(),
    };

    for (index, group) in discovery.series.iter().enumerate() {
        let initial = classify_header(group);
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
        tracing::info!(
            series = index + 1,
            total_series = series_total,
            dicom_files = group.files.len(),
            "Converting eligible EPI series"
        );
        let converted = match converter.convert(group, &paths.work) {
            Ok(converted) => converted,
            Err(_) => {
                let classification = coded_hold("conversion_failed", "converter_sidecar");
                summary.held += 1;
                held_series.push(held(&pseudonymizer, group, index, &classification));
                local_progress.checkpoint(&summary, index + 1)?;
                continue;
            }
        };
        if converted.images.is_empty() {
            let classification = refine_after_conversion(initial, &ConversionSignals::default());
            summary.held += 1;
            held_series.push(held(&pseudonymizer, group, index, &classification));
            local_progress.checkpoint(&summary, index + 1)?;
            continue;
        }
        let Some(echo_labels) = multi_echo_labels(&converted.images) else {
            let classification = coded_hold("conversion_output_ambiguous", "converter_sidecar");
            summary.held += 1;
            held_series.push(held(&pseudonymizer, group, index, &classification));
            local_progress.checkpoint(&summary, index + 1)?;
            continue;
        };
        let mut prepared = Vec::with_capacity(converted.images.len());
        let mut failed_classification = None;
        for (image_index, (image, echo_label)) in
            converted.images.iter().zip(echo_labels).enumerate()
        {
            let analyzed = match analyze_converted(group, image, 1) {
                Ok(analyzed) => analyzed,
                Err(_) => {
                    failed_classification =
                        Some(coded_hold("nifti_validation_failed", "nifti_header"));
                    break;
                }
            };
            let classification = refine_after_conversion(initial.clone(), &analyzed.signals);
            if classification.decision != ClassificationDecision::Accepted || !analyzed.qc.passed {
                failed_classification = Some(if analyzed.qc.passed {
                    classification
                } else {
                    coded_hold("qc_failed", "derived")
                });
                break;
            }
            prepared.push((image_index, echo_label, analyzed, classification));
        }
        if let Some(classification) = failed_classification {
            summary.held += 1;
            held_series.push(held(&pseudonymizer, group, index, &classification));
            local_progress.checkpoint(&summary, index + 1)?;
            continue;
        }
        let mut created = Vec::with_capacity(prepared.len());
        let mut creation_failed = false;
        for (image_index, echo_label, analyzed, classification) in prepared {
            match create_bundle(BundleRequest {
                group,
                converted: &converted,
                image: &converted.images[image_index],
                analyzed: &analyzed,
                classification,
                pseudonymizer: &pseudonymizer,
                bundle_root: &bundle_root,
                echo_label: echo_label.as_deref(),
            }) {
                Ok(bundle) => created.push(bundle),
                Err(_) => {
                    creation_failed = true;
                    break;
                }
            }
        }
        let exceeds_upload_limit = created
            .iter()
            .any(|bundle| bundle.nifti.size > MAX_NIFTI_BYTES_PER_BUNDLE);
        if creation_failed || exceeds_upload_limit {
            for bundle in &created {
                if let Some(directory) = Path::new(&bundle.nifti.local_path).parent() {
                    let _ = fs::remove_dir_all(directory);
                }
            }
            summary.held += 1;
            let classification = coded_hold(
                if exceeds_upload_limit {
                    "bundle_exceeds_upload_limit"
                } else {
                    "bundle_creation_failed"
                },
                "derived",
            );
            held_series.push(held(&pseudonymizer, group, index, &classification));
        } else {
            summary.accepted += 1;
            bundles.extend(created);
        }
        local_progress.checkpoint(&summary, index + 1)?;
    }

    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    tracing::info!("Performing final source stability check");
    let final_snapshot = snapshot_source_with_progress(source, |progress| {
        tracing::info!(
            files_checked = progress.files_seen,
            "Final source stability check progress"
        );
    })?;
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
        "Local validation complete"
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
        metadata_policy: crate::model::MetadataPolicy {
            policy_id: METADATA_POLICY_ID.into(),
            policy_version: METADATA_POLICY_VERSION.into(),
        },
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
        metadata_policy: crate::model::MetadataPolicy {
            policy_id: METADATA_POLICY_ID.into(),
            policy_version: METADATA_POLICY_VERSION.into(),
        },
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
            for object in [&bundle.nifti, &bundle.metadata] {
                let key = format!("{prefix}{}", object.relative_key);
                if descriptor_keys.contains(key.as_str()) {
                    bail!("ingest API allocated an object for an already archived bundle");
                }
            }
            continue;
        }
        for object in [&bundle.nifti, &bundle.metadata] {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
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
            nifti: crate::model::ManifestObject {
                relative_key: format!("{bundle_id}/scan_bold.nii.gz"),
                local_path: format!("/private/{bundle_id}/scan_bold.nii.gz"),
                size: 1_024,
                sha256: "a".repeat(64),
                uncompressed_sha256: Some("b".repeat(64)),
            },
            metadata: crate::model::ManifestObject {
                relative_key: format!("{bundle_id}/scan_bold.json"),
                local_path: format!("/private/{bundle_id}/scan_bold.json"),
                size: 512,
                sha256: "c".repeat(64),
                uncompressed_sha256: None,
            },
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
            nii_uncompressed_sha256: existing_bundle.nifti.uncompressed_sha256.clone().unwrap(),
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
            nii_uncompressed_sha256: bundle.nifti.uncompressed_sha256.clone().unwrap(),
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
    async fn completion_waits_through_a_busy_verifier_without_reuploading() {
        use axum::{
            Json, Router,
            http::StatusCode,
            routing::{get, post},
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let post_attempts = attempts.clone();
        let app = Router::new()
            .route(
                "/v1/uploads/{upload_id}/complete",
                post(move || {
                    let attempt = post_attempts.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if attempt < 5 {
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
                get(|| async {
                    Json(serde_json::json!({
                        "upload_id": "22222222-2222-4222-8222-222222222222",
                        "status": "uploading",
                        "verification": {"verified_series": 4, "total_series": 15}
                    }))
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
        let status = complete_with_recovery(&api, "22222222-2222-4222-8222-222222222222", &[])
            .await
            .unwrap();
        assert_eq!(status.status, "committed");
        assert_eq!(attempts.load(Ordering::SeqCst), 6);
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
            nii_uncompressed_sha256: requested.nifti.uncompressed_sha256.clone().unwrap(),
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
        bundle.metadata.local_path = sidecar_path.to_string_lossy().into_owned();
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
