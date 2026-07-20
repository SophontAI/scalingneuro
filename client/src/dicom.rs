use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use dicom_core::{Tag, VR, header::Header};
use dicom_object::{DefaultDicomObject, OpenFileOptions};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::model::SourceSummary;

const PIXEL_DATA: Tag = Tag(0x7FE0, 0x0010);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
pub const MR_IMAGE_STORAGE_UID: &str = "1.2.840.10008.5.1.4.1.1.4";
pub const ENHANCED_MR_IMAGE_STORAGE_UID: &str = "1.2.840.10008.5.1.4.1.1.4.1";
pub const ENHANCED_MR_COLOR_IMAGE_STORAGE_UID: &str = "1.2.840.10008.5.1.4.1.1.4.3";
pub const LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID: &str = "1.2.840.10008.5.1.4.1.1.4.4";
pub const MAX_DICOM_INSTANCES_PER_SERIES: usize = 500_000;
pub const MAX_DICOM_INSTANCE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub const MAX_DICOM_SERIES_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_DISTINCT_SERIES_HEADER_VALUES: usize = 64;
const MAX_LOCAL_PROTOCOL_TEXT_BYTES: usize = 512;

#[derive(Debug, Default)]
struct SeriesAggregationSets {
    sop_class_uids: HashSet<String>,
    modalities: HashSet<String>,
    manufacturers: HashSet<String>,
    models: HashSet<String>,
    software_versions: HashSet<String>,
    image_types: HashSet<String>,
    scanning_sequences: HashSet<String>,
    sequence_variants: HashSet<String>,
    scan_options: HashSet<String>,
    local_protocol_texts: HashSet<String>,
    burned_in_annotations: HashSet<String>,
}

pub fn dicom_instance_count_supported(count: usize) -> bool {
    count <= MAX_DICOM_INSTANCES_PER_SERIES
}

pub fn dicom_instance_size_supported(size: u64) -> bool {
    size <= MAX_DICOM_INSTANCE_BYTES
}

pub fn dicom_series_uncompressed_size_supported(sizes: impl IntoIterator<Item = u64>) -> bool {
    sizes
        .into_iter()
        .try_fold(0_u64, |total, size| total.checked_add(size))
        .is_some_and(|total| total <= MAX_DICOM_SERIES_UNCOMPRESSED_BYTES)
}

pub fn supported_mr_image_sop_class(value: &str) -> bool {
    matches!(
        value,
        MR_IMAGE_STORAGE_UID
            | ENHANCED_MR_IMAGE_STORAGE_UID
            | LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID
    )
}

#[derive(Debug, Clone, Default)]
pub struct DicomHeader {
    pub path: PathBuf,
    pub patient_id: Option<String>,
    pub issuer_of_patient_id: Option<String>,
    pub study_uid: Option<String>,
    pub series_uid: Option<String>,
    pub sop_class_uid: Option<String>,
    pub sop_instance_uid: Option<String>,
    pub modality: Option<String>,
    pub image_type: Vec<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub patient_position: Option<String>,
    pub software_versions: Option<String>,
    pub magnetic_field_strength: Option<f64>,
    pub series_description: Option<String>,
    pub protocol_name: Option<String>,
    pub sequence_name: Option<String>,
    pub scanning_sequence: Vec<String>,
    pub sequence_variant: Vec<String>,
    pub scan_options: Vec<String>,
    pub mr_acquisition_type: Option<String>,
    pub echo_planar_pulse_sequence: Option<String>,
    pub trigger_time_ms: Option<f64>,
    /// Local-only Philips signal used to suppress a redundant public trigger
    /// field at series scope. This private value is never serialized.
    pub philips_dynamic_scan_begin_time_seconds: Option<f64>,
    /// True when the creator-mapped Philips dynamic timing tag exists, even if
    /// its VR or value is malformed. Presence without a verified full-series
    /// contract must hold the series instead of silently dropping the signal.
    pub philips_dynamic_timing_tag_present: bool,
    pub philips_number_of_slices: Option<i64>,
    /// True when either member of the creator-mapped Philips private pixel
    /// scaling pair is present. Slice count and water-fat shift are independent
    /// optional provenance and do not make the scaling pair mandatory.
    pub philips_private_pixel_scaling_present: bool,
    /// Philips classic ASL may encode per-image label/control state in the
    /// non-standard DD 005 MR Image Label Type field. It is not in our safe
    /// private export allowlist, so its presence is an explicit hold signal.
    pub philips_private_asl_label_type_present: bool,
    /// True only when both private scaling members are present, unique,
    /// correctly typed, and bounded. A malformed/orphan pair can still be
    /// ignored when the public Rescale Intercept/Slope pair is valid.
    pub philips_private_pixel_scaling_usable: bool,
    /// True only when the public Rescale Intercept/Slope pair is a finite,
    /// single-valued quantitative fallback for classic Philips pixels.
    pub public_pixel_scaling_contract_verified: bool,
    /// UIH classic multi-frame GRID/VFRAME evidence and its creator-mapped
    /// slice-count contract are local eligibility signals. Private values are
    /// retained only by the archive sanitizer's separate narrow allowlist.
    pub uih_grid_or_vframe: bool,
    pub uih_grid_slice_count_present: bool,
    pub uih_grid_slice_count_verified: bool,
    pub public_diffusion_metadata_present: bool,
    pub public_diffusion_semantic_evidence: bool,
    pub reviewed_private_diffusion_metadata_present: bool,
    pub reviewed_private_diffusion_semantic_evidence: bool,
    pub diffusion_metadata_contract_verified: bool,
    pub public_asl_metadata_present: bool,
    /// Canonical LABEL/CONTROL/M_ZERO_SCAN values observed in public ASL
    /// macros. Multiple values are retained so private/public disagreement or
    /// an ambiguous multiframe private label cannot collapse to a boolean.
    pub public_asl_contexts: Vec<String>,
    pub reviewed_private_asl_metadata_present: bool,
    /// Canonical Philips DD005 label value (`LBL`/`CTL` aliases normalized).
    pub philips_private_asl_label: Option<String>,
    /// GE A3/A5 are bounded ASL context retained for provenance, but do not
    /// independently encode per-image LABEL/CONTROL state.
    pub ge_asl_supplemental_metadata_present: bool,
    pub asl_metadata_contract_verified: bool,
    pub image_position_patient: Vec<f64>,
    pub series_number: Option<i64>,
    pub acquisition_number: Option<i64>,
    pub instance_number: Option<i64>,
    pub echo_number: Option<i64>,
    pub repetition_time_ms: Option<f64>,
    pub echo_time_ms: Option<f64>,
    pub inversion_time_ms: Option<f64>,
    pub flip_angle_degrees: Option<f64>,
    pub number_of_temporal_positions: Option<i64>,
    pub temporal_position_identifier: Option<i64>,
    pub number_of_frames: Option<i64>,
    pub images_in_acquisition: Option<i64>,
    pub diffusion_b_value: Option<f64>,
    pub asl_technique: Option<String>,
    pub burned_in_annotation: Option<String>,
    /// True only when the Siemens CSA Image Header can be rewritten by the
    /// client's narrow numeric mosaic allowlist.
    pub siemens_csa_image_header_present: bool,
    pub siemens_csa_image_header_sanitizable: bool,
    pub overlay_or_graphics: bool,
    pub has_extended_offset_table: bool,
    pub has_per_frame_functional_groups: bool,
    pub acquisition: BTreeMap<String, Value>,
}

