use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
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
        CompleteUploadRequest, CreateUploadResponse, IngestApi, MultipartObject, has_error_code,
        normalize_base_url,
    },
    bundle::{BundleRequest, analyze_converted, create_bundle},
    classify::{ConversionSignals, classify_header, refine_after_conversion},
    config::{AppPaths, ClientConfig},
    convert::Converter,
    dicom::{Discovery, SeriesGroup, discover},
    model::{
        Classification, ClassificationDecision, ClassificationEvidence, HeldSeries, LocalManifest,
        ManifestBundle, ReportBundle, RunReport, SourceSummary,
    },
    pseudonym::Pseudonymizer,
    s3::MultipartUploader,
    state::{RunRecord, StateStore, UploadObjectRecord},
};

const MAX_BUNDLES_PER_UPLOAD: usize = 32;
const MAX_BYTES_PER_UPLOAD: u64 = 32 * 1024 * 1024 * 1024;
const MAX_NIFTI_BYTES_PER_BUNDLE: u64 = 5 * 1024 * 1024 * 1024;
const SOURCE_QUIET_INTERVAL: Duration = Duration::from_secs(2);

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

    pub async fn upload(&self, source: PathBuf, dry_run: bool) -> Result<String> {
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
        let run_id = Uuid::new_v4().to_string();
        self.state.create_run(&run_id, &canonical_source, dry_run)?;
        self.process_existing_run(&run_id, canonical_source, dry_run, config)
            .await?;
        Ok(run_id)
    }

    pub async fn upload_in_background(&self, source: PathBuf, dry_run: bool) -> Result<String> {
        let canonical_source = source.canonicalize()?;
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
        let run_id = Uuid::new_v4().to_string();
        self.state.create_run(&run_id, &canonical_source, dry_run)?;
        let runtime = self.clone();
        let background_id = run_id.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime
                .process_existing_run(&background_id, canonical_source, dry_run, config)
                .await
            {
                tracing::error!(run_id = %background_id, error = %error, "upload run failed");
            }
        });
        Ok(run_id)
    }

    async fn process_existing_run(
        &self,
        run_id: &str,
        source: PathBuf,
        dry_run: bool,
        config: ClientConfig,
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
        report.archive_commit_count = chunks
            .iter()
            .filter(|chunk| chunk.status == "committed")
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
            if chunk.status == "committed" {
                continue;
            }
            let end = chunk.bundle_start + chunk.bundle_count;
            let bundles = manifest
                .bundles
                .get(chunk.bundle_start..end)
                .context("local upload chunk points outside the prepared manifest")?;
            self.continue_upload_chunk(run_id, &chunk, bundles, &manifest.client_version, &api)
                .await?;
            for bundle in bundles {
                if let Some(directory) = Path::new(&bundle.nifti.local_path).parent() {
                    if let Err(error) = fs::remove_dir_all(directory) {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!(run_id, "could not remove committed local bundle cache");
                        }
                    }
                }
            }
        }
        if self
            .state
            .run_uploads(run_id)?
            .iter()
            .any(|chunk| chunk.status != "committed")
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
        let (worker_upload_id, object_prefix, descriptors, committed, revived) =
            if let Some(upload_id) = chunk.worker_upload_id.as_deref() {
                match api.status(upload_id).await {
                    Err(error) if has_error_code(&error, "UPLOAD_NOT_WRITABLE") => {
                        session_from_created(
                            api.create_upload(bundles, preparation_client_version)
                                .await?,
                            true,
                        )
                    }
                    Err(error) => return Err(error),
                    Ok(status) if status.status == "committed" => (
                        upload_id.to_owned(),
                        status.object_prefix.unwrap_or_default(),
                        Vec::new(),
                        true,
                        false,
                    ),
                    Ok(status) if status.status == "expired" => session_from_created(
                        api.create_upload(bundles, preparation_client_version)
                            .await?,
                        true,
                    ),
                    Ok(status) if status.status == "withdrawn" => {
                        bail!("archive upload was withdrawn and cannot be resumed");
                    }
                    Ok(_) => match api.refresh_credentials(upload_id).await {
                        Ok(refreshed) => (
                            refreshed.upload_id,
                            refreshed.object_prefix,
                            refreshed.multipart_objects,
                            false,
                            false,
                        ),
                        Err(error) if has_error_code(&error, "UPLOAD_NOT_WRITABLE") => {
                            session_from_created(
                                api.create_upload(bundles, preparation_client_version)
                                    .await?,
                                true,
                            )
                        }
                        Err(error) => return Err(error),
                    },
                }
            } else {
                session_from_created(
                    api.create_upload(bundles, preparation_client_version)
                        .await?,
                    false,
                )
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
            let status = api
                .complete_upload(&worker_upload_id, saved.objects)
                .await?;
            if status.status == "committed" || status.status == "complete" {
                self.state
                    .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
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
        let status = api
            .complete_upload(&worker_upload_id, request.objects)
            .await?;
        if status.status != "committed" && status.status != "complete" {
            bail!("ingest API did not commit the completed multipart upload");
        }
        self.state
            .set_chunk_status(run_id, chunk.chunk_index, "committed")?;
        Ok(())
    }

    pub async fn resume(&self, requested_id: Option<&str>) -> Result<Vec<String>> {
        let config = ClientConfig::load(&self.paths)?;
        let runs = self.state.resumable_runs(requested_id)?;
        if runs.is_empty() && requested_id.is_some() {
            bail!("the requested run is not resumable");
        }
        let mut completed = Vec::new();
        for run in runs {
            let manifest_path = run
                .manifest_path
                .as_deref()
                .context("resumable run has no manifest")?;
            let manifest: LocalManifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
            match self.continue_upload(&run.id, &manifest, &config).await {
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
                        .filter(|chunk| chunk.status == "committed")
                        .count() as u64;
                    update_report_status(
                        &self.paths,
                        &run.id,
                        "complete",
                        worker_upload_ids,
                        committed,
                    )?;
                    remove_bundle_cache(&self.paths, &run.id);
                    completed.push(run.id);
                }
                Err(error) => {
                    self.state.update_run(
                        &run.id,
                        "upload_failed",
                        &manifest.source_summary,
                        Some("upload_failed"),
                    )?;
                    return Err(error);
                }
            }
        }
        Ok(completed)
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
        let pending: PendingEnrollment = serde_json::from_slice(
            &fs::read(&paths.pending_enrollment)
                .context("could not read pending enrollment state")?,
        )
        .context("pending enrollment state is invalid")?;
        if pending.invite_sha256 == invite_sha256 && pending.api_origin == api_origin {
            validate_pending_enrollment(&pending)?;
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

fn validate_manifest_enrollment(manifest: &LocalManifest, config: &ClientConfig) -> Result<()> {
    if manifest.site_id != config.site_id || manifest.project_id != config.project_id {
        bail!("prepared run belongs to a different enrolled site or project");
    }
    if manifest.consent_policy_version.is_empty()
        || manifest.consent_policy_version != config.consent_policy_version
    {
        bail!("prepared run requires approval under the current contribution policy");
    }
    if manifest.client_version.trim().is_empty() {
        bail!("prepared run has no client provenance version");
    }
    Ok(())
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
    let initial_discovery = discover(source)?;
    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    let discovery = discover(source)?;
    let mut summary = discovery.summary.clone();
    state.update_run(run_id, "converting", &summary, None)?;
    let pseudonymizer = Pseudonymizer::from_base64(&config.pseudonym_key_b64)?;
    if !initial_discovery
        .source_snapshot
        .is_stable_with(&discovery.source_snapshot)
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
    let source_snapshot = discovery.source_snapshot.clone();
    let converter = Converter::discover(&paths.work)?;
    if !dry_run && converter.version != PINNED_DCM2NIIX_VERSION {
        bail!("unvalidated converter builds are allowed only with --dry-run");
    }
    let bundle_root = paths.bundles.join(run_id);
    fs::create_dir_all(&bundle_root)?;
    let mut bundles = Vec::new();
    let mut held_series = Vec::new();

    for (index, group) in discovery.series.into_iter().enumerate() {
        let initial = classify_header(&group);
        match initial.decision {
            ClassificationDecision::Excluded => {
                summary.excluded += 1;
                continue;
            }
            ClassificationDecision::Held => {
                summary.held += 1;
                held_series.push(held(&pseudonymizer, &group, index, &initial));
                continue;
            }
            ClassificationDecision::Accepted => {}
        }
        let converted = match converter.convert(&group, &paths.work) {
            Ok(converted) => converted,
            Err(_) => {
                let classification = coded_hold("conversion_failed", "converter_sidecar");
                summary.held += 1;
                held_series.push(held(&pseudonymizer, &group, index, &classification));
                continue;
            }
        };
        if converted.images.is_empty() {
            let classification = refine_after_conversion(initial, &ConversionSignals::default());
            summary.held += 1;
            held_series.push(held(&pseudonymizer, &group, index, &classification));
            continue;
        }
        let Some(echo_labels) = multi_echo_labels(&converted.images) else {
            let classification = coded_hold("conversion_output_ambiguous", "converter_sidecar");
            summary.held += 1;
            held_series.push(held(&pseudonymizer, &group, index, &classification));
            continue;
        };
        let mut prepared = Vec::with_capacity(converted.images.len());
        let mut failed_classification = None;
        for (image_index, (image, echo_label)) in
            converted.images.iter().zip(echo_labels).enumerate()
        {
            let analyzed = match analyze_converted(&group, image, 1) {
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
            held_series.push(held(&pseudonymizer, &group, index, &classification));
            continue;
        }
        let mut created = Vec::with_capacity(prepared.len());
        let mut creation_failed = false;
        for (image_index, echo_label, analyzed, classification) in prepared {
            match create_bundle(BundleRequest {
                group: &group,
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
            held_series.push(held(&pseudonymizer, &group, index, &classification));
        } else {
            summary.accepted += 1;
            bundles.extend(created);
        }
        state.update_run(run_id, "converting", &summary, None)?;
    }

    std::thread::sleep(SOURCE_QUIET_INTERVAL);
    let final_discovery = discover(source)?;
    if !source_snapshot.is_stable_with(&final_discovery.source_snapshot) {
        let _ = fs::remove_dir_all(&bundle_root);
        return finish_unstable_preparation(
            paths,
            state,
            run_id,
            config,
            started_at,
            final_discovery,
            &pseudonymizer,
        );
    }

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
    for bundle in bundles {
        for object in [&bundle.nifti, &bundle.metadata] {
            let key = format!("{prefix}{}", object.relative_key);
            if !descriptor_keys.contains(key.as_str()) {
                bail!("ingest API multipart plan key does not match the requested archive key");
            }
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
    archive_commit_count: u64,
) -> Result<()> {
    let path = paths.reports.join(format!("{run_id}.json"));
    let mut report: RunReport = serde_json::from_slice(&fs::read(&path)?)?;
    report.status = status.into();
    report.completed_at = Some(Utc::now().to_rfc3339());
    report.worker_upload_id = worker_upload_ids.first().cloned();
    report.worker_upload_ids = worker_upload_ids;
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
    fn resume_requires_the_original_enrollment_and_policy() {
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
            client_version: "0.0.9".into(),
            created_at: "2026-07-12T00:00:00Z".into(),
            source_summary: SourceSummary::default(),
            bundles: Vec::new(),
        };
        assert!(validate_manifest_enrollment(&manifest, &config).is_ok());
        manifest.project_id = "project-b".into();
        assert!(validate_manifest_enrollment(&manifest, &config).is_err());
        manifest.project_id = "project-a".into();
        manifest.consent_policy_version = "policy-1".into();
        assert!(validate_manifest_enrollment(&manifest, &config).is_err());
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
}
