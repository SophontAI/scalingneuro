use std::{
    collections::{HashMap, HashSet},
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
        AlreadyReceivedSeries, CompleteUploadRequest, ContributionInfo, IngestApi, MultipartObject,
        RegisterRequest, UploadStatus, has_error_code, is_transient_api_error, normalize_base_url,
    },
    archive::{
        ArchiveRequest, DICOM_METADATA_POLICY_ID, DICOM_METADATA_POLICY_VERSION,
        create_dicom_archive, metadata_policy,
    },
    classify::classify_header,
    config::{AppPaths, ClientConfig},
    dicom::{Discovery, DiscoveryPhase, SeriesGroup, SourceSnapshot, discover_with_progress},
    model::{
        Classification, ClassificationDecision, ClassificationEvidence, HeldSeries, LocalManifest,
        ManifestBundle, ReportBundle, RunReport, SourceSummary,
    },
    privacy,
    progress::{Progress, ProgressUnit},
    pseudonym::Pseudonymizer,
    s3::MultipartUploader,
    state::{RunRecord, StateStore, UploadObjectRecord},
};

// One archive per durable receipt keeps local staging bounded to one series and
// gives every series its own idempotent cleanup checkpoint.
const MAX_DICOM_SERIES_PER_UPLOAD: usize = 1;
const MAX_DICOM_ARCHIVE_BYTES_PER_SERIES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_DICOM_BYTES_PER_RECEIPT: u64 = 250 * 1024 * 1024 * 1024;
const SOURCE_QUIET_INTERVAL: Duration = Duration::from_secs(2);
const STAGING_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
struct StagingStorageExhausted {
    staging_root: PathBuf,
    required: u64,
    available: u64,
}

impl std::fmt::Display for StagingStorageExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "local staging storage is exhausted (requires at least {} bytes ({}), available {} bytes ({}) in {}); free local scratch space or set NEURO_SYNC_STAGING_DIR to a larger local directory",
            self.required,
            human_bytes(self.required),
            self.available,
            human_bytes(self.available),
            self.staging_root.display()
        )
    }
}

impl std::error::Error for StagingStorageExhausted {}

#[derive(Debug)]
struct UnreadableDicomLikeFiles {
    count: u64,
}

impl std::fmt::Display for UnreadableDicomLikeFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "found {} DICOM-like file{} that could not be parsed; nothing new was uploaded because the affected series could be incomplete. Re-export or repair those files, then rerun the same neuro-sync <folder> command",
            self.count,
            if self.count == 1 { "" } else { "s" }
        )
    }
}

impl std::error::Error for UnreadableDicomLikeFiles {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DicomReceiptPhase {
    /// Upload every multipart byte and persist the exact completion body, but
    /// do not ask the Worker to finalize R2 or record an archive receipt.
    TransferOnly,
    /// Finalize only an already-checkpointed transfer. This phase must never
    /// allocate a replacement session or read source bytes after the folder's
    /// final stability check.
    CommitOnly,
    /// Backward-compatible path for non-streaming checkpoints and focused
    /// recovery calls that still perform both phases in one invocation.
    TransferAndCommit,
}

struct InspectedDicomSource {
    started_at: String,
    summary: SourceSummary,
    unreadable_dicom_like_files: u64,
    source_snapshot: SourceSnapshot,
    series: Vec<(usize, SeriesGroup, Classification)>,
}

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
        let state = StateStore::open(&paths.database)?;
        recover_interrupted_preparations(&paths, &state)?;
        Ok(Self {
            paths,
            state,
            _instance_lock: Arc::new(instance_lock),
        })
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
        if response.registration_id != pending.registration_id
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

