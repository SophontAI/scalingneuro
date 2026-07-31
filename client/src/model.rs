use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationDecision {
    Accepted,
    Held,
    Excluded,
}

impl ClassificationDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Held => "held",
            Self::Excluded => "excluded",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classification {
    pub decision: ClassificationDecision,
    pub kind: String,
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ClassificationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationEvidence {
    pub code: String,
    pub source: String,
    pub effect: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceMetadata {
    pub dicom_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patient_position: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub software_versions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magnetic_field_strength: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receive_coil_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transmit_coil_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scanning_sequence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sequence_variant: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scan_options: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mr_acquisition_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_type: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acquisition_number: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataPolicy {
    pub policy_id: String,
    pub policy_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QcResult {
    pub passed: bool,
    pub checks: Vec<QcCheck>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QcCheck {
    pub code: String,
    pub status: QcStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QcStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceSummary {
    pub files_seen: u64,
    pub dicom_files: u64,
    pub series_found: u64,
    pub accepted: u64,
    pub held: u64,
    pub excluded: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalManifest {
    pub schema_version: String,
    pub run_id: String,
    pub site_id: String,
    pub project_id: String,
    #[serde(default)]
    pub consent_policy_version: String,
    pub client_version: String,
    #[serde(default)]
    pub classifier_contract_version: String,
    #[serde(default)]
    pub archive_contract_version: String,
    #[serde(default)]
    pub metadata_policy: MetadataPolicy,
    pub created_at: String,
    pub source_summary: SourceSummary,
    pub bundles: Vec<ManifestBundle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestBundle {
    pub bundle_id: String,
    pub series_id: String,
    pub subject_id: String,
    pub session_id: String,
    pub protocol_group_id: String,
    #[serde(default)]
    pub series_kind: String,
    #[serde(default)]
    pub archive_route: String,
    #[serde(default)]
    pub pixel_data_policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ManifestArchiveObject>,
    pub source_dicom_count: u64,
    pub classification: Classification,
    pub qc: QcResult,
}

impl ManifestBundle {
    pub fn is_dicom_archive(&self) -> bool {
        self.archive.is_some()
    }

    pub fn upload_objects(&self) -> Vec<&ManifestObject> {
        self.archive
            .as_ref()
            .map(|archive| vec![&archive.object])
            .unwrap_or_default()
    }

    pub fn total_size(&self) -> u64 {
        self.upload_objects()
            .into_iter()
            .map(|object| object.size)
            .sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestArchiveObject {
    #[serde(flatten)]
    pub object: ManifestObject,
    pub format: String,
    pub dicom_instance_count: u64,
    pub deidentification_profile: String,
    pub deidentification_profile_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestObject {
    pub relative_key: String,
    pub local_path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportBundle {
    pub bundle_id: String,
    pub series_id: String,
    pub subject_id: String,
    pub session_id: String,
    pub protocol_group_id: String,
    #[serde(default)]
    pub series_kind: String,
    #[serde(default)]
    pub archive_route: String,
    #[serde(default)]
    pub pixel_data_policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ReportArchiveObject>,
    pub source_dicom_count: u64,
    pub classification: Classification,
    pub qc: QcResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportArchiveObject {
    #[serde(flatten)]
    pub object: ReportObject,
    pub format: String,
    pub dicom_instance_count: u64,
    pub deidentification_profile: String,
    pub deidentification_profile_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportObject {
    pub relative_key: String,
    pub size: u64,
    pub sha256: String,
}

impl From<&ManifestObject> for ReportObject {
    fn from(object: &ManifestObject) -> Self {
        Self {
            relative_key: object.relative_key.clone(),
            size: object.size,
            sha256: object.sha256.clone(),
        }
    }
}

impl From<&ManifestBundle> for ReportBundle {
    fn from(bundle: &ManifestBundle) -> Self {
        Self {
            bundle_id: bundle.bundle_id.clone(),
            series_id: bundle.series_id.clone(),
            subject_id: bundle.subject_id.clone(),
            session_id: bundle.session_id.clone(),
            protocol_group_id: bundle.protocol_group_id.clone(),
            series_kind: bundle.series_kind.clone(),
            archive_route: bundle.archive_route.clone(),
            pixel_data_policy: bundle.pixel_data_policy.clone(),
            archive: bundle.archive.as_ref().map(|archive| ReportArchiveObject {
                object: ReportObject::from(&archive.object),
                format: archive.format.clone(),
                dicom_instance_count: archive.dicom_instance_count,
                deidentification_profile: archive.deidentification_profile.clone(),
                deidentification_profile_version: archive.deidentification_profile_version.clone(),
            }),
            source_dicom_count: bundle.source_dicom_count,
            classification: bundle.classification.clone(),
            qc: bundle.qc.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub run_id: String,
    pub status: String,
    pub site_id: String,
    pub project_id: String,
    pub project_name: String,
    #[serde(default)]
    pub consent_policy_version: String,
    #[serde(default)]
    pub client_version: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub source_summary: SourceSummary,
    pub bundles: Vec<ReportBundle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held_series: Vec<HeldSeries>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_upload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worker_upload_ids: Vec<String>,
    #[serde(default)]
    pub archive_commit_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldSeries {
    pub series_id: String,
    pub dicom_count: u64,
    pub reason_code: String,
    pub evidence: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shareable_report_objects_never_serialize_local_paths() {
        let manifest_bundle = ManifestBundle {
            bundle_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
            series_id: "bbbbbbbbbbbbbbbbbbbbbbbb".into(),
            subject_id: "cccccccccccccccccccccccc".into(),
            session_id: "dddddddddddddddddddddddd".into(),
            protocol_group_id: "eeeeeeeeeeeeeeeeeeeeeeee".into(),
            series_kind: "functional_epi".into(),
            archive_route: "functional-epi-v1".into(),
            pixel_data_policy: "scanner-native-not-defaced".into(),
            archive: Some(ManifestArchiveObject {
                object: ManifestObject {
                    relative_key: "bundle/dicom.tar.zst".into(),
                    local_path: "/private/source/workspace/dicom.tar.zst".into(),
                    size: 12,
                    sha256: "a".repeat(64),
                },
                format: "dicom-tar-zstd".into(),
                dicom_instance_count: 10,
                deidentification_profile: "scaling-neuro.dicom-deidentification".into(),
                deidentification_profile_version: "2.0.0".into(),
            }),
            source_dicom_count: 10,
            classification: Classification {
                decision: ClassificationDecision::Accepted,
                kind: "functional_epi".into(),
                confidence: 1.0,
                evidence: Vec::new(),
            },
            qc: QcResult {
                passed: true,
                checks: vec![QcCheck {
                    code: "privacy_gate".into(),
                    status: QcStatus::Pass,
                }],
                warnings: Vec::new(),
            },
        };
        let report = RunReport {
            run_id: "run".into(),
            status: "complete".into(),
            site_id: "site".into(),
            project_id: "project".into(),
            project_name: "Project".into(),
            consent_policy_version: "open-epi-3.0.0".into(),
            client_version: crate::CLIENT_VERSION.into(),
            started_at: "2026-07-12T00:00:00Z".into(),
            completed_at: Some("2026-07-12T00:01:00Z".into()),
            source_summary: SourceSummary::default(),
            bundles: vec![ReportBundle::from(&manifest_bundle)],
            held_series: Vec::new(),
            errors: Vec::new(),
            worker_upload_id: None,
            worker_upload_ids: Vec::new(),
            archive_commit_count: 1,
        };
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("local_path"));
        assert!(!serialized.contains("source_path"));
        assert!(!serialized.contains("/private/source"));
    }
}
