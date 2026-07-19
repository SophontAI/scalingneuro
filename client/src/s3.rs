use std::{
    collections::HashMap,
    convert::Infallible,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
};

use crate::{
    api::{
        ApiFailure, CompletedObject, CompletedPart, IngestApi, MultipartObject, PartUploadGrant,
        PartUploadRequest, UploadRoute,
    },
    progress::{Progress, ProgressUnit},
    state::{StateStore, UploadObjectRecord, UploadedPart},
};

const MAX_PARTS: u64 = 10_000;
const DICOM_OBJECT_CONCURRENCY: usize = 3;
const UPLOAD_BODY_CHUNK_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum UploadProgress {
    Checkpointed(u64),
    Transferred(u64),
}

#[derive(Clone)]
pub struct MultipartUploader {
    http: Client,
    api: IngestApi,
    state: StateStore,
    worker_upload_id: String,
    route: UploadRoute,
}

impl MultipartUploader {
    pub fn new(api: IngestApi, state: StateStore, worker_upload_id: String) -> Result<Self> {
        Self::new_for(api, state, worker_upload_id, UploadRoute::Legacy)
    }

    pub fn new_dicom(api: IngestApi, state: StateStore, worker_upload_id: String) -> Result<Self> {
        Self::new_for(api, state, worker_upload_id, UploadRoute::Dicom)
    }

    fn new_for(
        api: IngestApi,
        state: StateStore,
        worker_upload_id: String,
        route: UploadRoute,
    ) -> Result<Self> {
        let http = build_part_client()?;
        Ok(Self {
            http,
            api,
            state,
            worker_upload_id,
            route,
        })
    }

    pub async fn upload_all(
        &self,
        objects: &[UploadObjectRecord],
        descriptors: &[MultipartObject],
    ) -> Result<Vec<CompletedObject>> {
        let total_bytes = objects.iter().map(|object| object.size).sum();
        for object in objects {
            verify_local_object_size(Path::new(&object.local_path), object.size).await?;
        }
        let label = match self.route {
            UploadRoute::Legacy => "Uploading",
            UploadRoute::Dicom => "Uploading privacy-cleared EPI archives",
        };
        let mut progress = Progress::bounded(label, total_bytes, ProgressUnit::Bytes);
        let descriptor_map: HashMap<_, _> = descriptors
            .iter()
            .map(|item| (item.key.as_str(), item))
            .collect();
        if descriptor_map.len() != objects.len() {
            bail!("ingest API returned an incomplete multipart object plan");
        }
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let transfer = async {
            if self.route == UploadRoute::Dicom {
                stream::iter(objects.iter().enumerate())
                    .map(|(index, object)| {
                        let uploader = self.clone();
                        let descriptor = descriptor_map.get(object.key.as_str()).copied().context(
                            "ingest API omitted an expected object from the multipart plan",
                        );
                        let progress_tx = progress_tx.clone();
                        async move {
                            let descriptor = descriptor?;
                            tracing::debug!(
                                file = index + 1,
                                total_files = objects.len(),
                                bytes = object.size,
                                "Uploading prepared file"
                            );
                            let completed = uploader
                                .upload_object(object, descriptor, &progress_tx)
                                .await?;
                            tracing::debug!(
                                file = index + 1,
                                total_files = objects.len(),
                                "Prepared file upload complete"
                            );
                            Ok::<_, anyhow::Error>(completed)
                        }
                    })
                    .buffer_unordered(DICOM_OBJECT_CONCURRENCY)
                    .try_collect::<Vec<_>>()
                    .await
            } else {
                let mut completed = Vec::with_capacity(objects.len());
                for (index, object) in objects.iter().enumerate() {
                    let descriptor = descriptor_map
                        .get(object.key.as_str())
                        .context("ingest API omitted an expected object from the multipart plan")?;
                    tracing::debug!(
                        file = index + 1,
                        total_files = objects.len(),
                        bytes = object.size,
                        "Uploading prepared file"
                    );
                    completed.push(self.upload_object(object, descriptor, &progress_tx).await?);
                    tracing::debug!(
                        file = index + 1,
                        total_files = objects.len(),
                        "Prepared file upload complete"
                    );
                }
                Ok(completed)
            }
        };
        tokio::pin!(transfer);
        let mut completed = loop {
            tokio::select! {
                result = &mut transfer => break result?,
                Some(update) = progress_rx.recv() => apply_progress(&mut progress, update),
            }
        };
        while let Ok(update) = progress_rx.try_recv() {
            apply_progress(&mut progress, update);
        }
        completed.sort_by(|left, right| left.key.cmp(&right.key));
        progress.finish();
        Ok(completed)
    }

