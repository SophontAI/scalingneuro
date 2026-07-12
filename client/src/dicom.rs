use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use dicom_core::Tag;
use dicom_object::{DefaultDicomObject, OpenFileOptions};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::model::SourceSummary;

const PIXEL_DATA: Tag = Tag(0x7FE0, 0x0010);

#[derive(Debug, Clone, Default)]
pub struct DicomHeader {
    pub path: PathBuf,
    pub patient_id: Option<String>,
    pub issuer_of_patient_id: Option<String>,
    pub study_uid: Option<String>,
    pub series_uid: Option<String>,
    pub sop_class_uid: Option<String>,
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
    pub inconsistent_subject: bool,
    pub inconsistent_metadata: bool,
    pub sop_class_uids: Vec<String>,
    pub modalities: Vec<String>,
    pub image_types: Vec<String>,
    pub scanning_sequences: Vec<String>,
    pub sequence_variants: Vec<String>,
    pub scan_options: Vec<String>,
    pub local_protocol_texts: Vec<String>,
    pub burned_in_annotations: Vec<String>,
    pub diffusion_context: bool,
    pub asl_context: bool,
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
    unreadable_dicom_like_files: u64,
    changed_while_reading: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl SourceSnapshot {
    pub fn is_stable_with(&self, later: &Self) -> bool {
        !self.changed_while_reading
            && !later.changed_while_reading
            && self.unreadable_dicom_like_files == 0
            && later.unreadable_dicom_like_files == 0
            && self.files == later.files
    }
}

pub fn discover(root: &Path) -> Result<Discovery> {
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

    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.context("could not inspect every entry in the selected folder")?;
        if !entry.file_type().is_file() {
            continue;
        }
        summary.files_seen += 1;
        let path = entry.into_path();
        let before = file_fingerprint(&path)?;
        match read_header(&path) {
            Ok(header) => {
                let after = file_fingerprint(&path)?;
                changed_while_reading |= before != after;
                source_files.push(after);
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
                    inconsistent_subject: false,
                    inconsistent_metadata: false,
                    sop_class_uids: Vec::new(),
                    modalities: Vec::new(),
                    image_types: Vec::new(),
                    scanning_sequences: Vec::new(),
                    sequence_variants: Vec::new(),
                    scan_options: Vec::new(),
                    local_protocol_texts: Vec::new(),
                    burned_in_annotations: Vec::new(),
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
                group.diffusion_context |= header.diffusion_b_value.is_some();
                group.asl_context |= header.asl_technique.is_some();
                if representative_precedes(&header, &group.representative) {
                    let mut representative = header.clone();
                    merge_missing_common(&mut representative, &group.representative);
                    group.representative = representative;
                } else {
                    merge_missing_common(&mut group.representative, &header);
                }
                group.files.push(path);
            }
            Err(error) => {
                if looks_like_dicom(&path) {
                    unreadable_dicom_like_files += 1;
                    tracing::warn!(path = %path.display(), error = %error, "DICOM-like file could not be parsed");
                }
            }
        }
    }

    let mut series: Vec<_> = groups.into_values().collect();
    for group in &mut series {
        group.files.sort();
    }
    source_files.sort_by(|left, right| left.path.cmp(&right.path));
    series.sort_by(|a, b| {
        a.study_uid
            .cmp(&b.study_uid)
            .then(a.series_uid.cmp(&b.series_uid))
    });
    summary.series_found = series.len() as u64;
    Ok(Discovery {
        series,
        summary,
        unreadable_dicom_like_files,
        source_snapshot: SourceSnapshot {
            files: source_files,
            unreadable_dicom_like_files,
            changed_while_reading,
        },
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

fn required_metadata_conflicts(left: &DicomHeader, right: &DicomHeader) -> bool {
    option_conflicts(&left.sop_class_uid, &right.sop_class_uid)
        || option_conflicts(&left.modality, &right.modality)
        || option_conflicts(&left.mr_acquisition_type, &right.mr_acquisition_type)
        || float_conflicts(left.repetition_time_ms, right.repetition_time_ms)
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
    merge_option!(series_number);
    merge_option!(repetition_time_ms);
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
        acquisition,
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
}