    pub async fn accept_contribution_policy(
        &self,
        config: &ClientConfig,
        policy_version: &str,
    ) -> Result<ClientConfig> {
        if policy_version.trim().is_empty() || policy_version.len() > 64 {
            bail!("contribution policy version is invalid");
        }
        let response = IngestApi::from_config(config)?
            .accept_device_policy(policy_version)
            .await?;
        if response.status != "accepted"
            || response.device_id.trim().is_empty()
            || response.site_id != config.site_id
            || response.project_id != config.project_id
            || response.consent_policy_version != policy_version
            || response.project_name.as_ref().is_some_and(|name| {
                name.trim().is_empty() || name.len() > 160 || name.chars().any(char::is_control)
            })
        {
            bail!("policy acceptance response did not match this registered workstation");
        }
        let mut updated = config.clone();
        updated.consent_policy_version = response.consent_policy_version;
        if let Some(project_name) = response.project_name {
            updated.project_name = project_name;
        }
        updated.save(&self.paths)?;
        Ok(updated)
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
                        Progress::spinner("Checking completed sync", ProgressUnit::Files);
                    let current_snapshot = discover_with_progress(&canonical_source, |progress| {
                        comparison.set(progress.files_seen)
                    })?
                    .source_snapshot;
                    comparison.finish_at(comparison.completed());
                    let current = fingerprint_source_with_progress(
                        &current_snapshot,
                        &canonical_source,
                        "Verifying completed folder contents",
                    )?;
                    confirm_source_snapshot_stable(
                        &current_snapshot,
                        &canonical_source,
                        "Confirming completed folder stability",
                    )?;
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
        validate_manifest_scope(&manifest, &config)?;
        let policy_is_current = !manifest.consent_policy_version.is_empty()
            && manifest.consent_policy_version == config.consent_policy_version;
        if !manifest_uses_current_privacy_contract(&manifest) || !policy_is_current {
            let run_id = Uuid::new_v4().to_string();
            tracing::info!(
                old_run_id = %run.id,
                new_run_id = %run_id,
                old_policy = %manifest.consent_policy_version,
                new_policy = %config.consent_policy_version,
                "The privacy or contribution policy contract changed; safely re-preparing the same folder"
            );
            self.state
                .supersede_run_for_repreparation(&run.id, &run_id)?;
            remove_bundle_cache(&self.paths, &run.id);
            self.process_existing_run(&run_id, source, run.dry_run, config, None)
                .await?;
            return Ok(run_id);
        }
        validate_manifest_context(&manifest, &config)?;
        let Some(saved_fingerprint) = self.state.source_fingerprint(&run.id)? else {
            let run_id = Uuid::new_v4().to_string();
            tracing::info!(
                old_run_id = %run.id,
                new_run_id = %run_id,
                reason = "source_checkpoint_missing",
                "The unfinished run has no source identity; safely re-preparing the folder"
            );
            self.state
                .supersede_run(&run.id, &run_id, "source_checkpoint_missing")?;
            remove_bundle_cache(&self.paths, &run.id);
            self.process_existing_run(&run_id, source, run.dry_run, config, None)
                .await?;
            return Ok(run_id);
        };
        let state = self.state.clone();
        let inspect_id = run.id.clone();
        let inspect_source = source.clone();
        let inspection = tokio::task::spawn_blocking(move || {
            inspect_dicom_source(&state, &inspect_id, &inspect_source, false)
        })
        .await
        .context("local DICOM discovery task stopped unexpectedly")??;
        let current_fingerprint = fingerprint_source_with_progress(
            &inspection.source_snapshot,
            &source,
            "Verifying checkpointed folder contents",
        )?;
        if saved_fingerprint != current_fingerprint {
            let reason = "source_changed_since_checkpoint";
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

        let report_path = run
            .report_path
            .as_deref()
            .context("checkpointed DICOM run has no local report")?;
        let report: RunReport = serde_json::from_slice(&fs::read(report_path)?)
            .context("checkpointed DICOM report is invalid")?;
        tracing::info!(
            run_id = %run.id,
            previous_status = %run.status,
            "Found a source-matched EPI archive checkpoint; continuing one series at a time"
        );
        self.process_streaming_dicom_run(
            &run.id,
            source,
            config,
            Some((manifest, report)),
            Some(inspection),
        )
        .await?;
        Ok(run.id)
    }

    async fn process_existing_run(
        &self,
        run_id: &str,
        source: PathBuf,
        dry_run: bool,
        config: ClientConfig,
        expected_bundle_ids: Option<HashSet<String>>,
    ) -> Result<()> {
        if !dry_run && expected_bundle_ids.is_none() {
            return self
                .process_streaming_dicom_run(run_id, source, config, None, None)
                .await;
        }
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
                let code = preparation_failure_code(&error);
                self.state
                    .update_run(run_id, "failed", &summary, Some(code))?;
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

    async fn process_streaming_dicom_run(
        &self,
        run_id: &str,
        source: PathBuf,
        config: ClientConfig,
        checkpoint: Option<(LocalManifest, RunReport)>,
        inspection: Option<InspectedDicomSource>,
    ) -> Result<()> {
        let resuming = checkpoint.is_some();
        let inspection = match inspection {
            Some(inspection) => inspection,
            None => {
                let state = self.state.clone();
                let inspect_id = run_id.to_owned();
                let inspect_source = source.clone();
                let inspection = tokio::task::spawn_blocking(move || {
                    inspect_dicom_source(&state, &inspect_id, &inspect_source, true)
                })
                .await
                .context("local DICOM discovery task stopped unexpectedly")?;
                match inspection {
                    Ok(value) => value,
                    Err(error) => {
                        let summary = self
                            .state
                            .run(run_id)?
                            .map(|run| run.summary)
                            .unwrap_or_default();
                        let status = if resuming { "upload_failed" } else { "failed" };
                        let code = preparation_failure_code(&error);
                        self.state
                            .update_run(run_id, status, &summary, Some(code))?;
                        return Err(error);
                    }
                }
            }
        };

        let pseudonymizer = Pseudonymizer::from_base64(&config.pseudonym_key_b64)?;
        let (mut manifest, mut report) = match checkpoint {
            Some((manifest, report)) => {
                if report.run_id != run_id || manifest.run_id != run_id {
                    bail!("checkpointed DICOM artifacts do not match the local run identity");
                }
                (manifest, report)
            }
            None => {
                let artifacts =
                    initial_dicom_artifacts(run_id, &config, &inspection, &pseudonymizer);
                checkpoint_new_streaming_run(
                    &self.paths,
                    &self.state,
                    run_id,
                    &source,
                    &inspection.source_snapshot,
                    &artifacts.0,
                    &artifacts.1,
                )?;
                artifacts
            }
        };

        validate_manifest_sync_contract(&manifest, &config)?;
        cleanup_orphaned_bundle_archives(&self.paths, run_id, &manifest)?;
        let mut existing_by_series = HashMap::new();
        for (index, bundle) in manifest.bundles.iter().enumerate() {
            if existing_by_series
                .insert(bundle.series_id.clone(), index)
                .is_some()
            {
                bail!("checkpointed DICOM manifest repeats a series identity");
            }
        }
        let accepted_series_ids = inspection
            .series
            .iter()
            .filter(|(_, _, classification)| {
                classification.decision == ClassificationDecision::Accepted
            })
            .map(|(_, group, _)| pseudonymizer.id("series", &group.series_uid))
            .collect::<HashSet<_>>();
        if manifest.source_summary.accepted != manifest.bundles.len() as u64
            || existing_by_series
                .keys()
                .any(|series_id| !accepted_series_ids.contains(series_id))
        {
            mark_streaming_failure(
                &self.paths,
                &self.state,
                run_id,
                &mut report,
                &manifest,
                "checkpoint_classification_mismatch",
            )?;
            bail!("checkpointed DICOM series no longer match the current classifier contract");
        }

        let api = IngestApi::from_config(&config)?;
        // Releases before bounded staging could checkpoint several series in
        // one server-bound receipt. Never rechunk those rows. Upload and
        // checkpoint every remaining archive first, then use the same final
        // folder-stability gate as current one-series runs before committing
        // any new receipt.
        let legacy_receipt_layout = self
            .state
            .run_uploads(run_id)?
            .iter()
            .any(|chunk| chunk.bundle_count != 1);
        if legacy_receipt_layout {
            tracing::info!(
                "Checkpointing a legacy multi-series transfer before the final source-stability gate"
            );
            for chunk in self.state.run_uploads(run_id)? {
                let end = chunk.bundle_start + chunk.bundle_count;
                let bundles = manifest
                    .bundles
                    .get(chunk.bundle_start..end)
                    .context("legacy DICOM upload chunk points outside its manifest")?;
                if !matches!(chunk.status.as_str(), "committed" | "reconciled") {
                    verify_prepared_objects_for_bundles(bundles).await?;
                    if let Err(error) = self
                        .checkpoint_dicom_upload_chunk(
                            run_id,
                            &chunk,
                            bundles,
                            &manifest.client_version,
                            &api,
                        )
                        .await
                    {
                        mark_streaming_failure(
                            &self.paths,
                            &self.state,
                            run_id,
                            &mut report,
                            &manifest,
                            "upload_failed",
                        )?;
                        return Err(error);
                    }
                }
                for bundle in bundles {
                    cleanup_prepared_bundle(&self.paths, run_id, bundle)?;
                }
            }
        }

        if !legacy_receipt_layout {
            let held_ids = report
                .held_series
                .iter()
                .map(|held| held.series_id.clone())
                .collect::<HashSet<_>>();
            let accepted_total = accepted_series_ids.len();
            let mut accepted_position = 0_usize;

            for (source_index, group, classification) in inspection.series {
                if classification.decision != ClassificationDecision::Accepted {
                    continue;
                }
                accepted_position += 1;
                let series_id = pseudonymizer.id("series", &group.series_uid);
                if held_ids.contains(&series_id) && !existing_by_series.contains_key(&series_id) {
                    continue;
                }

                if let Some(&bundle_index) = existing_by_series.get(&series_id) {
                    let mut chunk = self
                        .state
                        .ensure_single_series_upload(run_id, bundle_index)?;
                    let existing = manifest.bundles[bundle_index].clone();
                    if matches!(chunk.status.as_str(), "committed" | "reconciled") {
                        cleanup_prepared_bundle(&self.paths, run_id, &existing)?;
                        continue;
                    }

                    match prepared_bundle_state(&existing).await? {
                        PreparedBundleState::Valid => {}
                        PreparedBundleState::Missing => {
                            // The server may already have the bytes (for example,
                            // after a crash between receipt and local cleanup).
                            match self
                                .checkpoint_dicom_upload_chunk(
                                    run_id,
                                    &chunk,
                                    std::slice::from_ref(&existing),
                                    &manifest.client_version,
                                    &api,
                                )
                                .await
                            {
                                Ok(()) => {
                                    cleanup_prepared_bundle(&self.paths, run_id, &existing)?;
                                    continue;
                                }
                                Err(error)
                                    if error_has_io_kind(&error, std::io::ErrorKind::NotFound) =>
                                {
                                    chunk = self
                                        .state
                                        .ensure_single_series_upload(run_id, bundle_index)?;
                                }
                                Err(error) => {
                                    mark_streaming_failure(
                                        &self.paths,
                                        &self.state,
                                        run_id,
                                        &mut report,
                                        &manifest,
                                        "upload_failed",
                                    )?;
                                    return Err(error);
                                }
                            }
                        }
                        PreparedBundleState::Invalid => {
                            cleanup_prepared_bundle(&self.paths, run_id, &existing)?;
                        }
                    }

                    if !prepared_archive_path(&existing).is_file() {
                        let regenerated = match prepare_one_dicom_series(
                            &self.paths,
                            run_id,
                            &group,
                            classification.clone(),
                            &pseudonymizer,
                            accepted_position,
                            accepted_total,
                        )
                        .await
                        {
                            Ok(bundle) => bundle,
                            Err(error) => {
                                let code = preparation_failure_code(&error);
                                mark_streaming_failure(
                                    &self.paths,
                                    &self.state,
                                    run_id,
                                    &mut report,
                                    &manifest,
                                    code,
                                )?;
                                return Err(error);
                            }
                        };
                        if !same_prepared_bundle(&existing, &regenerated)? {
                            cleanup_prepared_bundle(&self.paths, run_id, &regenerated)?;
                            mark_streaming_failure(
                                &self.paths,
                                &self.state,
                                run_id,
                                &mut report,
                                &manifest,
                                "regenerated_archive_identity_mismatch",
                            )?;
                            bail!(
                                "recreated DICOM archive identity does not match its upload checkpoint"
                            );
                        }
                        manifest.bundles[bundle_index] = regenerated;
                        checkpoint_streaming_artifacts(
                            &self.paths,
                            &self.state,
                            run_id,
                            &mut manifest,
                            &mut report,
                        )?;
                    }

                    if let Err(error) = self
                        .checkpoint_dicom_upload_chunk(
                            run_id,
                            &chunk,
                            std::slice::from_ref(&manifest.bundles[bundle_index]),
                            &manifest.client_version,
                            &api,
                        )
                        .await
                    {
                        mark_streaming_failure(
                            &self.paths,
                            &self.state,
                            run_id,
                            &mut report,
                            &manifest,
                            "upload_failed",
                        )?;
                        return Err(error);
                    }
                    cleanup_prepared_bundle(&self.paths, run_id, &manifest.bundles[bundle_index])?;
                    continue;
                }

                let bundle = match prepare_one_dicom_series(
                    &self.paths,
                    run_id,
                    &group,
                    classification,
                    &pseudonymizer,
                    accepted_position,
                    accepted_total,
                )
                .await
                {
                    Ok(bundle) if bundle.total_size() <= MAX_DICOM_ARCHIVE_BYTES_PER_SERIES => {
                        bundle
                    }
                    Ok(bundle) => {
                        cleanup_prepared_bundle(&self.paths, run_id, &bundle)?;
                        manifest.source_summary.held += 1;
                        report.held_series.push(held(
                            &pseudonymizer,
                            &group,
                            source_index,
                            &coded_hold("bundle_exceeds_upload_limit", "local_safety_check"),
                        ));
                        checkpoint_streaming_artifacts(
                            &self.paths,
                            &self.state,
                            run_id,
                            &mut manifest,
                            &mut report,
                        )?;
                        continue;
                    }
                    Err(error) if deterministic_series_hold_code(&error).is_some() => {
                        let code = deterministic_series_hold_code(&error)
                            .expect("guarded deterministic preparation error");
                        tracing::warn!(
                            series = accepted_position,
                            total_series = accepted_total,
                            error = %error,
                            "Functional EPI DICOM series was held by a local safety check"
                        );
                        manifest.source_summary.held += 1;
                        report.held_series.push(held(
                            &pseudonymizer,
                            &group,
                            source_index,
                            &coded_hold(code, "local_safety_check"),
                        ));
                        checkpoint_streaming_artifacts(
                            &self.paths,
                            &self.state,
                            run_id,
                            &mut manifest,
                            &mut report,
                        )?;
                        continue;
                    }
                    Err(error) => {
                        let code = preparation_failure_code(&error);
                        mark_streaming_failure(
                            &self.paths,
                            &self.state,
                            run_id,
                            &mut report,
                            &manifest,
                            code,
                        )?;
                        return Err(error);
                    }
                };

                manifest.source_summary.accepted += 1;
                manifest.bundles.push(bundle);
                let bundle_index = manifest.bundles.len() - 1;
                existing_by_series.insert(series_id, bundle_index);
                checkpoint_streaming_artifacts(
                    &self.paths,
                    &self.state,
                    run_id,
                    &mut manifest,
                    &mut report,
                )?;
                let chunk = self
                    .state
                    .ensure_single_series_upload(run_id, bundle_index)?;
                if let Err(error) = self
                    .checkpoint_dicom_upload_chunk(
                        run_id,
                        &chunk,
                        std::slice::from_ref(&manifest.bundles[bundle_index]),
                        &manifest.client_version,
                        &api,
                    )
                    .await
                {
                    mark_streaming_failure(
                        &self.paths,
                        &self.state,
                        run_id,
                        &mut report,
                        &manifest,
                        "upload_failed",
                    )?;
                    return Err(error);
                }
                cleanup_prepared_bundle(&self.paths, run_id, &manifest.bundles[bundle_index])?;
            }
        }

        std::thread::sleep(SOURCE_QUIET_INTERVAL);
        if let Err(error) = confirm_final_streaming_source_stability(
            &inspection.source_snapshot,
            &source,
            inspection.summary.files_seen,
        ) {
            mark_streaming_failure(
                &self.paths,
                &self.state,
                run_id,
                &mut report,
                &manifest,
                "source_changed_during_sync",
            )?;
            return Err(error);
        }

        if let Err(error) = self
            .commit_checkpointed_dicom_receipts(run_id, &manifest, &api)
            .await
        {
            mark_streaming_failure(
                &self.paths,
                &self.state,
                run_id,
                &mut report,
                &manifest,
                "upload_failed",
            )?;
            return Err(error);
        }

        finalize_streaming_run(&self.paths, &self.state, run_id, &mut report, &manifest)
    }

    async fn continue_upload_verified(
        &self,
        run_id: &str,
        manifest: &LocalManifest,
        config: &ClientConfig,
    ) -> Result<()> {
        if !manifest
            .bundles
            .iter()
            .all(ManifestBundle::is_dicom_archive)
        {
            bail!("checkpoint predates the functional EPI archive contract; rerun the folder");
        }
        self.continue_dicom_upload(run_id, manifest, config).await
    }

    async fn continue_dicom_upload(
        &self,
        run_id: &str,
        manifest: &LocalManifest,
        config: &ClientConfig,
    ) -> Result<()> {
        validate_manifest_sync_contract(manifest, config)?;
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

    async fn checkpoint_dicom_upload_chunk(
        &self,
        run_id: &str,
        chunk: &crate::state::RunUploadRecord,
        bundles: &[ManifestBundle],
        preparation_client_version: &str,
        api: &IngestApi,
    ) -> Result<()> {
        self.drive_dicom_upload_chunk(
            run_id,
            chunk,
            bundles,
            preparation_client_version,
            api,
            DicomReceiptPhase::TransferOnly,
        )
        .await
    }

    async fn commit_dicom_upload_chunk(
        &self,
        run_id: &str,
        chunk: &crate::state::RunUploadRecord,
        bundles: &[ManifestBundle],
        preparation_client_version: &str,
        api: &IngestApi,
    ) -> Result<()> {
        self.drive_dicom_upload_chunk(
            run_id,
            chunk,
            bundles,
            preparation_client_version,
            api,
            DicomReceiptPhase::CommitOnly,
        )
        .await
    }

    async fn commit_checkpointed_dicom_receipts(
        &self,
        run_id: &str,
        manifest: &LocalManifest,
        api: &IngestApi,
    ) -> Result<()> {
        let chunks = self.state.run_uploads(run_id)?;
        let receipt_total = chunks
            .iter()
            .filter(|chunk| !matches!(chunk.status.as_str(), "committed" | "reconciled"))
            .map(|chunk| chunk.bundle_count as u64)
            .sum();
        let mut receipt_progress = (receipt_total > 0)
            .then(|| Progress::bounded("Recording receipts", receipt_total, ProgressUnit::Series));
        for chunk in chunks {
            if matches!(chunk.status.as_str(), "committed" | "reconciled") {
                continue;
            }
            let end = chunk
                .bundle_start
                .checked_add(chunk.bundle_count)
                .context("DICOM receipt bundle range overflow")?;
            let bundles = manifest
                .bundles
                .get(chunk.bundle_start..end)
                .context("DICOM receipt points outside its manifest")?;
            self.commit_dicom_upload_chunk(run_id, &chunk, bundles, &manifest.client_version, api)
                .await?;
            if let Some(progress) = receipt_progress.as_mut() {
                progress.inc(chunk.bundle_count as u64);
            }
        }
        if let Some(progress) = receipt_progress.as_mut() {
            progress.finish();
        }
        if self
            .state
            .run_uploads(run_id)?
            .iter()
            .any(|chunk| !matches!(chunk.status.as_str(), "committed" | "reconciled"))
        {
            bail!("not every DICOM series archive has a durable receipt");
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
        self.drive_dicom_upload_chunk(
            run_id,
            chunk,
            bundles,
            preparation_client_version,
            api,
            DicomReceiptPhase::TransferAndCommit,
        )
        .await
    }

    async fn drive_dicom_upload_chunk(
        &self,
        run_id: &str,
        chunk: &crate::state::RunUploadRecord,
        bundles: &[ManifestBundle],
        preparation_client_version: &str,
        api: &IngestApi,
        phase: DicomReceiptPhase,
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
                Ok(status)
                    if matches!(
                        status.status.as_str(),
                        "created" | "uploading" | "checkpointed"
                    ) =>
                {
                    let saved: CompleteUploadRequest =
                        serde_json::from_slice(&fs::read(&completion_path)?)?;
                    if phase == DicomReceiptPhase::TransferOnly {
                        let checkpoint = api
                            .checkpoint_dicom_upload(upload_id, saved.objects)
                            .await?;
                        require_dicom_transfer_checkpoint(&checkpoint, bundles.len())?;
                        self.state
                            .set_chunk_status(run_id, chunk.chunk_index, "uploaded")?;
                        tracing::info!(
                            series = bundles.len(),
                            "Series bytes are uploaded and checkpointed; durable receipt is deferred until the folder is stable"
                        );
                        return Ok(());
                    }
                    let status = api.complete_dicom_upload(upload_id, saved.objects).await?;
                    if !dicom_receipt_complete(&status.status) {
                        bail!("DICOM receipt API did not commit the transferred archives");
                    }
                    record_dicom_receipt(&self.state, run_id, chunk.chunk_index, bundles, &status)?;
                    tracing::info!("Durable functional EPI archive receipt confirmed");
                    return Ok(());
                }
                Ok(status) if status.status == "expired" => {
                    if phase == DicomReceiptPhase::CommitOnly {
                        bail!(
                            "checkpointed DICOM upload expired before the folder-level receipt gate; rerun the same neuro-sync <folder> command to resume safely"
                        );
                    }
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
                    if phase == DicomReceiptPhase::TransferOnly {
                        let checkpoint = api
                            .checkpoint_dicom_upload(upload_id, saved.objects)
                            .await?;
                        require_dicom_transfer_checkpoint(&checkpoint, bundles.len())?;
                        self.state
                            .set_chunk_status(run_id, chunk.chunk_index, "uploaded")?;
                        tracing::info!(
                            series = bundles.len(),
                            "Series bytes remain checkpointed while receipt status is temporarily unavailable"
                        );
                        return Ok(());
                    }
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

        if phase == DicomReceiptPhase::CommitOnly {
            if let Some(upload_id) = chunk.worker_upload_id.as_deref() {
                let status = api.dicom_status(upload_id).await?;
                if dicom_receipt_complete(&status.status) {
                    record_dicom_receipt(&self.state, run_id, chunk.chunk_index, bundles, &status)?;
                    return Ok(());
                }
            }
            bail!(
                "DICOM series has no completed transfer checkpoint after the folder-level stability gate; rerun the same neuro-sync <folder> command"
            );
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
        if phase == DicomReceiptPhase::TransferOnly {
            let checkpoint = api
                .checkpoint_dicom_upload(&upload_id, request.objects)
                .await?;
            require_dicom_transfer_checkpoint(&checkpoint, bundles.len())?;
            self.state
                .set_chunk_status(run_id, chunk.chunk_index, "uploaded")?;
            tracing::info!(
                series = bundles.len(),
                "Series archive is durably checkpointed in object storage; its archive receipt is deferred until the folder is stable"
            );
            return Ok(());
        }
        self.state
            .set_chunk_status(run_id, chunk.chunk_index, "uploaded")?;
        let status = api
            .complete_dicom_upload(&upload_id, request.objects)
            .await?;
        if !dicom_receipt_complete(&status.status) {
            bail!("DICOM receipt API did not commit the completed multipart upload");
        }
        record_dicom_receipt(&self.state, run_id, chunk.chunk_index, bundles, &status)?;
        tracing::info!("Durable functional EPI archive receipt confirmed");
        Ok(())
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
    privacy::restrict_file(path)?;
    Ok(file)
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

fn write_private_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .context("pending registration state has no parent directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".pending-registration-")
        .tempfile_in(parent)
        .context("could not create private pending registration state")?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    privacy::restrict_file(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|_| anyhow::anyhow!("could not commit private pending registration state"))?;
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

const MINIMUM_EPI_CLIENT_VERSION: &str = "0.5.0";

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

fn verify_prepared_object_files(objects: &[crate::model::ManifestObject]) -> Result<()> {
    let total = objects.iter().try_fold(0_u64, |total, object| {
        total
            .checked_add(object.size)
            .context("prepared archive byte total overflow")
    })?;
    let mut progress = Progress::bounded("Verifying archive hashes", total, ProgressUnit::Bytes);
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
    let Ok(version) = semver::Version::parse(&manifest.client_version) else {
        return false;
    };
    let minimum = semver::Version::parse(MINIMUM_EPI_CLIENT_VERSION)
        .expect("minimum EPI client version must be valid semver");
    version >= minimum
        && manifest.schema_version == MANIFEST_SCHEMA_VERSION
        && manifest.classifier_contract_version == crate::DICOM_CLASSIFIER_CONTRACT_VERSION
        && manifest.archive_contract_version == crate::DICOM_ARCHIVE_CONTRACT_VERSION
        && manifest.metadata_policy.policy_id == DICOM_METADATA_POLICY_ID
        && manifest.metadata_policy.policy_version == DICOM_METADATA_POLICY_VERSION
        && manifest.bundles.iter().all(|bundle| {
            let Some(archive) = bundle.archive.as_ref() else {
                return false;
            };
            bundle.is_dicom_archive()
                && archive.format == crate::archive::DICOM_ARCHIVE_FORMAT
                && archive.deidentification_profile == DICOM_METADATA_POLICY_ID
                && archive.deidentification_profile_version == DICOM_METADATA_POLICY_VERSION
                && bundle.archive_route == crate::archive::FUNCTIONAL_EPI_ARCHIVE_ROUTE
                && bundle.series_kind == "functional_epi"
                && bundle.classification.decision == ClassificationDecision::Accepted
                && bundle.classification.kind == bundle.series_kind
                && bundle.pixel_data_policy
                    == crate::archive::SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY
                && archive.dicom_instance_count == bundle.source_dicom_count
        })
}

fn validate_manifest_context(manifest: &LocalManifest, config: &ClientConfig) -> Result<()> {
    validate_manifest_scope(manifest, config)?;
    if manifest.consent_policy_version.is_empty()
        || manifest.consent_policy_version != config.consent_policy_version
    {
        bail!("prepared run requires approval under the current contribution policy");
    }
    Ok(())
}

fn validate_manifest_scope(manifest: &LocalManifest, config: &ClientConfig) -> Result<()> {
    if manifest.site_id != config.site_id || manifest.project_id != config.project_id {
        bail!("prepared run belongs to a different enrolled site or project");
    }
    Ok(())
}

fn validate_manifest_sync_contract(manifest: &LocalManifest, config: &ClientConfig) -> Result<()> {
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
    load_checkpoint_manifest(run).is_ok_and(|manifest| {
        manifest.schema_version == MANIFEST_SCHEMA_VERSION
            && manifest.classifier_contract_version == crate::DICOM_CLASSIFIER_CONTRACT_VERSION
            && manifest.archive_contract_version == crate::DICOM_ARCHIVE_CONTRACT_VERSION
            && manifest.metadata_policy.policy_id == DICOM_METADATA_POLICY_ID
            && manifest.metadata_policy.policy_version == DICOM_METADATA_POLICY_VERSION
    })
}

fn inspect_dicom_source(
    state: &StateStore,
    run_id: &str,
    source: &Path,
    update_preparation_status: bool,
) -> Result<InspectedDicomSource> {
    let started_at = Utc::now().to_rfc3339();
    if update_preparation_status {
        state.update_run(run_id, "discovering", &SourceSummary::default(), None)?;
    }
    let mut discovery_phase = DiscoveryPhase::Inventory;
    let mut discovery_progress = Progress::spinner("Finding files", ProgressUnit::Files);
    let discovery = discover_with_progress(source, |progress| {
        if progress.phase != discovery_phase {
            let total = progress
                .total_files
                .expect("header discovery always reports its inventory total");
            discovery_progress.finish_at(total);
            discovery_progress = Progress::bounded("Reading DICOMs", total, ProgressUnit::Files);
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
    if update_preparation_status {
        state.update_run(run_id, "preparing", &discovery.summary, None)?;
    }
    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    confirm_source_snapshot_stable(&discovery.source_snapshot, source, "Verifying source")?;
    ensure_no_unreadable_dicom_like_files(discovery.unreadable_dicom_like_files)?;
    let classifications = discovery
        .series
        .iter()
        .map(classify_header)
        .collect::<Vec<_>>();
    let series = discovery
        .series
        .into_iter()
        .zip(classifications)
        .enumerate()
        .map(|(index, (group, classification))| (index, group, classification))
        .collect();
    Ok(InspectedDicomSource {
        started_at,
        summary: discovery.summary,
        unreadable_dicom_like_files: discovery.unreadable_dicom_like_files,
        source_snapshot: discovery.source_snapshot,
        series,
    })
}

fn initial_dicom_artifacts(
    run_id: &str,
    config: &ClientConfig,
    inspection: &InspectedDicomSource,
    pseudonymizer: &Pseudonymizer,
) -> (LocalManifest, RunReport) {
    let mut summary = inspection.summary.clone();
    summary.accepted = 0;
    summary.held = 0;
    summary.excluded = 0;
    let mut held_series = Vec::new();
    for (index, group, classification) in &inspection.series {
        match classification.decision {
            ClassificationDecision::Accepted => {}
            ClassificationDecision::Held => {
                summary.held += 1;
                held_series.push(held(pseudonymizer, group, *index, classification));
            }
            ClassificationDecision::Excluded => summary.excluded += 1,
        }
    }
    let manifest = LocalManifest {
        schema_version: MANIFEST_SCHEMA_VERSION.into(),
        run_id: run_id.into(),
        site_id: config.site_id.clone(),
        project_id: config.project_id.clone(),
        consent_policy_version: config.consent_policy_version.clone(),
        client_version: crate::CLIENT_VERSION.into(),
        classifier_contract_version: crate::DICOM_CLASSIFIER_CONTRACT_VERSION.into(),
        archive_contract_version: crate::DICOM_ARCHIVE_CONTRACT_VERSION.into(),
        metadata_policy: metadata_policy(),
        created_at: Utc::now().to_rfc3339(),
        source_summary: summary.clone(),
        bundles: Vec::new(),
    };
    let errors = (inspection.unreadable_dicom_like_files > 0)
        .then(|| "unreadable_dicom_like_files".to_owned())
        .into_iter()
        .collect();
    let report = RunReport {
        run_id: run_id.into(),
        status: "prepared".into(),
        site_id: config.site_id.clone(),
        project_id: config.project_id.clone(),
        project_name: config.project_name.clone(),
        consent_policy_version: config.consent_policy_version.clone(),
        client_version: crate::CLIENT_VERSION.into(),
        started_at: inspection.started_at.clone(),
        completed_at: None,
        source_summary: summary,
        bundles: Vec::new(),
        held_series,
        errors,
        worker_upload_id: None,
        worker_upload_ids: Vec::new(),
        archive_commit_count: 0,
    };
    (manifest, report)
}

fn checkpoint_new_streaming_run(
    paths: &AppPaths,
    state: &StateStore,
    run_id: &str,
    source: &Path,
    source_snapshot: &SourceSnapshot,
    manifest: &LocalManifest,
    report: &RunReport,
) -> Result<()> {
    let manifest_path = paths.reports.join(format!("{run_id}.manifest.json"));
    let report_path = paths.reports.join(format!("{run_id}.json"));
    write_json(&manifest_path, manifest)?;
    write_json(&report_path, report)?;
    state.set_artifacts(run_id, &manifest_path, &report_path)?;
    let fingerprint = fingerprint_source_with_progress(source_snapshot, source, "Hashing source")?;
    state.set_source_fingerprint(run_id, &fingerprint)?;
    state.update_run(run_id, "prepared", &manifest.source_summary, None)
}

fn fingerprint_source_with_progress(
    snapshot: &SourceSnapshot,
    source: &Path,
    label: &str,
) -> Result<crate::dicom::SourceFingerprint> {
    let mut progress = Progress::bounded(label, snapshot.total_bytes()?, ProgressUnit::Bytes);
    let fingerprint = snapshot.fingerprint_with_progress(source, |bytes| progress.set(bytes))?;
    progress.finish();
    Ok(fingerprint)
}

fn confirm_source_snapshot_stable(
    snapshot: &SourceSnapshot,
    source: &Path,
    label: &str,
) -> Result<()> {
    let mut progress = Progress::spinner(label, ProgressUnit::Files);
    let stable =
        snapshot.matches_current_with_progress(source, |files_seen| progress.set(files_seen))?;
    progress.finish_at(progress.completed());
    if !stable {
        bail!(
            "the selected DICOM folder changed while its content identity was being checked; wait for the export to finish, then rerun the same neuro-sync <folder> command"
        );
    }
    Ok(())
}

// Each accepted DICOM was already captured from a stable file handle and
// independently audited. Re-reading every source byte here would duplicate
// that work, so the receipt gate only needs one final folder snapshot.
fn confirm_final_streaming_source_stability(
    initial_snapshot: &SourceSnapshot,
    source: &Path,
    expected_files_seen: u64,
) -> Result<()> {
    let mut snapshot_progress = Progress::bounded(
        "Final source check",
        expected_files_seen,
        ProgressUnit::Files,
    );
    let stable = initial_snapshot
        .matches_current_with_progress(source, |files_seen| snapshot_progress.set(files_seen))?;
    snapshot_progress.finish();
    if !stable {
        bail!(
            "the selected DICOM folder changed during sync; no new durable receipts were committed. Rerun the same neuro-sync <folder> command after the export is complete"
        );
    }
    Ok(())
}

fn checkpoint_streaming_artifacts(
    paths: &AppPaths,
    state: &StateStore,
    run_id: &str,
    manifest: &mut LocalManifest,
    report: &mut RunReport,
) -> Result<()> {
    report.source_summary = manifest.source_summary.clone();
    report.bundles = manifest.bundles.iter().map(ReportBundle::from).collect();
    write_json(
        &paths.reports.join(format!("{run_id}.manifest.json")),
        manifest,
    )?;
    write_json(&paths.reports.join(format!("{run_id}.json")), report)?;
    state.update_run_summary(run_id, &manifest.source_summary)
}

fn mark_streaming_failure(
    paths: &AppPaths,
    state: &StateStore,
    run_id: &str,
    report: &mut RunReport,
    manifest: &LocalManifest,
    code: &str,
) -> Result<()> {
    report.status = "upload_failed".into();
    report.completed_at = None;
    report.source_summary = manifest.source_summary.clone();
    report.bundles = manifest.bundles.iter().map(ReportBundle::from).collect();
    if !report.errors.iter().any(|error| error == code) {
        report.errors.push(code.into());
    }
    write_json(&paths.reports.join(format!("{run_id}.json")), report)?;
    state.update_run(
        run_id,
        "upload_failed",
        &manifest.source_summary,
        Some(code),
    )
}

fn finalize_streaming_run(
    paths: &AppPaths,
    state: &StateStore,
    run_id: &str,
    report: &mut RunReport,
    manifest: &LocalManifest,
) -> Result<()> {
    let chunks = state.run_uploads(run_id)?;
    if chunks
        .iter()
        .any(|chunk| !matches!(chunk.status.as_str(), "committed" | "reconciled"))
    {
        bail!("not every DICOM series archive has a durable receipt");
    }
    let status = if manifest.bundles.is_empty() {
        "complete_no_eligible_series"
    } else {
        "complete"
    };
    report.status = status.into();
    report.completed_at = Some(Utc::now().to_rfc3339());
    report.source_summary = manifest.source_summary.clone();
    report.bundles = manifest.bundles.iter().map(ReportBundle::from).collect();
    report.worker_upload_ids = chunks
        .iter()
        .filter(|chunk| chunk.status == "committed")
        .filter_map(|chunk| chunk.worker_upload_id.clone())
        .collect();
    report.worker_upload_id = report.worker_upload_ids.first().cloned();
    report.archive_commit_count = chunks
        .iter()
        .filter(|chunk| chunk.status == "committed" && chunk.worker_upload_id.is_some())
        .count() as u64;
    write_json(&paths.reports.join(format!("{run_id}.json")), report)?;
    state.update_run(run_id, status, &manifest.source_summary, None)?;
    remove_bundle_cache(paths, run_id);
    Ok(())
}

async fn prepare_one_dicom_series(
    paths: &AppPaths,
    run_id: &str,
    group: &SeriesGroup,
    classification: Classification,
    pseudonymizer: &Pseudonymizer,
    position: usize,
    total: usize,
) -> Result<ManifestBundle> {
    let paths = paths.clone();
    let run_id = run_id.to_owned();
    let group = group.clone();
    let pseudonymizer = pseudonymizer.clone();
    tokio::task::spawn_blocking(move || {
        let bundle_root = paths.bundles.join(&run_id);
        fs::create_dir_all(&bundle_root)?;
        let staging_required = ensure_series_staging_capacity(&paths, &group)?;
        let source_bytes = group.files.iter().try_fold(0_u64, |total, path| {
            Ok::<_, std::io::Error>(total.saturating_add(fs::metadata(path)?.len()))
        })?;
        let mut progress = Progress::bounded(
            format!("Deidentifying EPI series {position}/{total}"),
            source_bytes,
            ProgressUnit::Bytes,
        );
        let result = create_dicom_archive(ArchiveRequest {
            group: &group,
            classification,
            pseudonymizer: &pseudonymizer,
            bundle_root: &bundle_root,
            progress: |bytes| progress.inc(bytes),
        });
        progress.finish_at(progress.completed());
        match result {
            Err(error) if is_storage_exhaustion(&error) => {
                let available = fs2::available_space(&paths.bundles).unwrap_or(0);
                Err(StagingStorageExhausted {
                    staging_root: paths.bundles,
                    required: staging_required,
                    available,
                }
                .into())
            }
            other => other,
        }
    })
    .await
    .context("DICOM archive preparation task stopped unexpectedly")?
}

fn ensure_series_staging_capacity(paths: &AppPaths, group: &SeriesGroup) -> Result<u64> {
    let sizes = group
        .files
        .iter()
        .map(|path| fs::metadata(path).map(|metadata| metadata.len()))
        .collect::<std::io::Result<Vec<_>>>()?;
    let required = required_series_staging_bytes(sizes)?;
    let available = fs2::available_space(&paths.bundles)?;
    validate_staging_capacity(&paths.bundles, required, available)?;
    Ok(required)
}

fn required_series_staging_bytes(sizes: impl IntoIterator<Item = u64>) -> Result<u64> {
    let (source_bytes, largest_instance) =
        sizes
            .into_iter()
            .try_fold((0_u64, 0_u64), |(total, largest), size| {
                Ok::<_, anyhow::Error>((
                    total
                        .checked_add(size)
                        .context("DICOM series byte total overflow")?,
                    largest.max(size),
                ))
            })?;
    // The writer may simultaneously hold the compressed archive-so-far, one
    // immutable staged source instance, and that instance's sanitized output.
    // The source total conservatively bounds the archive payload; two largest-
    // instance allowances cover both current-instance copies.
    source_bytes
        .checked_add(largest_instance)
        .and_then(|value| value.checked_add(largest_instance))
        .and_then(|value| value.checked_add(STAGING_HEADROOM_BYTES))
        .context("DICOM staging-space requirement overflow")
}

fn validate_staging_capacity(staging_root: &Path, required: u64, available: u64) -> Result<()> {
    if available < required {
        return Err(StagingStorageExhausted {
            staging_root: staging_root.to_path_buf(),
            required,
            available,
        }
        .into());
    }
    Ok(())
}

fn is_storage_exhaustion(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| matches!(code, 28 | 69 | 112 | 122))
    })
}

fn error_has_io_kind(error: &anyhow::Error, kind: std::io::ErrorKind) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == kind)
    })
}

fn error_contains_io(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
}

fn preparation_failure_code(error: &anyhow::Error) -> &'static str {
    if error.downcast_ref::<UnreadableDicomLikeFiles>().is_some() {
        "unreadable_dicom_like_files"
    } else if error.downcast_ref::<StagingStorageExhausted>().is_some()
        || is_storage_exhaustion(error)
    {
        "staging_storage_exhausted"
    } else if error_contains_io(error) {
        "local_preparation_io_failed"
    } else {
        "local_preparation_failed"
    }
}

fn ensure_no_unreadable_dicom_like_files(count: u64) -> Result<()> {
    if count > 0 {
        return Err(UnreadableDicomLikeFiles { count }.into());
    }
    Ok(())
}

fn deterministic_series_hold_code(error: &anyhow::Error) -> Option<&'static str> {
    let message = error.to_string();
    match message.as_str() {
        "dicom_instance_exceeds_64_gib" => Some("dicom_instance_exceeds_64_gib"),
        "series_exceeds_64_gib_uncompressed_dicom_limit" => {
            Some("series_exceeds_64_gib_uncompressed_dicom_limit")
        }
        "dicom_archive_expansion_ratio_exceeded" => Some("dicom_archive_expansion_ratio_exceeded"),
        "DICOM contains overlay or graphic data and was held locally" => {
            Some("dicom_overlay_or_graphics_present")
        }
        "DICOM declared possible burned-in annotation"
        | "MR DICOM series declared possible burned-in annotation" => {
            Some("possible_burned_in_annotation")
        }
        "deflated DICOM transfer syntax is not supported by the bounded privacy writer" => {
            Some("unsupported_deflated_transfer_syntax")
        }
        "DICOM transfer syntax is not supported for bounded pixel copying" => {
            Some("unsupported_transfer_syntax")
        }
        "DICOM file meta length overflow" => Some("dicom_file_meta_invalid"),
        "DICOM contained an unsupported semantic UID constant" => {
            Some("dicom_semantic_uid_unsupported")
        }
        "DICOM sequence nesting exceeds the local sanitizer limit"
        | "sanitized DICOM exceeded sequence-depth policy" => Some("dicom_sequence_depth_limit"),
        "DICOM contains more than 100000 aggregate sequence items"
        | "sanitized DICOM contains more than 100000 aggregate sequence items" => {
            Some("dicom_sequence_item_limit")
        }
        "DICOM has no readable PixelData element" => Some("dicom_pixel_data_unreadable"),
        "DICOM pixel module is missing or inconsistent with its MR SOP Class" => {
            Some("dicom_pixel_module_invalid")
        }
        "DICOM RealWorldValueMapping is not supported by the privacy writer" => {
            Some("dicom_real_world_value_mapping_unsupported")
        }
        "DICOM contains an unsupported pixel transform" => {
            Some("dicom_pixel_transform_unsupported")
        }
        "DICOM contains an incomplete or invalid rescale transform"
        | "DICOM contains an invalid PixelValueTransformationSequence"
        | "DICOM contains an unsupported PixelValueTransformationSequence item"
        | "DICOM contains an incomplete PixelValueTransformationSequence"
        | "DICOM contains an incomplete or invalid window transform" => {
            Some("dicom_pixel_transform_invalid")
        }
        "DICOM Extended Offset Table failed structural validation" => {
            Some("dicom_extended_offset_table_invalid")
        }
        "DICOM ImageType failed positional validation" => Some("dicom_image_type_invalid"),
        "DICOM contains an invalid UID" => Some("dicom_invalid_uid"),
        "DICOM contains ASL conditional metadata outside its required macro"
        | "DICOM contains an invalid ASL Context Sequence"
        | "DICOM ASL Context omitted a valid crusher flag"
        | "DICOM ASL Context omitted a valid bolus cut-off flag"
        | "DICOM contains an incomplete or invalid ASL crusher group"
        | "DICOM ASL crusher children contradict a NO flag"
        | "DICOM contains an incomplete ASL bolus cut-off group"
        | "DICOM ASL bolus cut-off sequence must contain exactly one item"
        | "DICOM contains an incomplete or invalid ASL bolus cut-off item"
        | "DICOM ASL bolus cut-off sequence contradicts a NO flag" => {
            Some("asl_scientific_metadata_incomplete")
        }
        _ if message.starts_with("sanitized DICOM omitted required Type 1 attribute")
            || message.starts_with("sanitized DICOM has an invalid required Type 1 attribute")
            || message.starts_with("sanitized DICOM omitted required Type 2 attribute")
            || message.starts_with("sanitized DICOM has an invalid required Type 2 attribute")
            || message.starts_with("sanitized classic MR omitted required Type 1")
            || message.starts_with("sanitized Enhanced MR omitted required Type 1")
            || message.starts_with("sanitized DICOM has an unsupported MR SOP Class UID")
            || message
                .starts_with("sanitized DICOM violates the supported MR identity contract")
            || message.starts_with("sanitized DICOM has a non-pseudonymous required UID")
            || message.starts_with("sanitized DICOM omitted required Frame of Reference UID")
            || message.starts_with("sanitized DICOM has an invalid Frame of Reference UID")
            || message.starts_with(
                "sanitized Enhanced MR retained a non-pseudonymous device serial number",
            )
            || message.starts_with(
                "sanitized DICOM retained a non-empty privacy-sensitive Type 2 attribute",
            )
            || message.starts_with("sanitized DICOM retained unsafe Manufacturer text")
            || message.starts_with("sanitized DICOM retained an invalid Type 2 attribute") =>
        {
            Some("dicom_iod_contract_invalid")
        }
        _ => None,
    }
}

enum PreparedBundleState {
    Valid,
    Missing,
    Invalid,
}

async fn prepared_bundle_state(bundle: &ManifestBundle) -> Result<PreparedBundleState> {
    let objects = bundle
        .upload_objects()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if objects.len() != 1 {
        bail!("DICOM checkpoint does not contain exactly one archive object");
    }
    let path = Path::new(&objects[0].local_path);
    match fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedBundleState::Missing);
        }
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.is_file() => return Ok(PreparedBundleState::Invalid),
        Ok(_) => {}
    }
    match tokio::task::spawn_blocking(move || verify_prepared_object_files(&objects)).await {
        Err(error) => Err(error).context("prepared archive verification task stopped unexpectedly"),
        Ok(Ok(())) => Ok(PreparedBundleState::Valid),
        Ok(Err(error)) if error_has_io_kind(&error, std::io::ErrorKind::NotFound) => {
            Ok(PreparedBundleState::Missing)
        }
        Ok(Err(error)) if error_contains_io(&error) => Err(error),
        Ok(Err(_)) => Ok(PreparedBundleState::Invalid),
    }
}