    async fn upload_object(
        &self,
        object: &UploadObjectRecord,
        descriptor: &MultipartObject,
        progress: &tokio::sync::mpsc::UnboundedSender<UploadProgress>,
    ) -> Result<CompletedObject> {
        if descriptor.part_size == 0 {
            bail!("ingest API returned an invalid zero multipart size");
        }
        let expected_part_count = object.size.div_ceil(descriptor.part_size).max(1);
        if expected_part_count > MAX_PARTS {
            bail!("object requires more than {MAX_PARTS} multipart parts");
        }
        let same_multipart = object.multipart_id.as_deref() == Some(descriptor.upload_id.as_str());
        if same_multipart {
            self.state.set_multipart_id(
                &object.worker_upload_id,
                &object.key,
                &descriptor.upload_id,
            )?;
        } else {
            self.state.reset_multipart(
                &object.worker_upload_id,
                &object.key,
                &descriptor.upload_id,
            )?;
        }

        let mut persisted = self
            .state
            .uploaded_parts(&object.worker_upload_id, &object.key)?;
        let persisted_bytes = persisted.iter().map(|part| part.size).sum();
        if same_multipart && valid_complete_part_set(&persisted, object.size, descriptor.part_size)
        {
            let _ = progress.send(UploadProgress::Checkpointed(persisted_bytes));
            return Ok(completed_object(object, persisted));
        }
        if persisted.iter().any(|part| {
            expected_part_size(
                object.size,
                descriptor.part_size,
                u64::from(part.part_number),
            ) != Some(part.size)
        }) {
            self.state.reset_multipart(
                &object.worker_upload_id,
                &object.key,
                &descriptor.upload_id,
            )?;
            persisted.clear();
        }
        let _ = progress.send(UploadProgress::Checkpointed(
            persisted.iter().map(|part| part.size).sum(),
        ));

        // Locally checkpointed ETags are sufficient. If the process stopped
        // after R2 accepted a part but before SQLite committed its receipt,
        // re-PUT of that part number to the same multipart ID is idempotent.
        let completed_by_number: HashMap<u32, UploadedPart> = persisted
            .into_iter()
            .map(|part| (part.part_number, part))
            .collect();
        let missing: Vec<u32> = (1..=expected_part_count as u32)
            .filter(|part_number| !completed_by_number.contains_key(part_number))
            .collect();
        let uploader = self.clone();
        let object = object.clone();
        let object_for_tasks = object.clone();
        let part_size = descriptor.part_size;
        let part_count = expected_part_count as u32;
        let descriptor = descriptor.clone();
        let progress = progress.clone();
        let mut uploaded = stream::iter(missing)
            .map(move |part_number| {
                let uploader = uploader.clone();
                let object = object_for_tasks.clone();
                let descriptor = descriptor.clone();
                let progress = progress.clone();
                async move {
                    uploader
                        .upload_part_with_retry(
                            &object,
                            &descriptor,
                            part_number,
                            part_count,
                            &progress,
                        )
                        .await
                }
            })
            // R2 applies a one-write-per-second-per-key limit. Keep parts for
            // one object sequential; retryable 429s receive a fresh grant.
            .buffer_unordered(1);
        while let Some(part) = uploaded.next().await {
            part?;
        }
        let parts = self
            .state
            .uploaded_parts(&object.worker_upload_id, &object.key)?;
        if !valid_complete_part_set(&parts, object.size, part_size) {
            bail!("multipart upload state is incomplete after transfer");
        }
        Ok(completed_object(&object, parts))
    }