impl DicomHeader {
    pub fn local_protocol_text(&self) -> String {
        [
            self.protocol_name.as_deref(),
            self.series_description.as_deref(),
            self.sequence_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct SeriesGroup {
    pub study_uid: String,
    pub series_uid: String,
    pub representative: DicomHeader,
    pub files: Vec<PathBuf>,
    pub instances: Vec<SeriesInstance>,
    pub duplicate_sop_instance_uid: bool,
    pub inconsistent_subject: bool,
    pub inconsistent_metadata: bool,
    pub manufacturers: Vec<String>,
    pub manufacturer_missing: bool,
    pub models: Vec<String>,
    pub model_missing: bool,
    pub software_version_values: Vec<String>,
    pub software_versions_missing: bool,
    pub sop_class_uids: Vec<String>,
    pub modalities: Vec<String>,
    pub image_types: Vec<String>,
    pub scanning_sequences: Vec<String>,
    pub sequence_variants: Vec<String>,
    pub scan_options: Vec<String>,
    pub local_protocol_texts: Vec<String>,
    pub burned_in_annotations: Vec<String>,
    pub burned_in_annotation_missing: bool,
    pub all_missing_bia_instances_original_primary: bool,
    pub siemens_csa_image_header_present: bool,
    pub all_siemens_csa_image_headers_sanitizable: bool,
    pub philips_dynamic_timing_detected: bool,
    pub philips_dynamic_timing_contract_verified: bool,
    pub philips_private_pixel_scaling_present: bool,
    pub philips_private_pixel_scaling_incomplete: bool,
    pub philips_private_asl_label_type_present: bool,
    /// Once any instance carries private Philips scaling, every instance must
    /// have either a complete private pair or a complete public fallback.
    pub all_philips_pixel_scaling_contracts_verified: bool,
    pub uih_grid_or_vframe: bool,
    pub uih_grid_slice_count_present: bool,
    pub all_uih_grid_slice_counts_verified: bool,
    pub diffusion_metadata_present: bool,
    pub all_diffusion_metadata_contracts_verified: bool,
    pub asl_metadata_present: bool,
    pub all_asl_metadata_contracts_verified: bool,
    pub overlay_or_graphics: bool,
    pub has_extended_offset_table: bool,
    pub temporal_position_identifiers: Vec<i64>,
    pub acquisition_numbers: Vec<i64>,
    pub has_per_frame_functional_groups: bool,
    pub diffusion_context: bool,
    pub asl_context: bool,
}

#[derive(Debug, Clone)]
pub struct SeriesInstance {
    pub path: PathBuf,
    pub instance_number: Option<i64>,
    pub sop_instance_uid: String,
    pub trigger_time_ms: Option<f64>,
    pub philips_dynamic_scan_begin_time_seconds: Option<f64>,
    pub philips_dynamic_timing_tag_present: bool,
    pub temporal_position_identifier: Option<i64>,
    pub number_of_temporal_positions: Option<i64>,
    pub philips_number_of_slices: Option<i64>,
    pub image_position_patient: Vec<f64>,
    pub acquisition_number: Option<i64>,
    pub repetition_time_ms: Option<f64>,
    pub echo_time_ms: Option<f64>,
}

#[derive(Debug)]
pub struct Discovery {
    pub series: Vec<SeriesGroup>,
    pub summary: SourceSummary,
    pub unreadable_dicom_like_files: u64,
    pub source_snapshot: SourceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    files: Vec<FileFingerprint>,
    changed_while_reading: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub sha256: String,
    pub file_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl SourceSnapshot {
    pub fn is_stable_with(&self, later: &Self) -> bool {
        !self.changed_while_reading && !later.changed_while_reading && self.files == later.files
    }

    pub fn fingerprint(&self, root: &Path) -> Result<SourceFingerprint> {
        self.fingerprint_with_progress(root, |_| {})
    }

    pub fn total_bytes(&self) -> Result<u64> {
        self.files.iter().try_fold(0_u64, |total, file| {
            total
                .checked_add(file.size)
                .context("source snapshot byte total overflow")
        })
    }

    /// Compute a folder identity from every source byte, not only mutable
    /// filesystem metadata. Relative paths and file sizes delimit the per-file
    /// SHA-256 values, so equal-size replacements are detected while an mtime-
    /// only change does not cause a scientifically identical export to upload
    /// again.
    pub fn fingerprint_with_progress(
        &self,
        root: &Path,
        mut report: impl FnMut(u64),
    ) -> Result<SourceFingerprint> {
        if self.changed_while_reading {
            bail!("the selected folder changed while its local sync identity was being checked");
        }
        let mut digest = Sha256::new();
        digest.update(b"scaling-neuro-source-snapshot-v2\0");
        digest.update((self.files.len() as u64).to_le_bytes());
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut bytes_hashed = 0_u64;
        for file in &self.files {
            let relative = file.path.strip_prefix(root).with_context(|| {
                format!(
                    "source snapshot entry is outside the selected folder: {}",
                    file.path.display()
                )
            })?;
            let encoded_path = relative.as_os_str().as_encoded_bytes();
            digest.update((encoded_path.len() as u64).to_le_bytes());
            digest.update(encoded_path);
            digest.update(file.size.to_le_bytes());

            let before = file_fingerprint(&file.path)?;
            if &before != file {
                bail!(
                    "the selected folder changed while its local sync identity was being checked"
                );
            }
            let mut input = File::open(&file.path).with_context(|| {
                format!(
                    "could not read source file while checking folder identity: {}",
                    file.path.display()
                )
            })?;
            let mut file_digest = Sha256::new();
            let mut file_bytes = 0_u64;
            loop {
                let read = input.read(&mut buffer).with_context(|| {
                    format!(
                        "could not read source file while checking folder identity: {}",
                        file.path.display()
                    )
                })?;
                if read == 0 {
                    break;
                }
                file_digest.update(&buffer[..read]);
                file_bytes = file_bytes
                    .checked_add(read as u64)
                    .context("source file byte counter overflow")?;
                bytes_hashed = bytes_hashed
                    .checked_add(read as u64)
                    .context("source fingerprint byte counter overflow")?;
                report(bytes_hashed);
            }
            let after = file_fingerprint(&file.path)?;
            if &after != file || file_bytes != file.size {
                bail!(
                    "the selected folder changed while its local sync identity was being checked"
                );
            }
            digest.update(file_digest.finalize());
        }
        report(bytes_hashed);
        Ok(SourceFingerprint {
            sha256: hex::encode(digest.finalize()),
            file_count: self.files.len() as u64,
        })
    }
}

pub fn discover(root: &Path) -> Result<Discovery> {
    discover_with_progress(root, |_| {})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryProgress {
    pub phase: DiscoveryPhase,
    pub files_seen: u64,
    pub total_files: Option<u64>,
    pub dicom_files: u64,
    pub series_found: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryPhase {
    Inventory,
    ReadHeaders,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotProgress {
    pub files_seen: u64,
}

pub fn discover_with_progress(
    root: &Path,
    mut report: impl FnMut(DiscoveryProgress),
) -> Result<Discovery> {
    if !root.is_dir() {
        bail!(
            "DICOM folder does not exist or is not a directory: {}",
            root.display()
        );
    }

    let mut groups: HashMap<(String, String), SeriesGroup> = HashMap::new();
    let mut group_sop_instance_uids: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let mut group_aggregation_sets: HashMap<(String, String), SeriesAggregationSets> =
        HashMap::new();
    let mut summary = SourceSummary::default();
    let mut unreadable_dicom_like_files = 0_u64;
    let mut source_files = Vec::new();
    let mut changed_while_reading = false;
    let inventory = inventory_regular_files(root, &mut report)?;
    let total_files = inventory.len() as u64;
    report(DiscoveryProgress {
        phase: DiscoveryPhase::ReadHeaders,
        files_seen: 0,
        total_files: Some(total_files),
        dicom_files: 0,
        series_found: 0,
    });
    let mut last_progress = Instant::now();

    for path in inventory {
        summary.files_seen += 1;
        let before = file_fingerprint(&path)?;
        let include_in_source_identity;
        match read_header(&path) {
            Ok(header) => {
                include_in_source_identity = true;
                summary.dicom_files += 1;
                let study_uid = header.study_uid.clone().unwrap_or_default();
                let series_uid = header.series_uid.clone().unwrap_or_default();
                // Missing UIDs are intentionally isolated rather than accidentally merged.
                let key = if study_uid.is_empty() || series_uid.is_empty() {
                    (
                        format!("missing-study:{}", summary.dicom_files),
                        format!("missing-series:{}", summary.dicom_files),
                    )
                } else {
                    (study_uid.clone(), series_uid.clone())
                };
                let patient_id = header.patient_id.clone();
                let issuer_of_patient_id = header.issuer_of_patient_id.clone();
                let seen_sop_instance_uids =
                    group_sop_instance_uids.entry(key.clone()).or_default();
                let aggregation_sets = group_aggregation_sets.entry(key.clone()).or_default();
                let group = groups.entry(key).or_insert_with(|| SeriesGroup {
                    study_uid,
                    series_uid,
                    representative: header.clone(),
                    files: Vec::new(),
                    instances: Vec::new(),
                    duplicate_sop_instance_uid: false,
                    inconsistent_subject: false,
                    inconsistent_metadata: false,
                    manufacturers: Vec::new(),
                    manufacturer_missing: false,
                    models: Vec::new(),
                    model_missing: false,
                    software_version_values: Vec::new(),
                    software_versions_missing: false,
                    sop_class_uids: Vec::new(),
                    modalities: Vec::new(),
                    image_types: Vec::new(),
                    scanning_sequences: Vec::new(),
                    sequence_variants: Vec::new(),
                    scan_options: Vec::new(),
                    local_protocol_texts: Vec::new(),
                    burned_in_annotations: Vec::new(),
                    burned_in_annotation_missing: header.burned_in_annotation.is_none(),
                    all_missing_bia_instances_original_primary: header
                        .burned_in_annotation
                        .is_some()
                        || declares_original_primary(&header.image_type),
                    siemens_csa_image_header_present: false,
                    all_siemens_csa_image_headers_sanitizable: header
                        .siemens_csa_image_header_sanitizable,
                    philips_dynamic_timing_detected: false,
                    philips_dynamic_timing_contract_verified: false,
                    philips_private_pixel_scaling_present: header
                        .philips_private_pixel_scaling_present,
                    philips_private_pixel_scaling_incomplete: header
                        .philips_private_pixel_scaling_present
                        && !header.philips_private_pixel_scaling_usable,
                    philips_private_asl_label_type_present: header
                        .philips_private_asl_label_type_present,
                    all_philips_pixel_scaling_contracts_verified: header
                        .philips_private_pixel_scaling_usable
                        || header.public_pixel_scaling_contract_verified,
                    uih_grid_or_vframe: header.uih_grid_or_vframe,
                    uih_grid_slice_count_present: header.uih_grid_slice_count_present,
                    all_uih_grid_slice_counts_verified: !header.uih_grid_or_vframe
                        || header.uih_grid_slice_count_verified,
                    diffusion_metadata_present: header.public_diffusion_metadata_present
                        || header.reviewed_private_diffusion_metadata_present,
                    all_diffusion_metadata_contracts_verified: header
                        .diffusion_metadata_contract_verified,
                    asl_metadata_present: header.public_asl_metadata_present
                        || header.reviewed_private_asl_metadata_present,
                    all_asl_metadata_contracts_verified: header.asl_metadata_contract_verified,
                    overlay_or_graphics: false,
                    has_extended_offset_table: false,
                    temporal_position_identifiers: Vec::new(),
                    acquisition_numbers: Vec::new(),
                    has_per_frame_functional_groups: false,
                    diffusion_context: false,
                    asl_context: false,
                });
                let issuer_conflicts = match (
                    group.representative.issuer_of_patient_id.as_deref(),
                    issuer_of_patient_id.as_deref(),
                ) {
                    (Some(existing), Some(candidate)) => existing != candidate,
                    _ => false,
                };
                if group.representative.patient_id != patient_id || issuer_conflicts {
                    group.inconsistent_subject = true;
                }
                if required_metadata_conflicts(&group.representative, &header) {
                    group.inconsistent_metadata = true;
                }
                extend_unique_bounded(
                    &mut group.sop_class_uids,
                    &mut aggregation_sets.sop_class_uids,
                    header.sop_class_uid.iter().cloned(),
                    "unsupported-overflow",
                );
                extend_unique_bounded(
                    &mut group.modalities,
                    &mut aggregation_sets.modalities,
                    header.modality.iter().cloned(),
                    "unsupported-overflow",
                );
                extend_unique_bounded(
                    &mut group.manufacturers,
                    &mut aggregation_sets.manufacturers,
                    header.manufacturer.iter().cloned(),
                    "unknown-overflow",
                );
                group.manufacturer_missing |= header.manufacturer.is_none();
                extend_unique_bounded(
                    &mut group.models,
                    &mut aggregation_sets.models,
                    header.model.iter().cloned(),
                    "unknown-overflow",
                );
                group.model_missing |= header.model.is_none();
                extend_unique_bounded(
                    &mut group.software_version_values,
                    &mut aggregation_sets.software_versions,
                    header.software_versions.iter().cloned(),
                    "unknown-overflow",
                );
                group.software_versions_missing |= header.software_versions.is_none();
                extend_unique_bounded(
                    &mut group.image_types,
                    &mut aggregation_sets.image_types,
                    header.image_type.iter().cloned(),
                    "DERIVED",
                );
                extend_unique_bounded(
                    &mut group.scanning_sequences,
                    &mut aggregation_sets.scanning_sequences,
                    header.scanning_sequence.iter().cloned(),
                    "unknown-overflow",
                );
                extend_unique_bounded(
                    &mut group.sequence_variants,
                    &mut aggregation_sets.sequence_variants,
                    header.sequence_variant.iter().cloned(),
                    "unknown-overflow",
                );
                extend_unique_bounded(
                    &mut group.scan_options,
                    &mut aggregation_sets.scan_options,
                    header.scan_options.iter().cloned(),
                    "unknown-overflow",
                );
                let local_text = bounded_local_protocol_text(&header.local_protocol_text());
                if !local_text.is_empty() {
                    extend_unique_bounded(
                        &mut group.local_protocol_texts,
                        &mut aggregation_sets.local_protocol_texts,
                        [local_text],
                        "derived",
                    );
                }
                extend_unique_bounded(
                    &mut group.burned_in_annotations,
                    &mut aggregation_sets.burned_in_annotations,
                    header.burned_in_annotation.iter().cloned(),
                    "UNKNOWN",
                );
                group.burned_in_annotation_missing |= header.burned_in_annotation.is_none();
                group.all_missing_bia_instances_original_primary &=
                    header.burned_in_annotation.is_some()
                        || declares_original_primary(&header.image_type);
                group.siemens_csa_image_header_present |= header.siemens_csa_image_header_present;
                group.all_siemens_csa_image_headers_sanitizable &=
                    header.siemens_csa_image_header_sanitizable;
                group.all_philips_pixel_scaling_contracts_verified &= header
                    .philips_private_pixel_scaling_usable
                    || header.public_pixel_scaling_contract_verified;
                group.philips_private_pixel_scaling_present |=
                    header.philips_private_pixel_scaling_present;
                group.philips_private_pixel_scaling_incomplete |= header
                    .philips_private_pixel_scaling_present
                    && !header.philips_private_pixel_scaling_usable;
                group.philips_private_asl_label_type_present |=
                    header.philips_private_asl_label_type_present;
                group.uih_grid_or_vframe |= header.uih_grid_or_vframe;
                group.uih_grid_slice_count_present |= header.uih_grid_slice_count_present;
                if header.uih_grid_or_vframe || header.uih_grid_slice_count_present {
                    group.all_uih_grid_slice_counts_verified &=
                        header.uih_grid_slice_count_verified;
                }
                group.diffusion_metadata_present |= header.public_diffusion_metadata_present
                    || header.reviewed_private_diffusion_metadata_present;
                group.all_diffusion_metadata_contracts_verified &=
                    header.diffusion_metadata_contract_verified;
                group.asl_metadata_present |= header.public_asl_metadata_present
                    || header.reviewed_private_asl_metadata_present;
                group.all_asl_metadata_contracts_verified &= header.asl_metadata_contract_verified;
                // Philips commonly writes DiffusionBValue=0 on ordinary fMRI.
                // Zero is not diffusion evidence without a direction/gradient
                // or diffusion-labelled sequence.
                group.diffusion_context |=
                    header.diffusion_b_value.is_some_and(|value| value > 1.0)
                        || header.public_diffusion_semantic_evidence
                        || header.reviewed_private_diffusion_semantic_evidence;
                group.asl_context |= header.asl_technique.is_some()
                    || header.reviewed_private_asl_metadata_present
                    || header.ge_asl_supplemental_metadata_present;
                group.overlay_or_graphics |= header.overlay_or_graphics;
                group.has_extended_offset_table |= header.has_extended_offset_table;
                if let Some(value) = header.temporal_position_identifier {
                    retain_two_distinct(&mut group.temporal_position_identifiers, value);
                }
                if let Some(value) = header.acquisition_number {
                    retain_two_distinct(&mut group.acquisition_numbers, value);
                }
                group.has_per_frame_functional_groups |= header.has_per_frame_functional_groups;
                if representative_precedes(&header, &group.representative) {
                    let mut representative = header.clone();
                    merge_missing_common(&mut representative, &group.representative);
                    group.representative = representative;
                } else {
                    merge_missing_common(&mut group.representative, &header);
                }
                group.files.push(path.clone());
                let sop_instance_uid = header.sop_instance_uid.clone().unwrap_or_default();
                if record_sop_instance_uid(seen_sop_instance_uids, &sop_instance_uid) {
                    group.duplicate_sop_instance_uid = true;
                }
                group.instances.push(SeriesInstance {
                    path: path.clone(),
                    instance_number: header.instance_number,
                    sop_instance_uid,
                    trigger_time_ms: header.trigger_time_ms,
                    philips_dynamic_scan_begin_time_seconds: header
                        .philips_dynamic_scan_begin_time_seconds,
                    philips_dynamic_timing_tag_present: header.philips_dynamic_timing_tag_present,
                    temporal_position_identifier: header.temporal_position_identifier,
                    number_of_temporal_positions: header.number_of_temporal_positions,
                    philips_number_of_slices: header.philips_number_of_slices,
                    image_position_patient: header.image_position_patient.clone(),
                    acquisition_number: header.acquisition_number,
                    repetition_time_ms: header.repetition_time_ms,
                    echo_time_ms: header.echo_time_ms,
                });
            }
            Err(error) => {
                include_in_source_identity = looks_like_dicom(&path);
                if include_in_source_identity {
                    unreadable_dicom_like_files += 1;
                    tracing::warn!(path = %path.display(), error = %error, "DICOM-like file could not be parsed");
                }
            }
        }
        let after = file_fingerprint(&path)?;
        if include_in_source_identity {
            changed_while_reading |= before != after;
            source_files.push(after);
        }
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            report(DiscoveryProgress {
                phase: DiscoveryPhase::ReadHeaders,
                files_seen: summary.files_seen,
                total_files: Some(total_files),
                dicom_files: summary.dicom_files,
                series_found: groups.len() as u64,
            });
            last_progress = Instant::now();
        }
    }

    let mut series: Vec<_> = groups.into_values().collect();
    for group in &mut series {
        group.instances.sort_by(|left, right| {
            left.instance_number
                .unwrap_or(i64::MAX)
                .cmp(&right.instance_number.unwrap_or(i64::MAX))
                .then(left.sop_instance_uid.cmp(&right.sop_instance_uid))
        });
        group.files = group
            .instances
            .iter()
            .map(|instance| instance.path.clone())
            .collect();
        group.philips_dynamic_timing_detected = group
            .instances
            .iter()
            .any(|instance| instance.philips_dynamic_timing_tag_present);
        group.philips_dynamic_timing_contract_verified =
            verify_philips_dynamic_timing_contract(group);
    }
    source_files.sort_by(|left, right| left.path.cmp(&right.path));
    series.sort_by(|a, b| {
        a.study_uid
            .cmp(&b.study_uid)
            .then(a.series_uid.cmp(&b.series_uid))
    });
    summary.series_found = series.len() as u64;
    report(DiscoveryProgress {
        phase: DiscoveryPhase::ReadHeaders,
        files_seen: summary.files_seen,
        total_files: Some(total_files),
        dicom_files: summary.dicom_files,
        series_found: summary.series_found,
    });
    Ok(Discovery {
        series,
        summary,
        unreadable_dicom_like_files,
        source_snapshot: SourceSnapshot {
            files: source_files,
            changed_while_reading,
        },
    })
}

fn record_sop_instance_uid(seen: &mut HashSet<String>, value: &str) -> bool {
    value.is_empty() || !seen.insert(value.to_owned())
}

fn inventory_regular_files(
    root: &Path,
    report: &mut impl FnMut(DiscoveryProgress),
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut last_progress = Instant::now();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.context("could not inspect every entry in the selected folder")?;
        if !entry.file_type().is_file() {
            continue;
        }
        files.push(entry.into_path());
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            report(DiscoveryProgress {
                phase: DiscoveryPhase::Inventory,
                files_seen: files.len() as u64,
                total_files: None,
                dicom_files: 0,
                series_found: 0,
            });
            last_progress = Instant::now();
        }
    }
    files.sort();
    report(DiscoveryProgress {
        phase: DiscoveryPhase::Inventory,
        files_seen: files.len() as u64,
        total_files: None,
        dicom_files: 0,
        series_found: 0,
    });
    Ok(files)
}

pub fn snapshot_source_with_progress(
    root: &Path,
    mut report: impl FnMut(SnapshotProgress),
) -> Result<SourceSnapshot> {
    if !root.is_dir() {
        bail!(
            "DICOM folder does not exist or is not a directory: {}",
            root.display()
        );
    }

    let mut files = Vec::new();
    let mut changed_while_reading = false;
    let mut files_seen = 0_u64;
    let mut last_progress = Instant::now();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.context("could not inspect every entry in the selected folder")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        files_seen += 1;
        let before = file_fingerprint(&path)?;
        let include_in_source_identity = read_header(&path).is_ok() || looks_like_dicom(&path);
        let after = file_fingerprint(&path)?;
        if include_in_source_identity {
            changed_while_reading |= before != after;
            files.push(after);
        }
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            report(SnapshotProgress { files_seen });
            last_progress = Instant::now();
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    report(SnapshotProgress { files_seen });
    Ok(SourceSnapshot {
        files,
        changed_while_reading,
    })
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint> {
    let metadata = path
        .metadata()
        .with_context(|| format!("could not stat source file: {}", path.display()))?;
    Ok(FileFingerprint {
        path: path.to_path_buf(),
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn representative_precedes(candidate: &DicomHeader, current: &DicomHeader) -> bool {
    (
        candidate.instance_number.unwrap_or(i64::MAX),
        candidate.path.as_path(),
    ) < (
        current.instance_number.unwrap_or(i64::MAX),
        current.path.as_path(),
    )
}

fn extend_unique_bounded<I>(
    target: &mut Vec<String>,
    seen: &mut HashSet<String>,
    values: I,
    overflow_value: &str,
) where
    I: IntoIterator<Item = String>,
{
    for value in values {
        if seen.contains(&value) {
            continue;
        }
        if seen.len() < MAX_DISTINCT_SERIES_HEADER_VALUES {
            seen.insert(value.clone());
            target.push(value);
        } else if seen.insert(overflow_value.to_owned()) {
            target.push(overflow_value.to_owned());
        }
    }
}

fn retain_two_distinct(target: &mut Vec<i64>, value: i64) {
    if target.len() < 2 && !target.contains(&value) {
        target.push(value);
    }
}

fn bounded_local_protocol_text(value: &str) -> String {
    if value.len() <= MAX_LOCAL_PROTOCOL_TEXT_BYTES {
        return value.to_owned();
    }

    let mut end = MAX_LOCAL_PROTOCOL_TEXT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn declares_original_primary(image_type: &[String]) -> bool {
    let has = |expected: &str| {
        image_type
            .iter()
            .any(|value| value.trim().eq_ignore_ascii_case(expected))
    };
    has("ORIGINAL") && has("PRIMARY") && !has("DERIVED") && !has("SECONDARY")
}

fn required_metadata_conflicts(left: &DicomHeader, right: &DicomHeader) -> bool {
    option_conflicts(&left.sop_class_uid, &right.sop_class_uid)
        || option_conflicts(&left.modality, &right.modality)
        || normalized_option_conflicts(
            left.manufacturer.as_deref(),
            right.manufacturer.as_deref(),
            |value| {
                crate::archive::canonical_manufacturer(value)
                    .unwrap_or_else(|| normalized_text(value))
            },
        )
        || normalized_option_conflicts(left.model.as_deref(), right.model.as_deref(), |value| {
            crate::archive::canonical_model(value).unwrap_or_else(|| normalized_text(value))
        })
        || scanner_software_versions(left) != scanner_software_versions(right)
            && left.software_versions.is_some()
            && right.software_versions.is_some()
        || normalized_option_conflicts(
            left.patient_position.as_deref(),
            right.patient_position.as_deref(),
            normalized_text,
        )
        || normalized_option_conflicts(
            left.mr_acquisition_type.as_deref(),
            right.mr_acquisition_type.as_deref(),
            normalized_text,
        )
        || normalized_option_conflicts(
            left.sequence_name.as_deref(),
            right.sequence_name.as_deref(),
            normalized_text,
        )
        || normalized_list_conflicts(&left.scanning_sequence, &right.scanning_sequence)
        || normalized_list_conflicts(&left.sequence_variant, &right.sequence_variant)
        || normalized_list_conflicts(&left.scan_options, &right.scan_options)
        || normalized_option_conflicts(
            acquisition_string(left, "receive_coil_name"),
            acquisition_string(right, "receive_coil_name"),
            normalized_text,
        )
        || normalized_option_conflicts(
            acquisition_string(left, "transmit_coil_name"),
            acquisition_string(right, "transmit_coil_name"),
            normalized_text,
        )
        || float_option_conflicts(left.magnetic_field_strength, right.magnetic_field_strength)
        || option_conflicts(&left.series_number, &right.series_number)
}

fn option_conflicts<T: PartialEq>(left: &Option<T>, right: &Option<T>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn normalized_option_conflicts(
    left: Option<&str>,
    right: Option<&str>,
    normalize: impl Fn(&str) -> String,
) -> bool {
    matches!((left, right), (Some(left), Some(right)) if normalize(left) != normalize(right))
}

fn normalized_text(value: &str) -> String {
    value
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

fn normalized_list_conflicts(left: &[String], right: &[String]) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let normalize = |values: &[String]| {
        let mut values = values
            .iter()
            .flat_map(|value| value.split('\\'))
            .map(normalized_text)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        values.sort_unstable();
        values.dedup();
        values
    };
    normalize(left) != normalize(right)
}

fn acquisition_string<'a>(header: &'a DicomHeader, key: &str) -> Option<&'a str> {
    header.acquisition.get(key).and_then(Value::as_str)
}

fn scanner_software_versions(header: &DicomHeader) -> Option<Vec<String>> {
    let raw = header.software_versions.as_deref()?;
    let manufacturer = header
        .manufacturer
        .as_deref()
        .and_then(crate::archive::canonical_manufacturer);
    let mut versions = crate::archive::canonical_software_versions(raw, manufacturer.as_deref());
    if versions.is_empty() {
        versions = raw
            .split(|character: char| {
                character.is_ascii_whitespace() || matches!(character, '\\' | ',' | ';' | '/' | '_')
            })
            .filter(|value| !value.is_empty())
            .map(normalized_text)
            .collect();
    }
    versions.sort_unstable();
    versions.dedup();
    Some(versions)
}

fn float_option_conflicts(left: Option<f64>, right: Option<f64>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if {
        !left.is_finite()
            || !right.is_finite()
            || (left - right).abs() > 1.0e-6_f64.max(left.abs().max(right.abs()) * 1.0e-6)
    })
}

fn merge_missing_common(target: &mut DicomHeader, source: &DicomHeader) {
    macro_rules! merge_option {
        ($field:ident) => {
            if target.$field.is_none() {
                target.$field = source.$field.clone();
            }
        };
    }
    merge_option!(patient_id);
    merge_option!(issuer_of_patient_id);
    merge_option!(study_uid);
    merge_option!(series_uid);
    merge_option!(sop_class_uid);
    merge_option!(modality);
    merge_option!(manufacturer);
    merge_option!(model);
    merge_option!(patient_position);
    merge_option!(software_versions);
    merge_option!(magnetic_field_strength);
    merge_option!(series_description);
    merge_option!(protocol_name);
    merge_option!(sequence_name);
    merge_option!(mr_acquisition_type);
    merge_option!(echo_planar_pulse_sequence);
    merge_option!(series_number);
    merge_option!(repetition_time_ms);
    merge_option!(echo_time_ms);
    merge_option!(inversion_time_ms);
    merge_option!(flip_angle_degrees);
    merge_option!(number_of_temporal_positions);
    merge_option!(number_of_frames);
    merge_option!(images_in_acquisition);
    merge_option!(burned_in_annotation);
    for (key, value) in &source.acquisition {
        target
            .acquisition
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
}

pub fn read_header(path: &Path) -> Result<DicomHeader> {
    let object = OpenFileOptions::new()
        .read_until(PIXEL_DATA)
        .open_file(path)
        .with_context(|| format!("not a readable DICOM Part 10 file: {}", path.display()))?;

    let mut acquisition = BTreeMap::new();
    insert_string(
        &mut acquisition,
        "receive_coil_name",
        string(&object, Tag(0x0018, 0x1250)),
    );
    insert_string(
        &mut acquisition,
        "transmit_coil_name",
        string(&object, Tag(0x0018, 0x1251)),
    );
    insert_float(
        &mut acquisition,
        "slice_thickness_mm",
        float(&object, Tag(0x0018, 0x0050)),
    );
    insert_float(
        &mut acquisition,
        "spacing_between_slices_mm",
        float(&object, Tag(0x0018, 0x0088)),
    );
    insert_float(
        &mut acquisition,
        "pixel_bandwidth_hz",
        float(&object, Tag(0x0018, 0x0095)),
    );
    insert_float(
        &mut acquisition,
        "number_of_averages",
        float(&object, Tag(0x0018, 0x0083)),
    );
    insert_float(
        &mut acquisition,
        "imaging_frequency_mhz",
        float(&object, Tag(0x0018, 0x0084)),
    );
    insert_string(
        &mut acquisition,
        "imaged_nucleus",
        string(&object, Tag(0x0018, 0x0085)),
    );
    insert_int(
        &mut acquisition,
        "echo_train_length",
        integer(&object, Tag(0x0018, 0x0091)),
    );
    insert_int(
        &mut acquisition,
        "phase_encoding_steps",
        integer(&object, Tag(0x0018, 0x0089)),
    );
    insert_string(
        &mut acquisition,
        "phase_encoding_axis",
        string(&object, Tag(0x0018, 0x1312)),
    );
    insert_float(
        &mut acquisition,
        "percent_sampling",
        float(&object, Tag(0x0018, 0x0093)),
    );
    insert_float(
        &mut acquisition,
        "percent_phase_fov",
        float(&object, Tag(0x0018, 0x0094)),
    );
    insert_float(
        &mut acquisition,
        "parallel_reduction_factor_in_plane",
        float(&object, Tag(0x0018, 0x9069)),
    );
    insert_string(
        &mut acquisition,
        "partial_fourier_direction",
        string(&object, Tag(0x0018, 0x9036)),
    );
    insert_multi_int(
        &mut acquisition,
        "acquisition_matrix",
        multi_int(&object, Tag(0x0018, 0x1310)),
    );
    insert_multi_float(
        &mut acquisition,
        "pixel_spacing_mm",
        multi_float(&object, Tag(0x0028, 0x0030)),
    );
    insert_int(
        &mut acquisition,
        "rows",
        integer(&object, Tag(0x0028, 0x0010)),
    );
    insert_int(
        &mut acquisition,
        "columns",
        integer(&object, Tag(0x0028, 0x0011)),
    );

    let image_type = {
        let root = multi_string(&object, Tag(0x0008, 0x0008));
        if root.is_empty() {
            recursive_multi_string(&object, Tag(0x0008, 0x9007), 0)
        } else {
            root
        }
    };
    let manufacturer = string(&object, Tag(0x0008, 0x0070));
    let uih_grid_or_vframe = image_type.iter().any(|value| {
        matches!(
            value.trim().to_ascii_uppercase().as_str(),
            "GRID" | "VFRAME"
        )
    });
    let (uih_grid_slice_count_present, uih_grid_slice_count_verified) =
        uih_grid_slice_count_contract(&object);
    let (philips_private_pixel_scaling_present, philips_private_pixel_scaling_usable) =
        philips_private_pixel_scaling_contract(&object);
    let public_diffusion_contract = public_diffusion_metadata_contract(&object);
    let public_diffusion_metadata_present = public_diffusion_contract.present;
    let public_diffusion_semantic_evidence = public_diffusion_contract.semantic;
    let private_diffusion_contract = reviewed_private_diffusion_metadata_contract(&object);
    let reviewed_private_diffusion_metadata_present = private_diffusion_contract.present;
    let reviewed_private_diffusion_semantic_evidence = private_diffusion_contract.semantic;
    let public_asl_contract = public_asl_metadata_contract(&object);
    let private_asl_contract = reviewed_private_asl_metadata_contract(&object);
    let public_asl_metadata_present = public_asl_contract.present;
    let reviewed_private_asl_metadata_present = private_asl_contract.philips_present;
    let ge_asl_supplemental_metadata_present = private_asl_contract.ge_present;
    let diffusion_metadata_contract_verified =
        diffusion_source_contract_verified(&public_diffusion_contract, &private_diffusion_contract);
    let asl_metadata_contract_verified = scientific_source_contract_verified(
        public_asl_metadata_present,
        public_asl_contract.valid,
        reviewed_private_asl_metadata_present,
        private_asl_contract.philips_valid,
    ) && (!ge_asl_supplemental_metadata_present
        || private_asl_contract.ge_valid)
        && philips_private_asl_agrees_with_public(
            mr_sop_storage_kind(&object),
            &public_asl_contract,
            &private_asl_contract,
        );
    let repetition_time_ms = float(&object, Tag(0x0018, 0x0080))
        .or_else(|| recursive_float(&object, Tag(0x0018, 0x0080), 0));
    let echo_time_ms = float(&object, Tag(0x0018, 0x0081))
        .or_else(|| recursive_float(&object, Tag(0x0018, 0x9082), 0));
    let temporal_positions = recursive_integers(&object, Tag(0x0020, 0x9128), 0);
    let derived_temporal_positions = (temporal_positions.len() >= 2)
        .then(|| i64::try_from(temporal_positions.len()).ok())
        .flatten();

    Ok(DicomHeader {
        path: path.to_path_buf(),
        patient_id: string(&object, Tag(0x0010, 0x0020)),
        issuer_of_patient_id: string(&object, Tag(0x0010, 0x0021)),
        study_uid: string(&object, Tag(0x0020, 0x000D)),
        series_uid: string(&object, Tag(0x0020, 0x000E)),
        sop_class_uid: string(&object, Tag(0x0008, 0x0016)),
        sop_instance_uid: string(&object, Tag(0x0008, 0x0018)),
        modality: string(&object, Tag(0x0008, 0x0060)),
        image_type,
        manufacturer,
        model: string(&object, Tag(0x0008, 0x1090)),
        patient_position: string(&object, Tag(0x0018, 0x5100)),
        software_versions: string(&object, Tag(0x0018, 0x1020)),
        magnetic_field_strength: float(&object, Tag(0x0018, 0x0087)),
        series_description: string(&object, Tag(0x0008, 0x103E)),
        protocol_name: string(&object, Tag(0x0018, 0x1030)),
        sequence_name: string(&object, Tag(0x0018, 0x0024)),
        scanning_sequence: multi_string(&object, Tag(0x0018, 0x0020)),
        sequence_variant: multi_string(&object, Tag(0x0018, 0x0021)),
        scan_options: multi_string(&object, Tag(0x0018, 0x0022)),
        mr_acquisition_type: string(&object, Tag(0x0018, 0x0023))
            .or_else(|| recursive_string(&object, Tag(0x0018, 0x0023), 0)),
        echo_planar_pulse_sequence: string(&object, Tag(0x0018, 0x9018))
            .or_else(|| recursive_string(&object, Tag(0x0018, 0x9018), 0)),
        trigger_time_ms: float(&object, Tag(0x0018, 0x1060)),
        philips_dynamic_scan_begin_time_seconds: philips_dynamic_scan_begin_time(&object),
        philips_dynamic_timing_tag_present: philips_dynamic_scan_begin_time_tag_present(&object),
        philips_number_of_slices: philips_number_of_slices(&object),
        philips_private_pixel_scaling_present,
        philips_private_asl_label_type_present: philips_private_asl_label_type_present(&object),
        philips_private_pixel_scaling_usable,
        public_pixel_scaling_contract_verified: public_pixel_scaling_contract_verified(&object),
        uih_grid_or_vframe,
        uih_grid_slice_count_present,
        uih_grid_slice_count_verified,
        public_diffusion_metadata_present,
        public_diffusion_semantic_evidence,
        reviewed_private_diffusion_metadata_present,
        reviewed_private_diffusion_semantic_evidence,
        diffusion_metadata_contract_verified,
        public_asl_metadata_present,
        public_asl_contexts: public_asl_contract.contexts.into_iter().collect(),
        reviewed_private_asl_metadata_present,
        philips_private_asl_label: private_asl_contract.philips_label,
        ge_asl_supplemental_metadata_present,
        asl_metadata_contract_verified,
        image_position_patient: multi_float(&object, Tag(0x0020, 0x0032)),
        series_number: integer(&object, Tag(0x0020, 0x0011)),
        acquisition_number: integer(&object, Tag(0x0020, 0x0012)),
        instance_number: integer(&object, Tag(0x0020, 0x0013)),
        echo_number: integer(&object, Tag(0x0018, 0x0086)),
        repetition_time_ms,
        echo_time_ms,
        inversion_time_ms: float(&object, Tag(0x0018, 0x0082)),
        flip_angle_degrees: float(&object, Tag(0x0018, 0x1314)),
        number_of_temporal_positions: integer(&object, Tag(0x0020, 0x0105))
            .or(derived_temporal_positions),
        temporal_position_identifier: integer(&object, Tag(0x0020, 0x0100)),
        number_of_frames: integer(&object, Tag(0x0028, 0x0008)),
        images_in_acquisition: integer(&object, Tag(0x0020, 0x1002)),
        diffusion_b_value: float(&object, Tag(0x0018, 0x9087)),
        asl_technique: string(&object, Tag(0x0018, 0x9250)),
        burned_in_annotation: string(&object, Tag(0x0028, 0x0301)),
        siemens_csa_image_header_present: object.element(Tag(0x0029, 0x1010)).is_ok(),
        siemens_csa_image_header_sanitizable: object
            .element(Tag(0x0029, 0x0010))
            .ok()
            .and_then(|element| element.to_str().ok())
            .is_some_and(|value| {
                value
                    .trim_matches([' ', '\0'])
                    .eq_ignore_ascii_case("SIEMENS CSA HEADER")
            })
            && object
                .element(Tag(0x0029, 0x1010))
                .ok()
                .and_then(|element| element.to_bytes().ok())
                .is_some_and(|bytes| {
                    crate::archive::sanitize_siemens_csa_image_header(bytes.as_ref()).is_some()
                }),
        overlay_or_graphics: contains_overlay_or_graphics(&object, 0),
        has_extended_offset_table: object.element(Tag(0x7FE0, 0x0001)).is_ok()
            || object.element(Tag(0x7FE0, 0x0002)).is_ok(),
        has_per_frame_functional_groups: object.element(Tag(0x5200, 0x9230)).is_ok(),
        acquisition,
    })
}

fn contains_overlay_or_graphics(object: &dicom_object::InMemDicomObject, depth: usize) -> bool {
    if depth > 32 {
        return true;
    }
    object.iter().any(|element| {
        let group = element.tag().group();
        (0x5000..=0x501e).contains(&group)
            || (0x6000..=0x601e).contains(&group)
            || group == 0x0070
            || element.value().items().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| contains_overlay_or_graphics(item, depth + 1))
            })
    })
}

fn philips_dynamic_scan_begin_time(object: &DefaultDicomObject) -> Option<f64> {
    let mut values =
        object.iter().filter_map(|element| {
            let tag = element.tag();
            let creator_tag = Tag(tag.group(), tag.element() >> 8);
            (tag.group() == 0x2005
                && tag.element() & 0x00ff == 0x00a0
                && element.vr() == VR::FL
                && object
                    .element(creator_tag)
                    .ok()
                    .and_then(|creator| creator.to_str().ok())
                    .is_some_and(|creator| {
                        creator
                            .trim_matches([' ', '\0'])
                            .eq_ignore_ascii_case("Philips MR Imaging DD 001")
                    }))
            .then(|| match element.value() {
                dicom_core::value::Value::Primitive(dicom_core::value::PrimitiveValue::F32(
                    values,
                )) if values.len() == 1 => Some(f64::from(values[0])),
                _ => None,
            })
            .flatten()
            .filter(|value| value.is_finite() && (0.0..=86_400.0).contains(value))
        });
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn philips_dynamic_scan_begin_time_tag_present(object: &DefaultDicomObject) -> bool {
    object.iter().any(|element| {
        let tag = element.tag();
        let creator_tag = Tag(tag.group(), tag.element() >> 8);
        tag.group() == 0x2005
            && tag.element() & 0x00ff == 0x00a0
            && object
                .element(creator_tag)
                .ok()
                .and_then(|creator| creator.to_str().ok())
                .is_some_and(|creator| {
                    creator
                        .trim_matches([' ', '\0'])
                        .eq_ignore_ascii_case("Philips MR Imaging DD 001")
                })
    })
}

fn philips_number_of_slices(object: &DefaultDicomObject) -> Option<i64> {
    let mut values =
        object.iter().filter_map(|element| {
            let tag = element.tag();
            let creator_tag = Tag(tag.group(), tag.element() >> 8);
            (tag.group() == 0x2001
                && tag.element() & 0x00ff == 0x0018
                && element.vr() == VR::SL
                && object
                    .element(creator_tag)
                    .ok()
                    .and_then(|creator| creator.to_str().ok())
                    .is_some_and(|creator| {
                        creator
                            .trim_matches([' ', '\0'])
                            .eq_ignore_ascii_case("Philips Imaging DD 001")
                    }))
            .then(|| match element.value() {
                dicom_core::value::Value::Primitive(dicom_core::value::PrimitiveValue::I32(
                    values,
                )) if values.len() == 1 => Some(i64::from(values[0])),
                _ => None,
            })
            .flatten()
            .filter(|value| (1..=4096).contains(value))
        });
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

/// Validate the creator-mapped UIH classic multi-frame slice count without
/// trusting a fixed private block number. `(0065,xx50)` is a DS VM1 integer in
/// the closed range 1..=4096 under creator `Image Private Header`.
fn uih_grid_slice_count_contract(object: &DefaultDicomObject) -> (bool, bool) {
    let mut candidates = object.iter().filter(|element| {
        let tag = element.tag();
        tag.group() == 0x0065
            && tag.element() & 0x00ff == 0x0050
            && private_creator_matches(object, tag, "Image Private Header")
    });
    let Some(element) = candidates.next() else {
        return (false, false);
    };
    if candidates.next().is_some() || element.vr() != VR::DS {
        return (true, false);
    }
    let Ok(values) = element.to_multi_str() else {
        return (true, false);
    };
    if values.len() != 1 {
        return (true, false);
    }
    let raw = values[0].trim_matches([' ', '\0']);
    let valid = !raw.is_empty()
        && raw.len() <= 16
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'))
        && raw.parse::<f64>().ok().is_some_and(|value| {
            value.is_finite() && value.fract() == 0.0 && (1.0..=4096.0).contains(&value)
        });
    (true, valid)
}

fn philips_private_pixel_scaling_contract(object: &DefaultDicomObject) -> (bool, bool) {
    let intercept = unique_private_numbers(
        object,
        0x2005,
        0x000d,
        "Philips MR Imaging DD 001",
        VR::FL,
        1,
        |values| values[0].abs() <= 1.0e9,
    );
    let slope = unique_private_numbers(
        object,
        0x2005,
        0x000e,
        "Philips MR Imaging DD 001",
        VR::FL,
        1,
        |values| values[0] > 0.0 && values[0] <= 1.0e9,
    );
    let present = intercept.present || slope.present;
    let usable = intercept.values.is_some() && slope.values.is_some();
    (present, usable)
}

fn philips_private_asl_label_type_present(object: &DefaultDicomObject) -> bool {
    object.iter().any(|element| {
        let tag = element.tag();
        tag.group() == 0x2005
            && tag.element() & 0x00ff == 0x0029
            && private_creator_matches(object, tag, "Philips MR Imaging DD 005")
    })
}

fn public_pixel_scaling_contract_verified(object: &DefaultDicomObject) -> bool {
    valid_public_ds_vm1(object, Tag(0x0028, 0x1052), |value| value.abs() <= 1.0e9)
        && valid_public_ds_vm1(object, Tag(0x0028, 0x1053), |value| {
            value > 0.0 && value <= 1.0e9
        })
}

fn valid_public_ds_vm1(object: &DefaultDicomObject, tag: Tag, valid: impl Fn(f64) -> bool) -> bool {
    let Ok(element) = object.element(tag) else {
        return false;
    };
    if element.vr() != VR::DS {
        return false;
    }
    let Ok(values) = element.to_multi_str() else {
        return false;
    };
    if values.len() != 1 {
        return false;
    }
    let raw = values[0].trim_matches([' ', '\0']);
    !raw.is_empty()
        && raw.len() <= 16
        && raw
            .parse::<f64>()
            .ok()
            .is_some_and(|value| value.is_finite() && valid(value))
}

fn private_creator_matches(
    object: &dicom_object::InMemDicomObject,
    private_tag: Tag,
    expected: &str,
) -> bool {
    let creator_tag = Tag(private_tag.group(), private_tag.element() >> 8);
    object
        .element(creator_tag)
        .ok()
        .and_then(|creator| creator.to_str().ok())
        .is_some_and(|creator| {
            creator
                .trim_matches([' ', '\0'])
                .eq_ignore_ascii_case(expected)
        })
}

#[derive(Debug, Default)]
struct PrivateNumericState {
    present: bool,
    values: Option<Vec<f64>>,
}

#[derive(Debug, Default)]
struct PrivateCodeState {
    present: bool,
    value: Option<String>,
}

fn private_numeric_well_formed(state: &PrivateNumericState) -> bool {
    !state.present || state.values.is_some()
}

fn private_code_well_formed(state: &PrivateCodeState) -> bool {
    !state.present || state.value.is_some()
}

fn numeric_vector_is_zero(values: &[f64]) -> bool {
    values.iter().all(|value| value.abs() <= 1.0e-6)
}

fn unit_direction_vector(values: &[f64]) -> bool {
    values.len() == 3
        && values.iter().all(|value| (-1.1..=1.1).contains(value))
        && (0.5..=1.5).contains(&values.iter().map(|value| value * value).sum::<f64>())
}

// Public FD/DS values and vendor-private FL/IS values routinely differ by
// decimal serialization or integer rounding. One s/mm² (or 0.1%, whichever is
// larger) is tight relative to acquisition b-values while avoiding false
// conflicts from those representations. Direction components use 1e-4; the
// comparison is performed through the b-tensor so antipodal g/-g vectors,
// which encode the same diffusion weighting, agree.
const DIFFUSION_ABSOLUTE_TOLERANCE: f64 = 1.0;
const DIFFUSION_RELATIVE_TOLERANCE: f64 = 1.0e-3;
const DIFFUSION_DIRECTION_TOLERANCE: f64 = 1.0e-4;

#[derive(Debug, Clone, PartialEq)]
enum DiffusionRepresentation {
    None,
    Isotropic,
    Direction([f64; 3]),
    BMatrix([f64; 6]),
}

#[derive(Debug, Clone, PartialEq)]
struct DiffusionSignature {
    b_value: f64,
    representation: DiffusionRepresentation,
}

#[derive(Debug, Default)]
struct DiffusionContract {
    present: bool,
    valid: bool,
    semantic: bool,
    signatures: Vec<DiffusionSignature>,
}

impl DiffusionContract {
    #[cfg(test)]
    fn flags(&self) -> (bool, bool, bool) {
        (self.present, self.valid, self.semantic)
    }
}

#[cfg(test)]
impl PartialEq<(bool, bool, bool)> for DiffusionContract {
    fn eq(&self, other: &(bool, bool, bool)) -> bool {
        self.flags() == *other
    }
}

fn approximately_equal(left: f64, right: f64, absolute: f64) -> bool {
    let tolerance = absolute.max(DIFFUSION_RELATIVE_TOLERANCE * left.abs().max(right.abs()));
    (left - right).abs() <= tolerance
}

fn diffusion_tensor(signature: &DiffusionSignature) -> Option<[f64; 6]> {
    match signature.representation {
        DiffusionRepresentation::Direction([x, y, z]) => Some([
            signature.b_value * x * x,
            signature.b_value * x * y,
            signature.b_value * x * z,
            signature.b_value * y * y,
            signature.b_value * y * z,
            signature.b_value * z * z,
        ]),
        DiffusionRepresentation::BMatrix(matrix) => Some(matrix),
        DiffusionRepresentation::None | DiffusionRepresentation::Isotropic => None,
    }
}

fn diffusion_signatures_agree(left: &DiffusionSignature, right: &DiffusionSignature) -> bool {
    if !approximately_equal(left.b_value, right.b_value, DIFFUSION_ABSOLUTE_TOLERANCE) {
        return false;
    }
    if left.b_value <= 1.0 && right.b_value <= 1.0 {
        return matches!(left.representation, DiffusionRepresentation::None)
            && matches!(right.representation, DiffusionRepresentation::None);
    }
    match (&left.representation, &right.representation) {
        (DiffusionRepresentation::Isotropic, DiffusionRepresentation::Isotropic) => true,
        (
            DiffusionRepresentation::Direction(_) | DiffusionRepresentation::BMatrix(_),
            DiffusionRepresentation::Direction(_) | DiffusionRepresentation::BMatrix(_),
        ) => diffusion_tensor(left)
            .zip(diffusion_tensor(right))
            .is_some_and(|(left, right)| {
                left.into_iter().zip(right).all(|(left, right)| {
                    approximately_equal(left, right, DIFFUSION_ABSOLUTE_TOLERANCE)
                        || (left.abs() <= DIFFUSION_DIRECTION_TOLERANCE
                            && right.abs() <= DIFFUSION_DIRECTION_TOLERANCE)
                })
            }),
        _ => false,
    }
}

fn diffusion_contracts_agree(left: &DiffusionContract, right: &DiffusionContract) -> bool {
    !left.signatures.is_empty()
        && !right.signatures.is_empty()
        && left.signatures.iter().all(|left_signature| {
            right
                .signatures
                .iter()
                .any(|right_signature| diffusion_signatures_agree(left_signature, right_signature))
        })
        && right.signatures.iter().all(|right_signature| {
            left.signatures
                .iter()
                .any(|left_signature| diffusion_signatures_agree(left_signature, right_signature))
        })
}

fn diffusion_signature(
    b_value: f64,
    directionality: Option<&str>,
    gradient: Option<&[f64]>,
    b_matrix: Option<&[f64]>,
) -> Option<DiffusionSignature> {
    let representation = if b_value <= 1.0 {
        DiffusionRepresentation::None
    } else {
        match directionality? {
            "ISOTROPIC" => DiffusionRepresentation::Isotropic,
            "DIRECTIONAL" | "AP" | "FH" | "RL" => {
                let values: [f64; 3] = gradient?.try_into().ok()?;
                DiffusionRepresentation::Direction(values)
            }
            "BMATRIX" => {
                let values: [f64; 6] = b_matrix?.try_into().ok()?;
                DiffusionRepresentation::BMatrix(values)
            }
            _ => return None,
        }
    };
    Some(DiffusionSignature {
        b_value,
        representation,
    })
}

fn diffusion_semantic(signatures: &[DiffusionSignature]) -> bool {
    signatures.iter().any(|signature| signature.b_value > 1.0)
}

fn single_source_diffusion_contract(
    present: bool,
    valid: bool,
    semantic: bool,
    signature: Option<DiffusionSignature>,
) -> DiffusionContract {
    DiffusionContract {
        present,
        valid,
        semantic,
        signatures: signature.into_iter().collect(),
    }
}

fn scientific_source_contract_verified(
    public_present: bool,
    public_verified: bool,
    private_present: bool,
    private_verified: bool,
) -> bool {
    (public_present || private_present)
        && (!public_present || public_verified)
        && (!private_present || private_verified)
}

fn diffusion_source_contract_verified(
    public: &DiffusionContract,
    private: &DiffusionContract,
) -> bool {
    scientific_source_contract_verified(
        public.present,
        public.valid,
        private.present,
        private.valid,
    ) && (!(public.present && private.present) || diffusion_contracts_agree(public, private))
}

fn unique_private_numbers(
    object: &dicom_object::InMemDicomObject,
    group: u16,
    low_element: u16,
    creator: &str,
    vr: VR,
    vm: usize,
    valid: impl Fn(&[f64]) -> bool,
) -> PrivateNumericState {
    let mut candidates = object.iter().filter(|element| {
        let tag = element.tag();
        tag.group() == group
            && tag.element() & 0x00ff == low_element
            && private_creator_matches(object, tag, creator)
    });
    let Some(element) = candidates.next() else {
        return PrivateNumericState::default();
    };
    if candidates.next().is_some() || element.vr() != vr {
        return PrivateNumericState {
            present: true,
            values: None,
        };
    }
    let values = element.to_multi_float64().ok().filter(|values| {
        values.len() == vm && values.iter().all(|value| value.is_finite()) && valid(values)
    });
    PrivateNumericState {
        present: true,
        values,
    }
}

fn unique_private_code(
    object: &dicom_object::InMemDicomObject,
    group: u16,
    low_element: u16,
    creator: &str,
    allowed: &[&str],
) -> PrivateCodeState {
    let mut candidates = object.iter().filter(|element| {
        let tag = element.tag();
        tag.group() == group
            && tag.element() & 0x00ff == low_element
            && private_creator_matches(object, tag, creator)
    });
    let Some(element) = candidates.next() else {
        return PrivateCodeState::default();
    };
    if candidates.next().is_some() || element.vr() != VR::CS {
        return PrivateCodeState {
            present: true,
            value: None,
        };
    }
    if matches!(
        element.value(),
        dicom_core::value::Value::Primitive(dicom_core::value::PrimitiveValue::Empty)
    ) {
        return PrivateCodeState::default();
    }
    let raw = element
        .to_multi_str()
        .ok()
        .filter(|values| values.len() == 1)
        .and_then(|values| values.first().cloned())
        .map(|value| value.trim_matches([' ', '\0']).to_ascii_uppercase());
    // Philips emits zero-length optional private code attributes on ordinary
    // non-ASL/non-diffusion series. An empty field is absence, not semantic
    // evidence and not a malformed scientific claim.
    if raw.as_deref() == Some("") {
        return PrivateCodeState::default();
    }
    let value = raw.filter(|value| value.len() <= 16 && allowed.contains(&value.as_str()));
    PrivateCodeState {
        present: true,
        value,
    }
}

fn read_csa_u32(source: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        source
            .get(offset..offset.checked_add(4)?)?
            .try_into()
            .ok()?,
    ))
}

fn parse_siemens_csa_diffusion_fields(source: &[u8]) -> Option<BTreeMap<String, Vec<f64>>> {
    const MAX_CSA_BYTES: usize = 16 * 1024 * 1024;
    const MAX_CSA_ITEMS: usize = 4096;
    if !(36..=MAX_CSA_BYTES).contains(&source.len())
        || source.get(..4)? != b"SV10"
        || read_csa_u32(source, 12)? != 77
    {
        return None;
    }
    let tag_count = usize::try_from(read_csa_u32(source, 8)?).ok()?;
    if !(1..=128).contains(&tag_count) {
        return None;
    }
    let mut cursor = 16_usize;
    let mut fields = BTreeMap::new();
    for _ in 0..tag_count {
        let header_end = cursor.checked_add(84)?;
        let header = source.get(cursor..header_end)?;
        cursor = header_end;
        let name_end = header[..64].iter().position(|byte| *byte == 0)?;
        let name = std::str::from_utf8(&header[..name_end]).ok()?;
        let declared_vm = i32::from_le_bytes(header[64..68].try_into().ok()?);
        let item_count =
            usize::try_from(u32::from_le_bytes(header[76..80].try_into().ok()?)).ok()?;
        if !(0..=4096).contains(&declared_vm) || item_count > MAX_CSA_ITEMS {
            return None;
        }
        let keep = matches!(name, "B_value" | "DiffusionGradientDirection" | "B_matrix");
        let mut values = Vec::new();
        for _ in 0..item_count {
            let item_end = cursor.checked_add(16)?;
            let item = source.get(cursor..item_end)?;
            cursor = item_end;
            let length = usize::try_from(u32::from_le_bytes(item[4..8].try_into().ok()?)).ok()?;
            if length > 1024 * 1024 {
                return None;
            }
            let value_end = cursor.checked_add(length)?;
            let bytes = source.get(cursor..value_end)?;
            cursor = cursor.checked_add(length.checked_add(3)? & !3)?;
            if cursor > source.len() || !keep {
                continue;
            }
            let value = std::str::from_utf8(bytes).ok()?.trim_matches([' ', '\0']);
            if value.is_empty() {
                continue;
            }
            if value.bytes().any(|byte| {
                !byte.is_ascii_digit() && !matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E')
            }) {
                return None;
            }
            values.push(
                value
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())?,
            );
        }
        if keep && !values.is_empty() && declared_vm > 0 && values.len() != declared_vm as usize {
            return None;
        }
        if keep && !values.is_empty() && fields.insert(name.to_owned(), values).is_some() {
            return None;
        }
    }
    Some(fields)
}

// Siemens stores the six symmetric matrix members as XX, YY, ZZ, XY, XZ,
// YZ; DICOM's public macro order is XX, XY, XZ, YY, YZ, ZZ.
fn canonical_siemens_b_matrix(values: &[f64]) -> Option<[f64; 6]> {
    let values: [f64; 6] = values.try_into().ok()?;
    Some([
        values[0], values[3], values[4], values[1], values[5], values[2],
    ])
}

fn siemens_csa_diffusion_contract(source: &[u8]) -> DiffusionContract {
    let (present, valid, semantic) = crate::archive::siemens_csa_diffusion_contract(source);
    if !valid {
        return single_source_diffusion_contract(present, false, semantic, None);
    }
    let Some(fields) = parse_siemens_csa_diffusion_fields(source) else {
        return single_source_diffusion_contract(present, false, semantic, None);
    };
    let b_value = fields
        .get("B_value")
        .filter(|values| values.len() == 1)
        .map(|values| values[0]);
    let gradient = fields.get("DiffusionGradientDirection");
    let b_matrix = fields
        .get("B_matrix")
        .and_then(|values| canonical_siemens_b_matrix(values));
    let directionality = if gradient.is_some() {
        Some("DIRECTIONAL")
    } else if b_matrix.is_some() {
        Some("BMATRIX")
    } else {
        Some("NONE")
    };
    let signature = b_value.and_then(|b_value| {
        diffusion_signature(
            b_value,
            directionality,
            gradient.map(Vec::as_slice),
            b_matrix.as_ref().map(<[f64; 6]>::as_slice),
        )
    });
    single_source_diffusion_contract(present, signature.is_some(), semantic, signature)
}

fn reviewed_private_diffusion_metadata_contract(
    object: &dicom_object::InMemDicomObject,
) -> DiffusionContract {
    let siemens_b = unique_private_numbers(
        object,
        0x0019,
        0x000c,
        "SIEMENS MR HEADER",
        VR::IS,
        1,
        |values| (0.0..=1.0e6).contains(&values[0]) && values[0].fract() == 0.0,
    );
    let siemens_direction = unique_private_code(
        object,
        0x0019,
        0x000d,
        "SIEMENS MR HEADER",
        &["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"],
    );
    let siemens_gradient = unique_private_numbers(
        object,
        0x0019,
        0x000e,
        "SIEMENS MR HEADER",
        VR::FD,
        3,
        |values| values.iter().all(|value| (-1.1..=1.1).contains(value)),
    );
    let siemens_b_matrix = unique_private_numbers(
        object,
        0x0019,
        0x0027,
        "SIEMENS MR HEADER",
        VR::FD,
        6,
        |values| values.iter().all(|value| (-1.0e9..=1.0e9).contains(value)),
    );
    let siemens_present = siemens_b.present
        || siemens_direction.present
        || siemens_gradient.present
        || siemens_b_matrix.present;
    let siemens_fields_well_formed = private_numeric_well_formed(&siemens_b)
        && private_code_well_formed(&siemens_direction)
        && private_numeric_well_formed(&siemens_gradient)
        && private_numeric_well_formed(&siemens_b_matrix);
    let siemens_tag_valid = siemens_fields_well_formed
        && siemens_b.values.as_ref().is_some_and(|values| {
            let b_value = values[0];
            let gradient = siemens_gradient.values.as_deref();
            let b_matrix = siemens_b_matrix.values.as_deref();
            if b_value <= 1.0 {
                return matches!(siemens_direction.value.as_deref(), None | Some("NONE"))
                    && gradient.is_none_or(numeric_vector_is_zero)
                    && b_matrix.is_none_or(numeric_vector_is_zero);
            }
            match siemens_direction.value.as_deref() {
                Some("DIRECTIONAL") => {
                    gradient.is_some_and(unit_direction_vector)
                        && b_matrix.is_none_or(numeric_vector_is_zero)
                }
                Some("BMATRIX") => {
                    b_matrix.is_some() && gradient.is_none_or(numeric_vector_is_zero)
                }
                Some("ISOTROPIC") => gradient.is_none() && b_matrix.is_none(),
                _ => false,
            }
        });
    let siemens_tag_semantic = siemens_b
        .values
        .as_ref()
        .is_some_and(|values| values[0] > 1.0)
        || matches!(
            siemens_direction.value.as_deref(),
            Some("ISOTROPIC" | "DIRECTIONAL" | "BMATRIX")
        );
    let siemens_tag_matrix = siemens_b_matrix
        .values
        .as_deref()
        .and_then(canonical_siemens_b_matrix);
    let siemens_tag_signature = if siemens_tag_valid {
        siemens_b.values.as_ref().and_then(|values| {
            diffusion_signature(
                *values.first()?,
                siemens_direction.value.as_deref().or(Some("NONE")),
                siemens_gradient.values.as_deref(),
                siemens_tag_matrix.as_ref().map(|values| values.as_slice()),
            )
        })
    } else {
        None
    };
    let siemens_tag_contract = single_source_diffusion_contract(
        siemens_present,
        siemens_tag_valid && siemens_tag_signature.is_some(),
        siemens_tag_semantic,
        siemens_tag_signature,
    );
    let siemens_csa_contract = object
        .element(Tag(0x0029, 0x1010))
        .ok()
        .filter(|_| private_creator_matches(object, Tag(0x0029, 0x1010), "SIEMENS CSA HEADER"))
        .and_then(|element| element.to_bytes().ok())
        .map(|bytes| siemens_csa_diffusion_contract(bytes.as_ref()))
        .unwrap_or_default();
    let siemens_valid = scientific_source_contract_verified(
        siemens_tag_contract.present,
        siemens_tag_contract.valid,
        siemens_csa_contract.present,
        siemens_csa_contract.valid,
    ) && (!(siemens_tag_contract.present && siemens_csa_contract.present)
        || diffusion_contracts_agree(&siemens_tag_contract, &siemens_csa_contract));
    let mut siemens_signatures = siemens_tag_contract.signatures;
    for signature in siemens_csa_contract.signatures {
        if !siemens_signatures
            .iter()
            .any(|existing| diffusion_signatures_agree(existing, &signature))
        {
            siemens_signatures.push(signature);
        }
    }
    let siemens_contract = DiffusionContract {
        present: siemens_tag_contract.present || siemens_csa_contract.present,
        valid: siemens_valid,
        semantic: siemens_tag_contract.semantic || siemens_csa_contract.semantic,
        signatures: siemens_signatures,
    };

    let philips_b = unique_private_numbers(
        object,
        0x2001,
        0x0003,
        "Philips Imaging DD 001",
        VR::FL,
        1,
        |values| (0.0..=1.0e6).contains(&values[0]),
    );
    let philips_direction = unique_private_code(
        object,
        0x2001,
        0x0004,
        "Philips Imaging DD 001",
        &["AP", "FH", "RL", "NONE", "ISOTROPIC", "DIRECTIONAL"],
    );
    let philips_rl = unique_private_numbers(
        object,
        0x2005,
        0x00b0,
        "Philips MR Imaging DD 001",
        VR::FL,
        1,
        |values| (-1.1..=1.1).contains(&values[0]),
    );
    let philips_ap = unique_private_numbers(
        object,
        0x2005,
        0x00b1,
        "Philips MR Imaging DD 001",
        VR::FL,
        1,
        |values| (-1.1..=1.1).contains(&values[0]),
    );
    let philips_fh = unique_private_numbers(
        object,
        0x2005,
        0x00b2,
        "Philips MR Imaging DD 001",
        VR::FL,
        1,
        |values| (-1.1..=1.1).contains(&values[0]),
    );
    // DD 005 b-value and gradient-orientation indices are useful provenance,
    // but are references into a vendor table rather than a complete diffusion
    // source. The sanitizer validates and retains them independently; they must
    // not poison a complete public or DD 001 scientific contract.
    let philips_present = philips_b.present
        || philips_direction.present
        || philips_rl.present
        || philips_ap.present
        || philips_fh.present;
    let philips_fields_well_formed = private_numeric_well_formed(&philips_b)
        && private_code_well_formed(&philips_direction)
        && private_numeric_well_formed(&philips_rl)
        && private_numeric_well_formed(&philips_ap)
        && private_numeric_well_formed(&philips_fh);
    let philips_vector = || {
        Some([
            *philips_rl.values.as_ref()?.first()?,
            *philips_ap.values.as_ref()?.first()?,
            *philips_fh.values.as_ref()?.first()?,
        ])
    };
    let philips_valid = philips_fields_well_formed
        && philips_b.values.as_ref().is_some_and(|values| {
            let direction = philips_direction.value.as_deref();
            let vector = philips_vector();
            if values[0] <= 1.0 {
                return matches!(direction, None | Some("NONE"))
                    && vector
                        .as_ref()
                        .is_none_or(|values| numeric_vector_is_zero(values));
            }
            match direction {
                Some("ISOTROPIC") => vector.is_none(),
                Some("AP" | "FH" | "RL" | "DIRECTIONAL") => vector
                    .as_ref()
                    .is_some_and(|values| unit_direction_vector(values)),
                _ => false,
            }
        });
    let philips_semantic = philips_b
        .values
        .as_ref()
        .is_some_and(|values| values[0] > 1.0)
        || philips_direction
            .value
            .as_deref()
            .is_some_and(|value| value != "NONE");
    let philips_signature = if philips_valid {
        philips_b.values.as_ref().and_then(|values| {
            diffusion_signature(
                *values.first()?,
                philips_direction.value.as_deref().or(Some("NONE")),
                philips_vector().as_ref().map(|values| values.as_slice()),
                None,
            )
        })
    } else {
        None
    };
    let philips_contract = single_source_diffusion_contract(
        philips_present,
        philips_valid && philips_signature.is_some(),
        philips_semantic,
        philips_signature,
    );

    let ge_b = unique_private_numbers(
        object,
        0x0043,
        0x0039,
        "GEMS_PARM_01",
        VR::IS,
        4,
        |values| {
            (0.0..=1.0e6).contains(&values[0])
                && values[1..]
                    .iter()
                    .all(|value| (-1.0e9..=1.0e9).contains(value))
                && values.iter().all(|value| value.fract() == 0.0)
        },
    );
    let ge_x = unique_private_numbers(
        object,
        0x0019,
        0x00bb,
        "GEMS_ACQU_01",
        VR::DS,
        1,
        |values| (-1.1..=1.1).contains(&values[0]),
    );
    let ge_y = unique_private_numbers(
        object,
        0x0019,
        0x00bc,
        "GEMS_ACQU_01",
        VR::DS,
        1,
        |values| (-1.1..=1.1).contains(&values[0]),
    );
    let ge_z = unique_private_numbers(
        object,
        0x0019,
        0x00bd,
        "GEMS_ACQU_01",
        VR::DS,
        1,
        |values| (-1.1..=1.1).contains(&values[0]),
    );
    let ge_present = ge_b.present || ge_x.present || ge_y.present || ge_z.present;
    let ge_fields_well_formed = private_numeric_well_formed(&ge_b)
        && private_numeric_well_formed(&ge_x)
        && private_numeric_well_formed(&ge_y)
        && private_numeric_well_formed(&ge_z);
    let ge_vector = || {
        Some([
            *ge_x.values.as_ref()?.first()?,
            *ge_y.values.as_ref()?.first()?,
            *ge_z.values.as_ref()?.first()?,
        ])
    };
    let ge_valid = ge_fields_well_formed
        && ge_b.values.as_ref().is_some_and(|values| {
            let vector = ge_vector();
            if values[0] <= 1.0 {
                vector
                    .as_ref()
                    .is_none_or(|values| numeric_vector_is_zero(values))
            } else {
                vector
                    .as_ref()
                    .is_some_and(|values| unit_direction_vector(values))
            }
        });
    let ge_semantic = ge_b.values.as_ref().is_some_and(|values| values[0] > 1.0);
    let ge_signature = if ge_valid {
        ge_b.values.as_ref().and_then(|values| {
            diffusion_signature(
                *values.first()?,
                Some(if values[0] <= 1.0 {
                    "NONE"
                } else {
                    "DIRECTIONAL"
                }),
                ge_vector().as_ref().map(|values| values.as_slice()),
                None,
            )
        })
    } else {
        None
    };
    let ge_contract = single_source_diffusion_contract(
        ge_present,
        ge_valid && ge_signature.is_some(),
        ge_semantic,
        ge_signature,
    );

    let uih_b = unique_private_numbers(
        object,
        0x0065,
        0x0009,
        "Image Private Header",
        VR::FD,
        1,
        |values| (0.0..=1.0e6).contains(&values[0]),
    );
    let uih_gradient = unique_private_numbers(
        object,
        0x0065,
        0x0037,
        "Image Private Header",
        VR::FD,
        3,
        |values| values.iter().all(|value| (-1.1..=1.1).contains(value)),
    );
    let uih_present = uih_b.present || uih_gradient.present;
    let uih_fields_well_formed =
        private_numeric_well_formed(&uih_b) && private_numeric_well_formed(&uih_gradient);
    let uih_valid = uih_fields_well_formed
        && uih_b.values.as_ref().is_some_and(|values| {
            if values[0] <= 1.0 {
                uih_gradient
                    .values
                    .as_deref()
                    .is_none_or(numeric_vector_is_zero)
            } else {
                uih_gradient
                    .values
                    .as_deref()
                    .is_some_and(unit_direction_vector)
            }
        });
    let uih_semantic = uih_b.values.as_ref().is_some_and(|values| values[0] > 1.0);
    let uih_signature = if uih_valid {
        uih_b.values.as_ref().and_then(|values| {
            diffusion_signature(
                *values.first()?,
                Some(if values[0] <= 1.0 {
                    "NONE"
                } else {
                    "DIRECTIONAL"
                }),
                uih_gradient.values.as_deref(),
                None,
            )
        })
    } else {
        None
    };
    let uih_contract = single_source_diffusion_contract(
        uih_present,
        uih_valid && uih_signature.is_some(),
        uih_semantic,
        uih_signature,
    );

    let contracts = [
        siemens_contract,
        philips_contract,
        ge_contract,
        uih_contract,
    ];
    let present_count = contracts.iter().filter(|contract| contract.present).count();
    let semantic = contracts.iter().any(|contract| contract.semantic);
    if present_count != 1 {
        return DiffusionContract {
            present: present_count > 0,
            valid: false,
            semantic,
            signatures: contracts
                .into_iter()
                .flat_map(|contract| contract.signatures)
                .collect(),
        };
    }
    contracts
        .into_iter()
        .find(|contract| contract.present)
        .unwrap_or_default()
}

fn reviewed_private_asl_metadata_contract(
    object: &dicom_object::InMemDicomObject,
) -> PrivateAslContract {
    let public_technique = direct_code(
        object,
        Tag(0x0018, 0x9250),
        &["CONTINUOUS", "PSEUDOCONTINUOUS", "PULSED"],
    );
    let ge_technique = unique_private_code(
        object,
        0x0043,
        0x00a3,
        "GEMS_PARM_01",
        &["CONTINUOUS", "PSEUDOCONTINUOUS", "PULSED"],
    );
    let ge_duration = unique_private_numbers(
        object,
        0x0043,
        0x00a5,
        "GEMS_PARM_01",
        VR::IS,
        1,
        |values| (0.0..=100_000_000.0).contains(&values[0]) && values[0].fract() == 0.0,
    );
    let ge_present = ge_technique.present || ge_duration.present;
    let ge_valid = private_code_well_formed(&ge_technique)
        && private_numeric_well_formed(&ge_duration)
        && (!(ge_technique.present && public_technique.is_some())
            || ge_technique.value == public_technique);

    let philips_label = unique_private_code(
        object,
        0x2005,
        0x0029,
        "Philips MR Imaging DD 005",
        &["LABEL", "CONTROL", "LBL", "CTL", "M_ZERO_SCAN"],
    );
    let philips_present = philips_label.present;
    let philips_label = philips_label
        .value
        .as_deref()
        .and_then(canonical_philips_asl_label)
        .map(str::to_owned);
    let public_pld_or_trigger = direct_numbers(object, Tag(0x0018, 0x0082), VR::DS, 1)
        .or_else(|| direct_numbers(object, Tag(0x0018, 0x1060), VR::DS, 1))
        .is_some_and(|values| (0.0..=100_000_000.0).contains(&values[0]));
    let philips_valid =
        philips_label.is_some() && public_technique.is_some() && public_pld_or_trigger;
    PrivateAslContract {
        philips_present,
        philips_valid,
        philips_label,
        ge_present,
        ge_valid,
    }
}

fn canonical_philips_asl_label(value: &str) -> Option<&'static str> {
    match value {
        "LABEL" | "LBL" => Some("LABEL"),
        "CONTROL" | "CTL" => Some("CONTROL"),
        "M_ZERO_SCAN" => Some("M_ZERO_SCAN"),
        _ => None,
    }
}

#[derive(Debug, Default)]
struct PrivateAslContract {
    philips_present: bool,
    philips_valid: bool,
    philips_label: Option<String>,
    ge_present: bool,
    ge_valid: bool,
}

#[derive(Debug, Default)]
struct AslContract {
    present: bool,
    valid: bool,
    contexts: BTreeSet<String>,
}

impl AslContract {
    #[cfg(test)]
    fn flags(&self) -> (bool, bool) {
        (self.present, self.valid)
    }
}

#[cfg(test)]
impl PartialEq<(bool, bool)> for AslContract {
    fn eq(&self, other: &(bool, bool)) -> bool {
        self.flags() == *other
    }
}

fn philips_private_asl_agrees_with_public(
    storage_kind: MrSopStorageKind,
    public: &AslContract,
    private: &PrivateAslContract,
) -> bool {
    if !private.philips_present {
        return true;
    }
    let Some(private_label) = private.philips_label.as_deref() else {
        return false;
    };
    match storage_kind {
        // Classic Philips may carry only the creator-mapped DD005 label plus
        // the public technique/timing fields. If it also carries a public ASL
        // macro, the per-image state must be singular and agree exactly.
        MrSopStorageKind::Classic if !public.present => true,
        MrSopStorageKind::Classic | MrSopStorageKind::Enhanced => {
            public.valid && public.contexts.len() == 1 && public.contexts.contains(private_label)
        }
        // A private label on an unknown storage representation cannot be
        // assigned safely to frames.
        MrSopStorageKind::Other => false,
    }
}

fn public_diffusion_gradient(object: &dicom_object::InMemDicomObject) -> Option<[f64; 3]> {
    if let Some(values) = direct_numbers(object, Tag(0x0018, 0x9089), VR::FD, 3) {
        return values.try_into().ok();
    }
    let items = object.element(Tag(0x0018, 0x9076)).ok()?.value().items()?;
    (items.len() == 1)
        .then(|| direct_numbers(&items[0], Tag(0x0018, 0x9089), VR::FD, 3))
        .flatten()?
        .try_into()
        .ok()
}

fn public_diffusion_b_matrix(object: &dicom_object::InMemDicomObject) -> Option<[f64; 6]> {
    let direct = [0x9602, 0x9603, 0x9604, 0x9605, 0x9606, 0x9607]
        .map(|element| direct_numbers(object, Tag(0x0018, element), VR::FD, 1));
    if direct.iter().all(Option::is_some) {
        return Some(direct.map(|values| values.expect("all matrix members checked")[0]));
    }
    let items = object.element(Tag(0x0018, 0x9601)).ok()?.value().items()?;
    (items.len() == 1).then_some(())?;
    [0x9602, 0x9603, 0x9604, 0x9605, 0x9606, 0x9607]
        .map(|element| direct_numbers(&items[0], Tag(0x0018, element), VR::FD, 1))
        .into_iter()
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .map(|values| values[0])
        .collect::<Vec<_>>()
        .try_into()
        .ok()
}

fn public_diffusion_signature(item: &dicom_object::InMemDicomObject) -> Option<DiffusionSignature> {
    let b_value = direct_numbers(item, Tag(0x0018, 0x9087), VR::FD, 1)?[0];
    let directionality = direct_code(
        item,
        Tag(0x0018, 0x9075),
        &["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"],
    )?;
    let gradient = public_diffusion_gradient(item);
    let b_matrix = public_diffusion_b_matrix(item);
    diffusion_signature(
        b_value,
        Some(directionality.as_str()),
        gradient.as_ref().map(|values| values.as_slice()),
        b_matrix.as_ref().map(|values| values.as_slice()),
    )
}

fn public_diffusion_macro_signature(
    container: &dicom_object::InMemDicomObject,
) -> Option<DiffusionSignature> {
    let items = container
        .element(Tag(0x0018, 0x9117))
        .ok()?
        .value()
        .items()?;
    (items.len() == 1)
        .then(|| public_diffusion_signature(&items[0]))
        .flatten()
}

fn push_unique_diffusion_signature(
    signatures: &mut Vec<DiffusionSignature>,
    signature: DiffusionSignature,
) {
    if !signatures
        .iter()
        .any(|existing| diffusion_signatures_agree(existing, &signature))
    {
        signatures.push(signature);
    }
}

fn enhanced_public_diffusion_signatures(
    object: &dicom_object::InMemDicomObject,
) -> Option<Vec<DiffusionSignature>> {
    let (origins, shared_item, per_frame_items) = enhanced_frame_origins(object)?;
    let mut signatures = Vec::new();
    if let Some(shared_item) = shared_item {
        if shared_item.element(Tag(0x0018, 0x9117)).is_ok() {
            push_unique_diffusion_signature(
                &mut signatures,
                public_diffusion_macro_signature(shared_item)?,
            );
            return Some(signatures);
        }
    }
    for (item, origin) in per_frame_items.iter().zip(origins) {
        if origin == FrameOrigin::Original {
            push_unique_diffusion_signature(
                &mut signatures,
                public_diffusion_macro_signature(item)?,
            );
        }
    }
    Some(signatures)
}

fn public_diffusion_metadata_contract(
    object: &dicom_object::InMemDicomObject,
) -> DiffusionContract {
    let direct_root_present = [
        Tag(0x0018, 0x9117),
        Tag(0x0018, 0x9087),
        Tag(0x0018, 0x9075),
        Tag(0x0018, 0x9076),
        Tag(0x0018, 0x9089),
        Tag(0x0018, 0x9601),
        Tag(0x0018, 0x9602),
        Tag(0x0018, 0x9603),
        Tag(0x0018, 0x9604),
        Tag(0x0018, 0x9605),
        Tag(0x0018, 0x9606),
        Tag(0x0018, 0x9607),
    ]
    .into_iter()
    .any(|tag| object.element(tag).is_ok());
    let recursive_present = contains_recursive_tag(object, Tag(0x0018, 0x9117), 0)
        || contains_recursive_tag(object, Tag(0x0018, 0x9087), 0)
        || contains_recursive_tag(object, Tag(0x0018, 0x9075), 0)
        || contains_recursive_tag(object, Tag(0x0018, 0x9076), 0)
        || contains_recursive_tag(object, Tag(0x0018, 0x9089), 0)
        || contains_recursive_tag(object, Tag(0x0018, 0x9601), 0)
        || (0x9602..=0x9607).any(|element| contains_recursive_tag(object, Tag(0x0018, element), 0));
    let storage_kind = mr_sop_storage_kind(object);
    let (present, mut valid, semantic) = match storage_kind {
        MrSopStorageKind::Classic => (
            direct_root_present,
            direct_root_present && valid_classic_public_diffusion_root(object),
            direct_root_present && classic_public_diffusion_semantic_evidence(object),
        ),
        MrSopStorageKind::Enhanced => (
            true,
            !direct_root_present && enhanced_diffusion_contract_complete(object),
            recursive_diffusion_macro_semantic_evidence(object, 0),
        ),
        MrSopStorageKind::Other => (recursive_present, false, false),
    };
    let signatures = if valid {
        match storage_kind {
            MrSopStorageKind::Classic => {
                let signature = if object.element(Tag(0x0018, 0x9117)).is_ok() {
                    public_diffusion_macro_signature(object)
                } else {
                    public_diffusion_signature(object)
                };
                signature.into_iter().collect()
            }
            MrSopStorageKind::Enhanced => {
                enhanced_public_diffusion_signatures(object).unwrap_or_default()
            }
            MrSopStorageKind::Other => Vec::new(),
        }
    } else {
        Vec::new()
    };
    if valid && storage_kind == MrSopStorageKind::Classic && signatures.is_empty() {
        valid = false;
    }
    DiffusionContract {
        present,
        valid,
        semantic: semantic || diffusion_semantic(&signatures),
        signatures,
    }
}

fn collect_direct_asl_contexts(
    container: &dicom_object::InMemDicomObject,
    contexts: &mut BTreeSet<String>,
) -> bool {
    let Ok(element) = container.element(Tag(0x0018, 0x9251)) else {
        return false;
    };
    let Some(items) = element.value().items() else {
        return false;
    };
    items.iter().all(|item| {
        let Some(context) = direct_code(
            item,
            Tag(0x0018, 0x9257),
            &["LABEL", "CONTROL", "M_ZERO_SCAN"],
        ) else {
            return false;
        };
        contexts.insert(context);
        true
    })
}

fn public_asl_metadata_contract(object: &dicom_object::InMemDicomObject) -> AslContract {
    let technique = direct_code(
        object,
        Tag(0x0018, 0x9250),
        &["CONTINUOUS", "PSEUDOCONTINUOUS", "PULSED"],
    );
    let recursive_macro_present = contains_recursive_tag(object, Tag(0x0018, 0x9251), 0)
        || contains_recursive_tag(object, Tag(0x0018, 0x9257), 0);
    let direct_macro_present =
        object.element(Tag(0x0018, 0x9251)).is_ok() || object.element(Tag(0x0018, 0x9257)).is_ok();
    let storage_kind = mr_sop_storage_kind(object);
    let (present, valid) = match storage_kind {
        MrSopStorageKind::Classic => (
            direct_macro_present,
            direct_macro_present
                && technique.is_some()
                && direct_macro_state(object, Tag(0x0018, 0x9251), one_or_more_valid_asl_items)
                    == MacroState::Valid,
        ),
        MrSopStorageKind::Enhanced => {
            let present = technique.is_some() || recursive_macro_present;
            (
                present,
                present
                    && technique.is_some()
                    && functional_group_macro_complete(
                        object,
                        Tag(0x0018, 0x9251),
                        one_or_more_valid_asl_items,
                    ),
            )
        }
        MrSopStorageKind::Other => (recursive_macro_present, false),
    };
    let mut contexts = BTreeSet::new();
    if valid {
        match storage_kind {
            MrSopStorageKind::Classic => {
                if !collect_direct_asl_contexts(object, &mut contexts) {
                    return AslContract {
                        present,
                        valid: false,
                        contexts,
                    };
                }
            }
            MrSopStorageKind::Enhanced => {
                let mut collected_shared = false;
                if let Ok(shared) = object.element(Tag(0x5200, 0x9229)) {
                    if let Some(items) = shared.value().items() {
                        if items.len() == 1 && items[0].element(Tag(0x0018, 0x9251)).is_ok() {
                            collected_shared = true;
                            if !collect_direct_asl_contexts(&items[0], &mut contexts) {
                                return AslContract {
                                    present,
                                    valid: false,
                                    contexts,
                                };
                            }
                        }
                    }
                }
                if !collected_shared {
                    if let Ok(per_frame) = object.element(Tag(0x5200, 0x9230)) {
                        if let Some(items) = per_frame.value().items() {
                            if !items
                                .iter()
                                .all(|item| collect_direct_asl_contexts(item, &mut contexts))
                            {
                                return AslContract {
                                    present,
                                    valid: false,
                                    contexts,
                                };
                            }
                        }
                    }
                }
            }
            MrSopStorageKind::Other => {}
        }
    }
    AslContract {
        present,
        valid,
        contexts,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MrSopStorageKind {
    Classic,
    Enhanced,
    Other,
}

fn mr_sop_storage_kind(object: &dicom_object::InMemDicomObject) -> MrSopStorageKind {
    let Some(sop_class_uid) = direct_text(object, Tag(0x0008, 0x0016), VR::UI) else {
        return MrSopStorageKind::Other;
    };
    match sop_class_uid.as_str() {
        MR_IMAGE_STORAGE_UID => MrSopStorageKind::Classic,
        ENHANCED_MR_IMAGE_STORAGE_UID | LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID => {
            MrSopStorageKind::Enhanced
        }
        _ => MrSopStorageKind::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacroState {
    Absent,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameOrigin {
    Original,
    Derived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameOriginState {
    Absent,
    Valid(FrameOrigin),
    Invalid,
}

/// Validate Enhanced MR frame provenance and diffusion atomically. ORIGINAL
/// frames require one complete MR Diffusion Macro. DERIVED frames must not
/// borrow or imply an acquired gradient. Shared and per-frame representations
/// are mutually exclusive, and the root ImageType summary must agree with the
/// frame-level FrameType values.
fn enhanced_diffusion_contract_complete(object: &dicom_object::InMemDicomObject) -> bool {
    let Some((origins, shared_item, per_frame_items)) = enhanced_frame_origins(object) else {
        return false;
    };

    let shared_diffusion = shared_item
        .map(|item| direct_macro_state(item, Tag(0x0018, 0x9117), exactly_one_valid_diffusion_item))
        .unwrap_or(MacroState::Absent);
    if shared_diffusion == MacroState::Invalid {
        return false;
    }

    match shared_diffusion {
        MacroState::Valid => {
            origins
                .iter()
                .all(|origin| *origin == FrameOrigin::Original)
                && per_frame_items.iter().all(|item| {
                    direct_macro_state(item, Tag(0x0018, 0x9117), exactly_one_valid_diffusion_item)
                        == MacroState::Absent
                })
        }
        MacroState::Absent => {
            per_frame_items.len() == origins.len()
                && per_frame_items
                    .iter()
                    .zip(origins.iter())
                    .all(|(item, origin)| {
                        let state = direct_macro_state(
                            item,
                            Tag(0x0018, 0x9117),
                            exactly_one_valid_diffusion_item,
                        );
                        match origin {
                            FrameOrigin::Original => state == MacroState::Valid,
                            FrameOrigin::Derived => state == MacroState::Absent,
                        }
                    })
        }
        MacroState::Invalid => false,
    }
}

fn enhanced_frame_origins(
    object: &dicom_object::InMemDicomObject,
) -> Option<(
    Vec<FrameOrigin>,
    Option<&dicom_object::InMemDicomObject>,
    &[dicom_object::InMemDicomObject],
)> {
    let legacy = direct_text(object, Tag(0x0008, 0x0016), VR::UI)
        .is_some_and(|value| value == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID);
    let number_of_frames = integer_from_object(object, Tag(0x0028, 0x0008))?;
    let number_of_frames = usize::try_from(number_of_frames)
        .ok()
        .filter(|count| *count > 0)?;

    let shared_items = match object.element(Tag(0x5200, 0x9229)) {
        Ok(element) if element.vr() == VR::SQ => {
            let items = element.value().items()?;
            (items.len() == 1).then_some(items)?
        }
        Ok(_) => return None,
        Err(_) => &[],
    };
    let per_frame_items = match object.element(Tag(0x5200, 0x9230)) {
        Ok(element) if element.vr() == VR::SQ => {
            let items = element.value().items()?;
            (items.len() == number_of_frames).then_some(items)?
        }
        Ok(_) => return None,
        Err(_) => &[],
    };
    let shared_item = shared_items.first();
    let shared_origin = shared_item
        .map(|item| direct_frame_origin_state(item, legacy))
        .unwrap_or(FrameOriginState::Absent);
    if shared_origin == FrameOriginState::Invalid {
        return None;
    }

    let origins = match shared_origin {
        FrameOriginState::Valid(origin) => {
            if per_frame_items
                .iter()
                .any(|item| direct_frame_origin_state(item, legacy) != FrameOriginState::Absent)
            {
                return None;
            }
            vec![origin; number_of_frames]
        }
        FrameOriginState::Absent => {
            if per_frame_items.is_empty() {
                return None;
            }
            per_frame_items
                .iter()
                .map(|item| match direct_frame_origin_state(item, legacy) {
                    FrameOriginState::Valid(origin) => Some(origin),
                    FrameOriginState::Absent | FrameOriginState::Invalid => None,
                })
                .collect::<Option<Vec<_>>>()?
        }
        FrameOriginState::Invalid => return None,
    };

    let root_origin = direct_root_image_origin(object, legacy)?;
    let has_original = origins.contains(&FrameOrigin::Original);
    let has_derived = origins.contains(&FrameOrigin::Derived);
    let summary_matches = match root_origin.as_str() {
        "ORIGINAL" => has_original && !has_derived,
        "DERIVED" => has_derived && !has_original,
        "MIXED" => has_original && has_derived,
        _ => false,
    };
    summary_matches.then_some((origins, shared_item, per_frame_items))
}

fn direct_frame_origin_state(
    functional_group_item: &dicom_object::InMemDicomObject,
    legacy: bool,
) -> FrameOriginState {
    let Ok(sequence) = functional_group_item.element(Tag(0x0018, 0x9226)) else {
        return FrameOriginState::Absent;
    };
    if sequence.vr() != VR::SQ {
        return FrameOriginState::Invalid;
    }
    let Some(items) = sequence.value().items() else {
        return FrameOriginState::Invalid;
    };
    if items.len() != 1 {
        return FrameOriginState::Invalid;
    }
    let Some(value) = items[0]
        .element(Tag(0x0008, 0x9007))
        .ok()
        .filter(|element| element.vr() == VR::CS)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| {
            crate::archive::canonical_enhanced_mr_type_for_scientific_contract(
                value.as_ref(),
                true,
                legacy,
            )
        })
        .map(|value| value.split('\\').map(str::to_owned).collect::<Vec<_>>())
    else {
        return FrameOriginState::Invalid;
    };
    match value[0].as_str() {
        "ORIGINAL" => FrameOriginState::Valid(FrameOrigin::Original),
        "DERIVED" => FrameOriginState::Valid(FrameOrigin::Derived),
        // A frame itself cannot be safely assigned acquired semantics from a
        // MIXED summary. The root may be MIXED, but each frame must resolve.
        "MIXED" => FrameOriginState::Invalid,
        _ => FrameOriginState::Invalid,
    }
}

fn direct_root_image_origin(
    object: &dicom_object::InMemDicomObject,
    legacy: bool,
) -> Option<String> {
    let value = object
        .element(Tag(0x0008, 0x0008))
        .ok()
        .filter(|element| element.vr() == VR::CS)
        .and_then(|element| element.to_str().ok())?;
    crate::archive::canonical_enhanced_mr_type_for_scientific_contract(
        value.as_ref(),
        false,
        legacy,
    )
    .and_then(|value| value.split('\\').next().map(str::to_owned))
}

/// Prove frame coverage without mixing evidence between functional-group
/// items. A macro may be shared by every frame, or present once in every
/// per-frame item, but the two representations cannot be combined.
fn functional_group_macro_complete(
    object: &dicom_object::InMemDicomObject,
    macro_tag: Tag,
    validate_items: fn(&[dicom_object::InMemDicomObject]) -> bool,
) -> bool {
    let shared = match object.element(Tag(0x5200, 0x9229)) {
        Ok(element) => {
            if element.vr() != VR::SQ {
                return false;
            }
            let Some(items) = element.value().items() else {
                return false;
            };
            if items.len() != 1 {
                return false;
            }
            direct_macro_state(&items[0], macro_tag, validate_items)
        }
        Err(_) => MacroState::Absent,
    };
    if shared == MacroState::Invalid {
        return false;
    }

    let per_frame = match object.element(Tag(0x5200, 0x9230)) {
        Ok(element) => {
            if element.vr() != VR::SQ {
                return false;
            }
            let Some(items) = element.value().items() else {
                return false;
            };
            let Some(number_of_frames) = integer_from_object(object, Tag(0x0028, 0x0008)) else {
                return false;
            };
            if number_of_frames <= 0 || items.len() != number_of_frames as usize {
                return false;
            }
            Some(items)
        }
        Err(_) => None,
    };

    match shared {
        MacroState::Valid => match per_frame {
            None => true,
            Some(items) => items.iter().all(|item| {
                direct_macro_state(item, macro_tag, validate_items) == MacroState::Absent
            }),
        },
        MacroState::Absent => per_frame.is_some_and(|items| {
            !items.is_empty()
                && items.iter().all(|item| {
                    direct_macro_state(item, macro_tag, validate_items) == MacroState::Valid
                })
        }),
        MacroState::Invalid => false,
    }
}

fn direct_macro_state(
    functional_group_item: &dicom_object::InMemDicomObject,
    macro_tag: Tag,
    validate_items: fn(&[dicom_object::InMemDicomObject]) -> bool,
) -> MacroState {
    let Ok(element) = functional_group_item.element(macro_tag) else {
        return MacroState::Absent;
    };
    if element.vr() != VR::SQ {
        return MacroState::Invalid;
    }
    let Some(items) = element.value().items() else {
        return MacroState::Invalid;
    };
    if validate_items(items) {
        MacroState::Valid
    } else {
        MacroState::Invalid
    }
}

fn exactly_one_valid_diffusion_item(items: &[dicom_object::InMemDicomObject]) -> bool {
    items.len() == 1 && valid_public_diffusion_item(&items[0])
}

fn one_or_more_valid_asl_items(items: &[dicom_object::InMemDicomObject]) -> bool {
    !items.is_empty() && items.iter().all(valid_public_asl_item)
}

fn valid_classic_public_diffusion_root(object: &dicom_object::InMemDicomObject) -> bool {
    if object.element(Tag(0x0018, 0x9117)).is_ok() {
        let has_loose_fields = [
            Tag(0x0018, 0x9087),
            Tag(0x0018, 0x9075),
            Tag(0x0018, 0x9076),
            Tag(0x0018, 0x9089),
            Tag(0x0018, 0x9601),
            Tag(0x0018, 0x9602),
            Tag(0x0018, 0x9603),
            Tag(0x0018, 0x9604),
            Tag(0x0018, 0x9605),
            Tag(0x0018, 0x9606),
            Tag(0x0018, 0x9607),
        ]
        .into_iter()
        .any(|tag| object.element(tag).is_ok());
        return !has_loose_fields
            && direct_macro_state(
                object,
                Tag(0x0018, 0x9117),
                exactly_one_valid_diffusion_item,
            ) == MacroState::Valid;
    }

    let Some(b_value) = direct_numbers(object, Tag(0x0018, 0x9087), VR::FD, 1)
        .filter(|values| (0.0..=1.0e6).contains(&values[0]))
        .map(|values| values[0])
    else {
        return false;
    };
    let Some(directionality) = direct_code(
        object,
        Tag(0x0018, 0x9075),
        &["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"],
    ) else {
        return false;
    };
    let direct_gradient = direct_numeric_state(object, Tag(0x0018, 0x9089), VR::FD, 3, |values| {
        values.iter().all(|value| (-1.1..=1.1).contains(value))
            && (0.5..=1.5).contains(&values.iter().map(|value| value * value).sum::<f64>())
    });
    let sequence_gradient = direct_optional_single_item_sequence(
        object,
        Tag(0x0018, 0x9076),
        valid_public_diffusion_gradient,
    );
    let gradient = exclusive_representation(direct_gradient, sequence_gradient);
    let direct_matrix = direct_b_matrix_state(object);
    let sequence_matrix = direct_optional_single_item_sequence(
        object,
        Tag(0x0018, 0x9601),
        valid_public_diffusion_b_matrix,
    );
    let b_matrix = exclusive_representation(direct_matrix, sequence_matrix);
    match directionality.as_str() {
        "NONE" => {
            b_value <= 1.0 && gradient == MacroState::Absent && b_matrix == MacroState::Absent
        }
        "ISOTROPIC" => {
            b_value > 1.0 && gradient == MacroState::Absent && b_matrix == MacroState::Absent
        }
        "DIRECTIONAL" => {
            b_value > 1.0 && gradient == MacroState::Valid && b_matrix == MacroState::Absent
        }
        "BMATRIX" => {
            b_value > 1.0 && b_matrix == MacroState::Valid && gradient == MacroState::Absent
        }
        _ => false,
    }
}

fn direct_numeric_state(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    vr: VR,
    vm: usize,
    valid: impl Fn(&[f64]) -> bool,
) -> MacroState {
    if object.element(tag).is_err() {
        return MacroState::Absent;
    }
    if direct_numbers(object, tag, vr, vm).is_some_and(|values| valid(&values)) {
        MacroState::Valid
    } else {
        MacroState::Invalid
    }
}

fn direct_b_matrix_state(object: &dicom_object::InMemDicomObject) -> MacroState {
    let states = [0x9602, 0x9603, 0x9604, 0x9605, 0x9606, 0x9607].map(|element| {
        direct_numeric_state(object, Tag(0x0018, element), VR::FD, 1, |values| {
            (-1.0e9..=1.0e9).contains(&values[0])
        })
    });
    if states.iter().all(|state| *state == MacroState::Absent) {
        MacroState::Absent
    } else if states.iter().all(|state| *state == MacroState::Valid) {
        MacroState::Valid
    } else {
        MacroState::Invalid
    }
}

fn exclusive_representation(left: MacroState, right: MacroState) -> MacroState {
    match (left, right) {
        (MacroState::Absent, MacroState::Absent) => MacroState::Absent,
        (MacroState::Valid, MacroState::Absent) | (MacroState::Absent, MacroState::Valid) => {
            MacroState::Valid
        }
        _ => MacroState::Invalid,
    }
}

fn classic_public_diffusion_semantic_evidence(object: &dicom_object::InMemDicomObject) -> bool {
    if let Ok(element) = object.element(Tag(0x0018, 0x9117)) {
        return element
            .value()
            .items()
            .and_then(|items| items.first())
            .is_some_and(public_diffusion_item_semantic_evidence);
    }
    direct_numbers(object, Tag(0x0018, 0x9087), VR::FD, 1).is_some_and(|values| values[0] > 1.0)
        || direct_code(
            object,
            Tag(0x0018, 0x9075),
            &["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"],
        )
        .is_some_and(|value| matches!(value.as_str(), "ISOTROPIC" | "DIRECTIONAL" | "BMATRIX"))
}

fn recursive_diffusion_macro_semantic_evidence(
    object: &dicom_object::InMemDicomObject,
    depth: usize,
) -> bool {
    if depth > 32 {
        return false;
    }
    if public_diffusion_item_semantic_evidence(object) {
        return true;
    }
    object.iter().any(|element| {
        element.value().items().is_some_and(|items| {
            items
                .iter()
                .any(|item| recursive_diffusion_macro_semantic_evidence(item, depth + 1))
        })
    })
}

fn public_diffusion_item_semantic_evidence(item: &dicom_object::InMemDicomObject) -> bool {
    direct_numbers(item, Tag(0x0018, 0x9087), VR::FD, 1).is_some_and(|values| values[0] > 1.0)
        || direct_code(
            item,
            Tag(0x0018, 0x9075),
            &["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"],
        )
        .is_some_and(|value| matches!(value.as_str(), "ISOTROPIC" | "DIRECTIONAL" | "BMATRIX"))
}

fn valid_public_diffusion_item(item: &dicom_object::InMemDicomObject) -> bool {
    let Some(b_value) = direct_numbers(item, Tag(0x0018, 0x9087), VR::FD, 1)
        .filter(|values| (0.0..=1.0e6).contains(&values[0]))
        .map(|values| values[0])
    else {
        return false;
    };
    let Some(directionality) = direct_code(
        item,
        Tag(0x0018, 0x9075),
        &["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"],
    ) else {
        return false;
    };
    let gradient = direct_optional_single_item_sequence(
        item,
        Tag(0x0018, 0x9076),
        valid_public_diffusion_gradient,
    );
    let b_matrix = direct_optional_single_item_sequence(
        item,
        Tag(0x0018, 0x9601),
        valid_public_diffusion_b_matrix,
    );
    match directionality.as_str() {
        "NONE" => {
            b_value <= 1.0 && gradient == MacroState::Absent && b_matrix == MacroState::Absent
        }
        "ISOTROPIC" => {
            b_value > 1.0 && gradient == MacroState::Absent && b_matrix == MacroState::Absent
        }
        "DIRECTIONAL" => {
            b_value > 1.0 && gradient == MacroState::Valid && b_matrix == MacroState::Absent
        }
        "BMATRIX" => {
            b_value > 1.0 && b_matrix == MacroState::Valid && gradient == MacroState::Absent
        }
        _ => false,
    }
}

fn direct_optional_single_item_sequence(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    validate_item: fn(&dicom_object::InMemDicomObject) -> bool,
) -> MacroState {
    let Ok(element) = object.element(tag) else {
        return MacroState::Absent;
    };
    if element.vr() != VR::SQ {
        return MacroState::Invalid;
    }
    let Some(items) = element.value().items() else {
        return MacroState::Invalid;
    };
    if items.len() == 1 && validate_item(&items[0]) {
        MacroState::Valid
    } else {
        MacroState::Invalid
    }
}

fn valid_public_diffusion_gradient(item: &dicom_object::InMemDicomObject) -> bool {
    direct_numbers(item, Tag(0x0018, 0x9089), VR::FD, 3).is_some_and(|values| {
        values.iter().all(|value| (-1.1..=1.1).contains(value))
            && (0.5..=1.5).contains(&values.iter().map(|value| value * value).sum::<f64>())
    })
}

fn valid_public_diffusion_b_matrix(item: &dicom_object::InMemDicomObject) -> bool {
    [0x9602, 0x9603, 0x9604, 0x9605, 0x9606, 0x9607]
        .into_iter()
        .all(|element| {
            direct_numbers(item, Tag(0x0018, element), VR::FD, 1)
                .is_some_and(|values| (-1.0e9..=1.0e9).contains(&values[0]))
        })
}

fn integer_from_object(object: &dicom_object::InMemDicomObject, tag: Tag) -> Option<i64> {
    object.element(tag).ok()?.to_int::<i64>().ok()
}

fn contains_recursive_tag(object: &dicom_object::InMemDicomObject, tag: Tag, depth: usize) -> bool {
    if depth > 32 {
        return true;
    }
    object.iter().any(|element| {
        element.tag() == tag
            || element.value().items().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| contains_recursive_tag(item, tag, depth + 1))
            })
    })
}

fn valid_public_asl_item(item: &dicom_object::InMemDicomObject) -> bool {
    if !direct_type_two_text_present(item, Tag(0x0018, 0x9252), VR::LO, 64) {
        return false;
    }
    let context = direct_code(
        item,
        Tag(0x0018, 0x9257),
        &["LABEL", "CONTROL", "M_ZERO_SCAN"],
    );
    let crusher = direct_code(item, Tag(0x0018, 0x9259), &["YES", "NO"]);
    let bolus = direct_code(item, Tag(0x0018, 0x925C), &["YES", "NO"]);
    let (Some(context), Some(crusher), Some(bolus)) = (context, crusher, bolus) else {
        return false;
    };
    if !valid_public_asl_crusher_group(item, &crusher)
        || !valid_public_asl_bolus_cutoff_group(item, &bolus)
    {
        return false;
    }
    if context == "M_ZERO_SCAN" {
        return true;
    }
    let Ok(slab_sequence) = item.element(Tag(0x0018, 0x9260)) else {
        return false;
    };
    if slab_sequence.vr() != VR::SQ {
        return false;
    }
    let Some(slabs) = slab_sequence.value().items() else {
        return false;
    };
    !slabs.is_empty() && slabs.iter().all(valid_public_asl_slab)
}

fn valid_public_asl_crusher_group(item: &dicom_object::InMemDicomObject, crusher: &str) -> bool {
    match crusher {
        "NO" => {
            item.element(Tag(0x0018, 0x925A)).is_err() && item.element(Tag(0x0018, 0x925B)).is_err()
        }
        "YES" => {
            direct_numbers(item, Tag(0x0018, 0x925A), VR::FD, 1)
                .is_some_and(|values| values[0] >= 0.0)
                && direct_text(item, Tag(0x0018, 0x925B), VR::LO)
                    .is_some_and(|value| value.chars().count() <= 64)
        }
        _ => false,
    }
}

fn valid_public_asl_bolus_cutoff_group(item: &dicom_object::InMemDicomObject, bolus: &str) -> bool {
    match bolus {
        "NO" => item.element(Tag(0x0018, 0x925D)).is_err(),
        "YES" => {
            let Ok(sequence) = item.element(Tag(0x0018, 0x925D)) else {
                return false;
            };
            if sequence.vr() != VR::SQ {
                return false;
            }
            let Some(items) = sequence.value().items() else {
                return false;
            };
            items.len() == 1
                && direct_type_two_text_present(&items[0], Tag(0x0018, 0x925E), VR::LO, 64)
                && direct_unsigned_integer(&items[0], Tag(0x0018, 0x925F), VR::UL).is_some()
        }
        _ => false,
    }
}

fn valid_public_asl_slab(item: &dicom_object::InMemDicomObject) -> bool {
    direct_unsigned_integer(item, Tag(0x0018, 0x9253), VR::US)
        .is_some_and(|value| (1..=4096).contains(&value))
        && direct_numbers(item, Tag(0x0018, 0x9254), VR::FD, 1)
            .is_some_and(|values| values[0] > 0.0 && values[0] <= 1.0e6)
        && direct_numbers(item, Tag(0x0018, 0x9255), VR::FD, 3).is_some_and(|values| {
            values.iter().all(|value| (-1.1..=1.1).contains(value))
                && (0.5..=1.5).contains(&values.iter().map(|value| value * value).sum::<f64>())
        })
        && direct_numbers(item, Tag(0x0018, 0x9256), VR::FD, 3)
            .is_some_and(|values| values.iter().all(|value| value.abs() <= 1.0e6))
        && direct_unsigned_integer(item, Tag(0x0018, 0x9258), VR::UL)
            .is_some_and(|value| value <= 100_000_000)
}

fn direct_code(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    allowed: &[&str],
) -> Option<String> {
    let element = object.element(tag).ok()?;
    if element.vr() != VR::CS {
        return None;
    }
    let values = element.to_multi_str().ok()?;
    if values.len() != 1 {
        return None;
    }
    let value = values[0].trim_matches([' ', '\0']).to_ascii_uppercase();
    allowed.contains(&value.as_str()).then_some(value)
}

fn direct_text(object: &dicom_object::InMemDicomObject, tag: Tag, vr: VR) -> Option<String> {
    let element = object.element(tag).ok()?;
    if element.vr() != vr {
        return None;
    }
    let values = element.to_multi_str().ok()?;
    if values.len() != 1 {
        return None;
    }
    let value = values[0].trim_matches([' ', '\0']).to_owned();
    (!value.is_empty()).then_some(value)
}

fn direct_type_two_text_present(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    vr: VR,
    max_chars: usize,
) -> bool {
    let Ok(element) = object.element(tag) else {
        return false;
    };
    if element.vr() != vr {
        return false;
    }
    element.to_multi_str().ok().is_some_and(|values| {
        values.len() <= 1
            && values
                .first()
                .is_none_or(|value| value.trim_matches([' ', '\0']).chars().count() <= max_chars)
    })
}

fn direct_numbers(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    vr: VR,
    vm: usize,
) -> Option<Vec<f64>> {
    let element = object.element(tag).ok()?;
    if element.vr() != vr {
        return None;
    }
    element
        .to_multi_float64()
        .ok()
        .filter(|values| values.len() == vm && values.iter().all(|value| value.is_finite()))
}

fn direct_unsigned_integer(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    vr: VR,
) -> Option<u64> {
    let element = object.element(tag).ok()?;
    if element.vr() != vr {
        return None;
    }
    let values = element.to_multi_int::<u64>().ok()?;
    (values.len() == 1).then_some(values[0])
}

fn verify_philips_dynamic_timing_contract(group: &SeriesGroup) -> bool {
    let manufacturer = group
        .representative
        .manufacturer
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    if !(manufacturer.contains("PHILIPS"))
        || group.sop_class_uids.len() != 1
        || group.sop_class_uids[0] != "1.2.840.10008.5.1.4.1.1.4"
        || group.instances.is_empty()
    {
        return false;
    }
    let Some(total_temporal_positions) = group.instances[0]
        .number_of_temporal_positions
        .filter(|value| (3..=100_000).contains(value))
    else {
        return false;
    };
    let Some(number_of_slices) = group.instances[0]
        .philips_number_of_slices
        .filter(|value| (1..=4096).contains(value))
    else {
        return false;
    };
    let Some(repetition_time_ms) = group.instances[0]
        .repetition_time_ms
        .filter(|value| value.is_finite() && (100.0..=20_000.0).contains(value))
    else {
        return false;
    };
    if group.instances.len()
        != usize::try_from(total_temporal_positions)
            .ok()
            .and_then(|temporal| {
                usize::try_from(number_of_slices)
                    .ok()
                    .and_then(|slices| temporal.checked_mul(slices))
            })
            .unwrap_or(usize::MAX)
    {
        return false;
    }

    #[derive(Default)]
    struct TemporalGroup {
        a0_seconds: Vec<f64>,
        trigger_ms: Vec<f64>,
        positions: Vec<[i64; 3]>,
        acquisition_numbers: Vec<i64>,
    }
    let mut temporal = BTreeMap::<i64, TemporalGroup>::new();
    for instance in &group.instances {
        if instance.number_of_temporal_positions != Some(total_temporal_positions)
            || instance.philips_number_of_slices != Some(number_of_slices)
            || !instance.repetition_time_ms.is_some_and(|value| {
                (value - repetition_time_ms).abs()
                    <= 1e-6 * value.abs().max(repetition_time_ms.abs()).max(1.0)
            })
        {
            return false;
        }
        let (Some(temporal_id), Some(a0_seconds), Some(trigger_ms)) = (
            instance.temporal_position_identifier,
            instance.philips_dynamic_scan_begin_time_seconds,
            instance.trigger_time_ms,
        ) else {
            return false;
        };
        if !(1..=total_temporal_positions).contains(&temporal_id)
            || !a0_seconds.is_finite()
            || !(0.0..=86_400.0).contains(&a0_seconds)
            || !trigger_ms.is_finite()
            || !(0.0..=86_400_000.0).contains(&trigger_ms)
            || instance.image_position_patient.len() != 3
            || instance
                .image_position_patient
                .iter()
                .any(|value| !value.is_finite() || value.abs() > 1_000_000.0)
        {
            return false;
        }
        let position = [
            (instance.image_position_patient[0] * 1_000_000.0).round() as i64,
            (instance.image_position_patient[1] * 1_000_000.0).round() as i64,
            (instance.image_position_patient[2] * 1_000_000.0).round() as i64,
        ];
        let group = temporal.entry(temporal_id).or_default();
        group.a0_seconds.push(a0_seconds);
        group.trigger_ms.push(trigger_ms);
        group.positions.push(position);
        if let Some(acquisition) = instance.acquisition_number {
            group.acquisition_numbers.push(acquisition);
        }
    }
    if temporal.len() != total_temporal_positions as usize
        || temporal.keys().copied().ne(1..=total_temporal_positions)
    {
        return false;
    }
    let mut reference_positions = None;
    let mut first_a0 = None;
    let mut prior_a0 = None;
    let mut prior_acquisition = None;
    let mut trigger_sequence_ms = Vec::with_capacity(temporal.len());
    for (temporal_id, group) in temporal {
        if group.a0_seconds.len() != number_of_slices as usize
            || group.trigger_ms.len() != number_of_slices as usize
            || group.positions.len() != number_of_slices as usize
            || !all_float_equal(&group.a0_seconds, 1e-6)
            || !all_float_equal(&group.trigger_ms, 1e-3)
        {
            return false;
        }
        let mut positions = group.positions;
        positions.sort_unstable();
        positions.dedup();
        if positions.len() != number_of_slices as usize {
            return false;
        }
        match &reference_positions {
            None => reference_positions = Some(positions),
            Some(reference) if *reference == positions => {}
            Some(_) => return false,
        }
        let a0 = group.a0_seconds[0];
        trigger_sequence_ms.push(group.trigger_ms[0]);
        let origin = *first_a0.get_or_insert(a0);
        if prior_a0.is_some_and(|prior| a0 <= prior + 1e-6) {
            return false;
        }
        let expected = origin + (temporal_id - 1) as f64 * repetition_time_ms / 1_000.0;
        let tolerance = 0.005_f64.max(repetition_time_ms / 1_000.0 * 1e-4);
        if (a0 - expected).abs() > tolerance {
            return false;
        }
        prior_a0 = Some(a0);

        if !group.acquisition_numbers.is_empty() {
            if group.acquisition_numbers.len() != number_of_slices as usize
                || group
                    .acquisition_numbers
                    .iter()
                    .any(|value| *value != group.acquisition_numbers[0])
                || prior_acquisition.is_some_and(|prior| group.acquisition_numbers[0] <= prior)
            {
                return false;
            }
            prior_acquisition = Some(group.acquisition_numbers[0]);
        }
    }
    redundant_philips_trigger_sequence(&trigger_sequence_ms, repetition_time_ms)
}

fn redundant_philips_trigger_sequence(values_ms: &[f64], repetition_time_ms: f64) -> bool {
    let tolerance_ms = 5.0_f64.max(repetition_time_ms * 1e-4);
    let mut steps = Vec::with_capacity(values_ms.len());
    for value in values_ms {
        if !value.is_finite() || *value < 0.0 {
            return false;
        }
        let rounded = (*value / repetition_time_ms).round();
        if rounded < 0.0
            || rounded > i64::MAX as f64
            || (*value - rounded * repetition_time_ms).abs() > tolerance_ms
        {
            return false;
        }
        steps.push(rounded as i64);
    }
    if steps.first() != Some(&0) {
        return false;
    }
    let cycle = steps
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, value)| (*value != index as i64).then_some(index as i64))
        .unwrap_or(steps.len() as i64);
    cycle > 0
        && steps
            .iter()
            .enumerate()
            .all(|(index, value)| *value == index as i64 % cycle)
}

fn all_float_equal(values: &[f64], tolerance: f64) -> bool {
    values.first().is_some_and(|first| {
        values
            .iter()
            .all(|value| (value - first).abs() <= tolerance)
    })
}

fn string(object: &DefaultDicomObject, tag: Tag) -> Option<String> {
    object
        .element(tag)
        .ok()?
        .to_str()
        .ok()
        .map(|value| value.trim_matches([' ', '\0']).to_owned())
        .filter(|value| !value.is_empty())
}

fn multi_string(object: &DefaultDicomObject, tag: Tag) -> Vec<String> {
    object
        .element(tag)
        .ok()
        .and_then(|element| element.to_multi_str().ok())
        .map(|values| {
            values
                .iter()
                .map(|value| value.trim_matches([' ', '\0']).to_owned())
                .filter(|value| !value.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn integer(object: &DefaultDicomObject, tag: Tag) -> Option<i64> {
    object.element(tag).ok()?.to_int::<i64>().ok()
}

fn float(object: &DefaultDicomObject, tag: Tag) -> Option<f64> {
    object
        .element(tag)
        .ok()?
        .to_float64()
        .ok()
        .filter(|v| v.is_finite())
}

fn recursive_string(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    depth: usize,
) -> Option<String> {
    if depth > 32 {
        return None;
    }
    if let Ok(element) = object.element(tag) {
        if let Ok(value) = element.to_str() {
            let value = value.trim_matches([' ', '\0']).to_owned();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    object.iter().find_map(|element| {
        element.value().items().and_then(|items| {
            items
                .iter()
                .find_map(|item| recursive_string(item, tag, depth + 1))
        })
    })
}

fn recursive_multi_string(
    object: &dicom_object::InMemDicomObject,
    tag: Tag,
    depth: usize,
) -> Vec<String> {
    if depth > 32 {
        return Vec::new();
    }
    if let Ok(element) = object.element(tag) {
        if let Ok(values) = element.to_multi_str() {
            let values = values
                .iter()
                .map(|value| value.trim_matches([' ', '\0']).to_owned())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            if !values.is_empty() {
                return values;
            }
        }
    }
    object
        .iter()
        .filter_map(|element| element.value().items())
        .flat_map(|items| items.iter())
        .find_map(|item| {
            let values = recursive_multi_string(item, tag, depth + 1);
            (!values.is_empty()).then_some(values)
        })
        .unwrap_or_default()
}

fn recursive_float(object: &dicom_object::InMemDicomObject, tag: Tag, depth: usize) -> Option<f64> {
    if depth > 32 {
        return None;
    }
    if let Ok(element) = object.element(tag) {
        if let Ok(value) = element.to_float64() {
            if value.is_finite() {
                return Some(value);
            }
        }
    }
    object.iter().find_map(|element| {
        element.value().items().and_then(|items| {
            items
                .iter()
                .find_map(|item| recursive_float(item, tag, depth + 1))
        })
    })
}

fn recursive_integers(object: &dicom_object::InMemDicomObject, tag: Tag, depth: usize) -> Vec<i64> {
    if depth > 32 {
        return Vec::new();
    }
    let mut output = Vec::new();
    if let Ok(element) = object.element(tag) {
        if let Ok(value) = element.to_int::<i64>() {
            output.push(value);
        } else if let Ok(values) = element.to_multi_str() {
            for value in values.iter().filter_map(|value| value.trim().parse().ok()) {
                if !output.contains(&value) {
                    output.push(value);
                }
            }
        }
    }
    for element in object.iter() {
        if let Some(items) = element.value().items() {
            for item in items {
                for value in recursive_integers(item, tag, depth + 1) {
                    if !output.contains(&value) {
                        output.push(value);
                    }
                }
            }
        }
    }
    output
}

fn multi_int(object: &DefaultDicomObject, tag: Tag) -> Vec<i64> {
    multi_string(object, tag)
        .iter()
        .filter_map(|value| value.parse().ok())
        .collect()
}

fn multi_float(object: &DefaultDicomObject, tag: Tag) -> Vec<f64> {
    multi_string(object, tag)
        .iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .collect()
}

fn insert_string(map: &mut BTreeMap<String, Value>, name: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(name.to_owned(), Value::String(value));
    }
}

fn insert_float(map: &mut BTreeMap<String, Value>, name: &str, value: Option<f64>) {
    if let Some(value) = value {
        map.insert(name.to_owned(), json!(value));
    }
}

fn insert_int(map: &mut BTreeMap<String, Value>, name: &str, value: Option<i64>) {
    if let Some(value) = value {
        map.insert(name.to_owned(), json!(value));
    }
}

fn insert_multi_int(map: &mut BTreeMap<String, Value>, name: &str, values: Vec<i64>) {
    if !values.is_empty() {
        map.insert(name.to_owned(), json!(values));
    }
}

fn insert_multi_float(map: &mut BTreeMap<String, Value>, name: &str, values: Vec<f64>) {
    if !values.is_empty() {
        map.insert(name.to_owned(), json!(values));
    }
}

fn looks_like_dicom(path: &Path) -> bool {
    let extension_matches = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            matches!(value.to_ascii_lowercase().as_str(), "dcm" | "dicom" | "ima")
        });
    if extension_matches {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut preamble = [0_u8; 132];
    file.read_exact(&mut preamble).is_ok() && &preamble[128..] == b"DICM"
}

#[cfg(test)]
mod tests {
    use super::*;
    use dicom_core::{
        DataElement, Length,
        value::{DataSetSequence, PrimitiveValue, Value},
    };
    use dicom_object::InMemDicomObject;
    use tempfile::tempdir;

    fn put_sequence(object: &mut InMemDicomObject, tag: Tag, items: Vec<InMemDicomObject>) {
        object.put(DataElement::new(
            tag,
            VR::SQ,
            Value::Sequence(DataSetSequence::new(items, Length::UNDEFINED)),
        ));
    }

    fn public_directional_diffusion(b_value: f64, direction: [f64; 3]) -> DiffusionContract {
        DiffusionContract {
            present: true,
            valid: true,
            semantic: b_value > 1.0,
            signatures: vec![
                diffusion_signature(
                    b_value,
                    Some("DIRECTIONAL"),
                    Some(direction.as_slice()),
                    None,
                )
                .unwrap(),
            ],
        }
    }

    fn siemens_csa_diffusion_fixture(b_value: &str, direction: [&str; 3]) -> Vec<u8> {
        fn append_tag(output: &mut Vec<u8>, name: &str, vr: [u8; 4], values: &[&str]) {
            let mut name_bytes = [0_u8; 64];
            name_bytes[..name.len()].copy_from_slice(name.as_bytes());
            output.extend_from_slice(&name_bytes);
            output.extend_from_slice(&(values.len() as i32).to_le_bytes());
            output.extend_from_slice(&vr);
            output.extend_from_slice(&0_i32.to_le_bytes());
            output.extend_from_slice(&(values.len() as i32).to_le_bytes());
            output.extend_from_slice(&77_i32.to_le_bytes());
            for value in values {
                let mut bytes = value.as_bytes().to_vec();
                bytes.push(0);
                let length = i32::try_from(bytes.len()).unwrap();
                output.extend_from_slice(&length.to_le_bytes());
                output.extend_from_slice(&length.to_le_bytes());
                output.extend_from_slice(&77_i32.to_le_bytes());
                output.extend_from_slice(&0_i32.to_le_bytes());
                output.extend_from_slice(&bytes);
                output.resize(output.len() + (4 - bytes.len() % 4) % 4, 0);
            }
        }

        let mut source = b"SV10\x04\x03\x02\x01".to_vec();
        source.extend_from_slice(&2_u32.to_le_bytes());
        source.extend_from_slice(&77_u32.to_le_bytes());
        append_tag(&mut source, "B_value", [b'I', b'S', 0, 0], &[b_value]);
        append_tag(
            &mut source,
            "DiffusionGradientDirection",
            [b'F', b'D', 0, 0],
            direction.as_slice(),
        );
        source
    }

    fn siemens_tag_and_csa_diffusion(
        tag_b_value: &str,
        tag_direction: [f64; 3],
        csa_b_value: &str,
        csa_direction: [&str; 3],
    ) -> InMemDicomObject {
        let mut object = InMemDicomObject::new_empty();
        object.put_str(Tag(0x0019, 0x0010), VR::LO, "SIEMENS MR HEADER");
        object.put_str(Tag(0x0019, 0x100C), VR::IS, tag_b_value);
        object.put_str(Tag(0x0019, 0x100D), VR::CS, "DIRECTIONAL");
        object.put(DataElement::new(
            Tag(0x0019, 0x100E),
            VR::FD,
            PrimitiveValue::F64(tag_direction.to_vec().into()),
        ));
        object.put_str(Tag(0x0029, 0x0010), VR::LO, "SIEMENS CSA HEADER");
        object.put(DataElement::new(
            Tag(0x0029, 0x1010),
            VR::OB,
            PrimitiveValue::U8(siemens_csa_diffusion_fixture(csa_b_value, csa_direction).into()),
        ));
        object
    }

    fn valid_asl_item(context: &str) -> InMemDicomObject {
        let mut slab = InMemDicomObject::new_empty();
        slab.put(DataElement::new(
            Tag(0x0018, 0x9253),
            VR::US,
            PrimitiveValue::from(1_u16),
        ));
        slab.put(DataElement::new(
            Tag(0x0018, 0x9254),
            VR::FD,
            PrimitiveValue::from(100.0_f64),
        ));
        slab.put(DataElement::new(
            Tag(0x0018, 0x9255),
            VR::FD,
            PrimitiveValue::F64(vec![0.0, 0.0, 1.0].into()),
        ));
        slab.put(DataElement::new(
            Tag(0x0018, 0x9256),
            VR::FD,
            PrimitiveValue::F64(vec![0.0, 0.0, 0.0].into()),
        ));
        slab.put(DataElement::new(
            Tag(0x0018, 0x9258),
            VR::UL,
            PrimitiveValue::from(1_800_u32),
        ));
        let mut item = InMemDicomObject::new_empty();
        item.put_str(Tag(0x0018, 0x9252), VR::LO, "");
        item.put_str(Tag(0x0018, 0x9257), VR::CS, context);
        item.put_str(Tag(0x0018, 0x9259), VR::CS, "NO");
        item.put_str(Tag(0x0018, 0x925C), VR::CS, "NO");
        put_sequence(&mut item, Tag(0x0018, 0x9260), vec![slab]);
        item
    }

    fn add_philips_private_asl_label(object: &mut InMemDicomObject, label: &str) {
        object.put_str(Tag(0x2005, 0x0014), VR::LO, "Philips MR Imaging DD 005");
        object.put_str(Tag(0x2005, 0x1429), VR::CS, label);
    }

    #[test]
    fn protocol_text_is_local_only_join() {
        let header = DicomHeader {
            protocol_name: Some("rest".into()),
            series_description: Some("BOLD".into()),
            sequence_name: Some("ep2d".into()),
            ..Default::default()
        };
        assert_eq!(header.local_protocol_text(), "rest BOLD ep2d");
    }

    #[test]
    fn representative_selection_prefers_lowest_instance_then_path() {
        let later = DicomHeader {
            path: PathBuf::from("b.dcm"),
            instance_number: Some(2),
            ..Default::default()
        };
        let earlier = DicomHeader {
            path: PathBuf::from("z.dcm"),
            instance_number: Some(1),
            ..Default::default()
        };
        assert!(representative_precedes(&earlier, &later));

        let same_instance_a = DicomHeader {
            path: PathBuf::from("a.dcm"),
            instance_number: Some(2),
            ..Default::default()
        };
        assert!(representative_precedes(&same_instance_a, &later));
    }

    #[test]
    fn archive_grouping_allows_instance_geometry_but_rejects_series_provenance_changes() {
        let mut left = DicomHeader {
            sop_class_uid: Some(MR_IMAGE_STORAGE_UID.into()),
            modality: Some("MR".into()),
            manufacturer: Some("Siemens Healthineers".into()),
            model: Some("Prisma_fit".into()),
            software_versions: Some("syngo MR E11".into()),
            patient_position: Some("HFS".into()),
            magnetic_field_strength: Some(3.0),
            series_number: Some(7),
            mr_acquisition_type: Some("2D".into()),
            sequence_name: Some("ep2d_bold".into()),
            scanning_sequence: vec!["EP".into()],
            sequence_variant: vec!["SK".into()],
            scan_options: vec!["FS".into()],
            number_of_temporal_positions: Some(2),
            ..Default::default()
        };
        left.acquisition
            .insert("receive_coil_name".into(), json!("HEAD_32"));
        left.acquisition.insert("rows".into(), json!(64));
        left.acquisition.insert("columns".into(), json!(64));
        left.acquisition
            .insert("acquisition_matrix".into(), json!([64, 0, 0, 64]));
        left.acquisition
            .insert("pixel_spacing_mm".into(), json!([3.0, 3.0]));
        left.acquisition
            .insert("slice_thickness_mm".into(), json!(3.0));

        let mut varied = left.clone();
        varied.manufacturer = Some("SIEMENS".into());
        varied.model = Some("MAGNETOM Prisma_fit".into());
        varied.software_versions = Some("E11".into());
        varied.scanning_sequence = vec!["ep".into()];
        varied.number_of_temporal_positions = Some(240);
        varied.acquisition.insert("rows".into(), json!(96));
        varied.acquisition.insert("columns".into(), json!(128));
        varied
            .acquisition
            .insert("acquisition_matrix".into(), json!([96, 0, 0, 128]));
        varied
            .acquisition
            .insert("pixel_spacing_mm".into(), json!([2.0, 2.0]));
        varied
            .acquisition
            .insert("slice_thickness_mm".into(), json!(1.0));
        assert!(!required_metadata_conflicts(&left, &varied));

        varied.modality = Some("CT".into());
        assert!(required_metadata_conflicts(&left, &varied));
        varied.modality = Some("MR".into());
        varied.sop_class_uid = Some(ENHANCED_MR_IMAGE_STORAGE_UID.into());
        assert!(required_metadata_conflicts(&left, &varied));

        let mut acquisition_changed = left.clone();
        acquisition_changed.mr_acquisition_type = Some("3D".into());
        assert!(required_metadata_conflicts(&left, &acquisition_changed));

        let mut scanner_changed = left.clone();
        scanner_changed.manufacturer = Some("GE MEDICAL SYSTEMS".into());
        assert!(required_metadata_conflicts(&left, &scanner_changed));

        let mut software_changed = left.clone();
        software_changed.software_versions = Some("syngo MR E12".into());
        assert!(required_metadata_conflicts(&left, &software_changed));

        let mut series_changed = left.clone();
        series_changed.series_number = Some(8);
        assert!(required_metadata_conflicts(&left, &series_changed));

        let mut sequence_changed = left.clone();
        sequence_changed.scanning_sequence = vec!["GR".into()];
        assert!(required_metadata_conflicts(&left, &sequence_changed));

        let sparse = DicomHeader {
            sop_class_uid: left.sop_class_uid.clone(),
            modality: left.modality.clone(),
            ..Default::default()
        };
        assert!(!required_metadata_conflicts(&left, &sparse));
    }

    #[test]
    fn source_fingerprint_is_stable_and_detects_folder_changes() {
        let directory = tempdir().unwrap();
        let first_path = directory.path().join("one.dcm");
        std::fs::write(&first_path, b"first").unwrap();
        let first = snapshot_source_with_progress(directory.path(), |_| {})
            .unwrap()
            .fingerprint(directory.path())
            .unwrap();
        let unchanged = snapshot_source_with_progress(directory.path(), |_| {})
            .unwrap()
            .fingerprint(directory.path())
            .unwrap();
        assert_eq!(first, unchanged);
        assert_eq!(first.file_count, 1);

        std::fs::write(directory.path().join("README"), b"operator notes").unwrap();
        std::fs::write(directory.path().join("scanner.log"), b"export complete").unwrap();
        std::fs::write(directory.path().join(".DS_Store"), b"finder metadata").unwrap();
        let with_non_dicom_files = snapshot_source_with_progress(directory.path(), |_| {})
            .unwrap()
            .fingerprint(directory.path())
            .unwrap();
        assert_eq!(first, with_non_dicom_files);
        std::fs::write(directory.path().join("scanner.log"), b"changed notes only").unwrap();
        let with_changed_non_dicom = snapshot_source_with_progress(directory.path(), |_| {})
            .unwrap()
            .fingerprint(directory.path())
            .unwrap();
        assert_eq!(first, with_changed_non_dicom);

        let original_modified = first_path.metadata().unwrap().modified().unwrap();
        std::fs::write(&first_path, b"other").unwrap();
        File::options()
            .write(true)
            .open(&first_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .unwrap();
        let equal_size_replacement = snapshot_source_with_progress(directory.path(), |_| {})
            .unwrap()
            .fingerprint(directory.path())
            .unwrap();
        assert_ne!(first, equal_size_replacement);

        std::fs::write(directory.path().join("two.dcm"), b"second").unwrap();
        let changed = snapshot_source_with_progress(directory.path(), |_| {})
            .unwrap()
            .fingerprint(directory.path())
            .unwrap();
        assert_ne!(first, changed);
        assert_eq!(changed.file_count, 2);
    }

    #[test]
    fn raw_dicom_resource_limits_accept_exact_boundaries_only() {
        assert!(dicom_instance_count_supported(500_000));
        assert!(!dicom_instance_count_supported(500_001));
        assert!(dicom_instance_size_supported(64 * 1024 * 1024 * 1024));
        assert!(!dicom_instance_size_supported(64 * 1024 * 1024 * 1024 + 1));
        assert!(dicom_series_uncompressed_size_supported([
            32 * 1024 * 1024 * 1024,
            32 * 1024 * 1024 * 1024,
        ]));
        assert!(!dicom_series_uncompressed_size_supported([
            64 * 1024 * 1024 * 1024,
            1,
        ]));
        assert!(!dicom_series_uncompressed_size_supported([u64::MAX, 1]));
    }

    #[test]
    fn sop_uid_deduplication_scales_to_the_series_instance_limit() {
        let mut seen = HashSet::with_capacity(MAX_DICOM_INSTANCES_PER_SERIES);
        for index in 0..MAX_DICOM_INSTANCES_PER_SERIES {
            assert!(!record_sop_instance_uid(
                &mut seen,
                &format!("2.25.{index}")
            ));
        }
        assert_eq!(seen.len(), MAX_DICOM_INSTANCES_PER_SERIES);
        assert!(record_sop_instance_uid(&mut seen, "2.25.499999"));
        assert!(record_sop_instance_uid(&mut seen, ""));
    }

    #[test]
    fn series_header_aggregates_are_bounded_and_temporal_evidence_needs_only_two_values() {
        let mut target = Vec::new();
        let mut seen = HashSet::new();
        extend_unique_bounded(
            &mut target,
            &mut seen,
            (0..MAX_DICOM_INSTANCES_PER_SERIES).map(|index| format!("value-{index}")),
            "overflow",
        );
        assert_eq!(target.len(), MAX_DISTINCT_SERIES_HEADER_VALUES + 1);
        assert_eq!(target.last().map(String::as_str), Some("overflow"));
        assert_eq!(seen.len(), MAX_DISTINCT_SERIES_HEADER_VALUES + 1);

        let mut temporal_positions = Vec::new();
        for value in 0..MAX_DICOM_INSTANCES_PER_SERIES as i64 {
            retain_two_distinct(&mut temporal_positions, value);
        }
        assert_eq!(temporal_positions, vec![0, 1]);

        let oversized = format!("{}suffix", "x".repeat(MAX_LOCAL_PROTOCOL_TEXT_BYTES));
        let bounded = bounded_local_protocol_text(&oversized);
        assert_eq!(bounded.len(), MAX_LOCAL_PROTOCOL_TEXT_BYTES);
        assert!(!bounded.contains("suffix"));
    }

    #[test]
    fn public_bmatrix_is_a_complete_diffusion_direction_contract_without_bvec() {
        fn matrix_item(include_zz: bool) -> InMemDicomObject {
            let mut matrix = InMemDicomObject::new_empty();
            for (element, value) in [
                (0x9602, 1_000.0),
                (0x9603, 0.0),
                (0x9604, 0.0),
                (0x9605, 0.0),
                (0x9606, 0.0),
                (0x9607, 0.0),
            ] {
                if include_zz || element != 0x9607 {
                    matrix.put(DataElement::new(
                        Tag(0x0018, element),
                        VR::FD,
                        PrimitiveValue::from(value),
                    ));
                }
            }
            matrix
        }

        fn frame(include_zz: bool) -> InMemDicomObject {
            let mut diffusion = InMemDicomObject::new_empty();
            diffusion.put(DataElement::new(
                Tag(0x0018, 0x9087),
                VR::FD,
                PrimitiveValue::from(1_000.0_f64),
            ));
            diffusion.put_str(Tag(0x0018, 0x9075), VR::CS, "BMATRIX");
            put_sequence(
                &mut diffusion,
                Tag(0x0018, 0x9601),
                vec![matrix_item(include_zz)],
            );
            let mut frame = InMemDicomObject::new_empty();
            put_sequence(&mut frame, Tag(0x0018, 0x9117), vec![diffusion]);
            let mut frame_type = InMemDicomObject::new_empty();
            frame_type.put_str(
                Tag(0x0008, 0x9007),
                VR::CS,
                "ORIGINAL\\PRIMARY\\DIFFUSION\\NONE",
            );
            put_sequence(&mut frame, Tag(0x0018, 0x9226), vec![frame_type]);
            frame
        }

        let mut object = InMemDicomObject::new_empty();
        object.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
        object.put_str(
            Tag(0x0008, 0x0008),
            VR::CS,
            "ORIGINAL\\PRIMARY\\DIFFUSION\\NONE",
        );
        object.put_str(Tag(0x0028, 0x0008), VR::IS, "2");
        put_sequence(
            &mut object,
            Tag(0x5200, 0x9230),
            vec![frame(true), frame(true)],
        );
        let object_frames = object
            .element(Tag(0x5200, 0x9230))
            .unwrap()
            .value()
            .items()
            .unwrap();
        assert_eq!(
            direct_frame_origin_state(&object_frames[0], false),
            FrameOriginState::Valid(FrameOrigin::Original)
        );
        assert_eq!(
            direct_macro_state(
                &object_frames[0],
                Tag(0x0018, 0x9117),
                exactly_one_valid_diffusion_item,
            ),
            MacroState::Valid
        );
        assert!(enhanced_frame_origins(&object).is_some());
        assert_eq!(
            public_diffusion_metadata_contract(&object),
            (true, true, true)
        );

        let mut scattered = InMemDicomObject::new_empty();
        scattered.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
        scattered.put_str(
            Tag(0x0008, 0x0008),
            VR::CS,
            "ORIGINAL\\PRIMARY\\DIFFUSION\\NONE",
        );
        scattered.put_str(Tag(0x0028, 0x0008), VR::IS, "2");
        let mut zz_only_matrix = InMemDicomObject::new_empty();
        zz_only_matrix.put(DataElement::new(
            Tag(0x0018, 0x9607),
            VR::FD,
            PrimitiveValue::from(0.0_f64),
        ));
        let mut zz_only_diffusion = InMemDicomObject::new_empty();
        zz_only_diffusion.put(DataElement::new(
            Tag(0x0018, 0x9087),
            VR::FD,
            PrimitiveValue::from(1_000.0_f64),
        ));
        zz_only_diffusion.put_str(Tag(0x0018, 0x9075), VR::CS, "BMATRIX");
        put_sequence(
            &mut zz_only_diffusion,
            Tag(0x0018, 0x9601),
            vec![zz_only_matrix],
        );
        let mut zz_only_frame = InMemDicomObject::new_empty();
        put_sequence(
            &mut zz_only_frame,
            Tag(0x0018, 0x9117),
            vec![zz_only_diffusion],
        );
        let mut zz_frame_type = InMemDicomObject::new_empty();
        zz_frame_type.put_str(
            Tag(0x0008, 0x9007),
            VR::CS,
            "ORIGINAL\\PRIMARY\\DIFFUSION\\NONE",
        );
        put_sequence(&mut zz_only_frame, Tag(0x0018, 0x9226), vec![zz_frame_type]);
        put_sequence(
            &mut scattered,
            Tag(0x5200, 0x9230),
            vec![frame(false), zz_only_frame],
        );
        assert_eq!(
            public_diffusion_metadata_contract(&scattered),
            (true, false, true)
        );
    }

    #[test]
    fn diffusion_contract_uses_sop_class_not_stray_functional_groups() {
        fn directional_root(sop_class_uid: &str, b_value: f64) -> InMemDicomObject {
            let mut object = InMemDicomObject::new_empty();
            object.put_str(Tag(0x0008, 0x0016), VR::UI, sop_class_uid);
            object.put(DataElement::new(
                Tag(0x0018, 0x9087),
                VR::FD,
                PrimitiveValue::from(b_value),
            ));
            object.put_str(Tag(0x0018, 0x9075), VR::CS, "DIRECTIONAL");
            object.put(DataElement::new(
                Tag(0x0018, 0x9089),
                VR::FD,
                PrimitiveValue::F64(vec![1.0, 0.0, 0.0].into()),
            ));
            object
        }

        let mut classic = directional_root(MR_IMAGE_STORAGE_UID, 1_000.0);
        classic.put_str(Tag(0x5200, 0x9230), VR::LO, "stray");
        assert_eq!(
            public_diffusion_metadata_contract(&classic),
            (true, true, true)
        );

        for sop_class_uid in [
            ENHANCED_MR_IMAGE_STORAGE_UID,
            LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        ] {
            let enhanced_root = directional_root(sop_class_uid, 1_000.0);
            assert_eq!(
                public_diffusion_metadata_contract(&enhanced_root),
                (true, false, true)
            );

            let mut missing_groups = InMemDicomObject::new_empty();
            missing_groups.put_str(Tag(0x0008, 0x0016), VR::UI, sop_class_uid);
            assert_eq!(
                public_diffusion_metadata_contract(&missing_groups),
                (true, false, false)
            );
        }

        let classic_b_zero = directional_root(MR_IMAGE_STORAGE_UID, 0.0);
        assert_eq!(
            public_diffusion_metadata_contract(&classic_b_zero),
            (true, false, true)
        );

        let mut incomplete_direct_matrix = InMemDicomObject::new_empty();
        incomplete_direct_matrix.put_str(Tag(0x0008, 0x0016), VR::UI, MR_IMAGE_STORAGE_UID);
        incomplete_direct_matrix.put(DataElement::new(
            Tag(0x0018, 0x9602),
            VR::FD,
            PrimitiveValue::from(1_000.0_f64),
        ));
        assert_eq!(
            public_diffusion_metadata_contract(&incomplete_direct_matrix),
            (true, false, false)
        );
    }

    #[test]
    fn supplemental_philips_indices_do_not_claim_or_poison_diffusion_source() {
        let mut object = InMemDicomObject::new_empty();
        object.put_str(Tag(0x2005, 0x0010), VR::LO, "Philips MR Imaging DD 005");
        object.put_str(Tag(0x2005, 0x1012), VR::IS, "3");
        object.put_str(Tag(0x2005, 0x1013), VR::IS, "7");

        assert_eq!(
            reviewed_private_diffusion_metadata_contract(&object),
            (false, false, false)
        );
        let public = single_source_diffusion_contract(
            true,
            true,
            true,
            diffusion_signature(1_000.0, Some("ISOTROPIC"), None, None),
        );
        assert!(diffusion_source_contract_verified(
            &public,
            &DiffusionContract::default(),
        ));
    }

    #[test]
    fn ge_diffusion_four_value_candidate_bounds_every_component() {
        let mut object = InMemDicomObject::new_empty();
        object.put_str(Tag(0x0043, 0x0010), VR::LO, "GEMS_PARM_01");
        object.put_str(Tag(0x0043, 0x1039), VR::IS, "1000\\1000000001\\0\\0");
        assert_eq!(
            reviewed_private_diffusion_metadata_contract(&object),
            (true, false, false)
        );
    }

    #[test]
    fn enhanced_mixed_diffusion_pairs_frame_origin_with_each_macro() {
        fn frame(origin: &str, include_diffusion: bool) -> InMemDicomObject {
            let mut frame = InMemDicomObject::new_empty();
            let mut frame_type = InMemDicomObject::new_empty();
            frame_type.put_str(
                Tag(0x0008, 0x9007),
                VR::CS,
                format!("{origin}\\PRIMARY\\DIFFUSION\\NONE"),
            );
            put_sequence(&mut frame, Tag(0x0018, 0x9226), vec![frame_type]);
            if include_diffusion {
                let mut gradient = InMemDicomObject::new_empty();
                gradient.put(DataElement::new(
                    Tag(0x0018, 0x9089),
                    VR::FD,
                    PrimitiveValue::F64(vec![1.0, 0.0, 0.0].into()),
                ));
                let mut diffusion = InMemDicomObject::new_empty();
                diffusion.put(DataElement::new(
                    Tag(0x0018, 0x9087),
                    VR::FD,
                    PrimitiveValue::from(1_000.0_f64),
                ));
                diffusion.put_str(Tag(0x0018, 0x9075), VR::CS, "DIRECTIONAL");
                put_sequence(&mut diffusion, Tag(0x0018, 0x9076), vec![gradient]);
                put_sequence(&mut frame, Tag(0x0018, 0x9117), vec![diffusion]);
            }
            frame
        }

        fn enhanced(root_origin: &str, frames: Vec<InMemDicomObject>) -> InMemDicomObject {
            let mut object = InMemDicomObject::new_empty();
            object.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
            object.put_str(
                Tag(0x0008, 0x0008),
                VR::CS,
                format!("{root_origin}\\PRIMARY\\DIFFUSION\\NONE"),
            );
            object.put_str(Tag(0x0028, 0x0008), VR::IS, frames.len().to_string());
            put_sequence(&mut object, Tag(0x5200, 0x9230), frames);
            object
        }

        let mixed = enhanced(
            "MIXED",
            vec![frame("ORIGINAL", true), frame("DERIVED", false)],
        );
        assert_eq!(
            public_diffusion_metadata_contract(&mixed),
            (true, true, true)
        );

        let mismatched_summary = enhanced(
            "ORIGINAL",
            vec![frame("ORIGINAL", true), frame("DERIVED", false)],
        );
        assert_eq!(
            public_diffusion_metadata_contract(&mismatched_summary),
            (true, false, true)
        );

        let derived = enhanced(
            "DERIVED",
            vec![frame("DERIVED", false), frame("DERIVED", false)],
        );
        assert_eq!(
            public_diffusion_metadata_contract(&derived),
            (true, true, false)
        );

        let derived_claiming_gradient = enhanced(
            "DERIVED",
            vec![frame("DERIVED", true), frame("DERIVED", false)],
        );
        assert_eq!(
            public_diffusion_metadata_contract(&derived_claiming_gradient),
            (true, false, true)
        );
    }

    #[test]
    fn enhanced_frame_origin_uses_the_same_positional_contract_as_sanitization() {
        fn functional_group(frame_type: &str) -> InMemDicomObject {
            let mut frame_content = InMemDicomObject::new_empty();
            frame_content.put_str(Tag(0x0008, 0x9007), VR::CS, frame_type);
            let mut group = InMemDicomObject::new_empty();
            put_sequence(&mut group, Tag(0x0018, 0x9226), vec![frame_content]);
            group
        }

        assert_eq!(
            direct_frame_origin_state(
                &functional_group("DERIVED\\PRIMARY\\VOLUME\\RESAMPLED"),
                false,
            ),
            FrameOriginState::Valid(FrameOrigin::Derived)
        );
        assert_eq!(
            direct_frame_origin_state(&functional_group("DERIVED\\PRIMARY\\VOLUME\\MIXED"), false,),
            FrameOriginState::Invalid
        );
        assert_eq!(
            direct_frame_origin_state(&functional_group("DERIVED\\PRIMARY\\VOLUME\\"), true),
            FrameOriginState::Valid(FrameOrigin::Derived)
        );
    }

    #[test]
    fn every_present_scientific_source_must_validate_and_agree_on_diffusion_context() {
        let signature = diffusion_signature(1_000.0, Some("ISOTROPIC"), None, None).unwrap();
        let contract = |valid: bool, signature: DiffusionSignature| DiffusionContract {
            present: true,
            valid,
            semantic: true,
            signatures: vec![signature],
        };
        assert!(!diffusion_source_contract_verified(
            &contract(false, signature.clone()),
            &contract(true, signature.clone()),
        ));
        assert!(!diffusion_source_contract_verified(
            &contract(true, signature.clone()),
            &contract(false, signature.clone()),
        ));
        let b_zero = diffusion_signature(0.0, Some("NONE"), None, None).unwrap();
        assert!(!diffusion_source_contract_verified(
            &contract(true, b_zero),
            &contract(true, signature.clone()),
        ));
        assert!(diffusion_source_contract_verified(
            &contract(true, signature.clone()),
            &contract(true, signature),
        ));
        assert!(!scientific_source_contract_verified(
            true, false, true, true,
        ));
        assert!(!scientific_source_contract_verified(
            true, true, true, false,
        ));
    }

    #[test]
    fn overlapping_diffusion_sources_compare_full_numeric_signatures() {
        let reference = public_directional_diffusion(1_000.0, [1.0, 0.0, 0.0]);
        let different_b = public_directional_diffusion(2_000.0, [1.0, 0.0, 0.0]);
        let different_direction = public_directional_diffusion(1_000.0, [0.0, 1.0, 0.0]);
        let rounded = public_directional_diffusion(1_000.5, [1.0, 0.0, 0.0]);
        assert!(!diffusion_source_contract_verified(
            &reference,
            &different_b,
        ));
        assert!(!diffusion_source_contract_verified(
            &reference,
            &different_direction,
        ));
        assert!(diffusion_source_contract_verified(&reference, &rounded));

        let matrix = |xx: f64, yy: f64| DiffusionContract {
            present: true,
            valid: true,
            semantic: true,
            signatures: vec![DiffusionSignature {
                b_value: 1_000.0,
                representation: DiffusionRepresentation::BMatrix([xx, 0.0, 0.0, yy, 0.0, 0.0]),
            }],
        };
        assert!(diffusion_source_contract_verified(
            &reference,
            &matrix(1_000.0, 0.0),
        ));
        assert!(!diffusion_source_contract_verified(
            &reference,
            &matrix(0.0, 1_000.0),
        ));
    }

    #[test]
    fn siemens_mr_header_and_csa_must_match_b_value_and_direction() {
        let matching =
            siemens_tag_and_csa_diffusion("1000", [1.0, 0.0, 0.0], "1000", ["1", "0", "0"]);
        assert!(reviewed_private_diffusion_metadata_contract(&matching).valid);

        let different_b =
            siemens_tag_and_csa_diffusion("1000", [1.0, 0.0, 0.0], "2000", ["1", "0", "0"]);
        assert!(!reviewed_private_diffusion_metadata_contract(&different_b).valid);

        let different_direction =
            siemens_tag_and_csa_diffusion("1000", [1.0, 0.0, 0.0], "1000", ["0", "1", "0"]);
        assert!(!reviewed_private_diffusion_metadata_contract(&different_direction).valid);
    }

    #[test]
    fn public_and_philips_private_asl_contexts_must_be_unambiguous_and_agree() {
        fn classic(
            public_context: &str,
            private_context: &str,
        ) -> (AslContract, PrivateAslContract) {
            let mut object = InMemDicomObject::new_empty();
            object.put_str(Tag(0x0008, 0x0016), VR::UI, MR_IMAGE_STORAGE_UID);
            object.put_str(Tag(0x0018, 0x9250), VR::CS, "PSEUDOCONTINUOUS");
            object.put_str(Tag(0x0018, 0x0082), VR::DS, "1800");
            put_sequence(
                &mut object,
                Tag(0x0018, 0x9251),
                vec![valid_asl_item(public_context)],
            );
            add_philips_private_asl_label(&mut object, private_context);
            (
                public_asl_metadata_contract(&object),
                reviewed_private_asl_metadata_contract(&object),
            )
        }

        let (public, private) = classic("LABEL", "LBL");
        assert_eq!(
            public.contexts.iter().cloned().collect::<Vec<_>>(),
            ["LABEL"]
        );
        assert_eq!(private.philips_label.as_deref(), Some("LABEL"));
        assert!(philips_private_asl_agrees_with_public(
            MrSopStorageKind::Classic,
            &public,
            &private,
        ));

        let (public, private) = classic("CONTROL", "LBL");
        assert!(!philips_private_asl_agrees_with_public(
            MrSopStorageKind::Classic,
            &public,
            &private,
        ));

        let enhanced = |contexts: &[&str]| {
            let mut object = InMemDicomObject::new_empty();
            object.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
            object.put_str(Tag(0x0018, 0x9250), VR::CS, "PSEUDOCONTINUOUS");
            object.put_str(Tag(0x0018, 0x0082), VR::DS, "1800");
            object.put_str(Tag(0x0028, 0x0008), VR::IS, contexts.len().to_string());
            put_sequence(
                &mut object,
                Tag(0x5200, 0x9230),
                contexts
                    .iter()
                    .map(|context| {
                        let mut frame = InMemDicomObject::new_empty();
                        put_sequence(
                            &mut frame,
                            Tag(0x0018, 0x9251),
                            vec![valid_asl_item(context)],
                        );
                        frame
                    })
                    .collect(),
            );
            add_philips_private_asl_label(&mut object, "LBL");
            object
        };

        let mixed = enhanced(&["LABEL", "CONTROL"]);
        let public = public_asl_metadata_contract(&mixed);
        let private = reviewed_private_asl_metadata_contract(&mixed);
        assert_eq!(public.contexts.len(), 2);
        assert!(!philips_private_asl_agrees_with_public(
            MrSopStorageKind::Enhanced,
            &public,
            &private,
        ));

        let matching = enhanced(&["LABEL", "LABEL"]);
        let public = public_asl_metadata_contract(&matching);
        let private = reviewed_private_asl_metadata_contract(&matching);
        assert_eq!(public.contexts.len(), 1);
        assert!(philips_private_asl_agrees_with_public(
            MrSopStorageKind::Enhanced,
            &public,
            &private,
        ));
    }

    #[test]
    fn empty_optional_philips_asl_label_is_not_asl_evidence() {
        let mut object = InMemDicomObject::new_empty();
        object.put_str(Tag(0x2005, 0x0014), VR::LO, "Philips MR Imaging DD 005");
        object.put(DataElement::new(
            Tag(0x2005, 0x1429),
            VR::CS,
            PrimitiveValue::Empty,
        ));

        let private = reviewed_private_asl_metadata_contract(&object);
        assert!(!private.philips_present);
        assert!(!private.philips_valid);
        assert!(private.philips_label.is_none());
    }

    #[test]
    fn public_asl_positive_crusher_and_bolus_groups_are_atomic() {
        let mut valid = valid_asl_item("LABEL");
        valid.put_str(Tag(0x0018, 0x9259), VR::CS, "YES");
        valid.put(DataElement::new(
            Tag(0x0018, 0x925A),
            VR::FD,
            PrimitiveValue::from(12.5_f64),
        ));
        valid.put_str(Tag(0x0018, 0x925B), VR::LO, "sensitive crusher description");
        valid.put_str(Tag(0x0018, 0x925C), VR::CS, "YES");
        let mut bolus = InMemDicomObject::new_empty();
        bolus.put_str(Tag(0x0018, 0x925E), VR::LO, "QUIPSS II");
        bolus.put(DataElement::new(
            Tag(0x0018, 0x925F),
            VR::UL,
            PrimitiveValue::from(700_u32),
        ));
        put_sequence(&mut valid, Tag(0x0018, 0x925D), vec![bolus]);
        assert!(valid_public_asl_item(&valid));

        let mut missing_crusher_flow = valid.clone();
        missing_crusher_flow.remove_element(Tag(0x0018, 0x925A));
        assert!(!valid_public_asl_item(&missing_crusher_flow));

        let mut negative_crusher_flow = valid.clone();
        negative_crusher_flow.put(DataElement::new(
            Tag(0x0018, 0x925A),
            VR::FD,
            PrimitiveValue::from(-1.0_f64),
        ));
        assert!(!valid_public_asl_item(&negative_crusher_flow));

        let mut missing_bolus_delay = valid.clone();
        let mut incomplete_bolus = InMemDicomObject::new_empty();
        incomplete_bolus.put_str(Tag(0x0018, 0x925E), VR::LO, "");
        put_sequence(
            &mut missing_bolus_delay,
            Tag(0x0018, 0x925D),
            vec![incomplete_bolus],
        );
        assert!(!valid_public_asl_item(&missing_bolus_delay));

        let mut conflicting_no = valid_asl_item("LABEL");
        conflicting_no.put(DataElement::new(
            Tag(0x0018, 0x925A),
            VR::FD,
            PrimitiveValue::from(1.0_f64),
        ));
        assert!(!valid_public_asl_item(&conflicting_no));
    }

    #[test]
    fn public_asl_contract_requires_atomic_coverage_of_every_frame() {
        fn asl_item(include_description: bool) -> InMemDicomObject {
            let mut slab = InMemDicomObject::new_empty();
            slab.put(DataElement::new(
                Tag(0x0018, 0x9253),
                VR::US,
                PrimitiveValue::from(1_u16),
            ));
            for (tag, values) in [
                (Tag(0x0018, 0x9254), vec![100.0_f64]),
                (Tag(0x0018, 0x9255), vec![0.0_f64, 0.0, 1.0]),
                (Tag(0x0018, 0x9256), vec![0.0_f64, 0.0, 0.0]),
            ] {
                slab.put(DataElement::new(
                    tag,
                    VR::FD,
                    PrimitiveValue::F64(values.into()),
                ));
            }
            slab.put(DataElement::new(
                Tag(0x0018, 0x9258),
                VR::UL,
                PrimitiveValue::from(1_800_u32),
            ));

            let mut asl = InMemDicomObject::new_empty();
            if include_description {
                asl.put_str(Tag(0x0018, 0x9252), VR::LO, "");
            }
            asl.put_str(Tag(0x0018, 0x9257), VR::CS, "LABEL");
            asl.put_str(Tag(0x0018, 0x9259), VR::CS, "NO");
            asl.put_str(Tag(0x0018, 0x925C), VR::CS, "NO");
            put_sequence(&mut asl, Tag(0x0018, 0x9260), vec![slab]);
            asl
        }

        fn asl_frame(items: Vec<InMemDicomObject>) -> InMemDicomObject {
            let mut frame = InMemDicomObject::new_empty();
            put_sequence(&mut frame, Tag(0x0018, 0x9251), items);
            frame
        }

        let mut complete = InMemDicomObject::new_empty();
        complete.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
        complete.put_str(Tag(0x0018, 0x9250), VR::CS, "PSEUDOCONTINUOUS");
        complete.put_str(Tag(0x0028, 0x0008), VR::IS, "2");
        put_sequence(
            &mut complete,
            Tag(0x5200, 0x9230),
            vec![
                asl_frame(vec![asl_item(true)]),
                asl_frame(vec![asl_item(true)]),
            ],
        );
        assert_eq!(public_asl_metadata_contract(&complete), (true, true));

        let mut shared_multiple = InMemDicomObject::new_empty();
        shared_multiple.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
        shared_multiple.put_str(Tag(0x0018, 0x9250), VR::CS, "PSEUDOCONTINUOUS");
        put_sequence(
            &mut shared_multiple,
            Tag(0x5200, 0x9229),
            vec![asl_frame(vec![asl_item(true), asl_item(true)])],
        );
        assert_eq!(public_asl_metadata_contract(&shared_multiple), (true, true));

        let mut partial = InMemDicomObject::new_empty();
        partial.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
        partial.put_str(Tag(0x0018, 0x9250), VR::CS, "PSEUDOCONTINUOUS");
        partial.put_str(Tag(0x0028, 0x0008), VR::IS, "2");
        put_sequence(
            &mut partial,
            Tag(0x5200, 0x9230),
            vec![
                asl_frame(vec![asl_item(true)]),
                InMemDicomObject::new_empty(),
            ],
        );
        assert_eq!(public_asl_metadata_contract(&partial), (true, false));

        let mut missing_type_two = InMemDicomObject::new_empty();
        missing_type_two.put_str(Tag(0x0008, 0x0016), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
        missing_type_two.put_str(Tag(0x0018, 0x9250), VR::CS, "PSEUDOCONTINUOUS");
        put_sequence(
            &mut missing_type_two,
            Tag(0x5200, 0x9229),
            vec![asl_frame(vec![asl_item(false)])],
        );
        assert_eq!(
            public_asl_metadata_contract(&missing_type_two),
            (true, false)
        );
    }
}