async fn verify_prepared_objects_for_bundles(bundles: &[ManifestBundle]) -> Result<()> {
    let objects = bundles
        .iter()
        .flat_map(ManifestBundle::upload_objects)
        .cloned()
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || verify_prepared_object_files(&objects))
        .await
        .context("prepared archive verification task stopped unexpectedly")?
}

fn same_prepared_bundle(left: &ManifestBundle, right: &ManifestBundle) -> Result<bool> {
    fn durable_identity(bundle: &ManifestBundle) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(bundle)?;
        if let Some(archive) = value
            .get_mut("archive")
            .and_then(serde_json::Value::as_object_mut)
        {
            archive.remove("local_path");
        }
        Ok(value)
    }

    Ok(durable_identity(left)? == durable_identity(right)?)
}

fn prepared_archive_path(bundle: &ManifestBundle) -> PathBuf {
    bundle
        .archive
        .as_ref()
        .map(|archive| PathBuf::from(&archive.object.local_path))
        .unwrap_or_default()
}

fn cleanup_prepared_bundle(paths: &AppPaths, run_id: &str, bundle: &ManifestBundle) -> Result<()> {
    let archive = bundle
        .archive
        .as_ref()
        .context("DICOM bundle has no prepared archive")?;
    let expected_directory = bundle_staging_roots(paths)
        .into_iter()
        .map(|root| root.join(run_id).join(&bundle.bundle_id))
        .find(|directory| Path::new(&archive.object.local_path) == directory.join("dicom.tar.zst"))
        .context("refused to clean a prepared archive outside its run staging directory")?;
    match fs::remove_dir_all(&expected_directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "durable receipt was recorded, but local staging cleanup failed at {}",
                expected_directory.display()
            )
        }),
    }
}