    async fn upload_part_with_retry(
        &self,
        object: &UploadObjectRecord,
        descriptor: &MultipartObject,
        part_number: u32,
        part_count: u32,
        progress: &tokio::sync::mpsc::UnboundedSender<UploadProgress>,
    ) -> Result<UploadedPart> {
        let offset = u64::from(part_number - 1) * descriptor.part_size;
        let size = (object.size - offset).min(descriptor.part_size);
        let body = Arc::new(read_file_part(Path::new(&object.local_path), offset, size).await?);
        let sha256 = hex::encode(Sha256::digest(body.as_slice()));
        let reported_bytes = Arc::new(AtomicU64::new(0));
        let mut last_error = None;

        for attempt in 0..5_u32 {
            let request = PartUploadRequest {
                key: object.key.clone(),
                part_number,
                size,
                sha256: sha256.clone(),
            };
            let grant = match match self.route {
                UploadRoute::Legacy => {
                    self.api
                        .create_part_upload(&self.worker_upload_id, request)
                        .await
                }
                UploadRoute::Dicom => {
                    self.api
                        .create_dicom_part_upload(&self.worker_upload_id, request)
                        .await
                }
            } {
                Ok(grant) => grant,
                Err(error) => {
                    let retry_after = if let Some(failure) = error.downcast_ref::<ApiFailure>() {
                        if !failure.is_retryable() {
                            return Err(error);
                        }
                        failure.retry_after()
                    } else {
                        None
                    };
                    last_error = Some(error);
                    retry_delay(attempt, retry_after).await;
                    continue;
                }
            };
            validate_part_grant(
                &grant,
                &object.key,
                part_number,
                &descriptor.upload_id,
                size,
                &sha256,
            )?;
            let response = self
                .http
                .put(&grant.url)
                .header("content-length", &grant.headers.content_length)
                .header("x-amz-content-sha256", &grant.headers.content_sha256)
                .body(progress_body(
                    Arc::clone(&body),
                    Arc::clone(&reported_bytes),
                    progress.clone(),
                ))
                .send()
                .await;
            let mut server_delay = None;
            match response {
                Ok(response) if response.status().is_success() => {
                    let etag = response
                        .headers()
                        .get("etag")
                        .and_then(|value| value.to_str().ok())
                        .context("R2 UploadPart response omitted ETag")?
                        .trim_matches('"')
                        .to_owned();
                    if etag.is_empty() {
                        bail!("R2 UploadPart response returned an empty ETag");
                    }
                    let part = UploadedPart {
                        part_number,
                        etag,
                        size,
                    };
                    self.state
                        .save_part(&object.worker_upload_id, &object.key, &part)?;
                    tracing::debug!(
                        part = part_number,
                        total_parts = part_count,
                        bytes = size,
                        "Uploaded file part"
                    );
                    return Ok(part);
                }
                Ok(response)
                    if response.status() == StatusCode::UNAUTHORIZED
                        || response.status() == StatusCode::FORBIDDEN
                        || is_retryable(response.status()) =>
                {
                    server_delay = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(|seconds| Duration::from_secs(seconds.min(30)));
                    last_error = Some(anyhow::anyhow!(
                        "presigned part upload failed (http_{})",
                        response.status().as_u16()
                    ));
                }
                Ok(response) => {
                    bail!(
                        "R2 rejected the allocated part upload (http_{})",
                        response.status().as_u16()
                    );
                }
                Err(_) => {
                    // reqwest errors may render the full presigned URL. Never
                    // retain or surface that secret-bearing request target.
                    last_error = Some(anyhow::anyhow!("presigned_part_transport_failed"));
                }
            }
            retry_delay(attempt, server_delay).await;
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("R2 part upload exhausted retries")))
    }
}

fn apply_progress(progress: &mut Progress, update: UploadProgress) {
    match update {
        UploadProgress::Checkpointed(bytes) => progress.inc_checkpointed(bytes),
        UploadProgress::Transferred(bytes) => progress.inc(bytes),
    }
}

fn progress_body(
    bytes: Arc<Vec<u8>>,
    reported_bytes: Arc<AtomicU64>,
    progress: tokio::sync::mpsc::UnboundedSender<UploadProgress>,
) -> reqwest::Body {
    let chunks = stream::unfold((bytes, 0_usize), move |(bytes, offset)| {
        let reported_bytes = Arc::clone(&reported_bytes);
        let progress = progress.clone();
        async move {
            if offset >= bytes.len() {
                return None;
            }
            let end = offset
                .saturating_add(UPLOAD_BODY_CHUNK_SIZE)
                .min(bytes.len());
            let chunk = bytes[offset..end].to_vec();
            let sent = end as u64;
            let previously_reported = reported_bytes.fetch_max(sent, Ordering::Relaxed);
            if sent > previously_reported {
                let _ = progress.send(UploadProgress::Transferred(sent - previously_reported));
            }
            Some((Ok::<_, Infallible>(chunk), (bytes, end)))
        }
    });
    reqwest::Body::wrap_stream(chunks)
}

