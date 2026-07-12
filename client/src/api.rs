use anyhow::{Context, Result};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;

use crate::{CLIENT_VERSION, config::ClientConfig, model::ManifestBundle};

#[derive(Debug, thiserror::Error)]
#[error("ingest API request failed ({code})")]
pub struct ApiFailure {
    pub code: String,
    pub status: u16,
    retry_after: Option<Duration>,
}

impl ApiFailure {
    pub fn is_retryable(&self) -> bool {
        matches!(self.status, 408 | 425 | 429 | 500..=599)
            || matches!(
                self.code.as_str(),
                "CREDENTIALS_UNAVAILABLE" | "STORAGE_UNAVAILABLE" | "CONFLICT"
            )
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
}

pub fn has_error_code(error: &anyhow::Error, expected: &str) -> bool {
    error
        .downcast_ref::<ApiFailure>()
        .is_some_and(|failure| failure.code == expected)
}

#[derive(Clone)]
pub struct IngestApi {
    client: Client,
    base_url: String,
    device_token: Option<String>,
}

#[derive(Serialize)]
pub struct EnrollRequest {
    pub invite_code: String,
    pub enrollment_id: String,
    pub device_token: String,
    pub device_name: String,
    pub client_version: String,
    pub platform: String,
}

#[derive(Clone, Deserialize)]
pub struct EnrollResponse {
    #[serde(alias = "enrollmentId")]
    pub enrollment_id: String,
    #[serde(alias = "deviceToken")]
    pub device_token: String,
    #[serde(alias = "siteId")]
    pub site_id: String,
    #[serde(alias = "projectId")]
    pub project_id: String,
    #[serde(alias = "projectName")]
    pub project_name: String,
    #[serde(alias = "consentPolicyVersion")]
    pub consent_policy_version: String,
    #[serde(alias = "pseudonymKeyB64")]
    pub pseudonym_key_b64: String,
}

#[derive(Debug, Serialize)]
pub struct CreateUploadRequest {
    pub bundles: Vec<UploadBundleRequest>,
    pub client_version: String,
}

#[derive(Debug, Serialize)]
pub struct UploadBundleRequest {
    pub bundle_id: String,
    pub series_id: String,
    pub subject_id: String,
    pub session_id: String,
    pub protocol_group_id: String,
    pub nii: UploadObjectRequest,
    pub metadata: UploadObjectRequest,
}

#[derive(Debug, Serialize)]
pub struct UploadObjectRequest {
    pub relative_key: String,
    pub size: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncompressed_sha256: Option<String>,
}

impl From<&ManifestBundle> for UploadBundleRequest {
    fn from(bundle: &ManifestBundle) -> Self {
        Self {
            bundle_id: bundle.bundle_id.clone(),
            series_id: bundle.series_id.clone(),
            subject_id: bundle.subject_id.clone(),
            session_id: bundle.session_id.clone(),
            protocol_group_id: bundle.protocol_group_id.clone(),
            nii: UploadObjectRequest {
                relative_key: bundle.nifti.relative_key.clone(),
                size: bundle.nifti.size,
                sha256: bundle.nifti.sha256.clone(),
                uncompressed_sha256: bundle.nifti.uncompressed_sha256.clone(),
            },
            metadata: UploadObjectRequest {
                relative_key: bundle.metadata.relative_key.clone(),
                size: bundle.metadata.size,
                sha256: bundle.metadata.sha256.clone(),
                uncompressed_sha256: None,
            },
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct CreateUploadResponse {
    #[serde(alias = "uploadId")]
    pub upload_id: String,
    pub status: String,
    #[serde(alias = "objectPrefix")]
    pub object_prefix: String,
    #[serde(default, alias = "multipartObjects")]
    pub multipart_objects: Vec<MultipartObject>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MultipartObject {
    pub key: String,
    #[serde(alias = "uploadId")]
    pub upload_id: String,
    #[serde(alias = "partSize")]
    pub part_size: u64,
}

#[derive(Debug, Serialize)]
pub struct PartUploadRequest {
    pub key: String,
    pub part_number: u32,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PartUploadGrant {
    pub url: String,
    pub headers: PartUploadHeaders,
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PartUploadHeaders {
    #[serde(rename = "content-length")]
    pub content_length: String,
    #[serde(rename = "x-amz-content-sha256")]
    pub content_sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompleteUploadRequest {
    pub objects: Vec<CompletedObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedObject {
    pub key: String,
    pub size: u64,
    pub sha256: String,
    pub parts: Vec<CompletedPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedPart {
    pub part_number: u32,
    pub etag: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UploadStatus {
    #[serde(alias = "uploadId")]
    pub upload_id: String,
    pub status: String,
    #[serde(default, alias = "objectPrefix")]
    pub object_prefix: Option<String>,
    #[serde(default)]
    pub objects: Vec<serde_json::Value>,
}

impl IngestApi {
    pub fn unauthenticated(base_url: &str) -> Result<Self> {
        Self::new(base_url, None)
    }

    pub fn from_config(config: &ClientConfig) -> Result<Self> {
        Self::new(&config.api_url, Some(config.device_token.clone()))
    }

    fn new(base_url: &str, device_token: Option<String>) -> Result<Self> {
        let base_url = normalize_base_url(base_url)?;
        let client = Client::builder()
            .https_only(!is_loopback_http(&base_url))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .user_agent(format!("neuro-sync/{CLIENT_VERSION}"))
            .build()?;
        Ok(Self {
            client,
            base_url,
            device_token,
        })
    }

    pub async fn enroll(
        &self,
        invite_code: String,
        enrollment_id: String,
        device_token: String,
        device_name: String,
        client_version: String,
        platform: String,
    ) -> Result<EnrollResponse> {
        let request = EnrollRequest {
            invite_code,
            enrollment_id,
            device_token,
            device_name,
            client_version,
            platform,
        };
        self.send_idempotent(|| Ok(self.client.post(self.url("/v1/enroll")).json(&request)))
            .await
    }

    pub async fn create_upload(
        &self,
        bundles: &[ManifestBundle],
        client_version: &str,
    ) -> Result<CreateUploadResponse> {
        let request = CreateUploadRequest {
            bundles: bundles.iter().map(UploadBundleRequest::from).collect(),
            client_version: client_version.into(),
        };
        self.send_idempotent(|| {
            Ok(self
                .authorized(self.client.post(self.url("/v1/uploads")))?
                .json(&request))
        })
        .await
    }

    pub async fn refresh_credentials(&self, upload_id: &str) -> Result<RefreshedUpload> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(alias = "uploadId")]
            upload_id: String,
            #[serde(alias = "objectPrefix")]
            object_prefix: String,
            #[serde(default, alias = "multipartObjects")]
            multipart_objects: Vec<MultipartObject>,
            #[serde(default)]
            status: Option<String>,
        }
        let wrapper: Wrapper = self
            .send_idempotent(|| {
                self.authorized(
                    self.client
                        .post(self.url(&format!("/v1/uploads/{upload_id}/credentials"))),
                )
            })
            .await?;
        Ok(RefreshedUpload {
            upload_id: wrapper.upload_id,
            object_prefix: wrapper.object_prefix,
            multipart_objects: wrapper.multipart_objects,
            status: wrapper.status,
        })
    }

    pub async fn create_part_upload(
        &self,
        upload_id: &str,
        request: PartUploadRequest,
    ) -> Result<PartUploadGrant> {
        let response = self
            .authorized(
                self.client
                    .post(self.url(&format!("/v1/uploads/{upload_id}/parts"))),
            )?
            .json(&request)
            .send()
            .await?;
        decode(response).await
    }

    pub async fn complete_upload(
        &self,
        upload_id: &str,
        objects: Vec<CompletedObject>,
    ) -> Result<UploadStatus> {
        let request = CompleteUploadRequest { objects };
        self.send_idempotent(|| {
            Ok(self
                .authorized(
                    self.client
                        .post(self.url(&format!("/v1/uploads/{upload_id}/complete"))),
                )?
                .timeout(std::time::Duration::from_secs(30 * 60))
                .json(&request))
        })
        .await
    }

    pub async fn status(&self, upload_id: &str) -> Result<UploadStatus> {
        self.send_idempotent(|| {
            self.authorized(
                self.client
                    .get(self.url(&format!("/v1/uploads/{upload_id}"))),
            )
        })
        .await
    }

    fn authorized(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let token = self
            .device_token
            .as_deref()
            .context("device is not enrolled")?;
        Ok(builder.bearer_auth(token))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn send_idempotent<T, F>(&self, mut build: F) -> Result<T>
    where
        T: DeserializeOwned,
        F: FnMut() -> Result<reqwest::RequestBuilder>,
    {
        let mut last_error = None;
        for attempt in 0..5_u32 {
            let response = match build()?.send().await {
                Ok(response) => response,
                Err(_) => {
                    last_error = Some(anyhow::anyhow!("control_plane_transport_failed"));
                    control_retry_delay(attempt, None).await;
                    continue;
                }
            };
            match decode(response).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let retry_after = if let Some(failure) = error.downcast_ref::<ApiFailure>() {
                        if !failure.is_retryable() {
                            return Err(error);
                        }
                        failure.retry_after()
                    } else {
                        return Err(error);
                    };
                    last_error = Some(error);
                    control_retry_delay(attempt, retry_after).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("control_plane_retry_exhausted")))
    }
}

pub(crate) fn normalize_base_url(value: &str) -> Result<String> {
    let url = url::Url::parse(value.trim()).context("invalid ingest API URL")?;
    let loopback_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !loopback_http {
        anyhow::bail!("ingest API URL must use HTTPS");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        anyhow::bail!("ingest API URL must contain only an origin");
    }
    Ok(url.origin().ascii_serialization())
}

#[derive(Clone)]
pub struct RefreshedUpload {
    pub upload_id: String,
    pub object_prefix: String,
    pub multipart_objects: Vec<MultipartObject>,
    pub status: Option<String>,
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let code = error_code(&bytes).unwrap_or_else(|| format!("http_{}", status.as_u16()));
        return Err(ApiFailure {
            code,
            status: status.as_u16(),
            retry_after,
        }
        .into());
    }
    serde_json::from_slice(&bytes).context("ingest API returned an invalid response")
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    const MAX_RETRY_AFTER_SECONDS: u64 = 30;
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds.min(MAX_RETRY_AFTER_SECONDS)));
    }
    let retry_at = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&chrono::Utc);
    let seconds = (retry_at - chrono::Utc::now()).num_seconds().max(0) as u64;
    Some(Duration::from_secs(seconds.min(MAX_RETRY_AFTER_SECONDS)))
}

async fn control_retry_delay(attempt: u32, retry_after: Option<Duration>) {
    let exponential = Duration::from_millis(250 * 2_u64.pow(attempt));
    tokio::time::sleep(retry_after.unwrap_or(exponential).max(exponential)).await;
}

fn error_code(bytes: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    json.pointer("/error/code")
        .or_else(|| json.get("code"))?
        .as_str()
        .map(str::to_owned)
}

fn is_loopback_http(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Classification, ClassificationDecision, ManifestObject, QcResult};
    use axum::{
        Router,
        http::{StatusCode, header::LOCATION},
        routing::{any, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn worker_payload_never_contains_local_path() {
        let bundle = ManifestBundle {
            bundle_id: "bundle".into(),
            series_id: "series".into(),
            subject_id: "subject".into(),
            session_id: "session".into(),
            protocol_group_id: "abababababababababababab".into(),
            nifti: ManifestObject {
                relative_key: "bundle/a.nii.gz".into(),
                local_path: "/private/phi/a".into(),
                size: 1,
                sha256: "aa".into(),
                uncompressed_sha256: Some("cc".into()),
            },
            metadata: ManifestObject {
                relative_key: "bundle/a.json".into(),
                local_path: "/private/phi/b".into(),
                size: 2,
                sha256: "bb".into(),
                uncompressed_sha256: None,
            },
            source_dicom_count: 1,
            classification: Classification {
                decision: ClassificationDecision::Accepted,
                kind: "functional_epi".into(),
                confidence: 1.0,
                evidence: vec![],
            },
            qc: QcResult {
                passed: true,
                checks: vec![],
                warnings: vec![],
            },
        };
        let json = serde_json::to_string(&UploadBundleRequest::from(&bundle)).unwrap();
        assert!(!json.contains("private"));
        assert!(!json.contains("local_path"));
        assert!(json.contains("\"protocol_group_id\":\"abababababababababababab\""));
        let prepared_version = CreateUploadRequest {
            bundles: vec![UploadBundleRequest::from(&bundle)],
            client_version: "0.0.9".into(),
        };
        let json = serde_json::to_string(&prepared_version).unwrap();
        assert!(json.contains("\"client_version\":\"0.0.9\""));
        assert!(!json.contains(concat!(
            "\"client_version\":\"",
            env!("CARGO_PKG_VERSION"),
            "\""
        )));
    }

    #[test]
    fn api_failures_retry_only_transient_statuses_and_codes() {
        let deterministic = ApiFailure {
            code: "INVALID_REQUEST".into(),
            status: 400,
            retry_after: None,
        };
        assert!(!deterministic.is_retryable());
        let unavailable = ApiFailure {
            code: "STORAGE_UNAVAILABLE".into(),
            status: 503,
            retry_after: Some(Duration::from_secs(2)),
        };
        assert!(unavailable.is_retryable());
        assert_eq!(unavailable.retry_after(), Some(Duration::from_secs(2)));
        let busy = ApiFailure {
            code: "CONFLICT".into(),
            status: 409,
            retry_after: None,
        };
        assert!(busy.is_retryable());
    }

    #[test]
    fn retry_after_is_capped() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "600".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn control_plane_client_never_follows_307_or_308() {
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

            let location = format!("http://{target_address}/credential-capture");
            let redirect_app = Router::new().route(
                "/v1/uploads/{upload_id}/complete",
                post(move || {
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

            let api = IngestApi::new(
                &format!("http://{redirect_address}"),
                Some("sn_device_sensitive".into()),
            )
            .unwrap();
            let error = api
                .complete_upload("fixture", Vec::new())
                .await
                .unwrap_err();
            let failure = error.downcast_ref::<ApiFailure>().unwrap();
            assert_eq!(failure.status, redirect_status.as_u16());
            tokio::task::yield_now().await;
            assert_eq!(target_hits.load(Ordering::SeqCst), 0);

            redirect_server.abort();
            target_server.abort();
        }
    }
}
