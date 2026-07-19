use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
pub const MAX_DICOM_INSTANCES_PER_SERIES: usize = 500_000;
pub const MAX_DICOM_INSTANCE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_DICOM_SERIES_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024 * 1024;

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
    pub philips_classic_private_metadata_contract_verified: bool,
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
    pub all_philips_classic_private_metadata_contract_verified: bool,
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
        if self.changed_while_reading {
            bail!("the selected folder changed while its local sync identity was being checked");
        }
        let mut digest = Sha256::new();
        digest.update(b"scaling-neuro-source-snapshot-v1\0");
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
            match file.modified {
                None => digest.update([0]),
                Some(modified) => match modified.duration_since(UNIX_EPOCH) {
                    Ok(duration) => {
                        digest.update([1]);
                        digest.update(duration.as_secs().to_le_bytes());
                        digest.update(duration.subsec_nanos().to_le_bytes());
                    }
                    Err(error) => {
                        let duration = error.duration();
                        digest.update([2]);
                        digest.update(duration.as_secs().to_le_bytes());
                        digest.update(duration.subsec_nanos().to_le_bytes());
                    }
                },
            }
        }
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
        match read_header(&path) {
            Ok(header) => {
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
                    all_philips_classic_private_metadata_contract_verified: header
                        .philips_classic_private_metadata_contract_verified,
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
                extend_unique(
                    &mut group.sop_class_uids,
                    header.sop_class_uid.iter().cloned(),
                );
                extend_unique(&mut group.modalities, header.modality.iter().cloned());
                extend_unique(
                    &mut group.manufacturers,
                    header.manufacturer.iter().cloned(),
                );
                group.manufacturer_missing |= header.manufacturer.is_none();
                extend_unique(&mut group.models, header.model.iter().cloned());
                group.model_missing |= header.model.is_none();
                extend_unique(
                    &mut group.software_version_values,
                    header.software_versions.iter().cloned(),
                );
                group.software_versions_missing |= header.software_versions.is_none();
                extend_unique(&mut group.image_types, header.image_type.iter().cloned());
                extend_unique(
                    &mut group.scanning_sequences,
                    header.scanning_sequence.iter().cloned(),
                );
                extend_unique(
                    &mut group.sequence_variants,
                    header.sequence_variant.iter().cloned(),
                );
                extend_unique(&mut group.scan_options, header.scan_options.iter().cloned());
                let local_text = header.local_protocol_text();
                if !local_text.is_empty() {
                    extend_unique(&mut group.local_protocol_texts, [local_text]);
                }
                extend_unique(
                    &mut group.burned_in_annotations,
                    header.burned_in_annotation.iter().cloned(),
                );
                group.burned_in_annotation_missing |= header.burned_in_annotation.is_none();
                group.all_missing_bia_instances_original_primary &=
                    header.burned_in_annotation.is_some()
                        || declares_original_primary(&header.image_type);
                group.siemens_csa_image_header_present |= header.siemens_csa_image_header_present;
                group.all_siemens_csa_image_headers_sanitizable &=
                    header.siemens_csa_image_header_sanitizable;
                group.all_philips_classic_private_metadata_contract_verified &=
                    header.philips_classic_private_metadata_contract_verified;
                // Philips commonly writes DiffusionBValue=0 on ordinary fMRI.
                // Zero is not diffusion evidence without a direction/gradient
                // or diffusion-labelled sequence.
                group.diffusion_context |=
                    header.diffusion_b_value.is_some_and(|value| value > 1.0);
                group.asl_context |= header.asl_technique.is_some();
                group.overlay_or_graphics |= header.overlay_or_graphics;
                group.has_extended_offset_table |= header.has_extended_offset_table;
                if let Some(value) = header.temporal_position_identifier {
                    if !group.temporal_position_identifiers.contains(&value) {
                        group.temporal_position_identifiers.push(value);
                    }
                }
                if let Some(value) = header.acquisition_number {
                    if !group.acquisition_numbers.contains(&value) {
                        group.acquisition_numbers.push(value);
                    }
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
                if sop_instance_uid.is_empty()
                    || group
                        .instances
                        .iter()
                        .any(|instance| instance.sop_instance_uid == sop_instance_uid)
                {
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
                if looks_like_dicom(&path) {
                    unreadable_dicom_like_files += 1;
                    tracing::warn!(path = %path.display(), error = %error, "DICOM-like file could not be parsed");
                }
            }
        }
        let after = file_fingerprint(&path)?;
        changed_while_reading |= before != after;
        source_files.push(after);
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
    let mut last_progress = Instant::now();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.context("could not inspect every entry in the selected folder")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();
        let before = file_fingerprint(&path)?;
        let after = file_fingerprint(&path)?;
        changed_while_reading |= before != after;
        files.push(after);
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            report(SnapshotProgress {
                files_seen: files.len() as u64,
            });
            last_progress = Instant::now();
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    report(SnapshotProgress {
        files_seen: files.len() as u64,
    });
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

fn extend_unique<I>(target: &mut Vec<String>, values: I)
where
    I: IntoIterator<Item = String>,
{
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
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
        || option_conflicts(&left.mr_acquisition_type, &right.mr_acquisition_type)
        || float_conflicts(left.repetition_time_ms, right.repetition_time_ms)
        || float_conflicts(left.echo_time_ms, right.echo_time_ms)
        || float_conflicts(left.flip_angle_degrees, right.flip_angle_degrees)
        || option_conflicts(
            &left.number_of_temporal_positions,
            &right.number_of_temporal_positions,
        )
        || acquisition_value_conflicts(left, right, "rows")
        || acquisition_value_conflicts(left, right, "columns")
        || acquisition_value_conflicts(left, right, "acquisition_matrix")
        || acquisition_value_conflicts(left, right, "pixel_spacing_mm")
        || acquisition_value_conflicts(left, right, "slice_thickness_mm")
}

fn option_conflicts<T: PartialEq>(left: &Option<T>, right: &Option<T>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn float_conflicts(left: Option<f64>, right: Option<f64>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if (left - right).abs() > 1e-6 * left.abs().max(right.abs()).max(1.0))
}

fn acquisition_value_conflicts(left: &DicomHeader, right: &DicomHeader, key: &str) -> bool {
    matches!((left.acquisition.get(key), right.acquisition.get(key)), (Some(left), Some(right)) if left != right)
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

    Ok(DicomHeader {
        path: path.to_path_buf(),
        patient_id: string(&object, Tag(0x0010, 0x0020)),
        issuer_of_patient_id: string(&object, Tag(0x0010, 0x0021)),
        study_uid: string(&object, Tag(0x0020, 0x000D)),
        series_uid: string(&object, Tag(0x0020, 0x000E)),
        sop_class_uid: string(&object, Tag(0x0008, 0x0016)),
        sop_instance_uid: string(&object, Tag(0x0008, 0x0018)),
        modality: string(&object, Tag(0x0008, 0x0060)),
        image_type: multi_string(&object, Tag(0x0008, 0x0008)),
        manufacturer: string(&object, Tag(0x0008, 0x0070)),
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
        mr_acquisition_type: string(&object, Tag(0x0018, 0x0023)),
        echo_planar_pulse_sequence: string(&object, Tag(0x0018, 0x9018)),
        trigger_time_ms: float(&object, Tag(0x0018, 0x1060)),
        philips_dynamic_scan_begin_time_seconds: philips_dynamic_scan_begin_time(&object),
        philips_dynamic_timing_tag_present: philips_dynamic_scan_begin_time_tag_present(&object),
        philips_number_of_slices: philips_number_of_slices(&object),
        philips_classic_private_metadata_contract_verified:
            philips_classic_private_metadata_contract_verified(&object),
        image_position_patient: multi_float(&object, Tag(0x0020, 0x0032)),
        series_number: integer(&object, Tag(0x0020, 0x0011)),
        acquisition_number: integer(&object, Tag(0x0020, 0x0012)),
        instance_number: integer(&object, Tag(0x0020, 0x0013)),
        echo_number: integer(&object, Tag(0x0018, 0x0086)),
        repetition_time_ms: float(&object, Tag(0x0018, 0x0080)),
        echo_time_ms: float(&object, Tag(0x0018, 0x0081)),
        inversion_time_ms: float(&object, Tag(0x0018, 0x0082)),
        flip_angle_degrees: float(&object, Tag(0x0018, 0x1314)),
        number_of_temporal_positions: integer(&object, Tag(0x0020, 0x0105)),
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

fn philips_classic_private_metadata_contract_verified(object: &DefaultDicomObject) -> bool {
    philips_number_of_slices(object).is_some()
        && unique_philips_private_float32(
            object,
            0x2005,
            0x000d,
            "Philips MR Imaging DD 001",
            |value| value.abs() <= 1.0e9,
        )
        && unique_philips_private_float32(
            object,
            0x2005,
            0x000e,
            "Philips MR Imaging DD 001",
            |value| value > 0.0 && value <= 1.0e9,
        )
        && unique_philips_private_float32(
            object,
            0x2001,
            0x0022,
            "Philips Imaging DD 001",
            |value| (0.0..=1.0e6).contains(&value),
        )
}

fn unique_philips_private_float32(
    object: &DefaultDicomObject,
    group: u16,
    low_element: u16,
    expected_creator: &str,
    valid: impl Fn(f32) -> bool,
) -> bool {
    let mut matches = object.iter().filter_map(|element| {
        let tag = element.tag();
        let creator_tag = Tag(tag.group(), tag.element() >> 8);
        (tag.group() == group
            && tag.element() & 0x00ff == low_element
            && object
                .element(creator_tag)
                .ok()
                .and_then(|creator| creator.to_str().ok())
                .is_some_and(|creator| {
                    creator
                        .trim_matches([' ', '\0'])
                        .eq_ignore_ascii_case(expected_creator)
                }))
        .then(|| {
            element.vr() == VR::FL
                && matches!(
                    element.value(),
                    dicom_core::value::Value::Primitive(
                        dicom_core::value::PrimitiveValue::F32(values)
                    ) if values.len() == 1 && values[0].is_finite() && valid(values[0])
                )
        })
    });
    matches.next() == Some(true) && matches.next().is_none()
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
    use tempfile::tempdir;

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
    fn source_fingerprint_is_stable_and_detects_folder_changes() {
        let directory = tempdir().unwrap();
        std::fs::write(directory.path().join("one.dcm"), b"first").unwrap();
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
        assert!(dicom_instance_size_supported(256 * 1024 * 1024));
        assert!(!dicom_instance_size_supported(256 * 1024 * 1024 + 1));
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
}