fn build_part_client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(15 * 60))
        .user_agent(format!("neuro-sync/{}", crate::CLIENT_VERSION))
        .build()?)
}

fn validate_part_grant(
    grant: &PartUploadGrant,
    key: &str,
    part_number: u32,
    multipart_id: &str,
    size: u64,
    sha256: &str,
) -> Result<()> {
    let url = url::Url::parse(&grant.url).context("ingest API returned an invalid part URL")?;
    let loopback_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !loopback_http {
        bail!("ingest API part URL must use HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("ingest API returned an unsafe part URL");
    }
    if !loopback_http {
        let account = url
            .host_str()
            .and_then(|host| host.strip_suffix(".r2.cloudflarestorage.com"))
            .context("ingest API part URL is not a Cloudflare R2 endpoint")?;
        if account.len() != 32
            || !account.bytes().all(|byte| byte.is_ascii_hexdigit())
            || url.port().is_some()
        {
            bail!("ingest API part URL has an invalid R2 account endpoint");
        }
    }
    if !url.path().ends_with(&format!("/{key}")) {
        bail!("ingest API part URL does not target the allocated object key");
    }
    let mut query = HashMap::new();
    for (name, value) in url.query_pairs() {
        if query
            .insert(name.into_owned(), value.into_owned())
            .is_some()
        {
            bail!("ingest API part URL contains a duplicate query parameter");
        }
    }
    if query.get("partNumber").map(String::as_str) != Some(part_number.to_string().as_str()) {
        bail!("ingest API part URL binds a different part number");
    }
    if multipart_id.is_empty() || query.get("uploadId").map(String::as_str) != Some(multipart_id) {
        bail!("ingest API part URL binds a different multipart upload");
    }
    let expires_seconds = query
        .get("X-Amz-Expires")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| (1..=900).contains(value))
        .context("ingest API part URL has an invalid lifetime")?;
    let signed_at = query
        .get("X-Amz-Date")
        .and_then(|value| chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ").ok())
        .map(|value| value.and_utc())
        .context("ingest API part URL has no valid signing time")?;
    if query.get("X-Amz-Algorithm").map(String::as_str) != Some("AWS4-HMAC-SHA256") {
        bail!("ingest API part URL uses an unexpected signing algorithm");
    }
    if query.get("X-Amz-SignedHeaders").map(String::as_str)
        != Some("content-length;host;x-amz-content-sha256")
    {
        bail!("ingest API part URL does not bind every required header");
    }
    let credential = query
        .get("X-Amz-Credential")
        .context("ingest API part URL has no credential scope")?;
    let mut credential_parts = credential.split('/');
    let access_key = credential_parts.next().unwrap_or_default();
    let scope_date = credential_parts.next().unwrap_or_default();
    let region = credential_parts.next().unwrap_or_default();
    let service = credential_parts.next().unwrap_or_default();
    let terminator = credential_parts.next().unwrap_or_default();
    if access_key.is_empty()
        || !access_key.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || scope_date != signed_at.format("%Y%m%d").to_string()
        || region != "auto"
        || service != "s3"
        || terminator != "aws4_request"
        || credential_parts.next().is_some()
    {
        bail!("ingest API part URL has an invalid credential scope");
    }
    query
        .get("X-Amz-Signature")
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .context("ingest API part URL has no valid signature")?;
    let declared_expiry = chrono::DateTime::parse_from_rfc3339(&grant.expires_at)
        .context("ingest API returned an invalid part URL expiry")?
        .with_timezone(&chrono::Utc);
    let signed_expiry = signed_at + chrono::Duration::seconds(expires_seconds);
    if (declared_expiry - signed_expiry).num_seconds().abs() > 5 {
        bail!("ingest API part URL expiry does not match its signed lifetime");
    }
    let now = chrono::Utc::now();
    if declared_expiry <= now - chrono::Duration::minutes(5)
        || declared_expiry > now + chrono::Duration::minutes(20)
    {
        bail!("ingest API part URL expiry is outside the allowed window");
    }
    if grant.headers.content_length != size.to_string() {
        bail!("ingest API signed a different part length than requested");
    }
    if grant.headers.content_sha256 != sha256 {
        bail!("ingest API signed a different part digest than requested");
    }
    Ok(())
}

async fn retry_delay(attempt: u32, retry_after: Option<Duration>) {
    let exponential = Duration::from_millis(250 * 2_u64.pow(attempt));
    tokio::time::sleep(retry_after.unwrap_or(exponential).max(exponential)).await;
}

async fn read_file_part(path: &Path, offset: u64, size: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path).await?;
    file.seek(SeekFrom::Start(offset)).await?;
    let mut bytes =
        vec![0_u8; usize::try_from(size).context("multipart part is too large for this machine")?];
    file.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn verify_local_object_size(path: &Path, expected_size: u64) -> Result<()> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("prepared bundle object is missing: {}", path.display()))?;
    if metadata.len() != expected_size {
        bail!("prepared bundle object size changed before upload");
    }
    Ok(())
}

