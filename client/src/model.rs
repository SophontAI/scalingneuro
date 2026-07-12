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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSidecar {
    pub schema_version: String,
    pub bundle_id: String,
    pub subject_id: String,
    pub session_id: String,
    pub series_id: String,
    pub protocol_group_id: String,
    pub modality: String,
    pub source: SourceMetadata,
    pub image: ImageMetadata,
    pub files: BundleFiles,
    pub metadata_policy: MetadataPolicy,
    pub conversion: ConversionProvenance,
    pub classification: Classification,
    pub qc: QcResult,
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
pub struct ImageMetadata {
    pub dimensions: Vec<u64>,
    pub voxel_size_mm: Vec<f64>,
    pub datatype: String,
    pub bits_per_voxel: u16,
    pub affine: [[f64; 4]; 4],
    pub orientation: String,
    pub volume_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tr_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub te_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inversion_time_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flip_angle_degrees: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice_thickness_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spacing_between_slices_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_bandwidth_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dwell_time_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_echo_spacing_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_readout_time_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_encoding_direction: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slice_timing_seconds: Vec<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acquisition_matrix: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recon_matrix: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiband_acceleration_factor: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_reduction_factor_in_plane: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_fourier: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo_train_length: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number_of_averages: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imaging_frequency_mhz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imaged_nucleus: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFiles {
    pub nifti: FileDigest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataPolicy {
    pub policy_id: String,
    pub policy_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDigest {
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncompressed_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionProvenance {
    pub client_version: String,
    pub converter: String,
    pub converter_version: String,
    pub arguments: Vec<String>,
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
    pub nifti: ManifestObject,
    pub metadata: ManifestObject,
    pub source_dicom_count: u64,
    pub classification: Classification,
    pub qc: QcResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestObject {
    pub relative_key: String,
    pub local_path: String,
    pub size: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncompressed_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportBundle {
    pub bundle_id: String,
    pub series_id: String,
    pub subject_id: String,
    pub session_id: String,
    pub protocol_group_id: String,
    pub nifti: ReportObject,
    pub metadata: ReportObject,
    pub source_dicom_count: u64,
    pub classification: Classification,
    pub qc: QcResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportObject {
    pub relative_key: String,
    pub size: u64,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncompressed_sha256: Option<String>,
}

impl From<&ManifestObject> for ReportObject {
    fn from(object: &ManifestObject) -> Self {
        Self {
            relative_key: object.relative_key.clone(),
            size: object.size,
            sha256: object.sha256.clone(),
            uncompressed_sha256: object.uncompressed_sha256.clone(),
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
            nifti: ReportObject::from(&bundle.nifti),
            metadata: ReportObject::from(&bundle.metadata),
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
    fn published_sidecar_example_roundtrips_through_rust_contract() {
        let original: serde_json::Value = serde_json::from_str(include_str!(
            "../../schemas/examples/scan-sidecar-v1.example.json"
        ))
        .unwrap();
        let sidecar: ScanSidecar = serde_json::from_value(original.clone()).unwrap();
        let roundtrip = serde_json::to_value(sidecar).unwrap();
        assert_eq!(json_shape(&roundtrip), json_shape(&original));
    }

    #[test]
    fn shareable_report_objects_never_serialize_local_paths() {
        let manifest_bundle = ManifestBundle {
            bundle_id: "aaaaaaaaaaaaaaaaaaaaaaaa".into(),
            series_id: "bbbbbbbbbbbbbbbbbbbbbbbb".into(),
            subject_id: "cccccccccccccccccccccccc".into(),
            session_id: "dddddddddddddddddddddddd".into(),
            protocol_group_id: "eeeeeeeeeeeeeeeeeeeeeeee".into(),
            nifti: ManifestObject {
                relative_key: "bundle/scan.nii.gz".into(),
                local_path: "/private/source/workspace/scan.nii.gz".into(),
                size: 12,
                sha256: "a".repeat(64),
                uncompressed_sha256: Some("b".repeat(64)),
            },
            metadata: ManifestObject {
                relative_key: "bundle/scan.json".into(),
                local_path: "/private/source/workspace/scan.json".into(),
                size: 13,
                sha256: "c".repeat(64),
                uncompressed_sha256: None,
            },
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
            consent_policy_version: "pilot-2026-07".into(),
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

    fn json_shape(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => serde_json::Value::Object(
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), json_shape(value)))
                    .collect(),
            ),
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(json_shape).collect())
            }
            serde_json::Value::Null => serde_json::Value::String("null".into()),
            serde_json::Value::Bool(_) => serde_json::Value::String("boolean".into()),
            serde_json::Value::Number(_) => serde_json::Value::String("number".into()),
            serde_json::Value::String(_) => serde_json::Value::String("string".into()),
        }
    }
}
