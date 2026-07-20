use anyhow::{Context, Result};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::time::Duration;

use crate::{
    CLIENT_VERSION,
    config::ClientConfig,
    model::{ExistingArchiveBundle, ManifestBundle},
};

#[derive(Debug, thiserror::Error)]
#[error("ingest API request failed ({code}): {message}")]
pub struct ApiFailure {
    pub code: String,
    pub status: u16,
    pub message: String,
    pub request_id: Option<String>,
    retry_after: Option<Duration>,
    duplicate_reason: Option<String>,
    existing_bundles: Vec<ExistingArchiveBundle>,
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

    pub fn exact_existing_bundles(&self) -> Option<&[ExistingArchiveBundle]> {
        (self.code == "DUPLICATE_BUNDLE"
            && self.duplicate_reason.as_deref() == Some("active_exact_match")
            && !self.existing_bundles.is_empty())
        .then_some(self.existing_bundles.as_slice())
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

#[derive(Serialize)]
pub struct EnrollRequest {
    pub invite_code: String,
    pub enrollment_id: String,
    pub device_token: String,
    pub device_name: String,
    pub client_version: String,
    pub platform: String,
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
pub struct CreateDicomUploadRequest {
    pub format: &'static str,
    pub client_version: String,
    pub deidentification: DicomDeidentificationRequest,
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
    pub processing_route: String,
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UploadRoute {
    Legacy,
    Dicom,
}

impl UploadRoute {
    fn base(self) -> &'static str {
        match self {
            Self::Legacy => "/v1/uploads",
            Self::Dicom => "/v1/dicom-uploads",
        }
    }
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
        let nifti = bundle
            .nifti
            .as_ref()
            .expect("legacy upload request requires a NIfTI object");
        let metadata = bundle
            .metadata
            .as_ref()
            .expect("legacy upload request requires a metadata object");
        Self {
            bundle_id: bundle.bundle_id.clone(),
            series_id: bundle.series_id.clone(),
            subject_id: bundle.subject_id.clone(),
            session_id: bundle.session_id.clone(),
            protocol_group_id: bundle.protocol_group_id.clone(),
            nii: UploadObjectRequest {
                relative_key: nifti.relative_key.clone(),
                size: nifti.size,
                sha256: nifti.sha256.clone(),
                uncompressed_sha256: nifti.uncompressed_sha256.clone(),
            },
            metadata: UploadObjectRequest {
                relative_key: metadata.relative_key.clone(),
                size: metadata.size,
                sha256: metadata.sha256.clone(),
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
    pub verification: Option<VerificationProgress>,
    #[serde(default)]
    pub receipt: Option<ReceiptProgress>,
    #[serde(default)]
    pub processing: Option<ProcessingProgress>,
    #[serde(default, alias = "alreadyReceivedSeries")]
    pub already_received_series: Vec<AlreadyReceivedSeries>,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReceiptProgress {
    pub received_series: u32,
    pub received_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessingProgress {
    pub status: String,
    pub queued_series: u32,
    pub processing_series: u32,
    pub processed_series: u32,
    pub failed_series: u32,
    #[serde(default)]
    pub purged_series: u32,
    #[serde(default)]
    pub repairable_series: u32,
    #[serde(default)]
    pub functional_epi_series: u32,
    #[serde(default)]
    pub archive_only_series: u32,
    #[serde(default)]
    pub archive_verified_series: u32,
    pub total_series: u32,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerificationProgress {
    pub verified_series: u32,
    pub total_series: u32,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub finalized_series: Option<u32>,
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

    pub async fn contribution_info(&self) -> Result<ContributionInfo> {
        self.send_idempotent(|| Ok(self.client.get(self.url("/v1/contribution"))))
            .await
    }

    pub async fn register(&self, request: &RegisterRequest) -> Result<EnrollResponse> {
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

    pub async fn create_dicom_upload(
        &self,
        bundles: &[ManifestBundle],
        client_version: &str,
        policy_id: &str,
        policy_version: &str,
    ) -> Result<CreateUploadResponse> {
        let mut series = Vec::with_capacity(bundles.len());
        for bundle in bundles {
            let archive = bundle
                .archive
                .as_ref()
                .context("DICOM receipt request requires one series archive")?;
            if !bundle.is_dicom_archive() {
                anyhow::bail!("DICOM receipt request mixed incompatible bundle formats");
            }
            if !crate::archive::supported_series_kind(&bundle.series_kind)
                || bundle.classification.decision != crate::model::ClassificationDecision::Accepted
                || bundle.classification.kind != bundle.series_kind
                || bundle.processing_route
                    != crate::archive::processing_route_for_kind(&bundle.series_kind)
                || bundle.pixel_data_policy
                    != crate::archive::SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY
            {
                anyhow::bail!("DICOM receipt request has an invalid MR routing contract");
            }
            series.push(DicomSeriesUploadRequest {
                series_archive_id: bundle.bundle_id.clone(),
                series_id: bundle.series_id.clone(),
                subject_id: bundle.subject_id.clone(),
                session_id: bundle.session_id.clone(),
                protocol_group_id: bundle.protocol_group_id.clone(),
                series_kind: bundle.series_kind.clone(),
                processing_route: bundle.processing_route.clone(),
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
            series,
        };
        self.send_idempotent(|| {
            Ok(self
                .authorized(self.client.post(self.url(UploadRoute::Dicom.base())))?
                .json(&request))
        })
        .await
    }

    pub async fn refresh_credentials(&self, upload_id: &str) -> Result<RefreshedUpload> {
        self.refresh_credentials_for(UploadRoute::Legacy, upload_id)
            .await
    }

    pub async fn refresh_dicom_credentials(&self, upload_id: &str) -> Result<RefreshedUpload> {
        self.refresh_credentials_for(UploadRoute::Dicom, upload_id)
            .await
    }

    async fn refresh_credentials_for(
        &self,
        route: UploadRoute,
        upload_id: &str,
    ) -> Result<RefreshedUpload> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(alias = "uploadId")]
            upload_id: String,
            #[serde(default, alias = "objectPrefix")]
            object_prefix: String,
            #[serde(default, alias = "multipartObjects")]
            multipart_objects: Vec<MultipartObject>,
            #[serde(default)]
            status: Option<String>,
            #[serde(default, alias = "alreadyReceivedSeries")]
            already_received_series: Vec<AlreadyReceivedSeries>,
        }
        let wrapper: Wrapper = self
            .send_idempotent(|| {
                self.authorized(
                    self.client
                        .post(self.url(&format!("{}/{upload_id}/credentials", route.base()))),
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

    pub async fn create_part_upload(
        &self,
        upload_id: &str,
        request: PartUploadRequest,
    ) -> Result<PartUploadGrant> {
        self.create_part_upload_for(UploadRoute::Legacy, upload_id, request)
            .await
    }

    pub async fn create_dicom_part_upload(
        &self,
        upload_id: &str,
        request: PartUploadRequest,
    ) -> Result<PartUploadGrant> {
        self.create_part_upload_for(UploadRoute::Dicom, upload_id, request)
            .await
    }

    async fn create_part_upload_for(
        &self,
        route: UploadRoute,
        upload_id: &str,
        request: PartUploadRequest,
    ) -> Result<PartUploadGrant> {
        let response = self
            .authorized(
                self.client
                    .post(self.url(&format!("{}/{upload_id}/parts", route.base()))),
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
        self.complete_upload_for(UploadRoute::Legacy, upload_id, objects, true)
            .await
    }

    pub async fn complete_dicom_upload(
        &self,
        upload_id: &str,
        objects: Vec<CompletedObject>,
    ) -> Result<UploadStatus> {
        let request = CompleteUploadRequest { objects };
        self.send_idempotent(|| {
            Ok(self
                .authorized(self.client.post(self.url(&format!(
                    "{}/{upload_id}/complete",
                    UploadRoute::Dicom.base()
                ))))?
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
                .authorized(self.client.post(self.url(&format!(
                    "{}/{upload_id}/checkpoint",
                    UploadRoute::Dicom.base()
                ))))?
                .timeout(Duration::from_secs(60))
                .json(&request))
        })
        .await
    }

    async fn complete_upload_for(
        &self,
        route: UploadRoute,
        upload_id: &str,
        objects: Vec<CompletedObject>,
        scientific_verification: bool,
    ) -> Result<UploadStatus> {
        let request = CompleteUploadRequest { objects };
        let response = self
            .authorized(
                self.client
                    .post(self.url(&format!("{}/{upload_id}/complete", route.base()))),
            )?
            // A single bounded Worker step may stream and decompress a large
            // NIfTI. Keep the connection alive beyond the server's five-minute
            // CPU window; the outer state machine handles any real timeout.
            .timeout(if scientific_verification {
                Duration::from_secs(10 * 60)
            } else {
                Duration::from_secs(60)
            })
            .json(&request)
            .send()
            .await?;
        decode(response).await
    }

    pub async fn status(&self, upload_id: &str) -> Result<UploadStatus> {
        self.status_for(UploadRoute::Legacy, upload_id).await
    }

    pub async fn dicom_status(&self, upload_id: &str) -> Result<UploadStatus> {
        self.status_for(UploadRoute::Dicom, upload_id).await
    }

    async fn status_for(&self, route: UploadRoute, upload_id: &str) -> Result<UploadStatus> {
        self.send_idempotent(|| {
            self.authorized(
                self.client
                    .get(self.url(&format!("{}/{upload_id}", route.base()))),
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
        let (duplicate_reason, existing_bundles) = duplicate_details(&bytes);
        return Err(ApiFailure {
            code: code.unwrap_or_else(|| format!("http_{}", status.as_u16())),
            status: status.as_u16(),
            message: message.unwrap_or_else(|| "request was rejected".into()),
            request_id,
            retry_after,
            duplicate_reason,
            existing_bundles,
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

fn duplicate_details(bytes: &[u8]) -> (Option<String>, Vec<ExistingArchiveBundle>) {
    let json: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(_) => return (None, Vec::new()),
    };
    let reason = json
        .pointer("/error/details/reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let existing_bundles = json
        .pointer("/error/details/existing_bundles")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    (reason, existing_bundles)
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
        Json, Router,
        http::{HeaderMap, Response as HttpResponse, StatusCode, header::LOCATION},
        routing::{any, get, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn policy_acceptance_response_keeps_older_worker_compatibility() {
        let response: AcceptDevicePolicyResponse = serde_json::from_value(serde_json::json!({
            "status": "accepted",
            "device_id": "device",
            "site_id": "site",
            "project_id": "project",
            "consent_policy_version": "open-mri-1.0.0"
        }))
        .unwrap();
        assert!(response.project_name.is_none());
    }

    #[tokio::test]
    async fn contribution_info_sends_exact_client_user_agent() {
        let app = Router::new().route(
            "/v1/contribution",
            get(|headers: HeaderMap| async move {
                assert_eq!(
                    headers
                        .get(reqwest::header::USER_AGENT)
                        .and_then(|value| value.to_str().ok()),
                    Some(concat!("neuro-sync/", env!("CARGO_PKG_VERSION")))
                );
                Json(serde_json::json!({
                    "registration_open": true,
                    "project_name": "Scaling Neuro public MRI contribution",
                    "consent_policy_version": "open-mri-1.0.0",
                    "policy_url": "https://scalingneuro.com/docs/contribution-policy",
                    "self_service_quota_bytes": null,
                    "minimum_client_version": "0.2.8"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let api = IngestApi::unauthenticated(&format!("http://{address}")).unwrap();
        let contribution = api.contribution_info().await.unwrap();

        assert_eq!(contribution.consent_policy_version, "open-mri-1.0.0");
        server.abort();
    }

    #[test]
    fn worker_payload_never_contains_local_path() {
        let bundle = ManifestBundle {
            bundle_id: "bundle".into(),
            series_id: "series".into(),
            subject_id: "subject".into(),
            session_id: "session".into(),
            protocol_group_id: "abababababababababababab".into(),
            series_kind: "functional_epi".into(),
            processing_route: "functional-epi-v1".into(),
            pixel_data_policy: "scanner-native-not-defaced".into(),
            nifti: Some(ManifestObject {
                relative_key: "bundle/a.nii.gz".into(),
                local_path: "/private/phi/a".into(),
                size: 1,
                sha256: "aa".into(),
                uncompressed_sha256: Some("cc".into()),
            }),
            metadata: Some(ManifestObject {
                relative_key: "bundle/a.json".into(),
                local_path: "/private/phi/b".into(),
                size: 2,
                sha256: "bb".into(),
                uncompressed_sha256: None,
            }),
            archive: None,
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
            message: "invalid".into(),
            request_id: None,
            retry_after: None,
            duplicate_reason: None,
            existing_bundles: Vec::new(),
        };
        assert!(!deterministic.is_retryable());
        let unavailable = ApiFailure {
            code: "STORAGE_UNAVAILABLE".into(),
            status: 503,
            message: "unavailable".into(),
            request_id: None,
            retry_after: Some(Duration::from_secs(2)),
            duplicate_reason: None,
            existing_bundles: Vec::new(),
        };
        assert!(unavailable.is_retryable());
        assert_eq!(unavailable.retry_after(), Some(Duration::from_secs(2)));
        let busy = ApiFailure {
            code: "CONFLICT".into(),
            status: 409,
            message: "busy".into(),
            request_id: None,
            retry_after: None,
            duplicate_reason: None,
            existing_bundles: Vec::new(),
        };
        assert!(busy.is_retryable());
    }

    #[test]
    fn verification_progress_accepts_old_and_phase_aware_worker_responses() {
        let legacy: UploadStatus = serde_json::from_value(serde_json::json!({
            "upload_id": "11111111-1111-4111-8111-111111111111",
            "status": "uploading",
            "verification": {"verified_series": 0, "total_series": 15}
        }))
        .unwrap();
        let legacy = legacy.verification.unwrap();
        assert_eq!(legacy.phase, None);
        assert_eq!(legacy.finalized_series, None);

        let current: UploadStatus = serde_json::from_value(serde_json::json!({
            "upload_id": "11111111-1111-4111-8111-111111111111",
            "status": "uploading",
            "verification": {
                "phase": "validating_scans",
                "finalized_series": 8,
                "verified_series": 4,
                "total_series": 15
            }
        }))
        .unwrap();
        let current = current.verification.unwrap();
        assert_eq!(current.phase.as_deref(), Some("validating_scans"));
        assert_eq!(current.finalized_series, Some(8));
    }

    #[test]
    fn processing_progress_accepts_and_serializes_mixed_route_counters() {
        let status: UploadStatus = serde_json::from_value(serde_json::json!({
            "upload_id": "11111111-1111-4111-8111-111111111111",
            "status": "committed",
            "processing": {
                "status": "processing",
                "queued_series": 0,
                "processing_series": 1,
                "processed_series": 1,
                "failed_series": 0,
                "purged_series": 0,
                "functional_epi_series": 1,
                "archive_only_series": 1,
                "archive_verified_series": 1,
                "total_series": 2,
                "updated_at": "2026-07-19T00:00:00Z"
            }
        }))
        .unwrap();
        let processing = status.processing.unwrap();
        assert_eq!(processing.functional_epi_series, 1);
        assert_eq!(processing.archive_only_series, 1);
        assert_eq!(processing.archive_verified_series, 1);
        let json = serde_json::to_value(processing).unwrap();
        assert_eq!(json["functional_epi_series"], 1);
        assert_eq!(json["archive_only_series"], 1);
        assert_eq!(json["archive_verified_series"], 1);
    }

    #[test]
    fn parses_only_structured_active_duplicate_reconciliation() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "error": {
                "code": "DUPLICATE_BUNDLE",
                "message": "already committed",
                "request_id": "request-123",
                "details": {
                    "reason": "active_exact_match",
                    "existing_bundles": [{
                        "bundle_id": "a".repeat(24),
                        "series_id": "b".repeat(24),
                        "subject_id": "c".repeat(24),
                        "session_id": "d".repeat(24),
                        "protocol_group_id": "e".repeat(24),
                        "upload_id": "11111111-1111-4111-8111-111111111111",
                        "nii_uncompressed_sha256": "f".repeat(64)
                    }]
                }
            }
        }))
        .unwrap();
        let (reason, bundles) = duplicate_details(&bytes);
        assert_eq!(reason.as_deref(), Some("active_exact_match"));
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].bundle_id, "a".repeat(24));
    }

    #[tokio::test]
    async fn api_failure_displays_the_safe_server_message_and_keeps_request_id() {
        let response = reqwest::Response::from(
            HttpResponse::builder()
                .status(409)
                .body(
                    serde_json::to_vec(&serde_json::json!({
                        "error": {
                            "code": "CONFLICT",
                            "message": "Upload verification is already in progress",
                            "request_id": "request-123"
                        }
                    }))
                    .unwrap(),
                )
                .unwrap(),
        );
        let error = decode::<serde_json::Value>(response).await.unwrap_err();
        let failure = error.downcast_ref::<ApiFailure>().unwrap();
        assert_eq!(failure.request_id.as_deref(), Some("request-123"));
        assert_eq!(
            error.to_string(),
            "ingest API request failed (CONFLICT): Upload verification is already in progress"
        );
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