fn expected_part_size(total: u64, part_size: u64, part_number: u64) -> Option<u64> {
    let offset = part_number.checked_sub(1)?.checked_mul(part_size)?;
    (offset < total).then(|| (total - offset).min(part_size))
}

fn valid_complete_part_set(parts: &[UploadedPart], total: u64, part_size: u64) -> bool {
    let expected_count = total.div_ceil(part_size).max(1);
    parts.len() == expected_count as usize
        && parts.iter().enumerate().all(|(index, part)| {
            part.part_number == index as u32 + 1
                && expected_part_size(total, part_size, u64::from(part.part_number))
                    == Some(part.size)
        })
}

fn completed_object(object: &UploadObjectRecord, parts: Vec<UploadedPart>) -> CompletedObject {
    CompletedObject {
        key: object.key.clone(),
        size: object.size,
        sha256: object.sha256.clone(),
        parts: parts
            .into_iter()
            .map(|part| CompletedPart {
                part_number: part.part_number,
                etag: part.etag.trim_matches('"').to_owned(),
            })
            .collect(),
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PartUploadGrant, PartUploadHeaders};
    use axum::{
        Json, Router,
        extract::Path as AxumPath,
        http::{StatusCode, header::LOCATION},
        routing::{any, put},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn part_sizes_cover_file_without_overlap() {
        assert_eq!(expected_part_size(11, 5, 1), Some(5));
        assert_eq!(expected_part_size(11, 5, 2), Some(5));
        assert_eq!(expected_part_size(11, 5, 3), Some(1));
        assert_eq!(expected_part_size(11, 5, 4), None);
    }

    #[test]
    fn etag_normalization_is_bare() {
        assert_eq!("\"abc123\"".trim_matches('"'), "abc123");
        assert_eq!("abc123".trim_matches('"'), "abc123");
    }

    #[test]
    fn part_grant_must_bind_exact_length_and_digest() {
        let digest = "a".repeat(64);
        let signed_at = chrono::Utc::now();
        let multipart_id = "multipart-fixture";
        let key = "prefix/bundle/scan.nii.gz";
        let grant = PartUploadGrant {
            url: format!(
                "https://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.r2.cloudflarestorage.com/bucket/{key}?partNumber=2&uploadId={multipart_id}&X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=ACCESSKEY%2F{}%2Fauto%2Fs3%2Faws4_request&X-Amz-Date={}&X-Amz-Expires=900&X-Amz-SignedHeaders=content-length%3Bhost%3Bx-amz-content-sha256&X-Amz-Signature={}",
                signed_at.format("%Y%m%d"),
                signed_at.format("%Y%m%dT%H%M%SZ"),
                "f".repeat(64)
            ),
            headers: PartUploadHeaders {
                content_length: "12".into(),
                content_sha256: digest.clone(),
            },
            expires_at: (signed_at + chrono::Duration::minutes(15)).to_rfc3339(),
        };
        assert!(validate_part_grant(&grant, key, 2, multipart_id, 12, &digest).is_ok());
        assert!(validate_part_grant(&grant, key, 3, multipart_id, 12, &digest).is_err());
        assert!(validate_part_grant(&grant, key, 2, multipart_id, 13, &digest).is_err());
        assert!(validate_part_grant(&grant, key, 2, multipart_id, 12, &"b".repeat(64)).is_err());
        let mut duplicate = grant.clone();
        duplicate.url.push_str("&partNumber=2");
        assert!(validate_part_grant(&duplicate, key, 2, multipart_id, 12, &digest).is_err());
    }

    #[tokio::test]
    async fn multipart_client_never_follows_307_or_308() {
        for redirect_status in [
            StatusCode::TEMPORARY_REDIRECT,
            StatusCode::PERMANENT_REDIRECT,
        ] {
            let target_hits = Arc::new(AtomicUsize::new(0));
            let target_hits_for_handler = target_hits.clone();
            let target_app = Router::new().fallback(any(move || {
                let target_hits = target_hits_for_handler.clone();
                async move {
                    target_hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                }
            }));
            let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target_address = target_listener.local_addr().unwrap();
            let target_server =
                tokio::spawn(
                    async move { axum::serve(target_listener, target_app).await.unwrap() },
                );

            let location = format!("http://{target_address}/signed-body-capture");
            let redirect_app = Router::new().route(
                "/part",
                put(move || {
                    let location = location.clone();
                    async move { (redirect_status, [(LOCATION, location)]) }
                }),
            );
            let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let redirect_address = redirect_listener.local_addr().unwrap();
            let redirect_server =
                tokio::spawn(
                    async move { axum::serve(redirect_listener, redirect_app).await.unwrap() },
                );

            let response = build_part_client()
                .unwrap()
                .put(format!("http://{redirect_address}/part"))
                .header(reqwest::header::AUTHORIZATION, "signed-sensitive-value")
                .body("sensitive-imaging-bytes")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), redirect_status.as_u16());
            tokio::task::yield_now().await;
            assert_eq!(target_hits.load(Ordering::SeqCst), 0);

            redirect_server.abort();
            target_server.abort();
        }
    }

    #[tokio::test]
    async fn upload_progress_advances_per_body_chunk_before_put_completion() {
        use axum::{body::Body, routing::post};
        use std::sync::{Mutex, atomic::AtomicU64};
        use tokio::sync::Semaphore;

        let origin = Arc::new(Mutex::new(String::new()));
        let release_first_response = Arc::new(Semaphore::new(0));
        let put_attempts = Arc::new(AtomicUsize::new(0));
        let received_bytes = Arc::new(AtomicU64::new(0));

        let grant_origin = Arc::clone(&origin);
        let grant_app = post(move |Json(request): Json<serde_json::Value>| {
            let origin = Arc::clone(&grant_origin);
            async move {
                let key = request["key"].as_str().unwrap();
                let part_number = request["part_number"].as_u64().unwrap();
                let size = request["size"].as_u64().unwrap();
                let sha256 = request["sha256"].as_str().unwrap();
                let multipart_id = "multipart-streaming-fixture";
                let signed_at = chrono::Utc::now();
                let origin = origin.lock().unwrap().clone();
                Json(serde_json::json!({
                    "url": format!(
                        "{origin}/bucket/{key}?partNumber={part_number}&uploadId={multipart_id}&X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=ACCESSKEY%2F{}%2Fauto%2Fs3%2Faws4_request&X-Amz-Date={}&X-Amz-Expires=900&X-Amz-SignedHeaders=content-length%3Bhost%3Bx-amz-content-sha256&X-Amz-Signature={}",
                        signed_at.format("%Y%m%d"),
                        signed_at.format("%Y%m%dT%H%M%SZ"),
                        "f".repeat(64),
                    ),
                    "headers": {
                        "content-length": size.to_string(),
                        "x-amz-content-sha256": sha256,
                    },
                    "expires_at": (signed_at + chrono::Duration::minutes(15)).to_rfc3339(),
                }))
            }
        });

        let put_release = Arc::clone(&release_first_response);
        let put_attempt_counter = Arc::clone(&put_attempts);
        let put_received_bytes = Arc::clone(&received_bytes);
        let put_app = put(move |body: Body| {
            let release = Arc::clone(&put_release);
            let attempts = Arc::clone(&put_attempt_counter);
            let received = Arc::clone(&put_received_bytes);
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                let mut body = body.into_data_stream();
                while let Some(chunk) = body.next().await {
                    received.fetch_add(chunk.unwrap().len() as u64, Ordering::SeqCst);
                }
                if attempt == 0 {
                    let _permit = release.acquire().await.unwrap();
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [(reqwest::header::ETAG.as_str(), "\"retry\"")],
                    );
                }
                (
                    StatusCode::OK,
                    [(reqwest::header::ETAG.as_str(), "\"streamed-etag\"")],
                )
            }
        });

        let app = Router::new()
            .route("/v1/dicom-uploads/{upload_id}/parts", grant_app)
            .route("/bucket/{*key}", put_app);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        *origin.lock().unwrap() = format!("http://{address}");
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let root = tempfile::tempdir().unwrap();
        let state = StateStore::open(&root.path().join("state.sqlite3")).unwrap();
        state.create_run("run", root.path(), false).unwrap();
        let worker_upload_id = "11111111-1111-4111-8111-111111111111";
        let bytes = vec![0x5a; UPLOAD_BODY_CHUNK_SIZE * 3 + 17];
        let path = root.path().join("dicom.tar.zst");
        std::fs::write(&path, &bytes).unwrap();
        let object = UploadObjectRecord {
            run_id: "run".into(),
            worker_upload_id: worker_upload_id.into(),
            key: "prefix/dicom.tar.zst".into(),
            local_path: path.to_string_lossy().into_owned(),
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
            multipart_id: None,
            status: "pending".into(),
            etag: None,
        };
        state.add_upload_object(&object).unwrap();
        let descriptor = MultipartObject {
            key: object.key.clone(),
            upload_id: "multipart-streaming-fixture".into(),
            part_size: object.size,
            kind: Some("dicom_archive".into()),
            series_archive_id: Some("streaming-fixture".into()),
        };
        let config = crate::config::ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();
        let uploader = MultipartUploader::new_dicom(api, state, worker_upload_id.into()).unwrap();
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let upload = tokio::spawn(async move {
            uploader
                .upload_object(&object, &descriptor, &progress_tx)
                .await
        });

        let mut transferred = Vec::new();
        while transferred.iter().sum::<u64>() < bytes.len() as u64 {
            let update = tokio::time::timeout(Duration::from_secs(2), progress_rx.recv())
                .await
                .expect("upload body should report progress before the response")
                .expect("progress channel should remain open");
            if let UploadProgress::Transferred(count) = update {
                transferred.push(count);
            }
        }
        assert!(transferred.len() > 1, "a PUT body must report many chunks");
        assert!(transferred[0] < bytes.len() as u64);
        assert_eq!(transferred.iter().sum::<u64>(), bytes.len() as u64);
        tokio::time::timeout(Duration::from_secs(2), async {
            while received_bytes.load(Ordering::SeqCst) < bytes.len() as u64 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("local fixture should receive the complete request body");
        assert_eq!(received_bytes.load(Ordering::SeqCst), bytes.len() as u64);
        assert!(
            !upload.is_finished(),
            "progress must arrive while the HTTP PUT is still awaiting its response"
        );

        release_first_response.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), upload)
            .await
            .expect("retry should complete")
            .unwrap()
            .unwrap();
        while let Ok(update) = progress_rx.try_recv() {
            if let UploadProgress::Transferred(count) = update {
                transferred.push(count);
            }
        }
        assert_eq!(
            transferred.iter().sum::<u64>(),
            bytes.len() as u64,
            "retries must not double-count a part"
        );
        assert_eq!(put_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            received_bytes.load(Ordering::SeqCst),
            bytes.len() as u64 * 2
        );
        server.abort();
    }

    #[tokio::test]
    async fn dicom_archives_upload_concurrently_but_parts_for_each_key_stay_sequential() {
        use axum::routing::post;
        use std::{collections::HashMap, sync::Mutex};

        let origin = Arc::new(Mutex::new(String::new()));
        let total_active = Arc::new(AtomicUsize::new(0));
        let total_max = Arc::new(AtomicUsize::new(0));
        let active_by_key = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let max_by_key = Arc::new(Mutex::new(HashMap::<String, usize>::new()));

        let grant_origin = Arc::clone(&origin);
        let grant_app = post(move |Json(request): Json<serde_json::Value>| {
            let origin = Arc::clone(&grant_origin);
            async move {
                let key = request["key"].as_str().unwrap();
                let part_number = request["part_number"].as_u64().unwrap();
                let size = request["size"].as_u64().unwrap();
                let sha256 = request["sha256"].as_str().unwrap();
                let multipart_id = format!("multipart-{}", key.rsplit('/').next().unwrap());
                let signed_at = chrono::Utc::now();
                let origin = origin.lock().unwrap().clone();
                Json(serde_json::json!({
                    "url": format!(
                        "{origin}/bucket/{key}?partNumber={part_number}&uploadId={multipart_id}&X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=ACCESSKEY%2F{}%2Fauto%2Fs3%2Faws4_request&X-Amz-Date={}&X-Amz-Expires=900&X-Amz-SignedHeaders=content-length%3Bhost%3Bx-amz-content-sha256&X-Amz-Signature={}",
                        signed_at.format("%Y%m%d"),
                        signed_at.format("%Y%m%dT%H%M%SZ"),
                        "f".repeat(64),
                    ),
                    "headers": {
                        "content-length": size.to_string(),
                        "x-amz-content-sha256": sha256,
                    },
                    "expires_at": (signed_at + chrono::Duration::minutes(15)).to_rfc3339(),
                }))
            }
        });

        let put_total_active = Arc::clone(&total_active);
        let put_total_max = Arc::clone(&total_max);
        let put_active_by_key = Arc::clone(&active_by_key);
        let put_max_by_key = Arc::clone(&max_by_key);
        let put_app = put(move |AxumPath(key): AxumPath<String>| {
            let total_active = Arc::clone(&put_total_active);
            let total_max = Arc::clone(&put_total_max);
            let active_by_key = Arc::clone(&put_active_by_key);
            let max_by_key = Arc::clone(&put_max_by_key);
            async move {
                let now_active = total_active.fetch_add(1, Ordering::SeqCst) + 1;
                total_max.fetch_max(now_active, Ordering::SeqCst);
                {
                    let mut active = active_by_key.lock().unwrap();
                    let count = active.entry(key.clone()).or_default();
                    *count += 1;
                    let mut maxima = max_by_key.lock().unwrap();
                    maxima
                        .entry(key.clone())
                        .and_modify(|maximum| *maximum = (*maximum).max(*count))
                        .or_insert(*count);
                }
                tokio::time::sleep(Duration::from_millis(75)).await;
                {
                    let mut active = active_by_key.lock().unwrap();
                    *active.get_mut(&key).unwrap() -= 1;
                }
                total_active.fetch_sub(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [(reqwest::header::ETAG.as_str(), format!("\"etag-{key}\""))],
                )
            }
        });

        let app = Router::new()
            .route("/v1/dicom-uploads/{upload_id}/parts", grant_app)
            .route("/bucket/{*key}", put_app);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        *origin.lock().unwrap() = format!("http://{address}");
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let root = tempfile::tempdir().unwrap();
        let state = StateStore::open(&root.path().join("state.sqlite3")).unwrap();
        state.create_run("run", root.path(), false).unwrap();
        let worker_upload_id = "11111111-1111-4111-8111-111111111111";
        let mut objects = Vec::new();
        let mut descriptors = Vec::new();
        for name in ["a.bin", "b.bin", "c.bin"] {
            let path = root.path().join(name);
            std::fs::write(&path, b"abcdefgh").unwrap();
            let key = format!("prefix/{name}");
            let object = UploadObjectRecord {
                run_id: "run".into(),
                worker_upload_id: worker_upload_id.into(),
                key: key.clone(),
                local_path: path.to_string_lossy().into_owned(),
                size: 8,
                sha256: hex::encode(Sha256::digest(b"abcdefgh")),
                multipart_id: None,
                status: "pending".into(),
                etag: None,
            };
            state.add_upload_object(&object).unwrap();
            objects.push(object);
            descriptors.push(MultipartObject {
                key,
                upload_id: format!("multipart-{name}"),
                part_size: 4,
                kind: Some("dicom_archive".into()),
                series_archive_id: Some(name.repeat(4)),
            });
        }
        let config = crate::config::ClientConfig {
            api_url: format!("http://{address}"),
            device_token: "sn_device_test".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "policy".into(),
            pseudonym_key_b64: "fixture".into(),
        };
        let api = IngestApi::from_config(&config).unwrap();
        let uploader = MultipartUploader::new_dicom(api, state, worker_upload_id.into()).unwrap();
        let completed = uploader.upload_all(&objects, &descriptors).await.unwrap();

        assert_eq!(completed.len(), 3);
        assert!(
            total_max.load(Ordering::SeqCst) >= 2,
            "different archive keys should overlap in flight"
        );
        assert!(
            max_by_key
                .lock()
                .unwrap()
                .values()
                .all(|maximum| *maximum == 1),
            "parts for one R2 object key must remain sequential"
        );
        server.abort();
    }
}
