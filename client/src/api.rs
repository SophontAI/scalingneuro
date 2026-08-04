use anyhow::{Context, Result};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;

use crate::{CLIENT_VERSION, config::ClientConfig, model::ManifestBundle};

#[derive(Debug, thiserror::Error)]
#[error("ingest API request failed ({code}): {message}")]
pub struct ApiFailure {
    pub code: String,
    pub status: u16,
    pub message: String,
    pub request_id: Option<String>,
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

pub fn is_not_found_api_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiFailure>()
        .is_some_and(|failure| failure.status == 404)
}

pub fn is_transient_api_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiFailure>()
        .is_some_and(ApiFailure::is_retryable)
        || error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
            error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
        })
}

#[derive(Clone)]
pub struct IngestApi {
    client: Client,
    base_url: String,
    device_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContributionInfo {
    pub registration_open: bool,
    pub project_name: String,
    pub consent_policy_version: String,
    pub policy_url: String,
    pub self_service_quota_bytes: Option<u64>,
    pub minimum_client_version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegisterRequest {
    pub registration_id: String,
    pub device_token: String,
    pub device_name: String,
    pub client_version: String,
    pub platform: String,
    pub contact_email: String,
    pub contact_name: String,
    pub institution_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub institution_ror_id: Option<String>,
    pub lab_name: String,
    pub contact_opt_in: bool,
    pub accepted_consent_policy_version: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AcceptDevicePolicyRequest {
    pub accepted_consent_policy_version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AcceptDevicePolicyResponse {
    pub status: String,
    pub device_id: String,
    pub site_id: String,
    pub project_id: String,
    #[serde(default)]
    pub project_name: Option<String>,
    pub consent_policy_version: String,
}

#[derive(Clone, Deserialize)]
pub struct RegistrationResponse {
    #[serde(alias = "registrationId")]
    pub registration_id: String,
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
pub struct CreateDicomUploadRequest {
    pub format: &'static str,
    pub client_version: String,
    pub deidentification: DicomDeidentificationRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment_description: Option<String>,
    pub series: Vec<DicomSeriesUploadRequest>,
}

#[derive(Debug, Serialize)]
pub struct DicomDeidentificationRequest {
    pub policy_id: String,
    pub policy_version: String,
}

#[derive(Debug, Serialize)]
pub struct DicomSeriesUploadRequest {
    pub series_archive_id: String,
    pub series_id: String,
    pub subject_id: String,
    pub session_id: String,
    pub protocol_group_id: String,
    pub series_kind: String,
    pub archive_route: String,
    pub pixel_data_policy: String,
    pub dicom_count: u64,
    pub archive: DicomArchiveUploadRequest,
}

#[derive(Debug, Serialize)]
pub struct DicomArchiveUploadRequest {
    pub relative_key: String,
    pub size: u64,
    pub sha256: String,
    pub format: String,
}

#[derive(Clone, Deserialize)]
pub struct CreateUploadResponse {
    #[serde(alias = "uploadId")]
    pub upload_id: String,
    pub status: String,
    #[serde(default, alias = "objectPrefix")]
    pub object_prefix: String,
    #[serde(default, alias = "multipartObjects")]
    pub multipart_objects: Vec<MultipartObject>,
    #[serde(default, alias = "alreadyReceivedSeries")]
    pub already_received_series: Vec<AlreadyReceivedSeries>,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct AlreadyReceivedSeries {
    #[serde(alias = "seriesArchiveId")]
    pub series_archive_id: String,
    #[serde(alias = "receiptUploadId")]
    pub receipt_upload_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MultipartObject {
    pub key: String,
    #[serde(alias = "uploadId")]
    pub upload_id: String,
    #[serde(alias = "partSize")]
    pub part_size: u64,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default, alias = "seriesArchiveId")]
    pub series_archive_id: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub receipt: Option<ReceiptProgress>,
    #[serde(default, alias = "alreadyReceivedSeries")]
    pub already_received_series: Vec<AlreadyReceivedSeries>,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReceiptProgress {
    pub received_series: u32,
    pub received_bytes: u64,
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

    pub async fn contribution_info(&self) -> Result<ContributionInfo> {
        self.send_idempotent(|| Ok(self.client.get(self.url("/v1/contribution"))))
            .await
    }

    pub async fn register(&self, request: &RegisterRequest) -> Result<RegistrationResponse> {
        self.send_idempotent(|| Ok(self.client.post(self.url("/v1/register")).json(request)))
            .await
    }

    pub async fn accept_device_policy(
        &self,
        accepted_consent_policy_version: &str,
    ) -> Result<AcceptDevicePolicyResponse> {
        let request = AcceptDevicePolicyRequest {
            accepted_consent_policy_version: accepted_consent_policy_version.into(),
        };
        self.send_idempotent(|| {
            Ok(self
                .authorized(self.client.post(self.url("/v1/device/policy")))?
                .json(&request))
        })
        .await
    }

    pub async fn create_dicom_upload(
        &self,
        bundles: &[ManifestBundle],
        client_version: &str,
        policy_id: &str,
        policy_version: &str,
        experiment_description: Option<&str>,
    ) -> Result<CreateUploadResponse> {
        let mut series = Vec::with_capacity(bundles.len());
        for bundle in bundles {
            let archive = bundle
                .archive
                .as_ref()
                .context("DICOM receipt request requires one series archive")?;
            if !bundle.is_dicom_archive() {
                anyhow::bail!("DICOM receipt request mixed incompatible archive formats");
            }
            if !crate::archive::supported_series_kind(&bundle.series_kind)
                || bundle.classification.decision != crate::model::ClassificationDecision::Accepted
                || bundle.classification.kind != bundle.series_kind
                || bundle.archive_route
                    != crate::archive::archive_route_for_kind(&bundle.series_kind)
                || bundle.pixel_data_policy
                    != crate::archive::SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY
            {
                anyhow::bail!("DICOM receipt request has an invalid functional EPI contract");
            }
            series.push(DicomSeriesUploadRequest {
                series_archive_id: bundle.bundle_id.clone(),
                series_id: bundle.series_id.clone(),
                subject_id: bundle.subject_id.clone(),
                session_id: bundle.session_id.clone(),
                protocol_group_id: bundle.protocol_group_id.clone(),
                series_kind: bundle.series_kind.clone(),
                archive_route: bundle.archive_route.clone(),
                pixel_data_policy: bundle.pixel_data_policy.clone(),
                dicom_count: archive.dicom_instance_count,
                archive: DicomArchiveUploadRequest {
                    relative_key: archive.object.relative_key.clone(),
                    size: archive.object.size,
                    sha256: archive.object.sha256.clone(),
                    format: archive.format.clone(),
                },
            });
        }
        let request = CreateDicomUploadRequest {
            format: "dicom-series-v1",
            client_version: client_version.into(),
            deidentification: DicomDeidentificationRequest {
                policy_id: policy_id.into(),
                policy_version: policy_version.into(),
            },
            experiment_description: experiment_description.map(str::to_owned),
            series,
        };
        self.send_idempotent(|| {
            Ok(self
                .authorized(self.client.post(self.url("/v1/dicom-uploads")))?
                .json(&request))
        })
        .await
    }

    pub async fn refresh_dicom_credentials(&self, upload_id: &str) -> Result<RefreshedUpload> {
        #[derive(Deserialize)]
        struct Wrapper {
            upload_id: String,
            #[serde(default)]
            object_prefix: String,
            #[serde(default)]
            multipart_objects: Vec<MultipartObject>,
            #[serde(default)]
            status: Option<String>,
            #[serde(default)]
            already_received_series: Vec<AlreadyReceivedSeries>,
        }
        let wrapper: Wrapper = self
            .send_idempotent(|| {
                self.authorized(
                    self.client
                        .post(self.url(&format!("/v1/dicom-uploads/{upload_id}/credentials"))),
                )
            })
            .await?;
        Ok(RefreshedUpload {
            upload_id: wrapper.upload_id,
            object_prefix: wrapper.object_prefix,
            multipart_objects: wrapper.multipart_objects,
            status: wrapper.status,
            already_received_series: wrapper.already_received_series,
        })
    }

    pub async fn create_dicom_part_upload(
        &self,
        upload_id: &str,
        request: PartUploadRequest,
    ) -> Result<PartUploadGrant> {
        let response = self
            .authorized(
                self.client
                    .post(self.url(&format!("/v1/dicom-uploads/{upload_id}/parts"))),
            )?
            .json(&request)
            .send()
            .await?;
        decode(response).await
    }

    pub async fn complete_dicom_upload(
        &self,
        upload_id: &str,
        objects: Vec<CompletedObject>,
    ) -> Result<UploadStatus> {
        let request = CompleteUploadRequest { objects };
        self.send_idempotent(|| {
            Ok(self
                .authorized(
                    self.client
                        .post(self.url(&format!("{}/{upload_id}/complete", "/v1/dicom-uploads"))),
                )?
                .timeout(Duration::from_secs(60))
                .json(&request))
        })
        .await
    }

    pub async fn checkpoint_dicom_upload(
        &self,
        upload_id: &str,
        objects: Vec<CompletedObject>,
    ) -> Result<UploadStatus> {
        let request = CompleteUploadRequest { objects };
        self.send_idempotent(|| {
            Ok(self
                .authorized(
                    self.client
                        .post(self.url(&format!("{}/{upload_id}/checkpoint", "/v1/dicom-uploads"))),
                )?
                .timeout(Duration::from_secs(60))
                .json(&request))
        })
        .await
    }

    pub async fn dicom_status(&self, upload_id: &str) -> Result<UploadStatus> {
        self.send_idempotent(|| {
            self.authorized(
                self.client
                    .get(self.url(&format!("/v1/dicom-uploads/{upload_id}"))),
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
    pub already_received_series: Vec<AlreadyReceivedSeries>,
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let (code, message, request_id) = error_details(&bytes);
        return Err(ApiFailure {
            code: code.unwrap_or_else(|| format!("http_{}", status.as_u16())),
            status: status.as_u16(),
            message: message.unwrap_or_else(|| "request was rejected".into()),
            request_id,
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

fn error_details(bytes: &[u8]) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return (None, None, None);
    };
    let error = json.get("error").unwrap_or(&json);
    let string = |field: &str| {
        error
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    (string("code"), string("message"), string("request_id"))
}

fn is_loopback_http(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
}
