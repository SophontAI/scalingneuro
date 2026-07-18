use std::{collections::HashMap, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
};

use crate::{
    api::{
        ApiFailure, CompletedObject, CompletedPart, IngestApi, MultipartObject, PartUploadGrant,
        PartUploadRequest,
    },
    state::{StateStore, UploadObjectRecord, UploadedPart},
};

const MAX_PARTS: u64 = 10_000;

#[derive(Clone)]
pub struct MultipartUploader {
    http: Client,
    api: IngestApi,
    state: StateStore,
    worker_upload_id: String,
}

impl MultipartUploader {
    pub fn new(api: IngestApi, state: StateStore, worker_upload_id: String) -> Result<Self> {
        let http = build_part_client()?;
        Ok(Self {
            http,
            api,
            state,
            worker_upload_id,
        })
    }

    pub async fn upload_all(
        &self,
        objects: &[UploadObjectRecord],
        descriptors: &[MultipartObject],
    ) -> Result<Vec<CompletedObject>> {
        let descriptor_map: HashMap<_, _> = descriptors
            .iter()
            .map(|item| (item.key.as_str(), item))
            .collect();
        if descriptor_map.len() != objects.len() {
            bail!("ingest API returned an incomplete multipart object plan");
        }
        let mut completed = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            let descriptor = descriptor_map
                .get(object.key.as_str())
                .context("ingest API omitted an expected object from the multipart plan")?;
            tracing::info!(
                file = index + 1,
                total_files = objects.len(),
                bytes = object.size,
                "Uploading prepared file"
            );
            completed.push(self.upload_object(object, descriptor).await?);
            tracing::info!(
                file = index + 1,
                total_files = objects.len(),
                "Prepared file upload complete"
            );
        }
        Ok(completed)
    }

    async fn upload_object(
        &self,
        object: &UploadObjectRecord,
        descriptor: &MultipartObject,
    ) -> Result<CompletedObject> {
        if descriptor.part_size == 0 {
            bail!("ingest API returned an invalid zero multipart size");
        }
        let expected_part_count = object.size.div_ceil(descriptor.part_size).max(1);
        if expected_part_count > MAX_PARTS {
            bail!("object requires more than {MAX_PARTS} multipart parts");
        }
        verify_local_object(Path::new(&object.local_path), object.size, &object.sha256).await?;

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
        if same_multipart && valid_complete_part_set(&persisted, object.size, descriptor.part_size)
        {
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
        let uploaded = stream::iter(missing)
            .map(move |part_number| {
                let uploader = uploader.clone();
                let object = object_for_tasks.clone();
                let descriptor = descriptor.clone();
                async move {
                    uploader
                        .upload_part_with_retry(&object, &descriptor, part_number, part_count)
                        .await
                }
            })
            // R2 applies a one-write-per-second-per-key limit. Keep parts for
            // one object sequential; retryable 429s receive a fresh grant.
            .buffer_unordered(1)
            .collect::<Vec<Result<UploadedPart>>>()
            .await;
        for part in uploaded {
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
    ) -> Result<UploadedPart> {
        let offset = u64::from(part_number - 1) * descriptor.part_size;
        let size = (object.size - offset).min(descriptor.part_size);
        let body = read_file_part(Path::new(&object.local_path), offset, size).await?;
        let sha256 = hex::encode(Sha256::digest(&body));
        let mut last_error = None;

        for attempt in 0..5_u32 {
            let grant = match self
                .api
                .create_part_upload(
                    &self.worker_upload_id,
                    PartUploadRequest {
                        key: object.key.clone(),
                        part_number,
                        size,
                        sha256: sha256.clone(),
                    },
                )
                .await
            {
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
                .body(body.clone())
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
                    tracing::info!(
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

async fn verify_local_object(path: &Path, expected_size: u64, expected_sha256: &str) -> Result<()> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("prepared bundle object is missing: {}", path.display()))?;
    if file.metadata().await?.len() != expected_size {
        bail!("prepared bundle object size changed before upload");
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if hex::encode(digest.finalize()) != expected_sha256 {
        bail!("prepared bundle object hash changed before upload");
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
        Router,
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
}