fn cleanup_orphaned_bundle_archives(
    paths: &AppPaths,
    run_id: &str,
    manifest: &LocalManifest,
) -> Result<()> {
    let referenced = manifest
        .bundles
        .iter()
        .map(|bundle| bundle.bundle_id.as_str())
        .collect::<HashSet<_>>();
    for staging_root in bundle_staging_roots(paths) {
        let root = staging_root.join(run_id);
        if staging_root == paths.bundles {
            fs::create_dir_all(&root)?;
        } else if !root.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name();
            let retained = name.to_str().is_some_and(|name| referenced.contains(name));
            if retained {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

fn bundle_staging_roots(paths: &AppPaths) -> Vec<PathBuf> {
    let mut roots = vec![paths.bundles.clone()];
    let legacy = paths.root.join("bundles");
    if legacy != paths.bundles {
        roots.push(legacy);
    }
    roots
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0_usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
    let mut discovery_progress = Progress::spinner("Finding files", ProgressUnit::Files);
    let discovery = discover_with_progress(source, |progress| {
        if progress.phase != discovery_phase {
            let total = progress
                .total_files
                .expect("header discovery always reports its inventory total");
            discovery_progress.finish_at(total);
            discovery_progress = Progress::bounded("Reading DICOMs", total, ProgressUnit::Files);
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
        "Verifying source",
        discovery.summary.files_seen,
        ProgressUnit::Files,
    );
    let source_stable =
        discovery
            .source_snapshot
            .matches_current_with_progress(source, |files_seen| {
                stability_progress.set(files_seen);
            })?;
    stability_progress.finish();
    if !source_stable {
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
    let source_snapshot = discovery.source_snapshot.clone();
    ensure_no_unreadable_dicom_like_files(discovery.unreadable_dicom_like_files)?;
    let source_fingerprint =
        fingerprint_source_with_progress(&source_snapshot, source, "Hashing source")?;
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
        .filter(|(_, classification)| classification.decision == ClassificationDecision::Accepted)
        .flat_map(|(group, _)| &group.files)
        .try_fold(0_u64, |total, path| {
            Ok::<_, anyhow::Error>(total.saturating_add(fs::metadata(path)?.len()))
        })?;
    let mut archive_progress = Progress::bounded(
        "Deidentifying EPI series",
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
        let before = archive_progress.completed();
        let staging_required = ensure_series_staging_capacity(paths, group)?;
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
                let classification =
                    coded_hold("bundle_exceeds_upload_limit", "local_safety_check");
                held_series.push(held(&pseudonymizer, group, index, &classification));
            }
            Err(error) if is_storage_exhaustion(&error) => {
                return Err(StagingStorageExhausted {
                    staging_root: paths.bundles.clone(),
                    required: staging_required,
                    available: fs2::available_space(&paths.bundles).unwrap_or(0),
                }
                .into());
            }
            Err(error) if error_contains_io(&error) => return Err(error),
            Err(error) => {
                tracing::warn!(
                    series = index + 1,
                    total_series = series_total,
                    error = %error,
                    "Functional EPI DICOM series was held by a local safety check"
                );
                summary.held += 1;
                let preparation_code = match error.to_string().as_str() {
                    "dicom_instance_exceeds_64_gib" => "dicom_instance_exceeds_64_gib",
                    "series_exceeds_64_gib_uncompressed_dicom_limit" => {
                        "series_exceeds_64_gib_uncompressed_dicom_limit"
                    }
                    "dicom_archive_expansion_ratio_exceeded" => {
                        "dicom_archive_expansion_ratio_exceeded"
                    }
                    "DICOM ImageType failed positional validation" => "dicom_image_type_invalid",
                    _ => "dicom_privacy_preparation_failed",
                };
                let classification = coded_hold(preparation_code, "local_safety_check");
                held_series.push(held(&pseudonymizer, group, index, &classification));
            }
        }
        local_progress.checkpoint(&summary, index + 1)?;
    }
    archive_progress.finish_at(archive_progress.completed());

    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    let mut final_stability_progress = Progress::bounded(
        "Final source check",
        discovery.summary.files_seen,
        ProgressUnit::Files,
    );
    let final_stable = source_snapshot.matches_current_with_progress(source, |files_seen| {
        final_stability_progress.set(files_seen);
    })?;
    final_stability_progress.finish();
    if !final_stable {
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
    state.set_source_fingerprint(run_id, &source_fingerprint)?;
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
        classifier_contract_version: crate::DICOM_CLASSIFIER_CONTRACT_VERSION.into(),
        archive_contract_version: crate::DICOM_ARCHIVE_CONTRACT_VERSION.into(),
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
        client_version: crate::CLIENT_VERSION.into(),
        started_at,
        completed_at: None,
        source_summary: summary.clone(),
        bundles: bundles.iter().map(ReportBundle::from).collect(),
        held_series,
        errors,
        worker_upload_id: None,
        worker_upload_ids: Vec::new(),
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
        classifier_contract_version: crate::DICOM_CLASSIFIER_CONTRACT_VERSION.into(),
        archive_contract_version: crate::DICOM_ARCHIVE_CONTRACT_VERSION.into(),
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
        client_version: crate::CLIENT_VERSION.into(),
        started_at,
        completed_at: None,
        source_summary: summary.clone(),
        bundles: Vec::new(),
        held_series,
        errors: vec!["source_changed_or_incomplete".into()],
        worker_upload_id: None,
        worker_upload_ids: Vec::new(),
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

fn require_dicom_transfer_checkpoint(status: &UploadStatus, series_count: usize) -> Result<()> {
    if dicom_receipt_complete(&status.status) {
        return Ok(());
    }
    let received = status
        .receipt
        .as_ref()
        .map_or(0, |receipt| receipt.received_series as usize);
    if status.status != "checkpointed" || received != series_count {
        bail!("DICOM checkpoint API did not durably receive every transferred series archive");
    }
    Ok(())
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
    for root in bundle_staging_roots(paths) {
        if let Err(error) = fs::remove_dir_all(root.join(run_id)) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(run_id, "could not remove local bundle cache");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared_bundle(local_path: &str) -> ManifestBundle {
        ManifestBundle {
            bundle_id: "bundle".into(),
            series_id: "series".into(),
            subject_id: "subject".into(),
            session_id: "session".into(),
            protocol_group_id: "protocol".into(),
            series_kind: "functional_epi".into(),
            archive_route: "functional-epi-v1".into(),
            pixel_data_policy: "scanner-native-not-defaced".into(),
            archive: Some(crate::model::ManifestArchiveObject {
                object: crate::model::ManifestObject {
                    relative_key: "bundle/dicom.tar.zst".into(),
                    local_path: local_path.into(),
                    size: 12,
                    sha256: "a".repeat(64),
                },
                format: "dicom-tar-zstd".into(),
                dicom_instance_count: 1,
                deidentification_profile: DICOM_METADATA_POLICY_ID.into(),
                deidentification_profile_version: DICOM_METADATA_POLICY_VERSION.into(),
            }),
            source_dicom_count: 1,
            classification: Classification {
                decision: ClassificationDecision::Accepted,
                kind: "functional_epi".into(),
                confidence: 1.0,
                evidence: Vec::new(),
            },
            qc: crate::model::QcResult {
                passed: true,
                checks: Vec::new(),
                warnings: Vec::new(),
            },
        }
    }

    #[test]
    fn regenerated_bundle_identity_ignores_ephemeral_staging_path() {
        let old = prepared_bundle("/home/user/.local/share/neuro-sync/bundles/archive");
        let regenerated = prepared_bundle("/tmp/neuro-sync-staging/archive");
        assert!(same_prepared_bundle(&old, &regenerated).unwrap());

        let mut changed = regenerated;
        changed.archive.as_mut().unwrap().object.sha256 = "b".repeat(64);
        assert!(!same_prepared_bundle(&old, &changed).unwrap());
    }

    #[test]
    fn final_streaming_stability_detects_a_changed_folder() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("one.dcm"), b"first").unwrap();
        let initial =
            crate::dicom::snapshot_source_with_progress(directory.path(), |_| {}).unwrap();

        confirm_final_streaming_source_stability(&initial, directory.path(), 1).unwrap();

        std::fs::write(directory.path().join("two.dcm"), b"second").unwrap();
        let error =
            confirm_final_streaming_source_stability(&initial, directory.path(), 1).unwrap_err();
        assert!(error.to_string().contains("changed during sync"));
    }
}
