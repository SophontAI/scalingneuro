use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use dicom_core::{
    DataElement, Tag, VR,
    header::{Header, Length},
    value::{DataSetSequence, PrimitiveValue, Value},
};
use dicom_encoding::transfer_syntax::TransferSyntaxIndex;
use dicom_object::{FileMetaTableBuilder, InMemDicomObject, OpenFileOptions};
use dicom_parser::{
    StatefulDecode,
    dataset::{LazyDataToken, lazy_read::LazyDataSetReader},
};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::{
    DICOM_ARCHIVE_CONTRACT_VERSION,
    dicom::{
        DicomHeader, ENHANCED_MR_IMAGE_STORAGE_UID, LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID,
        MAX_DICOM_INSTANCES_PER_SERIES, MAX_DICOM_SERIES_UNCOMPRESSED_BYTES, MR_IMAGE_STORAGE_UID,
        SeriesGroup, dicom_instance_size_supported, dicom_series_uncompressed_size_supported,
        supported_mr_image_sop_class,
    },
    model::{
        Classification, ManifestArchiveObject, ManifestBundle, ManifestObject, MetadataPolicy,
        QcCheck, QcResult, QcStatus, SourceMetadata,
    },
    pseudonym::Pseudonymizer,
};

pub const DICOM_ARCHIVE_FORMAT: &str = "dicom-tar-zstd";
pub const DICOM_MANIFEST_SCHEMA_VERSION: &str = "2.0.0";
pub const DICOM_METADATA_POLICY_ID: &str = "scaling-neuro.dicom-deidentification";
pub const DICOM_METADATA_POLICY_VERSION: &str = "2.0.0";
pub const FUNCTIONAL_EPI_ARCHIVE_ROUTE: &str = "functional-epi-v1";
pub const SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY: &str = "scanner-native-not-defaced";
const DICOM_IMPLEMENTATION_CLASS_UID: &str = "2.25.323468694959424494117938985101850441847";
const DICOM_IMPLEMENTATION_VERSION_NAME: &str = "NEUROSYNC_RAW_1";
const MAX_SEQUENCE_DEPTH: usize = 32;
const MAX_SEQUENCE_ITEMS: usize = 100_000;
const DICOM_ARCHIVE_EXPANSION_FLOOR_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DICOM_ARCHIVE_EXPANSION_RATIO: u64 = 20;
const EXTENDED_OFFSET_TABLE: Tag = Tag(0x7fe0, 0x0001);
const EXTENDED_OFFSET_TABLE_LENGTHS: Tag = Tag(0x7fe0, 0x0002);
const PIXEL_DATA: Tag = Tag(0x7fe0, 0x0010);
const REAL_WORLD_VALUE_MAPPING_SEQUENCE: Tag = Tag(0x0040, 0x9096);
const PIXEL_VALUE_TRANSFORMATION_SEQUENCE: Tag = Tag(0x0028, 0x9145);
const FRAME_VOI_LUT_SEQUENCE: Tag = Tag(0x0028, 0x9132);
const WINDOW_CENTER: Tag = Tag(0x0028, 0x1050);
const WINDOW_WIDTH: Tag = Tag(0x0028, 0x1051);
const WINDOW_CENTER_WIDTH_EXPLANATION: Tag = Tag(0x0028, 0x1055);
const VOI_LUT_FUNCTION: Tag = Tag(0x0028, 0x1056);
const LUT_LABEL: Tag = Tag(0x0040, 0x9210);
const RESCALE_INTERCEPT: Tag = Tag(0x0028, 0x1052);
const RESCALE_SLOPE: Tag = Tag(0x0028, 0x1053);
const RESCALE_TYPE: Tag = Tag(0x0028, 0x1054);
const SHARED_FUNCTIONAL_GROUPS_SEQUENCE: Tag = Tag(0x5200, 0x9229);
const PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE: Tag = Tag(0x5200, 0x9230);
const DIMENSION_INDEX_SEQUENCE: Tag = Tag(0x0020, 0x9222);
const DIMENSION_ORGANIZATION_UID: Tag = Tag(0x0020, 0x9164);
const DIMENSION_INDEX_POINTER: Tag = Tag(0x0020, 0x9165);
const FUNCTIONAL_GROUP_POINTER: Tag = Tag(0x0020, 0x9167);
const DIMENSION_INDEX_PRIVATE_CREATOR: Tag = Tag(0x0020, 0x9213);
const FUNCTIONAL_GROUP_PRIVATE_CREATOR: Tag = Tag(0x0020, 0x9238);
const DIMENSION_INDEX_VALUES: Tag = Tag(0x0020, 0x9157);
const FRAME_CONTENT_SEQUENCE: Tag = Tag(0x0020, 0x9111);
const PIXEL_MEASURES_SEQUENCE: Tag = Tag(0x0028, 0x9110);
const PLANE_POSITION_SEQUENCE: Tag = Tag(0x0020, 0x9113);
const PLANE_ORIENTATION_SEQUENCE: Tag = Tag(0x0020, 0x9116);
const FRAME_ANATOMY_SEQUENCE: Tag = Tag(0x0020, 0x9071);
const MR_IMAGE_FRAME_TYPE_SEQUENCE: Tag = Tag(0x0018, 0x9226);
const MR_METABOLITE_MAP_SEQUENCE: Tag = Tag(0x0018, 0x9152);
const METABOLITE_MAP_DESCRIPTION: Tag = Tag(0x0018, 0x9080);
const MR_RECEIVE_COIL_SEQUENCE: Tag = Tag(0x0018, 0x9042);
const RECEIVE_COIL_NAME: Tag = Tag(0x0018, 0x1250);
const RECEIVE_COIL_MANUFACTURER_NAME: Tag = Tag(0x0018, 0x9041);
const RECEIVE_COIL_TYPE: Tag = Tag(0x0018, 0x9043);
const QUADRATURE_RECEIVE_COIL: Tag = Tag(0x0018, 0x9044);
const MULTI_COIL_DEFINITION_SEQUENCE: Tag = Tag(0x0018, 0x9045);
const MULTI_COIL_CONFIGURATION: Tag = Tag(0x0018, 0x9046);
const MULTI_COIL_ELEMENT_NAME: Tag = Tag(0x0018, 0x9047);
const MULTI_COIL_ELEMENT_USED: Tag = Tag(0x0018, 0x9048);
const MAX_MULTI_COIL_ELEMENTS: usize = 256;
const MR_TRANSMIT_COIL_SEQUENCE: Tag = Tag(0x0018, 0x9049);
const TRANSMIT_COIL_NAME: Tag = Tag(0x0018, 0x1251);
const TRANSMIT_COIL_MANUFACTURER_NAME: Tag = Tag(0x0018, 0x9050);
const TRANSMIT_COIL_TYPE: Tag = Tag(0x0018, 0x9051);
const ACQUISITION_CONTEXT_SEQUENCE: Tag = Tag(0x0040, 0x0555);
const UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE: Tag = Tag(0x0020, 0x9170);
const UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE: Tag = Tag(0x0020, 0x9171);
const CONVERSION_SOURCE_ATTRIBUTES_SEQUENCE: Tag = Tag(0x0020, 0x9172);
const REFERENCED_IMAGE_SEQUENCE: Tag = Tag(0x0008, 0x1140);
const SOURCE_IMAGE_SEQUENCE: Tag = Tag(0x0008, 0x2112);
const DERIVATION_IMAGE_SEQUENCE: Tag = Tag(0x0008, 0x9124);
const DERIVATION_CODE_SEQUENCE: Tag = Tag(0x0008, 0x9215);
const PURPOSE_OF_REFERENCE_CODE_SEQUENCE: Tag = Tag(0x0040, 0xa170);
const LOCALIZER_PURPOSE_CONTEXT_UID: &str = "1.2.840.10008.6.1.508";
const ANATOMY_CONTEXT_UID: &str = "1.2.840.10008.6.1.307";
const RETIRED_GROUP_LENGTH: Tag = Tag(0x0008, 0x0000);
const MAX_REFERENCE_ITEM_GROUP_LENGTH: u32 = 1024 * 1024;
const ENHANCED_CONTENT_DATE_SENTINEL: &str = "19000101";
const ENHANCED_CONTENT_TIME_SENTINEL: &str = "000000";
const ENHANCED_FRAME_DATETIME_SENTINEL: &str = "19000101000000";

pub struct ArchiveRequest<'a, F> {
    pub group: &'a SeriesGroup,
    pub classification: Classification,
    pub pseudonymizer: &'a Pseudonymizer,
    pub bundle_root: &'a Path,
    pub progress: F,
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveManifest {
    schema_version: &'static str,
    series_archive_id: String,
    series_id: String,
    subject_id: String,
    session_id: String,
    protocol_group_id: String,
    modality: &'static str,
    series_kind: String,
    archive_route: &'static str,
    pixel_data_policy: &'static str,
    dicom_instance_count: u64,
    writer_contract: ArchiveWriterContract,
    deidentification: DeidentificationAudit,
    source: SourceMetadata,
    classification: Classification,
    instances: Vec<ArchiveInstance>,
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveWriterContract {
    name: &'static str,
    version: String,
}

#[derive(Debug, Clone, Serialize)]
struct DeidentificationAudit {
    policy_id: &'static str,
    policy_version: &'static str,
    method: &'static str,
    recursive: bool,
    private_text_removed: bool,
    unknown_private_removed: bool,
    uids_remapped: bool,
    pixel_data_retained: bool,
    defacing_performed: bool,
    recognizable_visual_features: &'static str,
    burned_in_annotation_status: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    safe_private_exceptions: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    metadata_transformations: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveInstance {
    path: String,
    size_bytes: u64,
    sha256: String,
    sop_instance_uid: String,
}

#[derive(Serialize)]
struct ArchiveIdentityPreimage<'a> {
    schema_version: &'static str,
    series_id: &'a str,
    subject_id: &'a str,
    session_id: &'a str,
    protocol_group_id: &'a str,
    modality: &'static str,
    series_kind: &'a str,
    archive_route: &'static str,
    pixel_data_policy: &'static str,
    dicom_instance_count: u64,
    writer_contract: &'a ArchiveWriterContract,
    deidentification: &'a DeidentificationAudit,
    source: &'a SourceMetadata,
    classification: &'a Classification,
    instances: &'a [ArchiveInstance],
}

struct PreparedDicom {
    path: tempfile::TempPath,
    size: u64,
    sop_instance_uid: String,
}

struct DigestReader<R> {
    inner: R,
    digest: Sha256,
}

struct DigestWriter<W> {
    inner: W,
    digest: Sha256,
    bytes: u64,
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.digest.update(&buffer[..written]);
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            self.digest.update(&buffer[..read]);
        }
        Ok(read)
    }
}

#[derive(Debug, Clone, Copy)]
struct FileSpan {
    start: u64,
    len: u64,
    value_len: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtendedOffsetTable {
    offsets: Vec<u64>,
    lengths: Vec<u64>,
}

struct TrackingReader<R> {
    inner: R,
    position: Arc<AtomicU64>,
}

impl<R: Read> Read for TrackingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.position.fetch_add(read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

impl<R: Seek> Seek for TrackingReader<R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let position = self.inner.seek(position)?;
        self.position.store(position, Ordering::Relaxed);
        Ok(position)
    }
}

struct UidRemapper<'a> {
    pseudonymizer: &'a Pseudonymizer,
    mapped: HashMap<String, String>,
}

#[derive(Default)]
struct SanitizationStats {
    classic_image_type_components_replaced_with_other: u64,
    siemens_csa_headers_rewritten: u64,
    siemens_ps315_diffusion_attributes_retained: u64,
    philips_ps315_diffusion_attributes_retained: u64,
    philips_ps315_phase_attributes_retained: u64,
    ge_ps315_diffusion_attributes_retained: u64,
    uih_grid_slice_count_attributes_retained: u64,
    uih_diffusion_attributes_retained: u64,
    philips_dd001_diffusion_vector_attributes_retained: u64,
    philips_dd005_diffusion_index_attributes_retained: u64,
    philips_dd005_asl_label_attributes_retained: u64,
    ge_acqu_diffusion_vector_attributes_retained: u64,
    ge_parm_asl_attributes_retained: u64,
    philips_ps315_scaling_attributes_retained: u64,
    philips_ps315_number_of_slices_retained: u64,
    philips_ps315_water_fat_shift_retained: u64,
    philips_ps315_per_frame_scale_sequences_rebuilt: u64,
    philips_redundant_trigger_times_suppressed: u64,
    asl_technique_descriptions_emptied: u64,
    asl_crusher_descriptions_redacted: u64,
    asl_bolus_cutoff_techniques_emptied: u64,
    current_sequence_items: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FunctionalGroupContext {
    None,
    Shared,
    PerFrame,
}

pub fn create_dicom_archive<F>(mut request: ArchiveRequest<'_, F>) -> Result<ManifestBundle>
where
    F: FnMut(u64),
{
    let group = request.group;
    if request.classification.decision != crate::model::ClassificationDecision::Accepted {
        bail!("only an accepted privacy-clearable MR image series can be archived");
    }
    if !supported_series_kind(&request.classification.kind) {
        bail!("accepted MR image series has an unsupported classification kind");
    }
    if group.files.is_empty() {
        bail!("cannot archive an empty DICOM series");
    }
    if group.files.len() > MAX_DICOM_INSTANCES_PER_SERIES {
        bail!("DICOM series exceeds the 500000-instance archive limit");
    }
    let source_sizes = group
        .files
        .iter()
        .map(|path| fs::metadata(path).map(|metadata| metadata.len()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if source_sizes
        .iter()
        .any(|size| !dicom_instance_size_supported(*size))
    {
        bail!("dicom_instance_exceeds_64_gib");
    }
    if !dicom_series_uncompressed_size_supported(source_sizes) {
        bail!("series_exceeds_64_gib_uncompressed_dicom_limit");
    }
    if group
        .burned_in_annotations
        .iter()
        .any(|value| !value.eq_ignore_ascii_case("NO"))
    {
        bail!("MR DICOM series declared possible burned-in annotation");
    }

    let subject_id = match group.representative.patient_id.as_deref() {
        Some(patient_id) => request.pseudonymizer.subject_id(
            patient_id,
            group.representative.issuer_of_patient_id.as_deref(),
        ),
        None => request
            .pseudonymizer
            .id("subject-session-fallback", &group.study_uid),
    };
    let session_id = request.pseudonymizer.id("session", &group.study_uid);
    let series_id = request.pseudonymizer.id("series", &group.series_uid);
    let protocol_group_id = request
        .pseudonymizer
        .protocol_group_id(&protocol_group_input(group));

    if group.instances.len() != group.files.len()
        || group.duplicate_sop_instance_uid
        || group
            .instances
            .iter()
            .any(|instance| instance.sop_instance_uid.is_empty())
    {
        bail!("DICOM series has an invalid SOP Instance UID inventory");
    }
    let sources = &group.instances;
    let suppress_redundant_philips_trigger = request.classification.kind == "functional_epi"
        && group.philips_dynamic_timing_contract_verified;

    let mut remapper = UidRemapper {
        pseudonymizer: request.pseudonymizer,
        mapped: HashMap::new(),
    };
    let mut instances = Vec::with_capacity(sources.len());
    let mut rewritten_dicom_bytes = 0_u64;
    let mut stats = SanitizationStats::default();
    let temporary = tempfile::NamedTempFile::new_in(request.bundle_root)?.into_temp_path();
    let output = DigestWriter {
        inner: BufWriter::with_capacity(1024 * 1024, File::create(&temporary)?),
        digest: Sha256::new(),
        bytes: 0,
    };
    let mut encoder = zstd::stream::write::Encoder::new(output, 1)?;
    encoder.include_checksum(true)?;
    let mut archive = tar::Builder::new(encoder);
    archive.mode(tar::HeaderMode::Deterministic);
    for (index, source) in sources.iter().enumerate() {
        let relative_path = format!("dicom/{:06}.dcm", index + 1);
        let prepared = prepare_sanitized_dicom(
            &source.path,
            &subject_id,
            &mut remapper,
            &mut stats,
            suppress_redundant_philips_trigger,
            request.bundle_root,
            &mut request.progress,
        )?;
        let sop_instance_uid = prepared.sop_instance_uid.clone();
        let size_bytes = prepared.size;
        if !dicom_instance_size_supported(size_bytes) {
            bail!("dicom_instance_exceeds_64_gib");
        }
        rewritten_dicom_bytes = rewritten_dicom_bytes
            .checked_add(size_bytes)
            .context("rewritten DICOM series byte total overflow")?;
        if rewritten_dicom_bytes > MAX_DICOM_SERIES_UNCOMPRESSED_BYTES {
            bail!("series_exceeds_64_gib_uncompressed_dicom_limit");
        }
        let sha256 = append_verified_dicom(&mut archive, &relative_path, prepared)?;
        instances.push(ArchiveInstance {
            path: relative_path,
            size_bytes,
            sha256,
            sop_instance_uid,
        });
    }

    let classification = request.classification;
    let series_kind = classification.kind.clone();
    let archive_route = archive_route_for_kind(&series_kind);
    let writer_contract = archive_writer_contract();
    let deidentification = DeidentificationAudit {
        policy_id: DICOM_METADATA_POLICY_ID,
        policy_version: DICOM_METADATA_POLICY_VERSION,
        method: "scaling-neuro-recursive-allowlist-v2",
        recursive: true,
        private_text_removed: true,
        unknown_private_removed: true,
        uids_remapped: true,
        pixel_data_retained: true,
        defacing_performed: false,
        recognizable_visual_features: "may_be_present",
        burned_in_annotation_status: if group.burned_in_annotation_missing {
            "not_declared"
        } else {
            "verified_no"
        },
        safe_private_exceptions: [
            (stats.siemens_csa_headers_rewritten > 0)
                .then_some("siemens_csa_image_header_numeric_v1"),
            (stats.siemens_ps315_diffusion_attributes_retained > 0)
                .then_some("dicom_ps3.15_siemens_mr_header_diffusion"),
            (stats.philips_ps315_diffusion_attributes_retained > 0)
                .then_some("dicom_ps3.15_philips_diffusion"),
            (stats.philips_ps315_phase_attributes_retained > 0)
                .then_some("dicom_ps3.15_philips_phase_number"),
            (stats.ge_ps315_diffusion_attributes_retained > 0)
                .then_some("dicom_ps3.15_ge_diffusion_b_value"),
            (stats.uih_grid_slice_count_attributes_retained > 0)
                .then_some("uih_image_private_header_grid_slice_count_numeric_v1"),
            (stats.uih_diffusion_attributes_retained > 0)
                .then_some("uih_image_private_header_diffusion_numeric_v1"),
            (stats.philips_dd001_diffusion_vector_attributes_retained > 0)
                .then_some("philips_mr_imaging_dd_001_diffusion_gradient_vector_numeric_v1"),
            (stats.philips_dd005_diffusion_index_attributes_retained > 0)
                .then_some("philips_mr_imaging_dd_005_diffusion_indices_numeric_v1"),
            (stats.philips_dd005_asl_label_attributes_retained > 0)
                .then_some("philips_mr_imaging_dd_005_asl_label_code_v1"),
            (stats.ge_acqu_diffusion_vector_attributes_retained > 0)
                .then_some("ge_gems_acqu_01_diffusion_gradient_vector_numeric_v1"),
            (stats.ge_parm_asl_attributes_retained > 0)
                .then_some("ge_gems_parm_01_asl_technique_duration_v1"),
            (stats.philips_ps315_scaling_attributes_retained > 0)
                .then_some("dicom_ps3.15_philips_scale_intercept_slope"),
            (stats.philips_ps315_number_of_slices_retained > 0)
                .then_some("dicom_ps3.15_philips_number_of_slices"),
            (stats.philips_ps315_water_fat_shift_retained > 0)
                .then_some("dicom_ps3.15_philips_water_fat_shift"),
            (stats.philips_ps315_per_frame_scale_sequences_rebuilt > 0)
                .then_some("dicom_ps3.15_philips_per_frame_scale_slope"),
        ]
        .into_iter()
        .flatten()
        .collect(),
        metadata_transformations: [
            (stats.classic_image_type_components_replaced_with_other > 0)
                .then_some("replaced_unknown_classic_image_type_components_with_other"),
            (stats.philips_redundant_trigger_times_suppressed > 0)
                .then_some("suppressed_redundant_philips_dynamic_trigger_time"),
            (stats.asl_technique_descriptions_emptied > 0)
                .then_some("emptied_asl_technique_description"),
            (stats.asl_crusher_descriptions_redacted > 0)
                .then_some("redacted_asl_crusher_description"),
            (stats.asl_bolus_cutoff_techniques_emptied > 0)
                .then_some("emptied_asl_bolus_cutoff_technique"),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    let source = safe_source_metadata(group);
    let series_archive_id = derive_series_archive_id(
        request.pseudonymizer,
        &series_id,
        &subject_id,
        &session_id,
        &protocol_group_id,
        &series_kind,
        archive_route,
        &writer_contract,
        &deidentification,
        &source,
        &classification,
        &instances,
    )?;
    let archive_manifest = ArchiveManifest {
        schema_version: DICOM_MANIFEST_SCHEMA_VERSION,
        series_archive_id: series_archive_id.clone(),
        series_id: series_id.clone(),
        subject_id: subject_id.clone(),
        session_id: session_id.clone(),
        protocol_group_id: protocol_group_id.clone(),
        modality: "mr",
        series_kind: series_kind.clone(),
        archive_route,
        pixel_data_policy: SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY,
        dicom_instance_count: instances.len() as u64,
        writer_contract,
        deidentification,
        source,
        classification: classification.clone(),
        instances,
    };
    let manifest_bytes = serde_json::to_vec(&archive_manifest)?;
    append_bytes(&mut archive, "manifest.json", &manifest_bytes)?;
    let encoder = archive.into_inner()?;
    let mut output = encoder.finish()?;
    output.flush()?;
    let DigestWriter {
        inner,
        digest,
        bytes: archive_size,
    } = output;
    drop(inner);
    let archive_sha256 = hex::encode(digest.finalize());
    if !dicom_archive_expansion_supported(rewritten_dicom_bytes, archive_size) {
        bail!("dicom_archive_expansion_ratio_exceeded");
    }
    let directory = request.bundle_root.join(&series_archive_id);
    fs::create_dir_all(&directory)?;
    let archive_path = directory.join("dicom.tar.zst");
    temporary.persist(&archive_path)?;
    if fs::metadata(&archive_path)?.len() != archive_size {
        bail!("prepared archive size changed while it was finalized");
    }

    Ok(ManifestBundle {
        bundle_id: series_archive_id.clone(),
        series_id,
        subject_id,
        session_id,
        protocol_group_id,
        series_kind,
        archive_route: archive_route.into(),
        pixel_data_policy: SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY.into(),
        archive: Some(ManifestArchiveObject {
            object: ManifestObject {
                relative_key: format!("{series_archive_id}/dicom.tar.zst"),
                local_path: archive_path.to_string_lossy().into_owned(),
                size: archive_size,
                sha256: archive_sha256,
            },
            format: DICOM_ARCHIVE_FORMAT.into(),
            dicom_instance_count: group.files.len() as u64,
            deidentification_profile: DICOM_METADATA_POLICY_ID.into(),
            deidentification_profile_version: DICOM_METADATA_POLICY_VERSION.into(),
        }),
        source_dicom_count: group.files.len() as u64,
        classification,
        qc: QcResult {
            passed: true,
            checks: vec![
                pass("supported_mr_image_gate"),
                pass("local_dicom_privacy_gate"),
                pass(if group.burned_in_annotation_missing {
                    "burned_in_annotation_not_declared_original_primary_gate"
                } else {
                    "burned_in_annotation_explicitly_no"
                }),
                pass("recursive_public_attribute_allowlist"),
                pass("private_text_and_unknown_private_removed"),
                pass("dicom_uids_deterministically_remapped"),
                pass("pixel_data_retained"),
            ],
            warnings: [
                (stats.classic_image_type_components_replaced_with_other > 0).then(|| {
                    format!(
                        "classic_image_type_supplemental_metadata_incomplete_replaced_with_other:{}",
                        stats.classic_image_type_components_replaced_with_other
                    )
                }),
                (stats.siemens_csa_headers_rewritten > 0).then(|| {
                    format!(
                        "rewritten_numeric_siemens_csa_image_headers:{}",
                        stats.siemens_csa_headers_rewritten
                    )
                }),
                (stats.siemens_ps315_diffusion_attributes_retained > 0).then(|| {
                    format!(
                        "retained_ps315_siemens_diffusion_attributes:{}",
                        stats.siemens_ps315_diffusion_attributes_retained
                    )
                }),
                (stats.philips_ps315_diffusion_attributes_retained > 0).then(|| {
                    format!(
                        "retained_ps315_philips_diffusion_attributes:{}",
                        stats.philips_ps315_diffusion_attributes_retained
                    )
                }),
                (stats.philips_ps315_phase_attributes_retained > 0).then(|| {
                    format!(
                        "retained_ps315_philips_phase_attributes:{}",
                        stats.philips_ps315_phase_attributes_retained
                    )
                }),
                (stats.ge_ps315_diffusion_attributes_retained > 0).then(|| {
                    format!(
                        "retained_ps315_ge_diffusion_attributes:{}",
                        stats.ge_ps315_diffusion_attributes_retained
                    )
                }),
                (stats.uih_grid_slice_count_attributes_retained > 0).then(|| {
                    format!(
                        "retained_uih_grid_slice_count_attributes:{}",
                        stats.uih_grid_slice_count_attributes_retained
                    )
                }),
                (stats.uih_diffusion_attributes_retained > 0).then(|| {
                    format!(
                        "retained_uih_diffusion_attributes:{}",
                        stats.uih_diffusion_attributes_retained
                    )
                }),
                (stats.philips_dd001_diffusion_vector_attributes_retained > 0).then(|| {
                    format!(
                        "retained_philips_dd001_diffusion_vector_attributes:{}",
                        stats.philips_dd001_diffusion_vector_attributes_retained
                    )
                }),
                (stats.philips_dd005_diffusion_index_attributes_retained > 0).then(|| {
                    format!(
                        "retained_philips_dd005_diffusion_index_attributes:{}",
                        stats.philips_dd005_diffusion_index_attributes_retained
                    )
                }),
                (stats.philips_dd005_asl_label_attributes_retained > 0).then(|| {
                    format!(
                        "retained_philips_dd005_asl_label_attributes:{}",
                        stats.philips_dd005_asl_label_attributes_retained
                    )
                }),
                (stats.ge_acqu_diffusion_vector_attributes_retained > 0).then(|| {
                    format!(
                        "retained_ge_acqu_diffusion_vector_attributes:{}",
                        stats.ge_acqu_diffusion_vector_attributes_retained
                    )
                }),
                (stats.ge_parm_asl_attributes_retained > 0).then(|| {
                    format!(
                        "retained_ge_parm_asl_attributes:{}",
                        stats.ge_parm_asl_attributes_retained
                    )
                }),
                (stats.philips_ps315_scaling_attributes_retained > 0).then(|| {
                    format!(
                        "retained_ps315_philips_scaling_attributes:{}",
                        stats.philips_ps315_scaling_attributes_retained
                    )
                }),
                (stats.philips_ps315_number_of_slices_retained > 0).then(|| {
                    format!(
                        "retained_ps315_philips_number_of_slices:{}",
                        stats.philips_ps315_number_of_slices_retained
                    )
                }),
                (stats.philips_ps315_water_fat_shift_retained > 0).then(|| {
                    format!(
                        "retained_ps315_philips_water_fat_shift:{}",
                        stats.philips_ps315_water_fat_shift_retained
                    )
                }),
                (stats.philips_ps315_per_frame_scale_sequences_rebuilt > 0).then(|| {
                    format!(
                        "rebuilt_ps315_philips_per_frame_scale_sequences:{}",
                        stats.philips_ps315_per_frame_scale_sequences_rebuilt
                    )
                }),
                (stats.philips_redundant_trigger_times_suppressed > 0).then(|| {
                    format!(
                        "suppressed_redundant_philips_dynamic_trigger_times:{}",
                        stats.philips_redundant_trigger_times_suppressed
                    )
                }),
                (stats.asl_technique_descriptions_emptied > 0).then(|| {
                    format!(
                        "emptied_asl_technique_descriptions:{}",
                        stats.asl_technique_descriptions_emptied
                    )
                }),
                (stats.asl_crusher_descriptions_redacted > 0).then(|| {
                    format!(
                        "redacted_asl_crusher_descriptions:{}",
                        stats.asl_crusher_descriptions_redacted
                    )
                }),
                (stats.asl_bolus_cutoff_techniques_emptied > 0).then(|| {
                    format!(
                        "emptied_asl_bolus_cutoff_techniques:{}",
                        stats.asl_bolus_cutoff_techniques_emptied
                    )
                }),
                group
                    .burned_in_annotation_missing
                    .then(|| "burned_in_annotation_not_declared".to_owned()),
            ]
            .into_iter()
            .flatten()
            .collect(),
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn derive_series_archive_id(
    pseudonymizer: &Pseudonymizer,
    series_id: &str,
    subject_id: &str,
    session_id: &str,
    protocol_group_id: &str,
    series_kind: &str,
    archive_route: &'static str,
    writer_contract: &ArchiveWriterContract,
    deidentification: &DeidentificationAudit,
    source: &SourceMetadata,
    classification: &Classification,
    instances: &[ArchiveInstance],
) -> Result<String> {
    let preimage = ArchiveIdentityPreimage {
        schema_version: DICOM_MANIFEST_SCHEMA_VERSION,
        series_id,
        subject_id,
        session_id,
        protocol_group_id,
        modality: "mr",
        series_kind,
        archive_route,
        pixel_data_policy: SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY,
        dicom_instance_count: instances.len() as u64,
        writer_contract,
        deidentification,
        source,
        classification,
        instances,
    };
    let mut digest = Sha256::new();
    digest.update(b"scaling-neuro-dicom-series-archive-identity-v3\0");
    digest.update(serde_json::to_vec(&preimage)?);
    Ok(pseudonymizer.id("dicom-series-archive-v3", &hex::encode(digest.finalize())))
}

pub fn archive_route_for_kind(series_kind: &str) -> &'static str {
    debug_assert_eq!(series_kind, "functional_epi");
    FUNCTIONAL_EPI_ARCHIVE_ROUTE
}

pub fn supported_series_kind(series_kind: &str) -> bool {
    series_kind == "functional_epi"
}

fn prepare_sanitized_dicom<F: FnMut(u64)>(
    source_path: &Path,
    subject_id: &str,
    remapper: &mut UidRemapper<'_>,
    stats: &mut SanitizationStats,
    suppress_redundant_philips_trigger: bool,
    temporary_root: &Path,
    progress: &mut F,
) -> Result<PreparedDicom> {
    let mut source_snapshot = stage_source_dicom(source_path, temporary_root, progress)?;
    let object = OpenFileOptions::new()
        .read_until(PIXEL_DATA)
        .open_file(source_snapshot.path())
        .with_context(|| format!("could not read selected DICOM: {}", source_path.display()))?;
    let image_type_profile = object_mr_image_type_profile(&object);
    validate_pixel_transforms(
        &object,
        0,
        PixelTransformValidationStage::Source,
        PixelTransformContext::root(),
    )?;
    validate_source_dimension_index_pointers(&object)?;
    validate_reference_semantics(&object, 0, ReferenceValidationStage::Source, false)?;
    validate_context_uid_placement(&object, 0, &mut Vec::new())?;
    validate_metabolite_map_placement(&object, 0, false)?;
    validate_surface_transmit_alias_placement(&object, 0, false)?;
    validate_source_enhanced_mr_surface(&object, image_type_profile)?;
    validate_source_asl_conditionals(&object, 0)?;
    if contains_overlay_or_graphics(&object, 0) {
        bail!("DICOM contains overlay or graphic data and was held locally");
    }
    let valid_image_type = object
        .get(Tag(0x0008, 0x0008))
        .filter(|element| element.vr() == VR::CS)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| canonical_image_type(value.as_ref(), image_type_profile))
        .is_some();
    if !valid_image_type {
        bail!("DICOM ImageType failed positional validation");
    }
    let burned_in = object
        .get(Tag(0x0028, 0x0301))
        .and_then(|element| element.to_str().ok())
        .map(|value| value.trim_matches([' ', '\0']).to_owned());
    match burned_in.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("NO") => {}
        Some(_) => bail!("DICOM declared possible burned-in annotation"),
        None if image_type_profile != MrImageTypeProfile::Classic => {
            bail!("Enhanced MR omitted required BurnedInAnnotation=NO")
        }
        None if image_type_profile != MrImageTypeProfile::Enhanced
            && declares_original_primary(&object) => {}
        None => bail!(
            "DICOM omitted BurnedInAnnotation without declaring ORIGINAL and PRIMARY image type"
        ),
    }
    let transfer_syntax = object
        .meta()
        .transfer_syntax
        .trim_matches([' ', '\0'])
        .to_owned();
    if transfer_syntax == "1.2.840.10008.1.2.1.99" {
        bail!("deflated DICOM transfer syntax is not supported by the bounded privacy writer");
    }
    let pixel_span = locate_pixel_data(
        source_snapshot.path(),
        &transfer_syntax,
        object.meta().information_group_length,
    )?;
    let extended_offset_table = validate_extended_offset_table(
        &object,
        source_snapshot.path(),
        &transfer_syntax,
        pixel_span,
    )
    .context("DICOM Extended Offset Table failed structural validation")?;
    stats.current_sequence_items = 0;
    let sanitized = sanitize_dataset(
        object.into_inner(),
        remapper,
        stats,
        0,
        image_type_profile,
        FunctionalGroupContext::None,
        None,
        suppress_redundant_philips_trigger,
        extended_offset_table.is_some(),
    )?;
    let sop_instance_uid = sanitized
        .element(Tag(0x0008, 0x0018))?
        .to_str()?
        .trim_matches([' ', '\0'])
        .to_owned();
    let sop_class_uid = sanitized
        .element(Tag(0x0008, 0x0016))?
        .to_str()?
        .trim_matches([' ', '\0'])
        .to_owned();
    let mut sanitized = sanitized;
    sanitized.put_str(Tag(0x0010, 0x0010), VR::PN, subject_id);
    sanitized.put_str(Tag(0x0010, 0x0020), VR::LO, subject_id);
    sanitized.put_str(Tag(0x0012, 0x0062), VR::CS, "YES");
    sanitized.put_str(
        Tag(0x0012, 0x0063),
        VR::LO,
        format!(
            "Scaling Neuro {} {}",
            DICOM_METADATA_POLICY_ID, DICOM_METADATA_POLICY_VERSION
        ),
    );
    // Preserve an explicit NO, but never manufacture a claim the scanner did
    // not make. The archive manifest records `not_declared` separately.
    if burned_in.is_some() {
        sanitized.put_str(Tag(0x0028, 0x0301), VR::CS, "NO");
    }
    sanitized.put_str(Tag(0x0028, 0x0303), VR::CS, "REMOVED");
    insert_required_type_two_attributes(&mut sanitized);
    if image_type_profile != MrImageTypeProfile::Classic {
        // Enhanced MR declares Content Date and Content Time as Type 1. Use
        // fixed, explicitly non-source sentinels so the IOD remains valid
        // without retaining acquisition chronology.
        sanitized.put_str(Tag(0x0008, 0x0023), VR::DA, ENHANCED_CONTENT_DATE_SENTINEL);
        sanitized.put_str(Tag(0x0008, 0x0033), VR::TM, ENHANCED_CONTENT_TIME_SENTINEL);
        // Acquisition Context is Type 2 for both Enhanced MR IODs. Source
        // contexts are accepted only when already empty; write a canonical
        // zero-item shell so the privacy and conformance contracts agree.
        sanitized.put(DataElement::new(
            ACQUISITION_CONTEXT_SEQUENCE,
            VR::SQ,
            Value::Sequence(DataSetSequence::new(Vec::new(), Length::UNDEFINED)),
        ));
    }
    let expected_pixel_value_len = validate_supported_mr_iod_contract(&sanitized, subject_id)?;
    let transfer_syntax_descriptor = TransferSyntaxRegistry
        .get(&transfer_syntax)
        .context("DICOM transfer syntax is not supported for pixel validation")?;
    if !transfer_syntax_descriptor.is_encapsulated_pixel_data()
        && pixel_span.value_len != Some(expected_pixel_value_len)
    {
        bail!("DICOM native PixelData length does not match its declared pixel matrix");
    }
    audit_dataset(&sanitized, subject_id, 0)?;
    let file = sanitized.with_meta(
        FileMetaTableBuilder::new()
            .media_storage_sop_class_uid(&sop_class_uid)
            .media_storage_sop_instance_uid(sop_instance_uid.clone())
            .transfer_syntax(&transfer_syntax)
            .implementation_class_uid(DICOM_IMPLEMENTATION_CLASS_UID)
            .implementation_version_name(DICOM_IMPLEMENTATION_VERSION_NAME),
    )?;
    let mut final_file = tempfile::NamedTempFile::new_in(temporary_root)?;
    file.write_all(final_file.as_file_mut())?;
    source_snapshot
        .as_file_mut()
        .seek(SeekFrom::Start(pixel_span.start))?;
    let mut source_pixel = DigestReader {
        inner: source_snapshot.as_file_mut().take(pixel_span.len),
        digest: Sha256::new(),
    };
    let copied = std::io::copy(&mut source_pixel, final_file.as_file_mut())?;
    if copied != pixel_span.len {
        bail!("source DICOM PixelData changed or was truncated during privacy preparation");
    }
    let source_pixel_sha256: [u8; 32] = source_pixel.digest.finalize().into();
    final_file.as_file_mut().flush()?;
    let size = final_file.as_file().metadata()?.len();
    audit_final_dicom(
        final_file.path(),
        subject_id,
        &sop_class_uid,
        &sop_instance_uid,
        &transfer_syntax,
        pixel_span,
        source_pixel_sha256,
        extended_offset_table.as_ref(),
    )?;
    Ok(PreparedDicom {
        path: final_file.into_temp_path(),
        size,
        sop_instance_uid,
    })
}

fn stage_source_dicom<F: FnMut(u64)>(
    source_path: &Path,
    temporary_root: &Path,
    progress: &mut F,
) -> Result<tempfile::NamedTempFile> {
    let path_before = fs::metadata(source_path)?;
    let source = File::open(source_path)?;
    let handle_before = source.metadata()?;
    if !same_file_observation(&path_before, &handle_before) {
        bail!("source DICOM changed while it was opened for privacy preparation");
    }
    let mut reader = BufReader::with_capacity(1024 * 1024, source);
    let mut snapshot = tempfile::NamedTempFile::new_in(temporary_root)?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        snapshot.write_all(&buffer[..read])?;
        copied = copied.saturating_add(read as u64);
        progress(read as u64);
    }
    snapshot.flush()?;
    let handle_after = reader.get_ref().metadata()?;
    let path_after = fs::metadata(source_path)?;
    if copied != handle_before.len()
        || !same_file_observation(&handle_before, &handle_after)
        || !same_file_observation(&handle_after, &path_after)
    {
        bail!("source DICOM changed while its immutable privacy snapshot was captured");
    }
    Ok(snapshot)
}

fn same_file_observation(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
        && platform_file_identity_matches(left, right)
}

#[cfg(unix)]
fn platform_file_identity_matches(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn platform_file_identity_matches(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

#[allow(clippy::too_many_arguments)]
fn audit_final_dicom(
    path: &Path,
    subject_id: &str,
    expected_sop_class_uid: &str,
    expected_sop_instance_uid: &str,
    expected_transfer_syntax: &str,
    expected_source_pixel: FileSpan,
    expected_pixel_sha256: [u8; 32],
    expected_extended_offset_table: Option<&ExtendedOffsetTable>,
) -> Result<()> {
    let object = OpenFileOptions::new()
        .read_until(PIXEL_DATA)
        .open_file(path)
        .context("could not reparse the exact sanitized DICOM output")?;
    let meta = object.meta();
    if clean_meta_value(&meta.media_storage_sop_class_uid) != expected_sop_class_uid
        || clean_meta_value(&meta.media_storage_sop_instance_uid) != expected_sop_instance_uid
        || clean_meta_value(&meta.transfer_syntax) != expected_transfer_syntax
        || clean_meta_value(&meta.implementation_class_uid) != DICOM_IMPLEMENTATION_CLASS_UID
        || meta
            .implementation_version_name
            .as_deref()
            .map(clean_meta_value)
            != Some(DICOM_IMPLEMENTATION_VERSION_NAME)
        || meta.source_application_entity_title.is_some()
        || meta.sending_application_entity_title.is_some()
        || meta.receiving_application_entity_title.is_some()
        || meta.private_information_creator_uid.is_some()
        || meta.private_information.is_some()
    {
        bail!("sanitized DICOM File Meta Information failed the privacy audit");
    }
    let dataset_sop_class = object
        .element(Tag(0x0008, 0x0016))?
        .to_str()?
        .trim_matches([' ', '\0'])
        .to_owned();
    let dataset_sop_instance = object
        .element(Tag(0x0008, 0x0018))?
        .to_str()?
        .trim_matches([' ', '\0'])
        .to_owned();
    if dataset_sop_class != expected_sop_class_uid
        || dataset_sop_instance != expected_sop_instance_uid
    {
        bail!("sanitized DICOM File Meta and dataset identities do not match");
    }
    let meta_group_length = object.meta().information_group_length;
    let final_pixel = locate_pixel_data(path, expected_transfer_syntax, meta_group_length)?;
    let final_extended_offset_table =
        validate_extended_offset_table(&object, path, expected_transfer_syntax, final_pixel)
            .context("DICOM Extended Offset Table failed structural validation")?;
    if final_extended_offset_table.as_ref() != expected_extended_offset_table {
        bail!("sanitized DICOM changed the validated Extended Offset Table values");
    }
    audit_dataset(&object.into_inner(), subject_id, 0)?;
    let final_size = fs::metadata(path)?.len();
    if final_pixel.len != expected_source_pixel.len
        || final_pixel.start.checked_add(final_pixel.len) != Some(final_size)
    {
        bail!("sanitized DICOM PixelData boundary failed its final-byte audit");
    }
    if hash_span(path, final_pixel)? != expected_pixel_sha256 {
        bail!("sanitized DICOM PixelData does not match the immutable source snapshot");
    }
    Ok(())
}

fn clean_meta_value(value: &str) -> &str {
    value.trim_matches([' ', '\0'])
}

fn hash_span(path: &Path, span: FileSpan) -> Result<[u8; 32]> {
    let mut file = BufReader::with_capacity(1024 * 1024, File::open(path)?);
    file.seek(SeekFrom::Start(span.start))?;
    let mut remaining = span.len;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64))?;
        let read = file.read(&mut buffer[..wanted])?;
        if read == 0 {
            bail!("DICOM PixelData was truncated during final-byte audit");
        }
        digest.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(digest.finalize().into())
}

fn locate_pixel_data(
    path: &Path,
    transfer_syntax_uid: &str,
    meta_group_length: u32,
) -> Result<FileSpan> {
    let transfer_syntax = TransferSyntaxRegistry
        .get(transfer_syntax_uid)
        .context("DICOM transfer syntax is not supported for bounded pixel copying")?;
    let dataset_offset = 144_u64
        .checked_add(u64::from(meta_group_length))
        .context("DICOM file meta length overflow")?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(dataset_offset))?;
    let position = Arc::new(AtomicU64::new(dataset_offset));
    let tracking = TrackingReader {
        inner: file,
        position: Arc::clone(&position),
    };
    let mut reader = LazyDataSetReader::new_with_ts(tracking, transfer_syntax)?;
    let mut pixel_start = None;
    loop {
        let before = position.load(Ordering::Relaxed);
        let Some(token) = reader.advance() else {
            break;
        };
        match token? {
            LazyDataToken::ElementHeader(header) if header.tag == Tag(0x7fe0, 0x0010) => {
                pixel_start = Some(before);
            }
            LazyDataToken::PixelSequenceStart => {
                pixel_start = Some(before);
            }
            LazyDataToken::LazyValue { header, decoder } => {
                let length = header
                    .len
                    .get()
                    .context("primitive DICOM element has undefined length")?;
                decoder.skip_bytes(length)?;
                if header.tag == Tag(0x7fe0, 0x0010) {
                    let start =
                        pixel_start.context("PixelData header position was not captured")?;
                    let end = position.load(Ordering::Relaxed);
                    return Ok(FileSpan {
                        start,
                        len: end.checked_sub(start).context("invalid PixelData span")?,
                        value_len: Some(u64::from(length)),
                    });
                }
            }
            LazyDataToken::LazyItemValue { len, decoder } => {
                decoder.skip_bytes(len)?;
            }
            LazyDataToken::SequenceEnd if pixel_start.is_some() => {
                let start = pixel_start.unwrap();
                let end = position.load(Ordering::Relaxed);
                return Ok(FileSpan {
                    start,
                    len: end
                        .checked_sub(start)
                        .context("invalid encapsulated PixelData span")?,
                    value_len: None,
                });
            }
            _ => {}
        }
    }
    bail!("DICOM has no readable PixelData element")
}

fn validate_extended_offset_table(
    object: &dicom_object::DefaultDicomObject,
    path: &Path,
    transfer_syntax_uid: &str,
    pixel_span: FileSpan,
) -> Result<Option<ExtendedOffsetTable>> {
    let offsets = extended_offset_values(object, EXTENDED_OFFSET_TABLE)?;
    let lengths = extended_offset_values(object, EXTENDED_OFFSET_TABLE_LENGTHS)?;
    let (offsets, lengths) = match (offsets, lengths) {
        (None, None) => return Ok(None),
        (Some(offsets), Some(lengths)) => (offsets, lengths),
        _ => bail!("Extended Offset Table and Extended Offset Table Lengths must both be present"),
    };
    if offsets.is_empty() || offsets.len() != lengths.len() {
        bail!("Extended Offset Table values must be non-empty equal-length arrays");
    }
    if offsets[0] != 0 || offsets.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("Extended Offset Table offsets must start at zero and increase strictly");
    }
    if lengths.contains(&0) {
        bail!("Extended Offset Table frame lengths must be positive");
    }

    let declared_frames = match object.element(Tag(0x0028, 0x0008)) {
        Ok(element) => element
            .to_int::<u64>()
            .context("NumberOfFrames is not a positive integer")?,
        Err(_) => 1,
    };
    if declared_frames == 0 || declared_frames != offsets.len() as u64 {
        bail!("Extended Offset Table entry count does not match NumberOfFrames");
    }

    let transfer_syntax = TransferSyntaxRegistry
        .get(transfer_syntax_uid)
        .context("Extended Offset Table transfer syntax is not supported")?;
    if !transfer_syntax.is_encapsulated_pixel_data() {
        bail!("Extended Offset Table requires an encapsulated PixelData transfer syntax");
    }
    validate_encapsulated_fragments(path, pixel_span, &offsets, &lengths)?;
    Ok(Some(ExtendedOffsetTable { offsets, lengths }))
}

fn extended_offset_values(
    object: &dicom_object::DefaultDicomObject,
    tag: Tag,
) -> Result<Option<Vec<u64>>> {
    let element = match object.element(tag) {
        Ok(element) => element,
        Err(_) => return Ok(None),
    };
    if element.vr() != VR::OV {
        bail!("Extended Offset Table attributes must use the OV value representation");
    }
    let Value::Primitive(PrimitiveValue::U64(values)) = element.value() else {
        bail!("Extended Offset Table attributes must contain unsigned 64-bit values");
    };
    let encoded_len = values
        .len()
        .checked_mul(std::mem::size_of::<u64>())
        .and_then(|length| u32::try_from(length).ok())
        .context("Extended Offset Table value length overflow")?;
    if element.header().len.get() != Some(encoded_len) {
        bail!("Extended Offset Table encoded length is not a complete OV array");
    }
    Ok(Some(values.iter().copied().collect()))
}

fn validate_encapsulated_fragments(
    path: &Path,
    pixel_span: FileSpan,
    offsets: &[u64],
    lengths: &[u64],
) -> Result<()> {
    let pixel_end = pixel_span
        .start
        .checked_add(pixel_span.len)
        .context("encapsulated PixelData boundary overflow")?;
    let mut file = BufReader::new(File::open(path)?);
    file.seek(SeekFrom::Start(pixel_span.start))?;

    let mut pixel_header = [0_u8; 12];
    file.read_exact(&mut pixel_header)
        .context("encapsulated PixelData header is truncated")?;
    if u16::from_le_bytes([pixel_header[0], pixel_header[1]]) != PIXEL_DATA.group()
        || u16::from_le_bytes([pixel_header[2], pixel_header[3]]) != PIXEL_DATA.element()
        || &pixel_header[4..6] != b"OB"
        || pixel_header[6..8] != [0, 0]
        || u32::from_le_bytes([
            pixel_header[8],
            pixel_header[9],
            pixel_header[10],
            pixel_header[11],
        ]) != u32::MAX
    {
        bail!("Extended Offset Table requires undefined-length explicit-VR OB PixelData");
    }

    let (basic_offset_tag, basic_offset_len) = read_item_header(&mut file)?;
    if basic_offset_tag != Tag(0xfffe, 0xe000) || basic_offset_len != 0 {
        bail!("Extended Offset Table requires an empty Basic Offset Table Item");
    }
    let first_fragment_start = file.stream_position()?;
    let mut frame_index = 0_usize;
    loop {
        let item_start = file.stream_position()?;
        if item_start >= pixel_end {
            bail!("encapsulated PixelData is missing its Sequence Delimitation Item");
        }
        let (tag, item_len) = read_item_header(&mut file)?;
        if tag == Tag(0xfffe, 0xe0dd) {
            if item_len != 0 || file.stream_position()? != pixel_end || frame_index != offsets.len()
            {
                bail!("encapsulated PixelData has an invalid Sequence Delimitation Item");
            }
            break;
        }
        if tag != Tag(0xfffe, 0xe000) || item_len == u32::MAX {
            bail!("encapsulated PixelData contains an invalid frame fragment Item");
        }
        if item_len < 2 || item_len % 2 != 0 {
            bail!("encapsulated PixelData frame fragments must have positive even lengths");
        }
        if frame_index == offsets.len() {
            bail!("Extended Offset Table does not index every PixelData fragment");
        }
        let value_start = file.stream_position()?;
        let value_end = value_start
            .checked_add(u64::from(item_len))
            .context("encapsulated PixelData fragment length overflow")?;
        if value_end > pixel_end {
            bail!("encapsulated PixelData frame fragment is truncated");
        }
        let offset = item_start
            .checked_sub(first_fragment_start)
            .context("encapsulated PixelData fragment precedes its offset base")?;
        if offsets[frame_index] != offset {
            bail!("Extended Offset Table entry {frame_index} does not point to its frame Item Tag");
        }
        let padded_length = lengths[frame_index]
            .checked_add(lengths[frame_index] % 2)
            .context("Extended Offset Table frame length overflow")?;
        if padded_length != u64::from(item_len) {
            bail!(
                "Extended Offset Table Lengths entry {frame_index} does not match its frame fragment"
            );
        }
        frame_index += 1;
        file.seek(SeekFrom::Start(value_end))?;
    }
    Ok(())
}

fn read_item_header<R: Read>(reader: &mut R) -> Result<(Tag, u32)> {
    let mut bytes = [0_u8; 8];
    reader
        .read_exact(&mut bytes)
        .context("encapsulated PixelData Item header is truncated")?;
    Ok((
        Tag(
            u16::from_le_bytes([bytes[0], bytes[1]]),
            u16::from_le_bytes([bytes[2], bytes[3]]),
        ),
        u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    ))
}

fn append_verified_dicom<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &str,
    prepared: PreparedDicom,
) -> Result<String> {
    let source = File::open(&prepared.path)?;
    let mut reader = DigestReader {
        inner: source.take(prepared.size),
        digest: Sha256::new(),
    };
    let header = deterministic_tar_header(path, prepared.size)?;
    archive.append(&header, &mut reader)?;
    if reader.inner.limit() != 0 {
        bail!("verified DICOM was truncated while appending it to the archive");
    }
    Ok(hex::encode(reader.digest.finalize()))
}

fn dicom_archive_expansion_supported(rewritten_dicom_bytes: u64, archive_bytes: u64) -> bool {
    archive_bytes
        .max(DICOM_ARCHIVE_EXPANSION_FLOOR_BYTES)
        .checked_mul(MAX_DICOM_ARCHIVE_EXPANSION_RATIO)
        .is_some_and(|limit| rewritten_dicom_bytes <= limit)
}

#[allow(clippy::too_many_arguments)]
fn sanitize_dataset(
    source: InMemDicomObject,
    remapper: &mut UidRemapper<'_>,
    stats: &mut SanitizationStats,
    depth: usize,
    image_type_profile: MrImageTypeProfile,
    functional_group_context: FunctionalGroupContext,
    inherited_manufacturer: Option<&str>,
    suppress_redundant_philips_trigger: bool,
    retain_extended_offset_table: bool,
) -> Result<InMemDicomObject> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    let drop_philips_enhanced_root_lut_label = depth == 0
        && image_type_profile == MrImageTypeProfile::Enhanced
        && root_text(&source, Tag(0x0008, 0x0070), VR::LO).as_deref()
            == Some("Philips Medical Systems")
        && root_text(&source, LUT_LABEL, VR::SH).as_deref() == Some("Philips");
    let manufacturer = source
        .get(Tag(0x0008, 0x0070))
        .and_then(|element| element.to_str().ok())
        .and_then(|value| canonical_manufacturer(value.as_ref()))
        .or_else(|| inherited_manufacturer.map(str::to_owned));
    let private_creators = private_creators(&source);
    let mut retained_private_creators = HashSet::new();
    let mut output = InMemDicomObject::new_empty();
    for element in source {
        let tag = element.tag();
        let vr = element.vr();
        if tag == LUT_LABEL && drop_philips_enhanced_root_lut_label {
            // Some Philips Enhanced MR objects carry this vendor label at the
            // root without any Real World Value Mapping. Source validation
            // binds the exact value to the exact scanner/IOD context; do not
            // retain it where the sanitized archive contract is strict.
            continue;
        }
        if tag == MR_RECEIVE_COIL_SEQUENCE
            && image_type_profile == MrImageTypeProfile::Enhanced
            && functional_group_context != FunctionalGroupContext::None
        {
            if let Some((value, element_count)) = rebuild_multi_coil_receive_sequence(&element)? {
                // Rebuild the complete standard macro as one privacy/science
                // unit. Source coil and element labels are accepted only when
                // they are exact generic aliases; fixed canonical labels and
                // the per-element use flags are all that leave the workstation.
                reserve_sequence_items(stats, 1 + element_count)?;
                output.put(DataElement::new(tag, VR::SQ, value));
                continue;
            }
        }
        if tag == MR_TRANSMIT_COIL_SEQUENCE
            && image_type_profile == MrImageTypeProfile::Enhanced
            && functional_group_context != FunctionalGroupContext::None
        {
            if let Some(value) = rebuild_surface_transmit_coil_sequence(&element)? {
                reserve_sequence_items(stats, 1)?;
                output.put(DataElement::new(tag, VR::SQ, value));
                continue;
            }
        }
        if tag == REFERENCED_IMAGE_SEQUENCE && image_type_profile == MrImageTypeProfile::Enhanced {
            validate_referenced_image_sequence(&element, ReferenceValidationStage::Source)?;
            let items = element
                .value()
                .items()
                .context("DICOM Referenced Image Sequence is not a sequence")?;
            // Account for both the reference items and their one-item purpose
            // code sequences before replacing the source surface atomically.
            reserve_sequence_items(stats, items.len())?;
            reserve_sequence_items(stats, items.len())?;
            let value = rebuild_referenced_image_sequence(element.value(), remapper)?;
            output.put(DataElement::new(tag, VR::SQ, value));
            continue;
        }
        if tag == MR_METABOLITE_MAP_SEQUENCE
            && functional_group_context == FunctionalGroupContext::PerFrame
        {
            validate_metabolite_map_sequence(&element)?;
            reserve_sequence_items(stats, 1)?;
            output.put(rebuild_metabolite_map_sequence());
            continue;
        }
        if depth == 0
            && tag == Tag(0x0018, 0x0081)
            && image_type_profile != MrImageTypeProfile::Classic
            && matches!(element.value(), Value::Primitive(PrimitiveValue::Empty))
        {
            if vr != VR::DS {
                bail!("Enhanced MR contains an empty classic Echo Time with the wrong VR");
            }
            // Echo Time is a Classic MR Type 2 shell. Enhanced MR uses the
            // frame-aware Effective Echo Time in MR Echo Sequence; do not
            // carry a semantically empty classic duplicate into the archive.
            continue;
        }
        if tag == Tag(0x0008, 0x0008) && depth != 0 {
            continue;
        }
        if matches!(tag, EXTENDED_OFFSET_TABLE | EXTENDED_OFFSET_TABLE_LENGTHS) {
            // These attributes are meaningful only as a validated top-level
            // pair tied to the encapsulated PixelData that is copied verbatim.
            // Never allow a same-tag value nested in an arbitrary sequence.
            if depth == 0 && retain_extended_offset_table {
                output.put(element);
            }
            continue;
        }
        if tag == Tag(0x0018, 0x1060) && suppress_redundant_philips_trigger {
            stats.philips_redundant_trigger_times_suppressed += 1;
            continue;
        }
        if tag.group() % 2 == 1 {
            let creator_tag = Tag(tag.group(), tag.element() >> 8);
            if let Some((value, kind)) =
                canonical_ps315_safe_private_attribute(tag, vr, element.value(), &private_creators)
            {
                retained_private_creators.insert(creator_tag);
                output.put(DataElement::new(tag, vr, value));
                match kind {
                    SafePrivateKind::SiemensDiffusion => {
                        stats.siemens_ps315_diffusion_attributes_retained += 1;
                    }
                    SafePrivateKind::PhilipsDiffusion => {
                        stats.philips_ps315_diffusion_attributes_retained += 1;
                    }
                    SafePrivateKind::PhilipsPhase => {
                        stats.philips_ps315_phase_attributes_retained += 1;
                    }
                    SafePrivateKind::GeDiffusion => {
                        stats.ge_ps315_diffusion_attributes_retained += 1;
                    }
                    SafePrivateKind::UihGridSliceCount => {
                        stats.uih_grid_slice_count_attributes_retained += 1;
                    }
                    SafePrivateKind::UihDiffusion => {
                        stats.uih_diffusion_attributes_retained += 1;
                    }
                    SafePrivateKind::PhilipsDd001DiffusionVector => {
                        stats.philips_dd001_diffusion_vector_attributes_retained += 1;
                    }
                    SafePrivateKind::PhilipsDd005DiffusionIndex => {
                        stats.philips_dd005_diffusion_index_attributes_retained += 1;
                    }
                    SafePrivateKind::PhilipsDd005AslLabel => {
                        stats.philips_dd005_asl_label_attributes_retained += 1;
                    }
                    SafePrivateKind::GeAcquDiffusionVector => {
                        stats.ge_acqu_diffusion_vector_attributes_retained += 1;
                    }
                    SafePrivateKind::GeParmAsl => {
                        stats.ge_parm_asl_attributes_retained += 1;
                    }
                }
                continue;
            }
            let is_siemens_csa_image_header = tag == Tag(0x0029, 0x1010)
                && creators_match(&private_creators, creator_tag, "SIEMENS CSA HEADER");
            if is_siemens_csa_image_header && matches!(vr, VR::OB | VR::UN) {
                let sanitized = element
                    .to_bytes()
                    .ok()
                    .and_then(|bytes| sanitize_siemens_csa_image_header(bytes.as_ref()));
                if let Some(sanitized) = sanitized {
                    retained_private_creators.insert(creator_tag);
                    output.put(DataElement::new(
                        tag,
                        VR::OB,
                        PrimitiveValue::from(sanitized),
                    ));
                    stats.siemens_csa_headers_rewritten += 1;
                }
                continue;
            }
            let is_philips_number_of_slices = tag.group() == 0x2001
                && tag.element() & 0x00ff == 0x0018
                && creators_match(&private_creators, creator_tag, "Philips Imaging DD 001");
            if is_philips_number_of_slices {
                if vr != VR::SL || !positive_i32_vm1(element.value(), 1..=4096) {
                    // A malformed private candidate is not safe to retain, but
                    // it is also not a reason to reject otherwise valid public
                    // DICOM. Default-drop it like every unknown private field.
                    continue;
                }
                retained_private_creators.insert(creator_tag);
                output.put(element);
                stats.philips_ps315_number_of_slices_retained += 1;
                continue;
            }
            let is_philips_water_fat_shift = tag.group() == 0x2001
                && tag.element() & 0x00ff == 0x0022
                && creators_match(&private_creators, creator_tag, "Philips Imaging DD 001");
            if is_philips_water_fat_shift {
                if vr != VR::FL
                    || !bounded_float32_vm1(element.value(), |v| (0.0..=1.0e6).contains(&v))
                {
                    continue;
                }
                retained_private_creators.insert(creator_tag);
                output.put(element);
                stats.philips_ps315_water_fat_shift_retained += 1;
                continue;
            }
            let is_philips_per_frame_scale = tag.group() == 0x2005
                && tag.element() & 0x00ff == 0x000f
                && creators_match(&private_creators, creator_tag, "Philips MR Imaging DD 005")
                && vr == VR::SQ;
            let is_philips_shared_duplicate = functional_group_context
                == FunctionalGroupContext::Shared
                && tag == Tag(0x2005, 0x140e)
                && creators_match(&private_creators, creator_tag, "Philips MR Imaging DD 005")
                && vr == VR::SQ;
            if is_philips_shared_duplicate {
                let Some(items) = element.value().items() else {
                    continue;
                };
                reserve_sequence_items(stats, items.len())?;
                continue;
            }
            if functional_group_context == FunctionalGroupContext::PerFrame
                && is_philips_per_frame_scale
            {
                let Some(items) = element.value().items() else {
                    continue;
                };
                reserve_sequence_items(stats, items.len())?;
                continue;
            }
            if is_philips_per_frame_scale {
                let Some(items) = element.value().items() else {
                    continue;
                };
                reserve_sequence_items(stats, items.len())?;
                match rebuild_philips_per_frame_scale_sequence(element.value()) {
                    PhilipsPerFrameScaleSequence::NotScaleMetadata => {}
                    PhilipsPerFrameScaleSequence::Rebuilt(value) => {
                        retained_private_creators.insert(creator_tag);
                        output.put(DataElement::new(tag, VR::SQ, value));
                        stats.philips_ps315_per_frame_scale_sequences_rebuilt += 1;
                    }
                    PhilipsPerFrameScaleSequence::Malformed => {
                        continue;
                    }
                }
                continue;
            }
            let is_philips_ps315_scaling = tag.group() == 0x2005
                && matches!(tag.element() & 0x00ff, 0x000d | 0x000e)
                && creators_match(&private_creators, creator_tag, "Philips MR Imaging DD 001");
            if is_philips_ps315_scaling {
                let valid = match tag.element() & 0x00ff {
                    0x000d => bounded_float32_vm1(element.value(), |v| v.abs() <= 1.0e9),
                    0x000e => bounded_float32_vm1(element.value(), |v| v > 0.0 && v <= 1.0e9),
                    _ => false,
                };
                if vr != VR::FL || !valid {
                    continue;
                }
                retained_private_creators.insert(creator_tag);
                output.put(element);
                stats.philips_ps315_scaling_attributes_retained += 1;
                continue;
            }
            // A known private creator plus a numeric VR is not a semantic
            // privacy guarantee: numeric private fields can still encode
            // dates, identifiers, and site-specific values. Default-drop all
            // private values except the rebuilt Siemens CSA exception above.
            continue;
        }
        if vr == VR::DT
            && matches!(tag, Tag(0x0018, 0x9074) | Tag(0x0018, 0x9151))
            && image_type_profile != MrImageTypeProfile::Classic
        {
            output.put_str(tag, VR::DT, ENHANCED_FRAME_DATETIME_SENTINEL);
            continue;
        }
        if is_date_or_time_vr(vr) || !public_attribute_allowed(tag, vr) {
            continue;
        }
        let (header, value) = element.into_parts();
        let child_functional_group_context = if depth == 0 {
            match tag {
                SHARED_FUNCTIONAL_GROUPS_SEQUENCE => FunctionalGroupContext::Shared,
                PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE => FunctionalGroupContext::PerFrame,
                _ => FunctionalGroupContext::None,
            }
        } else {
            FunctionalGroupContext::None
        };
        let value = match value {
            Value::Sequence(sequence) => {
                let items = sequence.into_items();
                reserve_sequence_items(stats, items.len())?;
                let items = items
                    .into_iter()
                    .map(|item| {
                        sanitize_dataset(
                            item,
                            remapper,
                            stats,
                            depth + 1,
                            image_type_profile,
                            child_functional_group_context,
                            manufacturer.as_deref(),
                            suppress_redundant_philips_trigger,
                            false,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                Value::Sequence(DataSetSequence::new(items, Length::UNDEFINED))
            }
            Value::Primitive(value) if tag == Tag(0x0018, 0x1000) && vr == VR::LO => {
                let Some(value) = remapper.map_device_serial(value.to_str().as_ref()) else {
                    continue;
                };
                Value::Primitive(PrimitiveValue::from(value))
            }
            Value::Primitive(value) if vr == VR::UI && !semantic_uid_constant(tag) => {
                let mapped = value
                    .to_str()
                    .split('\\')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| remapper.map(value))
                    .collect::<Result<Vec<_>>>()?;
                Value::Primitive(PrimitiveValue::from(mapped.join("\\")))
            }
            Value::Primitive(value) if vr == VR::UI => {
                let value = canonical_semantic_uid(tag, value.to_str().as_ref(), depth)
                    .context("DICOM contained an unsupported semantic UID constant")?;
                Value::Primitive(PrimitiveValue::from(value))
            }
            Value::Primitive(value) if tag == Tag(0x0020, 0x9056) && vr == VR::SH => {
                let Some(value) = remapper.map_stack_id(value.to_str().as_ref()) else {
                    continue;
                };
                Value::Primitive(PrimitiveValue::from(value))
            }
            Value::Primitive(value) if tag == Tag(0x0008, 0x0008) && vr == VR::CS => {
                let source_value = value.to_str();
                let value = canonical_image_type(source_value.as_ref(), image_type_profile)
                    .context("DICOM ImageType failed positional validation")?;
                stats.classic_image_type_components_replaced_with_other +=
                    classic_image_type_replacement_count(source_value.as_ref(), &value);
                Value::Primitive(PrimitiveValue::from(value))
            }
            Value::Primitive(_) if tag == Tag(0x0018, 0x9252) && vr == VR::LO => {
                // ASL Technique Description is Type 2 inside the ASL Context
                // macro. Preserve the required attribute while erasing its
                // unconstrained, potentially identifying free text.
                stats.asl_technique_descriptions_emptied += 1;
                Value::Primitive(PrimitiveValue::Empty)
            }
            Value::Primitive(_) if tag == Tag(0x0018, 0x925b) && vr == VR::LO => {
                // This is Type 1 when ASL Crusher Flag is YES. Replace the
                // unconstrained scanner description with a fixed, non-source
                // sentinel so the conditional macro remains conformant.
                stats.asl_crusher_descriptions_redacted += 1;
                Value::Primitive(PrimitiveValue::from("REDACTED"))
            }
            Value::Primitive(_) if tag == Tag(0x0018, 0x925e) && vr == VR::LO => {
                // Bolus Cut-off Technique is Type 2 within the required item.
                // Retain the shell while erasing potentially identifying text.
                stats.asl_bolus_cutoff_techniques_emptied += 1;
                Value::Primitive(PrimitiveValue::Empty)
            }
            Value::Primitive(_)
                if matches!(tag, Tag(0x0018, 0x9041) | Tag(0x0018, 0x9050)) && vr == VR::LO =>
            {
                // Coil Manufacturer Name is Type 2 inside the Enhanced MR
                // coil macros. Preserve the shell while removing arbitrary
                // scanner-entered text.
                Value::Primitive(PrimitiveValue::Empty)
            }
            Value::Primitive(_) if tag == Tag(0x0008, 0x0104) && vr == VR::LO => {
                // Code Value + Coding Scheme Designator carry the machine
                // semantics. Code Meaning is display text and is replaced by
                // a fixed safe label rather than copied from the scanner.
                Value::Primitive(PrimitiveValue::from("ANATOMY"))
            }
            Value::Primitive(value) => {
                let Some(value) = sanitize_public_primitive(
                    tag,
                    vr,
                    value,
                    manufacturer.as_deref(),
                    image_type_profile,
                ) else {
                    continue;
                };
                Value::Primitive(value)
            }
            _ => continue,
        };
        output.put(DataElement::new(header.tag, header.vr, value));
    }
    for creator_tag in retained_private_creators {
        let creator = private_creators
            .get(&creator_tag)
            .context("retained private value lost its creator")?;
        let creator = canonical_private_creator(creator)
            .context("retained private value has no canonical creator")?;
        output.put_str(creator_tag, VR::LO, creator);
    }
    Ok(output)
}

fn private_creators(source: &InMemDicomObject) -> BTreeMap<Tag, String> {
    source
        .iter()
        .filter_map(|element| {
            let tag = element.tag();
            (tag.group() % 2 == 1 && (0x0010..=0x00ff).contains(&tag.element()))
                .then(|| {
                    element
                        .to_str()
                        .ok()
                        .map(|value| (tag, value.trim_matches([' ', '\0']).to_owned()))
                })
                .flatten()
        })
        .collect()
}

fn creators_match(creators: &BTreeMap<Tag, String>, tag: Tag, expected: &str) -> bool {
    creators
        .get(&tag)
        .is_some_and(|creator| creator.eq_ignore_ascii_case(expected))
}

pub(crate) fn sanitize_siemens_csa_image_header(source: &[u8]) -> Option<Vec<u8>> {
    let retained = parse_siemens_csa_numeric_fields(source)?;
    let retained_fields = SIEMENS_CSA_NUMERIC_FIELDS
        .iter()
        .filter_map(|(name, vr)| retained.get(*name).map(|values| (*name, *vr, values)))
        .collect::<Vec<_>>();
    if retained_fields
        .iter()
        .all(|(name, _, _)| *name != "NumberOfImagesInMosaic")
    {
        return None;
    }
    let mut output = Vec::new();
    output.extend_from_slice(b"SV10");
    output.extend_from_slice(&[4, 3, 2, 1]);
    output.extend_from_slice(&(retained_fields.len() as u32).to_le_bytes());
    output.extend_from_slice(&77_u32.to_le_bytes());
    for (name, vr, values) in retained_fields {
        let value_count = values.len();
        let serialized_item_count = if matches!(
            name,
            "SliceMeasurementDuration" | "BandwidthPerPixelPhaseEncode"
        ) {
            3
        } else {
            values.len()
        };
        let mut name_bytes = [0_u8; 64];
        name_bytes[..name.len()].copy_from_slice(name.as_bytes());
        output.extend_from_slice(&name_bytes);
        output.extend_from_slice(&(value_count as i32).to_le_bytes());
        output.extend_from_slice(&vr);
        output.extend_from_slice(&0_i32.to_le_bytes());
        output.extend_from_slice(&(serialized_item_count as i32).to_le_bytes());
        output.extend_from_slice(&77_i32.to_le_bytes());
        for value in values {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            let length = i32::try_from(bytes.len()).ok()?;
            output.extend_from_slice(&length.to_le_bytes());
            output.extend_from_slice(&length.to_le_bytes());
            output.extend_from_slice(&77_i32.to_le_bytes());
            output.extend_from_slice(&0_i32.to_le_bytes());
            output.extend_from_slice(&bytes);
            output.resize(output.len().checked_add((4 - bytes.len() % 4) % 4)?, 0);
        }
        for _ in 0..serialized_item_count.saturating_sub(value_count) {
            output.extend_from_slice(&0_i32.to_le_bytes());
            output.extend_from_slice(&0_i32.to_le_bytes());
            output.extend_from_slice(&77_i32.to_le_bytes());
            output.extend_from_slice(&0_i32.to_le_bytes());
        }
    }
    (output.len() <= MAX_SIEMENS_CSA_BYTES).then_some(output)
}

const MAX_SIEMENS_CSA_BYTES: usize = 16 * 1024 * 1024;
const MAX_SIEMENS_CSA_ITEMS: usize = 4096;
const SIEMENS_CSA_NUMERIC_FIELDS: &[(&str, [u8; 4])] = &[
    ("NumberOfImagesInMosaic", [b'U', b'S', 0, 0]),
    ("SliceNormalVector", [b'D', b'S', 0, 0]),
    ("SliceMeasurementDuration", [b'D', b'S', 0, 0]),
    ("BandwidthPerPixelPhaseEncode", [b'D', b'S', 0, 0]),
    ("MosaicRefAcqTimes", [b'D', b'S', 0, 0]),
    ("ProtocolSliceNumber", [b'I', b'S', 0, 0]),
    ("PhaseEncodingDirectionPositive", [b'I', b'S', 0, 0]),
    ("B_value", [b'D', b'S', 0, 0]),
    ("DiffusionGradientDirection", [b'D', b'S', 0, 0]),
    ("B_matrix", [b'D', b'S', 0, 0]),
];
const SIEMENS_CSA_DIFFUSION_FIELDS: [&str; 3] =
    ["B_value", "DiffusionGradientDirection", "B_matrix"];

fn parse_siemens_csa_numeric_fields(source: &[u8]) -> Option<BTreeMap<String, Vec<String>>> {
    if !(36..=MAX_SIEMENS_CSA_BYTES).contains(&source.len())
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
    let mut retained = BTreeMap::<String, Vec<String>>::new();
    for _ in 0..tag_count {
        let header_end = cursor.checked_add(84)?;
        let header = source.get(cursor..header_end)?;
        cursor = header_end;
        let name_end = header[..64].iter().position(|byte| *byte == 0)?;
        let name = std::str::from_utf8(&header[..name_end]).ok()?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return None;
        }
        let item_count =
            usize::try_from(u32::from_le_bytes(header[76..80].try_into().ok()?)).ok()?;
        let declared_vm = i32::from_le_bytes(header[64..68].try_into().ok()?);
        if !(0..=4096).contains(&declared_vm) {
            return None;
        }
        if item_count > MAX_SIEMENS_CSA_ITEMS {
            return None;
        }
        let keep = SIEMENS_CSA_NUMERIC_FIELDS
            .iter()
            .any(|(allowed, _)| name == *allowed);
        let mut values = Vec::with_capacity(item_count.min(64));
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
            if cursor > source.len() {
                return None;
            }
            if keep {
                let value = std::str::from_utf8(bytes).ok()?.trim_matches([' ', '\0']);
                // Siemens CSA reserves fixed item capacity and commonly pads
                // real E11 mosaic fields with zero-length trailing items.
                if value.is_empty() {
                    continue;
                }
                if value.bytes().any(|byte| {
                    !byte.is_ascii_digit() && !matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E')
                }) {
                    return None;
                }
                let number = value.parse::<f64>().ok()?;
                if !number.is_finite() {
                    return None;
                }
                values.push(value.to_owned());
            }
        }
        // Siemens CSA commonly declares the nominal VM for optional fields but
        // serializes zero Items when the field is not applicable. In that form
        // the field is absent, not a malformed zero-length value. This is used
        // by real E11 mosaics for the optional diffusion fields.
        if keep && !values.is_empty() && declared_vm > 0 && values.len() != declared_vm as usize {
            return None;
        }
        if keep && !values.is_empty() && retained.insert(name.to_owned(), values).is_some() {
            return None;
        }
    }
    if cursor > source.len() {
        return None;
    }
    validate_csa_values(&mut retained)?;
    Some(retained)
}

pub(crate) fn siemens_csa_diffusion_contract(source: &[u8]) -> (bool, bool, bool) {
    let present = SIEMENS_CSA_DIFFUSION_FIELDS.iter().any(|name| {
        source
            .windows(name.len())
            .any(|candidate| candidate == name.as_bytes())
    });
    if !present {
        return (false, false, false);
    }
    let Some(fields) = parse_siemens_csa_numeric_fields(source) else {
        return (true, false, false);
    };
    let number = |name: &str| {
        fields
            .get(name)
            .filter(|values| values.len() == 1)
            .and_then(|values| values[0].parse::<f64>().ok())
    };
    let b_value = number("B_value");
    let gradient = fields.contains_key("DiffusionGradientDirection");
    let b_matrix = fields.contains_key("B_matrix");
    let valid = b_value.is_some_and(|value| {
        if value <= 1.0 {
            !gradient && !b_matrix
        } else {
            gradient ^ b_matrix
        }
    });
    let semantic = b_value.is_some_and(|value| value > 1.0) || gradient || b_matrix;
    (true, valid, semantic)
}

fn read_csa_u32(source: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        source
            .get(offset..offset.checked_add(4)?)?
            .try_into()
            .ok()?,
    ))
}

fn validate_csa_values(values: &mut BTreeMap<String, Vec<String>>) -> Option<()> {
    for (name, items) in values.iter_mut() {
        let numbers = items
            .iter()
            .map(|value| value.parse::<f64>().ok())
            .collect::<Option<Vec<_>>>()?;
        let valid = match name.as_str() {
            "NumberOfImagesInMosaic" => {
                numbers.len() == 1
                    && numbers[0].fract() == 0.0
                    && (2.0..=4096.0).contains(&numbers[0])
            }
            "SliceNormalVector" => {
                numbers.len() == 3 && numbers.iter().all(|value| (-1.1..=1.1).contains(value))
            }
            "SliceMeasurementDuration" => {
                (1..=3).contains(&numbers.len())
                    && numbers.iter().all(|value| (0.0..=1.0e12).contains(value))
            }
            "BandwidthPerPixelPhaseEncode" => {
                (1..=3).contains(&numbers.len())
                    && numbers.iter().all(|value| (0.0..=1.0e12).contains(value))
            }
            "MosaicRefAcqTimes" => {
                (4..=4096).contains(&numbers.len())
                    && numbers.iter().all(|value| (-1.0e9..=1.0e9).contains(value))
            }
            "ProtocolSliceNumber" => {
                numbers.len() == 1
                    && numbers[0].fract() == 0.0
                    && (0.0..=4096.0).contains(&numbers[0])
            }
            "PhaseEncodingDirectionPositive" => {
                numbers.len() == 1 && matches!(numbers[0], 0.0 | 1.0)
            }
            "B_value" => numbers.len() == 1 && (0.0..=1.0e6).contains(&numbers[0]),
            "DiffusionGradientDirection" => {
                numbers.len() == 3 && numbers.iter().all(|value| (-1.1..=1.1).contains(value))
            }
            "B_matrix" => {
                numbers.len() == 6 && numbers.iter().all(|value| (-1.0e9..=1.0e9).contains(value))
            }
            _ => false,
        };
        if !valid {
            return None;
        }
        *items = numbers
            .into_iter()
            .map(|number| number.to_string())
            .collect();
    }
    Some(())
}

fn safe_private_creator(value: &str) -> bool {
    canonical_private_creator(value).is_some()
}

fn canonical_private_creator(value: &str) -> Option<&'static str> {
    let value = value.trim_matches([' ', '\0']);
    [
        "SIEMENS CSA HEADER",
        "SIEMENS MR HEADER",
        "Image Private Header",
        "GEMS_ACQU_01",
        "GEMS_PARM_01",
        "Philips MR Imaging DD 001",
        "Philips MR Imaging DD 005",
        "Philips Imaging DD 001",
    ]
    .into_iter()
    .find(|known| value.eq_ignore_ascii_case(known))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SafePrivateKind {
    SiemensDiffusion,
    PhilipsDiffusion,
    PhilipsPhase,
    GeDiffusion,
    UihGridSliceCount,
    UihDiffusion,
    PhilipsDd001DiffusionVector,
    PhilipsDd005DiffusionIndex,
    PhilipsDd005AslLabel,
    GeAcquDiffusionVector,
    GeParmAsl,
}

fn canonical_ps315_safe_private_attribute(
    tag: Tag,
    vr: VR,
    value: &Value<InMemDicomObject, Vec<u8>>,
    creators: &BTreeMap<Tag, String>,
) -> Option<(PrimitiveValue, SafePrivateKind)> {
    let creator_tag = Tag(tag.group(), tag.element() >> 8);
    let low = tag.element() & 0x00ff;
    if tag.group() == 0x0019 && creators_match(creators, creator_tag, "SIEMENS MR HEADER") {
        let value = match (low, vr) {
            (0x000c, VR::IS) => {
                canonical_private_integers(value, 1, |values| (0..=1_000_000).contains(&values[0]))
            }
            (0x000d, VR::CS) => canonical_private_code(
                value,
                Some(&["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"]),
            ),
            (0x000e, VR::FD) => canonical_private_f64(value, 3, |values| {
                values.iter().all(|number| (-1.1..=1.1).contains(number))
            }),
            (0x0027, VR::FD) => canonical_private_f64(value, 6, |values| {
                values
                    .iter()
                    .all(|number| (-1.0e9..=1.0e9).contains(number))
            }),
            _ => None,
        }?;
        return Some((value, SafePrivateKind::SiemensDiffusion));
    }
    if tag.group() == 0x0065 && creators_match(creators, creator_tag, "Image Private Header") {
        return match (low, vr) {
            (0x0050, VR::DS) => canonical_private_decimal(value, 1, |values| {
                values[0].fract() == 0.0 && (1.0..=4096.0).contains(&values[0])
            })
            .map(|value| (value, SafePrivateKind::UihGridSliceCount)),
            (0x0009, VR::FD) => {
                canonical_private_f64(value, 1, |values| (0.0..=1.0e6).contains(&values[0]))
                    .map(|value| (value, SafePrivateKind::UihDiffusion))
            }
            (0x0037, VR::FD) => canonical_private_f64(value, 3, |values| {
                values.iter().all(|number| (-1.1..=1.1).contains(number))
            })
            .map(|value| (value, SafePrivateKind::UihDiffusion)),
            _ => None,
        };
    }
    if tag.group() == 0x2001 && creators_match(creators, creator_tag, "Philips Imaging DD 001") {
        return match (low, vr) {
            (0x0003, VR::FL) => {
                canonical_private_f32(value, 1, |values| (0.0..=1_000_000.0).contains(&values[0]))
                    .map(|value| (value, SafePrivateKind::PhilipsDiffusion))
            }
            (0x0004, VR::CS) => canonical_private_code(
                value,
                Some(&["AP", "FH", "RL", "NONE", "ISOTROPIC", "DIRECTIONAL"]),
            )
            .map(|value| (value, SafePrivateKind::PhilipsDiffusion)),
            (0x0008, VR::IS) => {
                canonical_private_integers(value, 1, |values| (0..=1_000_000).contains(&values[0]))
                    .map(|value| (value, SafePrivateKind::PhilipsPhase))
            }
            _ => None,
        };
    }
    if tag.group() == 0x2005 && creators_match(creators, creator_tag, "Philips MR Imaging DD 001") {
        return match (low, vr) {
            (0x00b0..=0x00b2, VR::FL) => {
                canonical_private_f32(value, 1, |values| (-1.1..=1.1).contains(&values[0]))
                    .map(|value| (value, SafePrivateKind::PhilipsDd001DiffusionVector))
            }
            _ => None,
        };
    }
    if tag.group() == 0x2005 && creators_match(creators, creator_tag, "Philips MR Imaging DD 005") {
        return match (low, vr) {
            (0x0012 | 0x0013, VR::IS) => {
                canonical_private_integers(value, 1, |values| (0..=1_000_000).contains(&values[0]))
                    .map(|value| (value, SafePrivateKind::PhilipsDd005DiffusionIndex))
            }
            (0x0029, VR::CS) => canonical_philips_asl_label(value)
                .map(|value| (value, SafePrivateKind::PhilipsDd005AslLabel)),
            _ => None,
        };
    }
    if tag.group() == 0x0019 && creators_match(creators, creator_tag, "GEMS_ACQU_01") {
        return match (low, vr) {
            (0x00bb..=0x00bd, VR::DS) => {
                canonical_private_decimal(value, 1, |values| (-1.1..=1.1).contains(&values[0]))
                    .map(|value| (value, SafePrivateKind::GeAcquDiffusionVector))
            }
            _ => None,
        };
    }
    if tag.group() == 0x0043 && creators_match(creators, creator_tag, "GEMS_PARM_01") {
        return match (low, vr) {
            (0x0039, VR::IS) => canonical_private_integers(value, 4, |values| {
                (0..=1_000_000).contains(&values[0])
                    && values[1..]
                        .iter()
                        .all(|number| (-1_000_000_000..=1_000_000_000).contains(number))
            })
            .map(|value| (value, SafePrivateKind::GeDiffusion)),
            (0x00a3, VR::CS) => {
                canonical_private_code(value, Some(&["CONTINUOUS", "PSEUDOCONTINUOUS", "PULSED"]))
                    .map(|value| (value, SafePrivateKind::GeParmAsl))
            }
            (0x00a5, VR::IS) => canonical_private_integers(value, 1, |values| {
                (0..=100_000_000).contains(&values[0])
            })
            .map(|value| (value, SafePrivateKind::GeParmAsl)),
            _ => None,
        };
    }
    None
}

fn canonical_private_integers(
    value: &Value<InMemDicomObject, Vec<u8>>,
    vm: usize,
    valid: impl Fn(&[i64]) -> bool,
) -> Option<PrimitiveValue> {
    let Value::Primitive(value) = value else {
        return None;
    };
    let values = value
        .to_str()
        .split('\\')
        .map(|value| value.trim_matches([' ', '\0']).parse::<i64>().ok())
        .collect::<Option<Vec<_>>>()?;
    (values.len() == vm && valid(&values)).then(|| {
        PrimitiveValue::from(
            values
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join("\\"),
        )
    })
}

fn canonical_private_decimal(
    value: &Value<InMemDicomObject, Vec<u8>>,
    vm: usize,
    valid: impl Fn(&[f64]) -> bool,
) -> Option<PrimitiveValue> {
    let Value::Primitive(value) = value else {
        return None;
    };
    let values = value
        .to_str()
        .split('\\')
        .map(|value| {
            let value = value.trim_matches([' ', '\0']);
            (!value.is_empty() && value.len() <= 16)
                .then(|| value.parse::<f64>().ok())
                .flatten()
        })
        .collect::<Option<Vec<_>>>()?;
    (values.len() == vm && values.iter().all(|number| number.is_finite()) && valid(&values)).then(
        || {
            PrimitiveValue::from(
                values
                    .iter()
                    .map(f64::to_string)
                    .collect::<Vec<_>>()
                    .join("\\"),
            )
        },
    )
}

fn canonical_private_code(
    value: &Value<InMemDicomObject, Vec<u8>>,
    allowed: Option<&[&str]>,
) -> Option<PrimitiveValue> {
    let Value::Primitive(value) = value else {
        return None;
    };
    let value = value.to_str();
    let value = value.trim_matches([' ', '\0']).to_ascii_uppercase();
    let grammar = !value.is_empty()
        && value.len() <= 16
        && !value.contains('\\')
        && value.bytes().all(|byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b' ')
        });
    (grammar && allowed.is_none_or(|allowed| allowed.contains(&value.as_str())))
        .then(|| PrimitiveValue::from(value))
}

fn canonical_philips_asl_label(value: &Value<InMemDicomObject, Vec<u8>>) -> Option<PrimitiveValue> {
    let Value::Primitive(value) = value else {
        return None;
    };
    let value = value
        .to_str()
        .trim_matches([' ', '\0'])
        .to_ascii_uppercase();
    let canonical = match value.as_str() {
        "LABEL" | "LBL" => "LABEL",
        "CONTROL" | "CTL" => "CONTROL",
        "M_ZERO_SCAN" => "M_ZERO_SCAN",
        _ => return None,
    };
    Some(PrimitiveValue::from(canonical))
}

fn canonical_private_f32(
    value: &Value<InMemDicomObject, Vec<u8>>,
    vm: usize,
    valid: impl Fn(&[f32]) -> bool,
) -> Option<PrimitiveValue> {
    let Value::Primitive(PrimitiveValue::F32(values)) = value else {
        return None;
    };
    (values.len() == vm && values.iter().all(|number| number.is_finite()) && valid(values))
        .then(|| PrimitiveValue::F32(values.clone()))
}

fn canonical_private_f64(
    value: &Value<InMemDicomObject, Vec<u8>>,
    vm: usize,
    valid: impl Fn(&[f64]) -> bool,
) -> Option<PrimitiveValue> {
    let Value::Primitive(PrimitiveValue::F64(values)) = value else {
        return None;
    };
    (values.len() == vm && values.iter().all(|number| number.is_finite()) && valid(values))
        .then(|| PrimitiveValue::F64(values.clone()))
}

fn bounded_float32_vm1(
    value: &Value<InMemDicomObject, Vec<u8>>,
    valid: impl Fn(f32) -> bool,
) -> bool {
    matches!(value, Value::Primitive(PrimitiveValue::F32(values)) if values.len() == 1 && values[0].is_finite() && valid(values[0]))
}

fn reserve_sequence_items(stats: &mut SanitizationStats, count: usize) -> Result<()> {
    stats.current_sequence_items = stats
        .current_sequence_items
        .checked_add(count)
        .context("DICOM sequence-item count overflow")?;
    if stats.current_sequence_items > MAX_SEQUENCE_ITEMS {
        bail!("DICOM contains more than 100000 aggregate sequence items");
    }
    Ok(())
}

fn positive_i32_vm1(
    value: &Value<InMemDicomObject, Vec<u8>>,
    range: std::ops::RangeInclusive<i32>,
) -> bool {
    matches!(value, Value::Primitive(PrimitiveValue::I32(values)) if values.len() == 1 && range.contains(&values[0]))
}

enum PhilipsPerFrameScaleSequence {
    NotScaleMetadata,
    Rebuilt(Value<InMemDicomObject, Vec<u8>>),
    Malformed,
}

fn rebuild_philips_per_frame_scale_sequence(
    value: &Value<InMemDicomObject, Vec<u8>>,
) -> PhilipsPerFrameScaleSequence {
    let Some(items) = value.items() else {
        return PhilipsPerFrameScaleSequence::Malformed;
    };
    let has_scale_candidate = items
        .iter()
        .flat_map(InMemDicomObject::iter)
        .any(|element| {
            let tag = element.tag();
            tag.group() == 0x2005 && tag.element() >= 0x1000 && tag.element() & 0x00ff == 0x000e
        });
    if !has_scale_candidate {
        return PhilipsPerFrameScaleSequence::NotScaleMetadata;
    }
    if items.is_empty() || items.len() > MAX_SEQUENCE_ITEMS {
        return PhilipsPerFrameScaleSequence::Malformed;
    }
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in items {
        let creators = private_creators(item);
        let scales = item
            .iter()
            .filter(|element| {
                let tag = element.tag();
                let creator_tag = Tag(tag.group(), tag.element() >> 8);
                tag.group() == 0x2005
                    && tag.element() & 0x00ff == 0x000e
                    && creators_match(&creators, creator_tag, "Philips MR Imaging DD 001")
                    && element.vr() == VR::FL
                    && bounded_float32_vm1(element.value(), |v| v > 0.0 && v <= 1.0e9)
            })
            .cloned()
            .collect::<Vec<_>>();
        if scales.len() != 1 {
            return PhilipsPerFrameScaleSequence::Malformed;
        }
        let Some(scale) = scales.into_iter().next() else {
            return PhilipsPerFrameScaleSequence::Malformed;
        };
        let creator_tag = Tag(scale.tag().group(), scale.tag().element() >> 8);
        let mut output = InMemDicomObject::new_empty();
        output.put_str(creator_tag, VR::LO, "Philips MR Imaging DD 001");
        output.put(scale);
        rebuilt.push(output);
    }
    PhilipsPerFrameScaleSequence::Rebuilt(Value::Sequence(DataSetSequence::new(
        rebuilt,
        Length::UNDEFINED,
    )))
}

fn canonical_philips_per_frame_scale_sequence(value: &Value<InMemDicomObject, Vec<u8>>) -> bool {
    value.items().is_some_and(|items| {
        !items.is_empty()
            && items.len() <= MAX_SEQUENCE_ITEMS
            && items.iter().all(|item| {
                let creators = private_creators(item);
                let mut creators_seen = 0;
                let mut scales_seen = 0;
                for element in item.iter() {
                    let tag = element.tag();
                    if tag.group() == 0x2005 && (0x0010..=0x00ff).contains(&tag.element()) {
                        if !creators_match(&creators, tag, "Philips MR Imaging DD 001") {
                            return false;
                        }
                        creators_seen += 1;
                    } else {
                        let creator_tag = Tag(tag.group(), tag.element() >> 8);
                        if tag.group() != 0x2005
                            || tag.element() & 0x00ff != 0x000e
                            || !creators_match(&creators, creator_tag, "Philips MR Imaging DD 001")
                            || element.vr() != VR::FL
                            || !bounded_float32_vm1(element.value(), |v| v > 0.0 && v <= 1.0e9)
                        {
                            return false;
                        }
                        scales_seen += 1;
                    }
                }
                creators_seen == 1 && scales_seen == 1
            })
    })
}

fn sanitize_public_primitive(
    tag: Tag,
    vr: VR,
    value: PrimitiveValue,
    manufacturer: Option<&str>,
    image_type_profile: MrImageTypeProfile,
) -> Option<PrimitiveValue> {
    if tag == Tag(0x0018, 0x9087) {
        return match value {
            PrimitiveValue::F64(values)
                if values.len() == 1
                    && values[0].is_finite()
                    && (0.0..=1.0e6).contains(&values[0]) =>
            {
                Some(PrimitiveValue::F64(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0018, 0x9089) {
        return match value {
            PrimitiveValue::F64(values)
                if values.len() == 3
                    && values.iter().all(|number| (-1.1..=1.1).contains(number)) =>
            {
                Some(PrimitiveValue::F64(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0018, 0x9253) {
        return match value {
            PrimitiveValue::U16(values) if values.len() == 1 && (1..=4096).contains(&values[0]) => {
                Some(PrimitiveValue::U16(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0018, 0x9254) {
        return match value {
            PrimitiveValue::F64(values)
                if values.len() == 1
                    && values[0].is_finite()
                    && values[0] > 0.0
                    && values[0] <= 1.0e6 =>
            {
                Some(PrimitiveValue::F64(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0018, 0x9255) {
        return match value {
            PrimitiveValue::F64(values)
                if values.len() == 3
                    && values.iter().all(|number| (-1.1..=1.1).contains(number))
                    && (0.5..=1.5)
                        .contains(&values.iter().map(|number| number * number).sum::<f64>()) =>
            {
                Some(PrimitiveValue::F64(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0018, 0x9256) {
        return match value {
            PrimitiveValue::F64(values)
                if values.len() == 3
                    && values
                        .iter()
                        .all(|number| number.is_finite() && number.abs() <= 1.0e6) =>
            {
                Some(PrimitiveValue::F64(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0018, 0x9258) {
        return match value {
            PrimitiveValue::U32(values) if values.len() == 1 && values[0] <= 100_000_000 => {
                Some(PrimitiveValue::U32(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0020, 0x9153) {
        return match value {
            PrimitiveValue::F64(values)
                if values.len() == 1 && values.iter().all(|number| number.is_finite()) =>
            {
                Some(PrimitiveValue::F64(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0020, 0x9157) {
        return match value {
            PrimitiveValue::U32(values)
                if !values.is_empty()
                    && values.len() <= 64
                    && values.iter().all(|number| *number > 0) =>
            {
                Some(PrimitiveValue::U32(values))
            }
            _ => None,
        };
    }
    if matches!(tag, Tag(0x0020, 0x9057)) {
        return match value {
            PrimitiveValue::U32(values) if values.len() == 1 && values[0] > 0 => {
                Some(PrimitiveValue::U32(values))
            }
            _ => None,
        };
    }
    if tag == Tag(0x0020, 0x9228) {
        return match value {
            PrimitiveValue::U32(values) if values.len() == 1 => Some(PrimitiveValue::U32(values)),
            _ => None,
        };
    }
    if matches!(tag, Tag(0x0020, 0x9162) | Tag(0x0020, 0x9163)) {
        return match value {
            PrimitiveValue::U16(values) if values.len() == 1 && values[0] > 0 => {
                Some(PrimitiveValue::U16(values))
            }
            _ => None,
        };
    }
    if matches!(tag, Tag(0x0020, 0x9165) | Tag(0x0020, 0x9167)) {
        return match value {
            PrimitiveValue::Tags(values) if values.len() == 1 => Some(PrimitiveValue::Tags(values)),
            _ => None,
        };
    }
    let canonical_text = if tag == Tag(0x0008, 0x0070) {
        canonical_manufacturer(value.to_str().as_ref())
    } else if tag == Tag(0x0008, 0x1090) {
        canonical_model(value.to_str().as_ref())
    } else if tag == RESCALE_TYPE {
        canonical_rescale_type(value.to_str().as_ref())
    } else if tag == Tag(0x0018, 0x0024) {
        canonical_sequence_name(value.to_str().as_ref())
    } else if tag == Tag(0x0018, 0x9005) {
        canonical_pulse_sequence_name(value.to_str().as_ref())
    } else if matches!(tag, Tag(0x0008, 0x0100) | Tag(0x0008, 0x0102)) {
        canonical_code_identifier(value.to_str().as_ref())
    } else if tag == Tag(0x0018, 0x1020) {
        let versions = canonical_software_versions(value.to_str().as_ref(), manufacturer);
        (!versions.is_empty()).then(|| versions.join("\\"))
    } else if matches!(tag, Tag(0x0018, 0x1250) | Tag(0x0018, 0x1251)) {
        canonical_coil_name(value.to_str().as_ref())
    } else {
        None
    };
    if matches!(
        tag,
        Tag(0x0008, 0x0070)
            | Tag(0x0008, 0x1090)
            | RESCALE_TYPE
            | Tag(0x0018, 0x0024)
            | Tag(0x0018, 0x9005)
            | Tag(0x0018, 0x1020)
            | Tag(0x0018, 0x1250)
            | Tag(0x0018, 0x1251)
            | Tag(0x0008, 0x0100)
            | Tag(0x0008, 0x0102)
    ) {
        return canonical_text.map(PrimitiveValue::from);
    }

    match vr {
        VR::DS => canonical_numeric_text(value.to_str().as_ref(), false).map(PrimitiveValue::from),
        VR::IS => canonical_numeric_text(value.to_str().as_ref(), true).map(PrimitiveValue::from),
        VR::CS if tag == Tag(0x0008, 0x9007) => {
            canonical_frame_type(value.to_str().as_ref(), image_type_profile)
                .map(PrimitiveValue::from)
        }
        VR::CS => canonical_code_string(tag, value.to_str().as_ref()).map(PrimitiveValue::from),
        VR::SH if tag == Tag(0x0018, 0x0085) => canonical_nucleus(value.to_str().as_ref())
            .map(str::to_owned)
            .map(PrimitiveValue::from),
        VR::US | VR::SS | VR::UL | VR::SL | VR::UV | VR::SV | VR::AT => Some(value),
        VR::FL => match &value {
            PrimitiveValue::F32(values) if values.iter().all(|number| number.is_finite()) => {
                Some(value)
            }
            _ => None,
        },
        VR::FD => match &value {
            PrimitiveValue::F64(values) if values.iter().all(|number| number.is_finite()) => {
                Some(value)
            }
            _ => None,
        },
        // Text and opaque binary values are default-deny. PixelData is copied as a
        // separately located byte span and never passes through this branch.
        _ => None,
    }
}

fn canonical_code_identifier(value: &str) -> Option<String> {
    let value = value.trim_matches([' ', '\0']);
    (!value.is_empty()
        && value.len() <= 16
        && !value.contains('\\')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    .then(|| value.to_owned())
}

fn canonical_numeric_text(value: &str, integer: bool) -> Option<String> {
    let values = value.split('\\').map(str::trim).collect::<Vec<_>>();
    if values.is_empty() || values.len() > 64 || values.iter().any(|value| value.is_empty()) {
        return None;
    }
    if integer {
        values
            .iter()
            .all(|value| value.parse::<i64>().is_ok())
            .then(|| values.join("\\"))
    } else {
        values
            .iter()
            .all(|value| value.parse::<f64>().is_ok_and(f64::is_finite))
            .then(|| values.join("\\"))
    }
}

fn canonical_code_string(tag: Tag, value: &str) -> Option<String> {
    let allowed: &[&str] = match tag {
        Tag(0x0008, 0x0060) => &["MR"],
        Tag(0x0008, 0x9205) => &["COLOR", "MONOCHROME", "MIXED"],
        Tag(0x0008, 0x9206) => &["VOLUME", "SAMPLED", "DISTORTED", "MIXED"],
        Tag(0x0008, 0x9207) => &[
            "MAX_IP",
            "MIN_IP",
            "VOLUME_RENDER",
            "SURFACE_RENDER",
            "MPR",
            "CURVED_MPR",
            "NONE",
            "MIXED",
        ],
        Tag(0x0008, 0x9208) => &["MAGNITUDE", "PHASE", "REAL", "IMAGINARY", "MIXED"],
        Tag(0x0008, 0x9209) => &[
            "UNKNOWN",
            "T1",
            "T2",
            "T2_STAR",
            "PROTON_DENSITY",
            "DIFFUSION",
            "FLOW_ENCODED",
            "FLUID_ATTENUATED",
            "PERFUSION",
            "STIR",
            "TAGGING",
            "TOF",
            "MIXED",
        ],
        Tag(0x0018, 0x0020) => &["SE", "IR", "GR", "EP", "RM"],
        Tag(0x0018, 0x0021) => &["SK", "MTC", "SS", "TRSS", "SP", "MP", "OSP", "NONE"],
        Tag(0x0018, 0x0022) => &["PER", "RG", "CG", "PPG", "FC", "PFF", "PFP", "SP", "FS"],
        Tag(0x0018, 0x0023) => &["2D", "3D"],
        Tag(0x0018, 0x0025) => &["Y", "N"],
        Tag(0x0018, 0x1312) => &["ROW", "COL", "COLUMN", "OTHER"],
        Tag(0x0018, 0x5100) => &["HFP", "HFS", "HFDR", "HFDL", "FFDR", "FFDL", "FFP", "FFS"],
        Tag(0x0018, 0x9008) => &["SPIN", "GRADIENT", "BOTH"],
        Tag(0x0018, 0x9009) => &["YES", "NO"],
        Tag(0x0018, 0x9010) => &["ACCELERATION", "VELOCITY", "OTHER", "NONE"],
        Tag(0x0018, 0x9011) | Tag(0x0018, 0x9012) | Tag(0x0018, 0x9014) | Tag(0x0018, 0x9015) => {
            &["YES", "NO"]
        }
        Tag(0x0018, 0x9016) => &["RF", "GRADIENT", "RF_AND_GRADIENT", "NONE"],
        Tag(0x0018, 0x9017) => &[
            "FREE_PRECESSION",
            "TRANSVERSE",
            "TIME_REVERSED",
            "LONGITUDINAL",
            "NONE",
        ],
        Tag(0x0018, 0x9018) => &["YES", "NO"],
        Tag(0x0018, 0x9020) => &["ON_RESONANCE", "OFF_RESONANCE", "NONE"],
        Tag(0x0018, 0x9021) | Tag(0x0018, 0x9022) | Tag(0x0018, 0x9024) => &["YES", "NO"],
        Tag(0x0018, 0x9025) => &["FAT", "WATER", "FAT_AND_WATER", "SILICON_GEL", "NONE"],
        Tag(0x0018, 0x9026) => &["WATER", "FAT", "NONE"],
        Tag(0x0018, 0x9027) => &["SLAB", "NONE"],
        Tag(0x0018, 0x9028) => &["GRID", "LINE", "NONE"],
        Tag(0x0018, 0x9029) => &["2D", "3D", "2D_3D", "NONE"],
        Tag(0x0018, 0x9032) => &["RECTILINEAR", "RADIAL", "SPIRAL"],
        Tag(0x0018, 0x9033) => &["SINGLE", "PARTIAL", "FULL"],
        Tag(0x0018, 0x9034) => &[
            "LINEAR",
            "REVERSE_LINEAR",
            "CENTRIC",
            "REVERSE_CENTRIC",
            "SEGMENTED",
            "UNKNOWN",
        ],
        Tag(0x0018, 0x9036) => &["PHASE", "FREQUENCY", "SLICE", "COMBINATION"],
        Tag(0x0018, 0x9043) | Tag(0x0018, 0x9051) => &["BODY", "VOLUME", "SURFACE", "MULTICOIL"],
        Tag(0x0018, 0x9044)
        | Tag(0x0018, 0x9048)
        | Tag(0x0018, 0x9077)
        | Tag(0x0018, 0x9081)
        | Tag(0x0018, 0x9624) => &["YES", "NO"],
        Tag(0x0018, 0x9075) => &["NONE", "ISOTROPIC", "DIRECTIONAL", "BMATRIX"],
        Tag(0x0018, 0x9078) => &["PILS", "SENSE", "GRAPPA", "ASSET", "SMASH", "OTHER", "NONE"],
        Tag(0x0018, 0x9183) => &[
            "PHASE",
            "FREQUENCY",
            "SLICE_SELECT",
            "SLICE_AND_FREQ",
            "SLICE_FREQ_PHASE",
            "PHASE_AND_FREQ",
            "SLICE_AND_PHASE",
            "OTHER",
        ],
        Tag(0x0018, 0x9250) => &["CONTINUOUS", "PULSED", "PSEUDOCONTINUOUS"],
        Tag(0x0018, 0x9257) => &["LABEL", "CONTROL", "M_ZERO_SCAN"],
        Tag(0x0018, 0x9259) | Tag(0x0018, 0x925C) => &["YES", "NO"],
        Tag(0x0020, 0x9072) => &["R", "L", "U", "B"],
        Tag(0x0028, 0x0004) => &[
            "MONOCHROME1",
            "MONOCHROME2",
            "PALETTE COLOR",
            "RGB",
            "YBR_FULL",
            "YBR_FULL_422",
            "YBR_ICT",
            "YBR_RCT",
            "YBR_PARTIAL_420",
        ],
        Tag(0x0028, 0x0301) => &["NO"],
        Tag(0x0028, 0x0303) => &["REMOVED"],
        Tag(0x0028, 0x2110) => &["00", "01"],
        Tag(0x0028, 0x2114) => &[
            "ISO_10918_1",
            "ISO_14495_1",
            "ISO_15444_1",
            "ISO_15444_2",
            "ISO_13818_2",
            "ISO_14496_10",
        ],
        Tag(0x2050, 0x0020) => &["IDENTITY", "INVERSE", "LIN OD"],
        _ => return None,
    };
    if matches!(
        tag,
        Tag(0x0018, 0x9257) | Tag(0x0018, 0x9259) | Tag(0x0018, 0x925C)
    ) {
        let part = value.trim().to_ascii_uppercase();
        return allowed.contains(&part.as_str()).then_some(part);
    }
    let mut output = Vec::new();
    for part in value.split('\\') {
        let part = part.trim().to_ascii_uppercase();
        if allowed.contains(&part.as_str()) && !output.contains(&part) {
            output.push(part);
        }
    }
    (!output.is_empty()).then(|| output.join("\\"))
}

fn canonical_image_type(value: &str, profile: MrImageTypeProfile) -> Option<String> {
    if profile != MrImageTypeProfile::Classic {
        return canonical_enhanced_mr_type(value, false, profile);
    }
    const CLASSIC_OPTIONAL_SAFE_VALUES: &[&str] = &[
        "OTHER",
        "M",
        "MAGNITUDE",
        "P",
        "PHASE",
        "R",
        "REAL",
        "I",
        "IMAGINARY",
        "MIXED",
        "ND",
        "NORM",
        "MOSAIC",
        "GRID",
        "VFRAME",
        "DIS2D",
        "FMRI",
        "BOLD",
        "EPI",
        "T1",
        "T1W",
        "T2",
        "T2W",
        "T2_STAR",
        "T2STAR",
        "FLAIR",
        "DIFFUSION",
        "DWI",
        "ADC",
        "TRACEW",
        "FA",
        "DTI",
        "ASL",
        "PERFUSION",
        "FIELD_MAP",
        "FIELDMAP",
        "PHASEDIFF",
        "SBREF",
        "LOCALIZER",
        "SCOUT",
        "SURVEY",
        "REF",
        "REFERENCE",
        "NONE",
        "FFE",
        "FFE_IP",
        "WATER",
        "FAT",
        "DENSITY MAP",
        "DIFFUSION MAP",
        "IMAGE ADDITION",
        "MODULUS SUBTRACT",
        "MPR",
        "PHASE MAP",
        "PHASE SUBTRACT",
        "PROJECTION IMAGE",
        "T1 MAP",
        "T2 MAP",
        "VELOCITY MAP",
    ];
    let values = value
        .split('\\')
        .map(|part| part.trim().to_ascii_uppercase())
        .collect::<Vec<_>>();
    if !(2..=64).contains(&values.len())
        || !matches!(values[0].as_str(), "ORIGINAL" | "DERIVED")
        || !matches!(values[1].as_str(), "PRIMARY" | "SECONDARY")
    {
        return None;
    }
    // In classic ImageType, Values 1 and 2 are required enumerated values,
    // while Value 3 and later are optional and may contain implementation-
    // specific terms. Retain only bounded scientific terms; replace every
    // unknown optional component with OTHER in place so neither its data nor
    // its positional meaning can shift into another slot.
    Some(
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                if index < 2
                    || value.is_empty()
                    || CLASSIC_OPTIONAL_SAFE_VALUES.contains(&value.as_str())
                {
                    value
                } else {
                    "OTHER".to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\\"),
    )
}

fn classic_image_type_replacement_count(source: &str, canonical: &str) -> u64 {
    source
        .split('\\')
        .zip(canonical.split('\\'))
        .skip(2)
        .filter(|(source, canonical)| {
            *canonical == "OTHER" && !source.trim().eq_ignore_ascii_case("OTHER")
        })
        .count() as u64
}

const ENHANCED_MR_VALUE_3: &[&str] = &[
    "ANGIO",
    "CARDIAC",
    "CARDIAC_GATED",
    "CARDRESP_GATED",
    "DYNAMIC",
    "FLUOROSCOPY",
    "LOCALIZER",
    "MOTION",
    "PERFUSION",
    "PRE_CONTRAST",
    "POST_CONTRAST",
    "RESP_GATED",
    "REST",
    "STATIC",
    "STRESS",
    "VOLUME",
    "NON_PARALLEL",
    "PARALLEL",
    "WHOLE_BODY",
    "ANGIO_TIME",
    "ASL",
    "CINE",
    "DIFFUSION",
    "DIXON",
    "FLOW_ENCODED",
    "FLUID_ATTENUATED",
    "FMRI",
    "MAX_IP",
    "MIN_IP",
    "M_MODE",
    "METABOLITE_MAP",
    "MULTIECHO",
    "PROTON_DENSITY",
    "REALTIME",
    "STIR",
    "TAGGING",
    "TEMPERATURE",
    "T1",
    "T2",
    "T2_STAR",
    "TOF",
    "VELOCITY",
];

const ENHANCED_MR_VALUE_4: &[&str] = &[
    "ADDITION",
    "DIVISION",
    "MASKED",
    "MAXIMUM",
    "MEAN",
    "MINIMUM",
    "MULTIPLICATION",
    "RESAMPLED",
    "STD_DEVIATION",
    "SUBTRACTION",
    "NONE",
    "QUANTITY",
    // Retain established MR defined terms that remain in deployed scanner
    // exports even though modern DICOM prefers QUANTITY plus coded units.
    "ADC",
    "DIFFUSION",
    "DIFFUSION_ANISO",
    "DIFFUSION_ATTNTD",
    "DIFFUSION_ISO",
    "ATTNTD",
    "FA",
    "TRACEW",
    "FAT",
    "FAT_FRACTION",
    "FIELD_MAP",
    "IN_PHASE",
    "METABOLITE_MAP",
    "NEI",
    "OUT_OF_PHASE",
    "PERFUSION_ASL",
    "R_COEFFICIENT",
    "R2_MAP",
    "R2_STAR_MAP",
    "RHO",
    "SCM",
    "SNR_MAP",
    "T1_MAP",
    "T2_STAR_MAP",
    "T2_MAP",
    "TCS",
    "TEMPERATURE",
    "VELOCITY",
    "WATER",
    "WATER_FRACTION",
];

fn canonical_frame_type(value: &str, profile: MrImageTypeProfile) -> Option<String> {
    (profile != MrImageTypeProfile::Classic)
        .then(|| canonical_enhanced_mr_type(value, true, profile))
        .flatten()
}

pub(crate) fn canonical_enhanced_mr_type_for_scientific_contract(
    value: &str,
    frame_type: bool,
    legacy: bool,
) -> Option<String> {
    canonical_enhanced_mr_type(
        value,
        frame_type,
        if legacy {
            MrImageTypeProfile::LegacyConvertedEnhanced
        } else {
            MrImageTypeProfile::Enhanced
        },
    )
}

fn canonical_enhanced_mr_type(
    value: &str,
    frame_type: bool,
    profile: MrImageTypeProfile,
) -> Option<String> {
    const ROOT_VALUE_1: &[&str] = &["ORIGINAL", "DERIVED", "MIXED"];
    const FRAME_VALUE_1: &[&str] = &["ORIGINAL", "DERIVED"];
    const VALUE_2: &[&str] = &["PRIMARY"];
    let values = value
        .split('\\')
        .map(|part| part.trim().to_ascii_uppercase())
        .collect::<Vec<_>>();
    let value_1 = if frame_type && profile == MrImageTypeProfile::LegacyConvertedEnhanced {
        ROOT_VALUE_1
    } else if frame_type {
        FRAME_VALUE_1
    } else {
        ROOT_VALUE_1
    };
    if values.len() != 4 {
        return None;
    }
    let value_4_valid = ENHANCED_MR_VALUE_4.contains(&values[3].as_str())
        || (!frame_type && values[3] == "MIXED")
        || (profile == MrImageTypeProfile::LegacyConvertedEnhanced && values[3].is_empty());
    if !value_1.contains(&values[0].as_str())
        || !VALUE_2.contains(&values[1].as_str())
        || !ENHANCED_MR_VALUE_3.contains(&values[2].as_str())
        || !value_4_valid
        || values[0] == "ORIGINAL"
            && values[3] != "NONE"
            && !(profile == MrImageTypeProfile::LegacyConvertedEnhanced && values[3].is_empty())
    {
        return None;
    }
    Some(values.join("\\"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MrImageTypeProfile {
    Classic,
    Enhanced,
    LegacyConvertedEnhanced,
}

fn object_mr_image_type_profile(object: &InMemDicomObject) -> MrImageTypeProfile {
    match object
        .get(Tag(0x0008, 0x0016))
        .and_then(|element| element.to_str().ok())
        .map(|value| value.trim_matches([' ', '\0']).to_owned())
        .as_deref()
    {
        Some(LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID) => {
            MrImageTypeProfile::LegacyConvertedEnhanced
        }
        Some(ENHANCED_MR_IMAGE_STORAGE_UID) => MrImageTypeProfile::Enhanced,
        _ => MrImageTypeProfile::Classic,
    }
}

pub(crate) fn canonical_manufacturer(value: &str) -> Option<String> {
    let normalized = bounded_equipment_text(value, 64)?;
    let upper = normalized.to_ascii_uppercase();
    if upper == "SIEMENS"
        || upper == "SIEMENS HEALTHCARE"
        || upper == "SIEMENS HEALTHINEERS"
        || upper.starts_with("SIEMENS MEDICAL ")
    {
        Some("SIEMENS".into())
    } else if upper == "PHILIPS"
        || upper.starts_with("PHILIPS MEDICAL ")
        || upper.starts_with("PHILIPS HEALTHCARE ")
    {
        Some("Philips Medical Systems".into())
    } else if upper.contains("GENERAL ELECTRIC")
        || upper == "GE"
        || upper.starts_with("GE MEDICAL")
        || upper.starts_with("GE HEALTHCARE")
    {
        Some("GE MEDICAL SYSTEMS".into())
    } else if upper.contains("CANON") || upper.contains("TOSHIBA") {
        Some("Canon/Toshiba".into())
    } else if upper.contains("UNITED IMAGING")
        || upper.contains("UNITEDIMAGING")
        || upper == "UIH"
        || upper.starts_with("UIH ")
    {
        Some("United Imaging".into())
    } else if upper.contains("BRUKER") {
        Some("Bruker".into())
    } else {
        Some(normalized)
    }
}

pub(crate) fn canonical_model(value: &str) -> Option<String> {
    let value = bounded_equipment_text(value, 64)?;
    match value.to_ascii_uppercase().as_str() {
        "PRISMA_FIT" => return Some("MAGNETOM Prisma_fit".into()),
        "ACHIEVA DSTREAM" => return Some("Achieva dStream".into()),
        _ => {}
    }
    const MODELS: &[&str] = &[
        "MAGNETOM Prisma_fit",
        "MAGNETOM Prisma",
        "MAGNETOM Skyra",
        "MAGNETOM TrioTim",
        "MAGNETOM Trio",
        "MAGNETOM Vida",
        "MAGNETOM Verio",
        "MAGNETOM Terra",
        "MAGNETOM Cima.X",
        "MAGNETOM Connectom",
        "MAGNETOM Sola",
        "MAGNETOM Aera",
        "MAGNETOM Avanto",
        "MAGNETOM Allegra",
        "MAGNETOM Espree",
        "Biograph mMR",
        "Ingenia Elition X",
        "Ingenia Ambition X",
        "Ingenia CX",
        "Ingenia",
        "Achieva dStream",
        "Achieva",
        "Intera",
        "MR 7700",
        "Discovery MR750w",
        "Discovery MR750",
        "Optima MR450w",
        "SIGNA Premier",
        "SIGNA Architect",
        "SIGNA PET/MR",
        "SIGNA HDxt",
        "SIGNA Voyager",
        "SIGNA Artist",
        "SIGNA Hero",
        "Vantage Galan",
        "Vantage Titan",
        "Vantage Orian",
        "Vantage Elan",
        "uMR Jupiter",
        "uMR Omega",
        "uMR 790",
        "uMR 780",
        "uMR 770",
        "uMR 670",
        "uMR 570",
        "uMR 560",
        "BioSpec",
        "PharmaScan",
    ];
    MODELS
        .iter()
        .find(|model| model.eq_ignore_ascii_case(&value))
        .map(|model| (*model).to_owned())
        .or(Some(value))
}

pub(crate) fn canonical_software_versions(value: &str, manufacturer: Option<&str>) -> Vec<String> {
    let manufacturer = manufacturer.unwrap_or("Scanner");
    let tokens = value
        .split(|character: char| {
            character.is_ascii_whitespace() || matches!(character, '\\' | ',' | ';' | '/' | '_')
        })
        .map(|token| {
            token.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.'
            })
        })
        .filter(|token| !token.is_empty());
    let mut output = Vec::new();
    for token in tokens {
        let upper = token.to_ascii_uppercase();
        let canonical = if manufacturer == "SIEMENS" && siemens_version_token(&upper) {
            Some(format!("Siemens {upper}"))
        } else if manufacturer == "Philips Medical Systems" && numeric_version_token(token) {
            Some(format!("Philips {token}"))
        } else if manufacturer == "GE MEDICAL SYSTEMS"
            && (upper.strip_prefix("DV").is_some_and(numeric_version_token)
                || numeric_version_token(token))
        {
            Some(format!("GE {upper}"))
        } else if matches!(manufacturer, "Canon/Toshiba" | "United Imaging" | "Bruker")
            && numeric_version_token(token)
        {
            Some(format!("{manufacturer} {token}"))
        } else {
            None
        };
        if let Some(canonical) = canonical {
            if !output.contains(&canonical) {
                output.push(canonical);
            }
        }
        if output.len() == 16 {
            break;
        }
    }
    if !output.is_empty() {
        return output;
    }
    value
        .split('\\')
        .filter_map(|part| bounded_equipment_text(part, 64))
        .take(16)
        .collect()
}

fn bounded_equipment_text(value: &str, maximum_bytes: usize) -> Option<String> {
    let normalized = value
        .trim_matches([' ', '\0'])
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty()
        || normalized.len() > maximum_bytes
        || normalized.contains('\\')
        || normalized.contains(" / ")
        || normalized.starts_with('/')
        || normalized.contains("..")
        || normalized.contains("://")
        || normalized.contains('@')
        || normalized
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':')
        || !normalized.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b' ' | b'.' | b',' | b'_' | b'-' | b'+' | b'&' | b'(' | b')' | b'/'
                )
        })
        || !normalized.bytes().any(|byte| byte.is_ascii_alphabetic())
    {
        return None;
    }
    let upper = normalized.to_ascii_uppercase();
    let tokens = upper
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.iter().any(|token| {
        [
            "EMAIL",
            "NAME",
            "MRN",
            "PATIENT",
            "PARTICIPANT",
            "SUBJECT",
            "BIRTH",
            "DOB",
            "SSN",
            "ACCESSION",
        ]
        .iter()
        .any(|prefix| token.starts_with(prefix))
    }) || tokens
        .iter()
        .any(|token| token.len() >= 7 && token.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some(normalized)
}

fn canonical_rescale_type(value: &str) -> Option<String> {
    let value = value
        .trim_matches([' ', '\0'])
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!value.is_empty()
        && value.len() <= 16
        && !value.contains('\\')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'_' | b'-' | b'.' | b'/' | b'%')
        })
        && bounded_equipment_text(&value, 16).is_some())
    .then_some(value)
}

fn siemens_version_token(value: &str) -> bool {
    if value == "E11" {
        return true;
    }
    let bytes = value.as_bytes();
    (4..=5).contains(&bytes.len())
        && matches!(
            &bytes[..2],
            b"VA" | b"VB" | b"VC" | b"VD" | b"VE" | b"XA" | b"XB"
        )
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes.get(4).is_none_or(u8::is_ascii_alphabetic)
}

fn numeric_version_token(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty() && part.len() <= 3 && part.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn canonical_sequence_name(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.contains("ep2d") && lower.contains("bold") {
        Some("ep2d_bold".into())
    } else if lower.contains("epfid") && lower.contains("bold") {
        Some("epfid_bold".into())
    } else if lower.contains("bold") {
        Some("bold".into())
    } else if lower.contains("fmri") {
        Some("fmri".into())
    } else if lower.contains("ep2d") {
        Some("ep2d".into())
    } else if lower.contains("epfid") {
        Some("epfid".into())
    } else if lower.contains("epi") {
        Some("epi".into())
    } else if lower.contains("mprage") || lower.contains("mp-rage") {
        Some("mprage".into())
    } else if lower.contains("flair") {
        Some("flair".into())
    } else if lower.contains("bravo") {
        Some("bravo".into())
    } else if lower.contains("spgr") {
        Some("spgr".into())
    } else if lower.contains("space") {
        Some("space".into())
    } else if lower.contains("diff") || lower.contains("dwi") || lower.contains("dti") {
        Some("diffusion".into())
    } else if lower.contains("pcasl") {
        Some("pcasl".into())
    } else if lower.contains("pasl") {
        Some("pasl".into())
    } else if lower.contains("field") && lower.contains("map") {
        Some("fieldmap".into())
    } else {
        None
    }
}

fn canonical_pulse_sequence_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.contains('\\') {
        return None;
    }
    canonical_sequence_name(trimmed).or_else(|| Some("OTHER".into()))
}

fn canonical_coil_name(value: &str) -> Option<String> {
    if let Some(value) = canonical_multi_coil_name_alias(value) {
        return Some(value.to_owned());
    }
    if value.trim_matches([' ', '\0']) == "SURFACE" {
        return Some("SURFACE".to_owned());
    }
    let normalized = value
        .trim()
        .to_ascii_uppercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let tokens = normalized
        .split('_')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let joined = tokens.join("_");
    let base =
        if (joined.contains("HEAD") && joined.contains("NECK")) || joined.contains("HEADNECK") {
            "HEAD_NECK"
        } else if joined.contains("HEAD") || tokens.contains(&"HNU") {
            "HEAD"
        } else {
            [
                "NECK", "BODY", "SPINE", "KNEE", "FLEX", "BREAST", "CARDIAC", "FOOT", "ANKLE",
                "SHOULDER", "WRIST",
            ]
            .into_iter()
            .find(|candidate| joined.contains(candidate))?
        };
    let channels = tokens.iter().find_map(|token| {
        let digits = token.strip_suffix("CH").unwrap_or(token);
        digits
            .parse::<u16>()
            .ok()
            .filter(|channels| (1..=256).contains(channels))
    });
    Some(match channels {
        Some(channels) => format!("{base}_{channels}"),
        None => base.to_owned(),
    })
}

fn canonical_nucleus(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_uppercase().as_str() {
        "1H" => Some("1H"),
        "13C" => Some("13C"),
        "17O" => Some("17O"),
        "19F" => Some("19F"),
        "23NA" => Some("23Na"),
        "31P" => Some("31P"),
        "129XE" => Some("129Xe"),
        _ => None,
    }
}

fn public_attribute_allowed(tag: Tag, vr: VR) -> bool {
    match tag.group() {
        0x0008 => matches!(
            tag.element(),
            0x0005
                | 0x0008
                | 0x0100
                | 0x0102
                | 0x0104
                | 0x0117
                | 0x0016
                | 0x0018
                | 0x001A
                | 0x001B
                | 0x0060
                | 0x0070
                | 0x1090
                | 0x1115
                | 0x1140
                | 0x1150
                | 0x1155
                | 0x1160
                | 0x2112
                | 0x2218
                | 0x9007
                | 0x9205
                | 0x9206
                | 0x9207
                | 0x9208
                | 0x9209
        ),
        0x0010 => false,
        0x0012 => false,
        0x0018 => {
            classic_acquisition_attribute(tag.element())
                || enhanced_acquisition_attribute(tag.element(), vr)
        }
        0x0020 => geometry_attribute(tag.element(), vr),
        0x0028 => pixel_attribute(tag.element(), vr),
        0x0040 => matches!(
            tag.element(),
            0x0555 | 0x9094 | 0x9210 | 0x9211 | 0x9212 | 0x9216 | 0xa170
        ),
        0x2050 => tag.element() == 0x0020,
        0x5200 => matches!(tag.element(), 0x9229 | 0x9230),
        0x7fe0 => {
            tag.element() == 0x0010 || vr == VR::OV && matches!(tag.element(), 0x0001 | 0x0002)
        }
        _ => false,
    }
}

fn classic_acquisition_attribute(element: u16) -> bool {
    matches!(
        element,
        0x0020
            | 0x0021
            | 0x0022
            | 0x0023
            | 0x0024
            | 0x0025
            | 0x0050
            | 0x0080
            | 0x0081
            | 0x0082
            | 0x0083
            | 0x0084
            | 0x0085
            | 0x0086
            | 0x0087
            | 0x0088
            | 0x0089
            | 0x0091
            | 0x0093
            | 0x0094
            | 0x0095
            | 0x1000
            | 0x1020
            | 0x1060
            | 0x1062
            | 0x1250
            | 0x1251
            | 0x1310
            | 0x1312
            | 0x1314
            | 0x1315
            | 0x5100
    )
}

fn enhanced_acquisition_vr(vr: VR) -> bool {
    matches!(
        vr,
        VR::SQ
            | VR::CS
            | VR::UI
            | VR::AT
            | VR::US
            | VR::SS
            | VR::UL
            | VR::SL
            | VR::UV
            | VR::SV
            | VR::FL
            | VR::FD
            | VR::IS
            | VR::DS
            | VR::SH
            | VR::LO
            | VR::DT
    )
}

fn enhanced_acquisition_attribute(element: u16, vr: VR) -> bool {
    match element {
        0x9080 => vr == VR::ST,
        0x9075 => vr == VR::CS,
        0x9076 | 0x9117 | 0x9251 | 0x9260 => vr == VR::SQ,
        0x9087 | 0x9089 | 0x9254 | 0x9255 | 0x9256 => vr == VR::FD,
        0x9252 | 0x925b | 0x925e => vr == VR::LO,
        0x9253 => vr == VR::US,
        0x9257 | 0x9259 | 0x925C => vr == VR::CS,
        0x9258 | 0x925f => vr == VR::UL,
        0x925a => vr == VR::FD,
        0x925d => vr == VR::SQ,
        _ => element >= 0x9000 && enhanced_acquisition_vr(vr),
    }
}

fn geometry_attribute(element: u16, vr: VR) -> bool {
    let enhanced = match element {
        0x0242 => vr == VR::UI,
        0x9056 => vr == VR::SH,
        0x9071 | 0x9170 | 0x9171 | 0x9172 => vr == VR::SQ,
        0x9072 => vr == VR::CS,
        0x9057 | 0x9128 | 0x9157 | 0x9228 => vr == VR::UL,
        0x9111 | 0x9113 | 0x9116 | 0x9221 | 0x9222 => vr == VR::SQ,
        0x9153 => vr == VR::FD,
        0x9156 | 0x9162 | 0x9163 => vr == VR::US,
        0x9161 | 0x9164 => vr == VR::UI,
        0x9165 | 0x9167 => vr == VR::AT,
        _ => false,
    };
    if enhanced {
        return true;
    }
    matches!(
        element,
        0x000D
            | 0x000E
            | 0x0011
            | 0x0012
            | 0x0013
            | 0x0032
            | 0x0037
            | 0x0052
            | 0x0100
            | 0x0105
            | 0x1002
            | 0x1041
    )
}

fn pixel_attribute(element: u16, vr: VR) -> bool {
    if matches!(element, 0x0300 | 0x0302) {
        return false;
    }
    matches!(
        element,
        0x0002..=0x0009
            | 0x0010..=0x0014
            | 0x0030
            | 0x0031
            | 0x0034
            | 0x0100..=0x0103
            | 0x0106..=0x0121
            | 0x0301
            | 0x0303
            | 0x1050..=0x1054
            | 0x1101..=0x1223
            | 0x2000..=0x3010
            | 0x9110
            | 0x9132
            | 0x9145
    ) || vr == VR::SQ && matches!(element, 0x3000 | 0x3010)
}

fn semantic_uid_constant(tag: Tag) -> bool {
    matches!(
        (tag.group(), tag.element()),
        (0x0008, 0x0016)
            | (0x0008, 0x001A)
            | (0x0008, 0x001B)
            | (0x0008, 0x010C)
            | (0x0008, 0x0117)
            | (0x0008, 0x1150)
    )
}

fn canonical_semantic_uid(tag: Tag, value: &str, depth: usize) -> Option<String> {
    let values = value
        .split('\\')
        .map(|value| value.trim_matches([' ', '\0']))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() || values.len() > 16 {
        return None;
    }
    let allowed = values.iter().all(|value| {
        valid_uid(value)
            && if tag == Tag(0x0008, 0x0016) && depth == 0 {
                supported_mr_image_sop_class(value)
            } else {
                value.starts_with("1.2.840.10008.")
            }
    });
    allowed.then(|| values.join("\\"))
}

fn valid_uid(value: &str) -> bool {
    value.len() <= 64
        && !value.starts_with('.')
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

fn is_date_or_time_vr(vr: VR) -> bool {
    matches!(vr, VR::DA | VR::DT | VR::TM)
}

fn contains_overlay_or_graphics(object: &InMemDicomObject, depth: usize) -> bool {
    if depth > MAX_SEQUENCE_DEPTH {
        return true;
    }
    object.iter().any(|element| {
        let tag = element.tag();
        let overlay_group =
            (0x5000..=0x501e).contains(&tag.group()) || (0x6000..=0x601e).contains(&tag.group());
        let graphic_group = tag.group() == 0x0070;
        overlay_group
            || graphic_group
            || element.value().items().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| contains_overlay_or_graphics(item, depth + 1))
            })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelTransformValidationStage {
    Source,
    Sanitized,
}

#[derive(Debug, Clone, Copy)]
struct PixelTransformContext {
    rescale_allowed_here: bool,
    window_allowed_here: bool,
    frame_voi_lut_allowed_here: bool,
    philips_dd005_lut_label_allowed_here: bool,
}

impl PixelTransformContext {
    const fn root() -> Self {
        Self {
            rescale_allowed_here: true,
            window_allowed_here: true,
            frame_voi_lut_allowed_here: false,
            philips_dd005_lut_label_allowed_here: false,
        }
    }

    const fn nested() -> Self {
        Self {
            rescale_allowed_here: false,
            window_allowed_here: false,
            frame_voi_lut_allowed_here: false,
            philips_dd005_lut_label_allowed_here: false,
        }
    }
}

fn validate_pixel_transforms(
    object: &InMemDicomObject,
    depth: usize,
    stage: PixelTransformValidationStage,
    context: PixelTransformContext,
) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    if object.get(REAL_WORLD_VALUE_MAPPING_SEQUENCE).is_some() {
        bail!("DICOM RealWorldValueMapping is not supported by the privacy writer");
    }

    const UNSUPPORTED_TRANSFORMS: &[Tag] = &[
        // Modality, VOI, and presentation LUT containers.
        Tag(0x0028, 0x3000),
        Tag(0x0028, 0x3010),
        Tag(0x2050, 0x0010),
        // LUT Descriptor, Explanation, Modality LUT Type, and LUT Data.
        Tag(0x0028, 0x3002),
        Tag(0x0028, 0x3003),
        Tag(0x0028, 0x3004),
        Tag(0x0028, 0x3006),
        // VOI LUT Function changes the interpretation of Window Center/Width
        // (for example, SIGMOID versus LINEAR_EXACT). It must never be
        // silently dropped.
        VOI_LUT_FUNCTION,
        // ICC profiles cannot be rewritten or substituted without changing
        // color science.
        Tag(0x0028, 0x2000),
        // Palette descriptors, UIDs, direct data, large data, and segmented
        // data. These are atomic pixel transforms, not optional decoration.
        Tag(0x0028, 0x1100),
        Tag(0x0028, 0x1101),
        Tag(0x0028, 0x1102),
        Tag(0x0028, 0x1103),
        Tag(0x0028, 0x1104),
        Tag(0x0028, 0x1111),
        Tag(0x0028, 0x1112),
        Tag(0x0028, 0x1113),
        Tag(0x0028, 0x1114),
        Tag(0x0028, 0x1199),
        Tag(0x0028, 0x1200),
        Tag(0x0028, 0x1201),
        Tag(0x0028, 0x1202),
        Tag(0x0028, 0x1203),
        Tag(0x0028, 0x1204),
        Tag(0x0028, 0x1211),
        Tag(0x0028, 0x1212),
        Tag(0x0028, 0x1213),
        Tag(0x0028, 0x1214),
        Tag(0x0028, 0x1221),
        Tag(0x0028, 0x1222),
        Tag(0x0028, 0x1223),
        Tag(0x0028, 0x1224),
        // The entire Real World Value Mapping family is scientifically
        // atomic. Reject every container, descriptor, range, code, and
        // intercept/slope member before the allowlist could drop only part of
        // a quantitative mapping.
        Tag(0x0040, 0x9094),
        Tag(0x0040, 0x9096),
        Tag(0x0040, 0x9098),
        Tag(0x0040, 0x9211),
        Tag(0x0040, 0x9212),
        Tag(0x0040, 0x9213),
        Tag(0x0040, 0x9214),
        Tag(0x0040, 0x9216),
        Tag(0x0040, 0x9220),
        Tag(0x0040, 0x9224),
        Tag(0x0040, 0x9225),
    ];
    if UNSUPPORTED_TRANSFORMS
        .iter()
        .any(|tag| object.get(*tag).is_some())
    {
        bail!("DICOM contains an unsupported pixel transform");
    }
    let philips_enhanced_root_lut_label = stage == PixelTransformValidationStage::Source
        && depth == 0
        && object_mr_image_type_profile(object) == MrImageTypeProfile::Enhanced
        && root_text(object, Tag(0x0008, 0x0070), VR::LO).as_deref()
            == Some("Philips Medical Systems")
        && root_text(object, LUT_LABEL, VR::SH).as_deref() == Some("Philips");
    if object.get(LUT_LABEL).is_some()
        && !(philips_enhanced_root_lut_label
            || stage == PixelTransformValidationStage::Source
                && context.philips_dd005_lut_label_allowed_here
                && root_text(object, LUT_LABEL, VR::SH).as_deref() == Some("Philips"))
    {
        bail!("DICOM contains an unsupported pixel transform");
    }
    if stage == PixelTransformValidationStage::Sanitized
        && object.get(WINDOW_CENTER_WIDTH_EXPLANATION).is_some()
    {
        bail!("sanitized DICOM contains unsupported window explanation text");
    }

    let rescale_present =
        [RESCALE_INTERCEPT, RESCALE_SLOPE, RESCALE_TYPE].map(|tag| object.get(tag).is_some());
    if rescale_present.iter().any(|present| *present)
        && (!context.rescale_allowed_here
            || !rescale_present.iter().all(|present| *present)
            || !valid_rescale_triplet(object))
    {
        bail!("DICOM contains an incomplete or invalid rescale transform");
    }

    let window_center = object.get(WINDOW_CENTER);
    let window_width = object.get(WINDOW_WIDTH);
    match (window_center, window_width) {
        (None, None) => {}
        (Some(center), Some(width))
            if context.window_allowed_here && valid_window_pair(center, width) => {}
        _ => bail!("DICOM contains an incomplete or invalid window transform"),
    }

    let creators = private_creators(object);
    for element in object.iter() {
        let Some(items) = element.value().items() else {
            continue;
        };
        if element.tag() == PIXEL_VALUE_TRANSFORMATION_SEQUENCE {
            if element.vr() != VR::SQ || items.len() != 1 {
                bail!("DICOM contains an invalid PixelValueTransformationSequence");
            }
            let item = &items[0];
            if item.iter().any(|element| {
                !matches!(
                    element.tag(),
                    RESCALE_INTERCEPT | RESCALE_SLOPE | RESCALE_TYPE
                )
            }) {
                bail!("DICOM contains an unsupported PixelValueTransformationSequence item");
            }
            validate_pixel_transforms(
                item,
                depth + 1,
                stage,
                PixelTransformContext {
                    rescale_allowed_here: true,
                    ..PixelTransformContext::nested()
                },
            )?;
            if ![RESCALE_INTERCEPT, RESCALE_SLOPE, RESCALE_TYPE]
                .iter()
                .all(|tag| item.get(*tag).is_some())
            {
                bail!("DICOM contains an incomplete PixelValueTransformationSequence");
            }
        } else if element.tag() == FRAME_VOI_LUT_SEQUENCE {
            if !context.frame_voi_lut_allowed_here || element.vr() != VR::SQ || items.len() != 1 {
                bail!("DICOM contains an invalid or off-context FrameVOILUTSequence");
            }
            let item = &items[0];
            if item.iter().any(|element| {
                !(matches!(element.tag(), WINDOW_CENTER | WINDOW_WIDTH)
                    || stage == PixelTransformValidationStage::Source
                        && element.tag() == WINDOW_CENTER_WIDTH_EXPLANATION)
            }) {
                bail!("DICOM contains an unsupported FrameVOILUTSequence item");
            }
            validate_pixel_transforms(
                item,
                depth + 1,
                stage,
                PixelTransformContext {
                    window_allowed_here: true,
                    ..PixelTransformContext::nested()
                },
            )?;
            if ![WINDOW_CENTER, WINDOW_WIDTH]
                .iter()
                .all(|tag| item.get(*tag).is_some())
            {
                bail!("DICOM contains an incomplete FrameVOILUTSequence");
            }
        } else {
            let tag = element.tag();
            let creator_tag = Tag(tag.group(), tag.element() >> 8);
            let philips_dd005_per_frame = tag.group() == 0x2005
                && tag.element() & 0x00ff == 0x000f
                && element.vr() == VR::SQ
                && creators_match(&creators, creator_tag, "Philips MR Imaging DD 005");
            let philips_dd005_sequence = philips_dd005_per_frame
                .then(|| rebuild_philips_per_frame_scale_sequence(element.value()));
            let philips_per_frame_scale = matches!(
                philips_dd005_sequence.as_ref(),
                Some(PhilipsPerFrameScaleSequence::Rebuilt(_))
            );
            let philips_non_scaling_private_metadata = stage
                == PixelTransformValidationStage::Source
                && matches!(
                    philips_dd005_sequence.as_ref(),
                    Some(PhilipsPerFrameScaleSequence::NotScaleMetadata)
                );
            let functional_group_item = depth == 0
                && matches!(
                    tag,
                    SHARED_FUNCTIONAL_GROUPS_SEQUENCE | PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE
                );
            for item in items {
                validate_pixel_transforms(
                    item,
                    depth + 1,
                    stage,
                    PixelTransformContext {
                        rescale_allowed_here: philips_per_frame_scale,
                        frame_voi_lut_allowed_here: functional_group_item,
                        philips_dd005_lut_label_allowed_here: philips_non_scaling_private_metadata,
                        ..PixelTransformContext::nested()
                    },
                )?;
            }
        }
    }
    Ok(())
}

fn validate_source_dimension_index_pointers(object: &InMemDicomObject) -> Result<()> {
    let Some(indexes) = exact_sequence_items(object, DIMENSION_INDEX_SEQUENCE) else {
        return Ok(());
    };
    for item in indexes {
        if item.get(DIMENSION_INDEX_PRIVATE_CREATOR).is_some()
            || item.get(FUNCTIONAL_GROUP_PRIVATE_CREATOR).is_some()
        {
            bail!("DICOM uses unsupported private Enhanced MR dimension pointers");
        }
        for pointer_tag in [DIMENSION_INDEX_POINTER, FUNCTIONAL_GROUP_POINTER] {
            if item.get(pointer_tag).is_some()
                && root_at(item, pointer_tag).is_none_or(|pointer| pointer.group() % 2 == 1)
            {
                bail!("DICOM uses an invalid or private Enhanced MR dimension pointer");
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceValidationStage {
    Source,
    Sanitized,
}

fn validate_reference_semantics(
    object: &InMemDicomObject,
    depth: usize,
    stage: ReferenceValidationStage,
    referenced_image_allowed_here: bool,
) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    const UNSUPPORTED_REFERENCE_SEMANTICS: &[Tag] = &[
        DERIVATION_IMAGE_SEQUENCE,
        DERIVATION_CODE_SEQUENCE,
        PURPOSE_OF_REFERENCE_CODE_SEQUENCE,
    ];
    for element in object.iter() {
        if UNSUPPORTED_REFERENCE_SEMANTICS.contains(&element.tag()) {
            bail!(
                "DICOM contains derived/reference semantics that cannot yet be preserved atomically"
            );
        }
        if element.tag() == REFERENCED_IMAGE_SEQUENCE {
            if referenced_image_allowed_here {
                validate_referenced_image_sequence(element, stage)?;
            } else if depth == 0
                && object_mr_image_type_profile(object) == MrImageTypeProfile::Classic
            {
                validate_simple_referenced_image_sequence(element, stage)?;
            } else {
                bail!(
                    "DICOM Referenced Image Sequence is outside the reviewed Shared Functional Groups context"
                );
            }
            // The exact purpose-code subtree is validated atomically above;
            // generic recursion intentionally continues to reject the same
            // code sequence everywhere else.
            continue;
        }
        if element.tag() == SOURCE_IMAGE_SEQUENCE {
            validate_source_image_sequence(element, stage)?;
            continue;
        }
        let Some(items) = element.value().items() else {
            continue;
        };
        let referenced_image_allowed_in_children = depth == 0
            && object_mr_image_type_profile(object) == MrImageTypeProfile::Enhanced
            && element.tag() == SHARED_FUNCTIONAL_GROUPS_SEQUENCE;
        for item in items {
            validate_reference_semantics(
                item,
                depth + 1,
                stage,
                referenced_image_allowed_in_children,
            )?;
        }
    }
    Ok(())
}

fn validate_context_uid_placement(
    object: &InMemDicomObject,
    depth: usize,
    path: &mut Vec<Tag>,
) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    for element in object.iter() {
        if element.tag() == Tag(0x0008, 0x0117) {
            let anatomy = path.ends_with(&[
                SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
                FRAME_ANATOMY_SEQUENCE,
                Tag(0x0008, 0x2218),
            ]) || path.ends_with(&[
                PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE,
                FRAME_ANATOMY_SEQUENCE,
                Tag(0x0008, 0x2218),
            ]);
            let localizer_purpose = path.ends_with(&[
                SHARED_FUNCTIONAL_GROUPS_SEQUENCE,
                REFERENCED_IMAGE_SEQUENCE,
                PURPOSE_OF_REFERENCE_CODE_SEQUENCE,
            ]);
            let expected = if anatomy {
                ANATOMY_CONTEXT_UID
            } else if localizer_purpose {
                LOCALIZER_PURPOSE_CONTEXT_UID
            } else {
                bail!("DICOM Context UID is outside a reviewed coded sequence");
            };
            if root_text(object, element.tag(), VR::UI).as_deref() != Some(expected) {
                bail!("DICOM coded sequence contains an invalid Context UID");
            }
            continue;
        }
        let Some(items) = element.value().items() else {
            continue;
        };
        path.push(element.tag());
        for item in items {
            validate_context_uid_placement(item, depth + 1, path)?;
        }
        path.pop();
    }
    Ok(())
}

fn validate_simple_referenced_image_sequence(
    element: &DataElement<InMemDicomObject>,
    stage: ReferenceValidationStage,
) -> Result<()> {
    if element.vr() != VR::SQ {
        bail!("DICOM Referenced Image Sequence has an invalid value representation");
    }
    let items = element
        .value()
        .items()
        .filter(|items| !items.is_empty() && items.len() <= MAX_SEQUENCE_ITEMS)
        .context("DICOM Referenced Image Sequence is empty or exceeds the bounded item limit")?;
    for item in items {
        let source_group_length = item.get(RETIRED_GROUP_LENGTH);
        let expected_count = 2 + usize::from(
            stage == ReferenceValidationStage::Source && source_group_length.is_some(),
        );
        if item.iter().count() != expected_count
            || item.iter().any(|child| {
                !(matches!(child.tag(), Tag(0x0008, 0x1150) | Tag(0x0008, 0x1155))
                    || stage == ReferenceValidationStage::Source
                        && child.tag() == RETIRED_GROUP_LENGTH)
            })
        {
            bail!("DICOM Referenced Image Sequence has unsupported classic reference semantics");
        }
        if source_group_length.is_some_and(|group_length| {
            group_length.vr() != VR::UL
                || !matches!(
                    group_length.value(),
                    Value::Primitive(PrimitiveValue::U32(values))
                        if values.len() == 1
                            && values[0] > 0
                            && values[0] <= MAX_REFERENCE_ITEM_GROUP_LENGTH
                )
        }) {
            bail!("DICOM Referenced Image Sequence has an invalid retired Group Length");
        }
        let sop_class = root_text(item, Tag(0x0008, 0x1150), VR::UI)
            .context("DICOM Referenced Image Sequence omitted Referenced SOP Class UID")?;
        let sop_instance = root_text(item, Tag(0x0008, 0x1155), VR::UI)
            .context("DICOM Referenced Image Sequence omitted Referenced SOP Instance UID")?;
        if !valid_uid(&sop_class)
            || !sop_class.starts_with("1.2.840.10008.")
            || !valid_uid(&sop_instance)
            || stage == ReferenceValidationStage::Sanitized && !sop_instance.starts_with("2.25.")
        {
            bail!("DICOM Referenced Image Sequence contains an invalid reference UID");
        }
    }
    Ok(())
}

fn validate_referenced_image_sequence(
    element: &DataElement<InMemDicomObject>,
    stage: ReferenceValidationStage,
) -> Result<()> {
    if element.vr() != VR::SQ {
        bail!("DICOM Referenced Image Sequence has an invalid value representation");
    }
    let items = element
        .value()
        .items()
        .filter(|items| !items.is_empty() && items.len() <= MAX_SEQUENCE_ITEMS)
        .context("DICOM Referenced Image Sequence is empty or exceeds the bounded item limit")?;
    for item in items {
        let expected_count = if stage == ReferenceValidationStage::Source {
            6
        } else {
            4
        };
        if item.iter().count() != expected_count
            || item.iter().any(|child| {
                !matches!(
                    child.tag(),
                    Tag(0x0008, 0x1150)
                        | Tag(0x0008, 0x1155)
                        | Tag(0x0008, 0x1160)
                        | PURPOSE_OF_REFERENCE_CODE_SEQUENCE
                        | Tag(0x2005, 0x0014)
                        | Tag(0x2005, 0x1411)
                )
            })
            || stage == ReferenceValidationStage::Sanitized
                && (item.get(Tag(0x2005, 0x0014)).is_some()
                    || item.get(Tag(0x2005, 0x1411)).is_some())
        {
            bail!("DICOM Referenced Image Sequence has unsupported item semantics");
        }
        if root_text(item, Tag(0x0008, 0x1150), VR::UI).as_deref()
            != Some(ENHANCED_MR_IMAGE_STORAGE_UID)
        {
            bail!("DICOM Referenced Image Sequence has an invalid referenced SOP class");
        }
        let instance = root_text(item, Tag(0x0008, 0x1155), VR::UI)
            .context("DICOM Referenced Image Sequence omitted its instance UID")?;
        if !valid_uid(&instance)
            || stage == ReferenceValidationStage::Sanitized && !instance.starts_with("2.25.")
        {
            bail!("DICOM Referenced Image Sequence has an invalid instance UID");
        }
        let frame_text = root_text(item, Tag(0x0008, 0x1160), VR::IS)
            .context("DICOM Referenced Image Sequence omitted its referenced frame")?;
        let frame = frame_text
            .parse::<u64>()
            .ok()
            .filter(|frame| (1..=MAX_DICOM_INSTANCES_PER_SERIES as u64).contains(frame))
            .context("DICOM Referenced Image Sequence has an invalid referenced frame")?;
        if stage == ReferenceValidationStage::Sanitized && frame_text != frame.to_string() {
            bail!("sanitized DICOM retained a non-canonical referenced frame");
        }
        validate_localizer_purpose_code(item)?;

        if stage == ReferenceValidationStage::Source {
            if root_text(item, Tag(0x2005, 0x0014), VR::LO).as_deref()
                != Some("Philips MR Imaging DD 005")
            {
                bail!("DICOM Referenced Image Sequence has an invalid Philips private creator");
            }
            let private_uid = root_text(item, Tag(0x2005, 0x1411), VR::UI)
                .context("DICOM Referenced Image Sequence omitted its Philips private duplicate")?;
            if !valid_uid(&private_uid) {
                bail!("DICOM Referenced Image Sequence has an invalid Philips private duplicate");
            }
        }
    }
    Ok(())
}

fn validate_localizer_purpose_code(item: &InMemDicomObject) -> Result<()> {
    let purpose = exact_sequence_items(item, PURPOSE_OF_REFERENCE_CODE_SEQUENCE)
        .filter(|items| items.len() == 1)
        .context("DICOM Referenced Image Sequence has an invalid purpose code sequence")?;
    let purpose = &purpose[0];
    if purpose.iter().count() != 4
        || purpose.iter().any(|element| {
            !matches!(
                element.tag(),
                Tag(0x0008, 0x0100)
                    | Tag(0x0008, 0x0102)
                    | Tag(0x0008, 0x0104)
                    | Tag(0x0008, 0x0117)
            )
        })
        || root_text(purpose, Tag(0x0008, 0x0100), VR::SH).as_deref() != Some("121311")
        || root_text(purpose, Tag(0x0008, 0x0102), VR::SH).as_deref() != Some("DCM")
        || root_text(purpose, Tag(0x0008, 0x0104), VR::LO).as_deref() != Some("Localizer")
        || root_text(purpose, Tag(0x0008, 0x0117), VR::UI).as_deref()
            != Some(LOCALIZER_PURPOSE_CONTEXT_UID)
    {
        bail!("DICOM Referenced Image Sequence has an unsupported purpose code");
    }
    Ok(())
}

fn rebuild_referenced_image_sequence(
    value: &Value<InMemDicomObject, Vec<u8>>,
    remapper: &mut UidRemapper<'_>,
) -> Result<Value<InMemDicomObject, Vec<u8>>> {
    let items = value
        .items()
        .context("DICOM Referenced Image Sequence is not a sequence")?;
    let mut rebuilt = Vec::with_capacity(items.len());
    for source in items {
        let source_instance = root_text(source, Tag(0x0008, 0x1155), VR::UI)
            .context("DICOM Referenced Image Sequence omitted its instance UID")?;
        let frame = root_text(source, Tag(0x0008, 0x1160), VR::IS)
            .and_then(|value| value.parse::<u64>().ok())
            .context("DICOM Referenced Image Sequence has an invalid referenced frame")?;
        let mut purpose = InMemDicomObject::new_empty();
        purpose.put_str(Tag(0x0008, 0x0100), VR::SH, "121311");
        purpose.put_str(Tag(0x0008, 0x0102), VR::SH, "DCM");
        purpose.put_str(Tag(0x0008, 0x0104), VR::LO, "Localizer");
        purpose.put_str(Tag(0x0008, 0x0117), VR::UI, LOCALIZER_PURPOSE_CONTEXT_UID);

        let mut item = InMemDicomObject::new_empty();
        item.put_str(Tag(0x0008, 0x1150), VR::UI, ENHANCED_MR_IMAGE_STORAGE_UID);
        item.put_str(Tag(0x0008, 0x1155), VR::UI, remapper.map(&source_instance)?);
        item.put_str(Tag(0x0008, 0x1160), VR::IS, frame.to_string());
        item.put(DataElement::new(
            PURPOSE_OF_REFERENCE_CODE_SEQUENCE,
            VR::SQ,
            Value::Sequence(DataSetSequence::new(vec![purpose], Length::UNDEFINED)),
        ));
        rebuilt.push(item);
    }
    Ok(Value::Sequence(DataSetSequence::new(
        rebuilt,
        Length::UNDEFINED,
    )))
}

fn validate_source_image_sequence(
    element: &DataElement<InMemDicomObject>,
    stage: ReferenceValidationStage,
) -> Result<()> {
    let items = element
        .value()
        .items()
        .filter(|items| !items.is_empty() && items.len() <= MAX_SEQUENCE_ITEMS)
        .context("DICOM Source Image Sequence is empty or exceeds the bounded item limit")?;
    if element.vr() != VR::SQ {
        bail!("DICOM Source Image Sequence has an invalid value representation");
    }
    for item in items {
        if item.iter().count() != 2
            || item
                .iter()
                .any(|child| !matches!(child.tag(), Tag(0x0008, 0x1150) | Tag(0x0008, 0x1155)))
        {
            bail!("DICOM Source Image Sequence has unsupported reference item semantics");
        }
        let sop_class = root_text(item, Tag(0x0008, 0x1150), VR::UI)
            .context("DICOM Source Image Sequence omitted Referenced SOP Class UID")?;
        let sop_instance = root_text(item, Tag(0x0008, 0x1155), VR::UI)
            .context("DICOM Source Image Sequence omitted Referenced SOP Instance UID")?;
        if !valid_uid(&sop_class)
            || !sop_class.starts_with("1.2.840.10008.")
            || !valid_uid(&sop_instance)
            || stage == ReferenceValidationStage::Sanitized && !sop_instance.starts_with("2.25.")
        {
            bail!("DICOM Source Image Sequence contains an invalid reference UID");
        }
    }
    Ok(())
}

fn validate_source_enhanced_mr_surface(
    object: &InMemDicomObject,
    profile: MrImageTypeProfile,
) -> Result<()> {
    if profile == MrImageTypeProfile::Classic {
        return Ok(());
    }

    if let Ok(context) = object.element(ACQUISITION_CONTEXT_SEQUENCE) {
        if context.vr() != VR::SQ
            || context
                .value()
                .items()
                .is_none_or(|items| !items.is_empty())
        {
            bail!(
                "Enhanced MR Acquisition Context is non-empty and cannot yet be de-identified without semantic loss"
            );
        }
    }

    // Concatenation validation is series-global. Until the writer verifies a
    // complete concatenation set atomically, never archive a partial member as
    // if it were a self-contained multi-frame object.
    for tag in [
        Tag(0x0020, 0x0242),
        Tag(0x0020, 0x9161),
        Tag(0x0020, 0x9162),
        Tag(0x0020, 0x9163),
        Tag(0x0020, 0x9228),
    ] {
        if object.get(tag).is_some() {
            bail!("Enhanced MR concatenations are not yet supported atomically");
        }
    }

    let shared = exact_sequence_items(object, SHARED_FUNCTIONAL_GROUPS_SEQUENCE);
    let per_frame = exact_sequence_items(object, PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE);
    if let Some(shared) = shared {
        if shared.len() != 1 {
            bail!("Enhanced MR has an invalid Shared Functional Groups Sequence");
        }
        validate_source_functional_group_container(object, &shared[0], profile, true)?;
    }
    if let Some(per_frame) = per_frame {
        for frame in per_frame {
            validate_source_functional_group_container(object, frame, profile, false)?;
        }
    }
    Ok(())
}

fn validate_source_functional_group_container(
    root: &InMemDicomObject,
    container: &InMemDicomObject,
    profile: MrImageTypeProfile,
    shared: bool,
) -> Result<()> {
    let philips_creator = container.get(Tag(0x2005, 0x0014));
    let philips_shared = container.get(Tag(0x2005, 0x140e));
    let philips_per_frame = container.get(Tag(0x2005, 0x140f));
    if philips_creator.is_some() || philips_shared.is_some() || philips_per_frame.is_some() {
        if profile != MrImageTypeProfile::Enhanced
            || root_text(container, Tag(0x2005, 0x0014), VR::LO).as_deref()
                != Some("Philips MR Imaging DD 005")
        {
            bail!("Enhanced MR contains an invalid Philips functional-group private creator");
        }
        if shared {
            let sequence = philips_shared.context(
                "Enhanced MR Philips Shared Functional Groups omitted its DD005 duplicate",
            )?;
            if philips_per_frame.is_some() {
                bail!(
                    "Enhanced MR contains a Philips per-frame duplicate in Shared Functional Groups"
                );
            }
            validate_philips_shared_functional_group_duplicate(root, container, sequence)?;
        } else {
            let sequence = philips_per_frame.context(
                "Enhanced MR Philips Per-frame Functional Groups omitted its DD005 duplicate",
            )?;
            if philips_shared.is_some() {
                bail!(
                    "Enhanced MR contains a Philips shared duplicate in Per-frame Functional Groups"
                );
            }
            validate_philips_per_frame_functional_group_duplicate(sequence)?;
        }
    }
    for element in container.iter() {
        let tag = element.tag();
        if matches!(
            tag,
            Tag(0x2005, 0x0014) | Tag(0x2005, 0x140e) | Tag(0x2005, 0x140f)
        ) {
            continue;
        }
        if element.vr() != VR::SQ {
            bail!("Enhanced MR functional-group containers may contain only sequence macros");
        }
        let common = matches!(
            tag,
            PIXEL_MEASURES_SEQUENCE
                | PLANE_POSITION_SEQUENCE
                | PLANE_ORIENTATION_SEQUENCE
                | FRAME_ANATOMY_SEQUENCE
                | MR_IMAGE_FRAME_TYPE_SEQUENCE
                | FRAME_CONTENT_SEQUENCE
                | PIXEL_VALUE_TRANSFORMATION_SEQUENCE
                | FRAME_VOI_LUT_SEQUENCE
        );
        let current_enhanced = matches!(
            tag,
            Tag(0x0018, 0x9006)
                | Tag(0x0018, 0x9042)
                | Tag(0x0018, 0x9049)
                | Tag(0x0018, 0x9112)
                | Tag(0x0018, 0x9114)
                | Tag(0x0018, 0x9115)
                | Tag(0x0018, 0x9117)
                | Tag(0x0018, 0x9119)
                | Tag(0x0018, 0x9125)
                | Tag(0x0018, 0x9251)
        );
        let legacy_converted = matches!(
            tag,
            UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE
                | UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE
        );
        let referenced_image =
            profile == MrImageTypeProfile::Enhanced && shared && tag == REFERENCED_IMAGE_SEQUENCE;
        let metabolite_map =
            profile == MrImageTypeProfile::Enhanced && !shared && tag == MR_METABOLITE_MAP_SEQUENCE;
        if !(common
            || profile == MrImageTypeProfile::Enhanced && current_enhanced
            || profile == MrImageTypeProfile::LegacyConvertedEnhanced && legacy_converted
            || referenced_image
            || metabolite_map)
        {
            bail!("Enhanced MR contains an unsupported functional-group macro {tag}");
        }
        if tag == FRAME_CONTENT_SEQUENCE && shared {
            bail!("Frame Content Sequence is permitted only per frame");
        }
        if tag == MR_METABOLITE_MAP_SEQUENCE {
            validate_metabolite_map_sequence(element)?;
        }
        if tag == MR_RECEIVE_COIL_SEQUENCE {
            // MULTICOIL is conditional on a complete Multi-coil Definition
            // macro. Validate its source surface before sanitization so that
            // free-text configuration or arbitrary element labels cannot be
            // silently discarded into a misleading partial macro.
            rebuild_multi_coil_receive_sequence(element)?;
        }
        if tag == MR_TRANSMIT_COIL_SEQUENCE {
            rebuild_surface_transmit_coil_sequence(element)?;
        }
        if tag == UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE && !shared
            || tag == UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE && shared
        {
            bail!("Legacy Converted MR contains a converted-attribute macro in the wrong context");
        }
        if tag == CONVERSION_SOURCE_ATTRIBUTES_SEQUENCE {
            bail!("Legacy Converted MR conversion-source references are not yet supported");
        }
        if legacy_converted {
            let items = element
                .value()
                .items()
                .context("Legacy Converted MR converted-attribute macro is not a sequence")?;
            if items.len() != 1 || items[0].iter().next().is_some() {
                bail!("non-empty Legacy Converted MR converted attributes are not yet supported");
            }
        }
    }
    Ok(())
}

fn validate_philips_shared_functional_group_duplicate(
    root: &InMemDicomObject,
    shared: &InMemDicomObject,
    element: &DataElement<InMemDicomObject>,
) -> Result<()> {
    let items = element
        .value()
        .items()
        .filter(|items| element.vr() == VR::SQ && items.len() == 1)
        .context("Enhanced MR Philips shared duplicate must be an exact one-item sequence")?;
    let item = &items[0];
    const ALLOWED: &[(Tag, VR)] = &[
        (Tag(0x0008, 0x0014), VR::UI),
        (Tag(0x0008, 0x0016), VR::UI),
        (Tag(0x0018, 0x0089), VR::IS),
        (Tag(0x0018, 0x9011), VR::CS),
        (Tag(0x0018, 0x9016), VR::CS),
        (Tag(0x0018, 0x9034), VR::CS),
        (Tag(0x0018, 0x9035), VR::FD),
        (Tag(0x0018, 0x9036), VR::CS),
        (Tag(0x0018, 0x9060), VR::CS),
        (Tag(0x0018, 0x9062), VR::CS),
        (Tag(0x0018, 0x9069), VR::FD),
        (Tag(0x0018, 0x9078), VR::CS),
        (Tag(0x0018, 0x9081), VR::CS),
        (Tag(0x0018, 0x9085), VR::CS),
        (Tag(0x0018, 0x9094), VR::CS),
        (Tag(0x0018, 0x9098), VR::FD),
        (Tag(0x0018, 0x9155), VR::FD),
        (Tag(0x0018, 0x9168), VR::FD),
        (Tag(0x0018, 0x9169), VR::CS),
        (Tag(0x0018, 0x9171), VR::CS),
        (Tag(0x0018, 0x9182), VR::FD),
        (Tag(0x0018, 0x9183), VR::CS),
        (Tag(0x0018, 0x9218), VR::FD),
    ];
    if item.iter().count() != ALLOWED.len()
        || item.iter().any(|candidate| {
            !ALLOWED
                .iter()
                .any(|(tag, vr)| candidate.tag() == *tag && candidate.vr() == *vr)
        })
    {
        bail!("Enhanced MR Philips shared duplicate has an unsupported field surface");
    }
    if root_text(item, Tag(0x0008, 0x0014), VR::UI).as_deref() != Some("1.3.46.670589.11.89.5")
        || root_text(item, Tag(0x0008, 0x0016), VR::UI).as_deref() != Some(MR_IMAGE_STORAGE_UID)
        || root_text(item, Tag(0x0018, 0x9011), VR::CS).as_deref() != Some("NO")
        || root_text(item, Tag(0x0018, 0x9171), VR::CS).as_deref() != Some("NONE")
        || exact_fd_values(item, Tag(0x0018, 0x9218), 1).as_deref() != Some(&[0.0])
    {
        bail!("Enhanced MR Philips shared duplicate has unsafe unique values");
    }
    for tag in [
        Tag(0x0018, 0x9085),
        Tag(0x0018, 0x9094),
        Tag(0x0018, 0x9169),
    ] {
        if item.get(tag).is_none_or(|element| {
            element.vr() != VR::CS
                || !matches!(element.value(), Value::Primitive(PrimitiveValue::Empty))
        }) {
            bail!("Enhanced MR Philips shared duplicate has unsafe unique values");
        }
    }
    for (tag, vr, macro_tag) in [
        (Tag(0x0018, 0x0089), VR::IS, None),
        (Tag(0x0018, 0x9016), VR::CS, Some(Tag(0x0018, 0x9115))),
        (Tag(0x0018, 0x9034), VR::CS, None),
        (Tag(0x0018, 0x9035), VR::FD, None),
        (Tag(0x0018, 0x9036), VR::CS, None),
        (Tag(0x0018, 0x9060), VR::CS, None),
        (Tag(0x0018, 0x9062), VR::CS, None),
        (Tag(0x0018, 0x9069), VR::FD, Some(Tag(0x0018, 0x9115))),
        (Tag(0x0018, 0x9078), VR::CS, Some(Tag(0x0018, 0x9115))),
        (Tag(0x0018, 0x9081), VR::CS, Some(Tag(0x0018, 0x9115))),
        (Tag(0x0018, 0x9098), VR::FD, Some(Tag(0x0018, 0x9006))),
        (Tag(0x0018, 0x9155), VR::FD, Some(Tag(0x0018, 0x9115))),
        (Tag(0x0018, 0x9168), VR::FD, Some(Tag(0x0018, 0x9115))),
        (Tag(0x0018, 0x9182), VR::FD, Some(Tag(0x0018, 0x9112))),
        (Tag(0x0018, 0x9183), VR::CS, None),
    ] {
        let expected = if let Some(macro_tag) = macro_tag {
            exact_sequence_items(shared, macro_tag)
                .filter(|items| items.len() == 1)
                .map(|items| &items[0])
                .context("Enhanced MR Philips shared duplicate lost its standard macro source")?
        } else {
            root
        };
        if !matching_vm1_attribute(item, expected, tag, vr) {
            bail!("Enhanced MR Philips shared duplicate disagrees with retained public metadata");
        }
    }
    Ok(())
}

fn matching_vm1_attribute(
    left: &InMemDicomObject,
    right: &InMemDicomObject,
    tag: Tag,
    vr: VR,
) -> bool {
    let (Ok(left_element), Ok(right_element)) = (left.element(tag), right.element(tag)) else {
        return false;
    };
    if left_element.vr() != vr || right_element.vr() != vr {
        return false;
    }
    if vr == VR::FD {
        return exact_fd_values(left, tag, 1) == exact_fd_values(right, tag, 1);
    }
    match (left_element.value(), right_element.value()) {
        (Value::Primitive(PrimitiveValue::Empty), Value::Primitive(PrimitiveValue::Empty)) => true,
        _ => root_text(left, tag, vr)
            .is_some_and(|value| root_text(right, tag, vr).as_deref() == Some(value.as_str())),
    }
}

fn validate_philips_per_frame_functional_group_duplicate(
    element: &DataElement<InMemDicomObject>,
) -> Result<()> {
    let items = element
        .value()
        .items()
        .filter(|items| element.vr() == VR::SQ && items.len() == 1)
        .context("Enhanced MR Philips per-frame duplicate must be an exact one-item sequence")?;
    if !matches!(
        rebuild_philips_per_frame_scale_sequence(element.value()),
        PhilipsPerFrameScaleSequence::Rebuilt(_)
    ) || items[0]
        .get(Tag(0x2005, 0x0010))
        .and_then(|creator| creator.to_str().ok())
        .is_none_or(|creator| creator.trim_matches([' ', '\0']) != "Philips MR Imaging DD 001")
    {
        bail!("Enhanced MR Philips per-frame duplicate is not the reviewed scale container");
    }
    Ok(())
}

fn canonical_multi_coil_name_alias(value: &str) -> Option<&'static str> {
    match value
        .trim_matches([' ', '\0'])
        .to_ascii_uppercase()
        .as_str()
    {
        "MULTI COIL" | "MULTI_COIL" | "MULTICOIL" => Some("MULTI_COIL"),
        _ => None,
    }
}

fn canonical_multi_coil_element_alias(value: &str) -> Option<&'static str> {
    match value
        .trim_matches([' ', '\0'])
        .to_ascii_uppercase()
        .as_str()
    {
        "MULTI ELEMENT" | "MULTI_ELEMENT" | "MULTIELEMENT" => Some("MULTI_ELEMENT"),
        _ => None,
    }
}

/// Validate and atomically rebuild the conditional Enhanced MR multi-coil
/// macro. `None` means the receive-coil macro is not a multi-coil macro and is
/// left to the normal field-level sanitizer. A multi-coil surface is never
/// partially accepted: arbitrary coil/element labels, configuration text,
/// extra fields, wrong VR/VM, or incomplete items reject the source object.
type RebuiltMultiCoilSequence = Option<(Value<InMemDicomObject, Vec<u8>>, usize)>;

fn rebuild_multi_coil_receive_sequence(
    element: &DataElement<InMemDicomObject>,
) -> Result<RebuiltMultiCoilSequence> {
    let Some(items) = element.value().items() else {
        return Ok(None);
    };
    let has_multi_coil_surface = items.iter().any(|item| {
        root_text(item, RECEIVE_COIL_TYPE, VR::CS).as_deref() == Some("MULTICOIL")
            || item.get(MULTI_COIL_DEFINITION_SEQUENCE).is_some()
            || item.get(MULTI_COIL_CONFIGURATION).is_some()
    });
    if !has_multi_coil_surface {
        return Ok(None);
    }
    if element.vr() != VR::SQ || items.len() != 1 {
        bail!("Enhanced MR multi-coil receive macro must be an exact one-item sequence");
    }
    let source = &items[0];
    const RECEIVE_FIELDS: &[Tag] = &[
        RECEIVE_COIL_NAME,
        RECEIVE_COIL_MANUFACTURER_NAME,
        RECEIVE_COIL_TYPE,
        QUADRATURE_RECEIVE_COIL,
        MULTI_COIL_DEFINITION_SEQUENCE,
    ];
    if source.iter().count() != RECEIVE_FIELDS.len()
        || source
            .iter()
            .any(|candidate| !RECEIVE_FIELDS.contains(&candidate.tag()))
    {
        bail!("Enhanced MR multi-coil receive macro has an unsupported field surface");
    }
    let name = root_text(source, RECEIVE_COIL_NAME, VR::SH)
        .and_then(|value| canonical_multi_coil_name_alias(&value))
        .context("Enhanced MR multi-coil receive name is not an exact generic alias")?;
    if name != "MULTI_COIL"
        || source
            .get(RECEIVE_COIL_MANUFACTURER_NAME)
            .is_none_or(|manufacturer| {
                manufacturer.vr() != VR::LO || !matches!(manufacturer.value(), Value::Primitive(_))
            })
        || root_text(source, RECEIVE_COIL_TYPE, VR::CS).as_deref() != Some("MULTICOIL")
    {
        bail!("Enhanced MR multi-coil receive macro has invalid required attributes");
    }
    let quadrature = root_text(source, QUADRATURE_RECEIVE_COIL, VR::CS)
        .filter(|value| matches!(value.as_str(), "YES" | "NO"))
        .context("Enhanced MR multi-coil receive macro has invalid quadrature semantics")?;
    let definition = source
        .get(MULTI_COIL_DEFINITION_SEQUENCE)
        .filter(|definition| definition.vr() == VR::SQ)
        .and_then(|definition| definition.value().items())
        .filter(|items| !items.is_empty() && items.len() <= MAX_MULTI_COIL_ELEMENTS)
        .context("Enhanced MR multi-coil definition must contain 1 to 256 elements")?;

    let mut rebuilt_elements = Vec::with_capacity(definition.len());
    for source_element in definition {
        if source_element.iter().count() != 2
            || source_element.iter().any(|candidate| {
                !matches!(
                    candidate.tag(),
                    MULTI_COIL_ELEMENT_NAME | MULTI_COIL_ELEMENT_USED
                )
            })
        {
            bail!("Enhanced MR multi-coil definition item has an unsupported field surface");
        }
        let element_name = root_text(source_element, MULTI_COIL_ELEMENT_NAME, VR::SH)
            .and_then(|value| canonical_multi_coil_element_alias(&value))
            .context("Enhanced MR multi-coil element name is not an exact generic alias")?;
        let used = root_text(source_element, MULTI_COIL_ELEMENT_USED, VR::CS)
            .filter(|value| matches!(value.as_str(), "YES" | "NO"))
            .context("Enhanced MR multi-coil element has invalid use semantics")?;
        let mut rebuilt = InMemDicomObject::new_empty();
        rebuilt.put_str(MULTI_COIL_ELEMENT_NAME, VR::SH, element_name);
        rebuilt.put_str(MULTI_COIL_ELEMENT_USED, VR::CS, used);
        rebuilt_elements.push(rebuilt);
    }

    let mut rebuilt_receive = InMemDicomObject::new_empty();
    rebuilt_receive.put_str(RECEIVE_COIL_NAME, VR::SH, "MULTI_COIL");
    rebuilt_receive.put(DataElement::new(
        RECEIVE_COIL_MANUFACTURER_NAME,
        VR::LO,
        PrimitiveValue::Empty,
    ));
    rebuilt_receive.put_str(RECEIVE_COIL_TYPE, VR::CS, "MULTICOIL");
    rebuilt_receive.put_str(QUADRATURE_RECEIVE_COIL, VR::CS, quadrature);
    rebuilt_receive.put(DataElement::new(
        MULTI_COIL_DEFINITION_SEQUENCE,
        VR::SQ,
        Value::Sequence(DataSetSequence::new(rebuilt_elements, Length::UNDEFINED)),
    ));
    Ok(Some((
        Value::Sequence(DataSetSequence::new(
            vec![rebuilt_receive],
            Length::UNDEFINED,
        )),
        definition.len(),
    )))
}

/// Philips writes the generic one-character transmit-coil alias `S` for a
/// standard SURFACE coil. Accept that alias only as the complete, exact
/// Enhanced MR transmit-coil macro and emit a fixed non-source name.
fn rebuild_surface_transmit_coil_sequence(
    element: &DataElement<InMemDicomObject>,
) -> Result<Option<Value<InMemDicomObject, Vec<u8>>>> {
    let Some(items) = element.value().items() else {
        return Ok(None);
    };
    let has_source_alias = items.iter().any(|item| {
        item.get(TRANSMIT_COIL_NAME)
            .and_then(|name| name.to_str().ok())
            .is_some_and(|name| name.trim_matches([' ', '\0']) == "S")
    });
    if !has_source_alias {
        return Ok(None);
    }
    if element.vr() != VR::SQ || items.len() != 1 {
        bail!("Enhanced MR surface transmit-coil alias must be an exact one-item sequence");
    }
    let source = &items[0];
    const TRANSMIT_FIELDS: &[Tag] = &[
        TRANSMIT_COIL_NAME,
        TRANSMIT_COIL_MANUFACTURER_NAME,
        TRANSMIT_COIL_TYPE,
    ];
    if source.iter().count() != TRANSMIT_FIELDS.len()
        || source
            .iter()
            .any(|candidate| !TRANSMIT_FIELDS.contains(&candidate.tag()))
        || root_text(source, TRANSMIT_COIL_NAME, VR::SH).as_deref() != Some("S")
        || !valid_type_two_empty(source, TRANSMIT_COIL_MANUFACTURER_NAME, VR::LO)
        || root_text(source, TRANSMIT_COIL_TYPE, VR::CS).as_deref() != Some("SURFACE")
    {
        bail!("Enhanced MR surface transmit-coil alias has an invalid atomic macro");
    }

    let mut rebuilt = InMemDicomObject::new_empty();
    rebuilt.put_str(TRANSMIT_COIL_NAME, VR::SH, "SURFACE");
    rebuilt.put(DataElement::new(
        TRANSMIT_COIL_MANUFACTURER_NAME,
        VR::LO,
        PrimitiveValue::Empty,
    ));
    rebuilt.put_str(TRANSMIT_COIL_TYPE, VR::CS, "SURFACE");
    Ok(Some(Value::Sequence(DataSetSequence::new(
        vec![rebuilt],
        Length::UNDEFINED,
    ))))
}

fn validate_surface_transmit_alias_placement(
    object: &InMemDicomObject,
    depth: usize,
    allowed_here: bool,
) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    for element in object.iter() {
        if element.tag() == MR_TRANSMIT_COIL_SEQUENCE
            && rebuild_surface_transmit_coil_sequence(element)?.is_some()
        {
            if !allowed_here {
                bail!("Enhanced MR surface transmit-coil alias is outside Functional Groups");
            }
            continue;
        }
        let Some(items) = element.value().items() else {
            continue;
        };
        let allowed_in_children = depth == 0
            && object_mr_image_type_profile(object) == MrImageTypeProfile::Enhanced
            && matches!(
                element.tag(),
                SHARED_FUNCTIONAL_GROUPS_SEQUENCE | PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE
            );
        for item in items {
            validate_surface_transmit_alias_placement(item, depth + 1, allowed_in_children)?;
        }
    }
    Ok(())
}

fn validate_metabolite_map_sequence(element: &DataElement<InMemDicomObject>) -> Result<()> {
    let items = element
        .value()
        .items()
        .filter(|items| element.vr() == VR::SQ && items.len() == 1)
        .context("Enhanced MR Metabolite Map Sequence must contain exactly one item")?;
    if items[0].iter().count() != 1
        || root_text(&items[0], METABOLITE_MAP_DESCRIPTION, VR::ST).as_deref() != Some("WATER")
    {
        bail!("Enhanced MR Metabolite Map Sequence has unsupported semantics");
    }
    Ok(())
}

fn validate_metabolite_map_placement(
    object: &InMemDicomObject,
    depth: usize,
    allowed_here: bool,
) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    for element in object.iter() {
        if element.tag() == MR_METABOLITE_MAP_SEQUENCE {
            if !allowed_here {
                bail!("Enhanced MR Metabolite Map Sequence is outside Per-frame Functional Groups");
            }
            validate_metabolite_map_sequence(element)?;
            continue;
        }
        let Some(items) = element.value().items() else {
            continue;
        };
        let allowed_in_children = depth == 0
            && object_mr_image_type_profile(object) == MrImageTypeProfile::Enhanced
            && element.tag() == PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE;
        for item in items {
            validate_metabolite_map_placement(item, depth + 1, allowed_in_children)?;
        }
    }
    Ok(())
}

fn rebuild_metabolite_map_sequence() -> DataElement<InMemDicomObject> {
    let mut item = InMemDicomObject::new_empty();
    item.put_str(METABOLITE_MAP_DESCRIPTION, VR::ST, "WATER");
    DataElement::new(
        MR_METABOLITE_MAP_SEQUENCE,
        VR::SQ,
        Value::Sequence(DataSetSequence::new(vec![item], Length::UNDEFINED)),
    )
}

fn validate_source_asl_conditionals(object: &InMemDicomObject, depth: usize) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    const CONDITIONAL_CHILDREN: &[Tag] = &[
        Tag(0x0018, 0x925a),
        Tag(0x0018, 0x925b),
        Tag(0x0018, 0x925d),
        Tag(0x0018, 0x925e),
        Tag(0x0018, 0x925f),
    ];
    if CONDITIONAL_CHILDREN
        .iter()
        .any(|tag| object.get(*tag).is_some())
    {
        bail!("DICOM contains ASL conditional metadata outside its required macro");
    }
    for element in object.iter() {
        let Some(items) = element.value().items() else {
            continue;
        };
        if element.tag() == Tag(0x0018, 0x9251) {
            if element.vr() != VR::SQ || items.is_empty() {
                bail!("DICOM contains an invalid ASL Context Sequence");
            }
            for item in items {
                validate_source_asl_item(item, depth + 1)?;
            }
        } else {
            for item in items {
                validate_source_asl_conditionals(item, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn validate_source_asl_item(object: &InMemDicomObject, depth: usize) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the local sanitizer limit");
    }
    let crusher = root_text(object, Tag(0x0018, 0x9259), VR::CS)
        .filter(|value| matches!(value.as_str(), "YES" | "NO"))
        .context("DICOM ASL Context omitted a valid crusher flag")?;
    let bolus = root_text(object, Tag(0x0018, 0x925c), VR::CS)
        .filter(|value| matches!(value.as_str(), "YES" | "NO"))
        .context("DICOM ASL Context omitted a valid bolus cut-off flag")?;

    let crusher_flow = direct_fd_vm1(object, Tag(0x0018, 0x925a));
    let crusher_description = direct_lo(object, Tag(0x0018, 0x925b), false);
    if crusher == "YES" {
        if crusher_flow.is_none_or(|value| !(0.0..=1.0e12).contains(&value))
            || crusher_description.is_none()
        {
            bail!("DICOM contains an incomplete or invalid ASL crusher group");
        }
    } else if crusher_flow.is_some() || object.get(Tag(0x0018, 0x925b)).is_some() {
        bail!("DICOM ASL crusher children contradict a NO flag");
    }

    let bolus_sequence = object.get(Tag(0x0018, 0x925d));
    if bolus == "YES" {
        let Some(sequence) = bolus_sequence.filter(|element| element.vr() == VR::SQ) else {
            bail!("DICOM contains an incomplete ASL bolus cut-off group");
        };
        let Some(items) = sequence.value().items().filter(|items| items.len() == 1) else {
            bail!("DICOM ASL bolus cut-off sequence must contain exactly one item");
        };
        let item = &items[0];
        if item
            .iter()
            .any(|element| !matches!(element.tag(), Tag(0x0018, 0x925e) | Tag(0x0018, 0x925f)))
            || direct_lo(item, Tag(0x0018, 0x925e), true).is_none()
            || direct_ul_vm1(item, Tag(0x0018, 0x925f)).is_none_or(|value| value > 100_000_000)
        {
            bail!("DICOM contains an incomplete or invalid ASL bolus cut-off item");
        }
    } else if bolus_sequence.is_some() {
        bail!("DICOM ASL bolus cut-off sequence contradicts a NO flag");
    }

    for element in object.iter() {
        if element.tag() == Tag(0x0018, 0x925d) {
            continue;
        }
        if let Some(items) = element.value().items() {
            for item in items {
                validate_source_asl_conditionals(item, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn direct_fd_vm1(object: &InMemDicomObject, tag: Tag) -> Option<f64> {
    let element = object.get(tag)?;
    if element.vr() != VR::FD {
        return None;
    }
    match element.value() {
        Value::Primitive(PrimitiveValue::F64(values))
            if values.len() == 1 && values[0].is_finite() =>
        {
            Some(values[0])
        }
        _ => None,
    }
}

fn direct_ul_vm1(object: &InMemDicomObject, tag: Tag) -> Option<u32> {
    let element = object.get(tag)?;
    if element.vr() != VR::UL {
        return None;
    }
    match element.value() {
        Value::Primitive(PrimitiveValue::U32(values)) if values.len() == 1 => Some(values[0]),
        _ => None,
    }
}

fn direct_lo(object: &InMemDicomObject, tag: Tag, allow_empty: bool) -> Option<String> {
    let element = object.get(tag)?;
    if element.vr() != VR::LO {
        return None;
    }
    if matches!(element.value(), Value::Primitive(PrimitiveValue::Empty)) {
        return allow_empty.then(String::new);
    }
    let values = element.to_multi_str().ok()?;
    if values.len() != 1 {
        return None;
    }
    let value = values[0].trim_matches([' ', '\0']);
    (value.len() <= 64 && (allow_empty || !value.is_empty())).then(|| value.to_owned())
}

fn valid_rescale_triplet(object: &InMemDicomObject) -> bool {
    let Some(intercept) = decimal_values(object, RESCALE_INTERCEPT, 1, 1) else {
        return false;
    };
    let Some(slope) = decimal_values(object, RESCALE_SLOPE, 1, 1) else {
        return false;
    };
    let Some(kind) = object
        .get(RESCALE_TYPE)
        .filter(|element| element.vr() == VR::LO)
        .and_then(|element| element.to_str().ok())
        .and_then(|value| canonical_rescale_type(value.as_ref()))
    else {
        return false;
    };
    intercept[0].abs() <= 1.0e12 && slope[0].abs() <= 1.0e12 && slope[0] != 0.0 && !kind.is_empty()
}

fn valid_window_pair(
    center: &DataElement<InMemDicomObject, Vec<u8>>,
    width: &DataElement<InMemDicomObject, Vec<u8>>,
) -> bool {
    let values = |element: &DataElement<InMemDicomObject, Vec<u8>>| {
        if element.vr() != VR::DS {
            return None;
        }
        let values = element.to_multi_str().ok()?;
        if values.is_empty() || values.len() > 16 {
            return None;
        }
        values
            .iter()
            .map(|value| {
                let value = value.trim_matches([' ', '\0']);
                (!value.is_empty() && value.len() <= 16)
                    .then(|| value.parse::<f64>().ok())
                    .flatten()
                    .filter(|number| number.is_finite())
            })
            .collect::<Option<Vec<_>>>()
    };
    let (Some(center), Some(width)) = (values(center), values(width)) else {
        return false;
    };
    center.len() == width.len()
        && center.iter().all(|value| value.abs() <= 1.0e12)
        && width.iter().all(|value| *value > 0.0 && *value <= 1.0e12)
}

fn decimal_values(
    object: &InMemDicomObject,
    tag: Tag,
    minimum_vm: usize,
    maximum_vm: usize,
) -> Option<Vec<f64>> {
    let element = object.get(tag)?.to_owned();
    if element.vr() != VR::DS {
        return None;
    }
    let source = element.to_str().ok()?;
    let values = source.split('\\').collect::<Vec<_>>();
    if !(minimum_vm..=maximum_vm).contains(&values.len()) {
        return None;
    }
    values
        .iter()
        .map(|value| {
            let value = value.trim_matches([' ', '\0']);
            (!value.is_empty() && value.len() <= 16)
                .then(|| value.parse::<f64>().ok())
                .flatten()
                .filter(|number| number.is_finite())
        })
        .collect()
}

fn declares_original_primary(object: &InMemDicomObject) -> bool {
    let values = object
        .get(Tag(0x0008, 0x0008))
        .and_then(|element| element.to_multi_str().ok())
        .unwrap_or_default();
    let has = |expected: &str| {
        values
            .iter()
            .any(|value| value.trim().eq_ignore_ascii_case(expected))
    };
    has("ORIGINAL") && has("PRIMARY") && !has("DERIVED") && !has("SECONDARY")
}

const PRIVACY_TYPE_TWO_ATTRIBUTES: &[(Tag, VR)] = &[
    (Tag(0x0008, 0x0020), VR::DA), // Study Date
    (Tag(0x0008, 0x0022), VR::DA), // Acquisition Date
    (Tag(0x0008, 0x0023), VR::DA), // Content Date
    (Tag(0x0008, 0x0030), VR::TM), // Study Time
    (Tag(0x0008, 0x0032), VR::TM), // Acquisition Time
    (Tag(0x0008, 0x0033), VR::TM), // Content Time
    (Tag(0x0008, 0x0050), VR::SH), // Accession Number
    (Tag(0x0008, 0x0090), VR::PN), // Referring Physician Name
    (Tag(0x0010, 0x0030), VR::DA), // Patient Birth Date
    (Tag(0x0010, 0x0040), VR::CS), // Patient Sex
    (Tag(0x0020, 0x0010), VR::SH), // Study ID
    (Tag(0x0020, 0x1040), VR::LO), // Position Reference Indicator
];

const PRESERVED_TYPE_TWO_ATTRIBUTES: &[(Tag, VR)] = &[
    (Tag(0x0008, 0x0070), VR::LO), // Manufacturer
    (Tag(0x0018, 0x0022), VR::CS), // Scan Options
    (Tag(0x0018, 0x0023), VR::CS), // MR Acquisition Type
    (Tag(0x0018, 0x0081), VR::DS), // Echo Time
    (Tag(0x0018, 0x0091), VR::IS), // Echo Train Length
    (Tag(0x0020, 0x0011), VR::IS), // Series Number
    (Tag(0x0020, 0x0012), VR::IS), // Acquisition Number
    (Tag(0x0020, 0x0013), VR::IS), // Instance Number
];

fn insert_required_type_two_attributes(object: &mut InMemDicomObject) {
    for &(tag, vr) in PRIVACY_TYPE_TWO_ATTRIBUTES {
        object.put(DataElement::new(tag, vr, PrimitiveValue::Empty));
    }
    let classic_mr =
        root_text(object, Tag(0x0008, 0x0016), VR::UI).as_deref() == Some(MR_IMAGE_STORAGE_UID);
    for &(tag, vr) in PRESERVED_TYPE_TWO_ATTRIBUTES {
        // Echo Time (0018,0081) is a Classic MR Type 2 shell. Current and
        // Legacy Converted Enhanced MR carry the scientifically precise
        // Effective Echo Time in MR Echo Sequence (0018,9114)/(0018,9082);
        // do not synthesize an empty classic root attribute into those IODs.
        if tag == Tag(0x0018, 0x0081) && !classic_mr {
            continue;
        }
        if object.get(tag).is_none() {
            object.put(DataElement::new(tag, vr, PrimitiveValue::Empty));
        }
    }
}

fn validate_supported_mr_iod_contract(object: &InMemDicomObject, subject_id: &str) -> Result<u64> {
    let required_text = [
        (Tag(0x0008, 0x0008), VR::CS),
        (Tag(0x0008, 0x0016), VR::UI),
        (Tag(0x0008, 0x0018), VR::UI),
        (Tag(0x0008, 0x0060), VR::CS),
        (Tag(0x0010, 0x0010), VR::PN),
        (Tag(0x0010, 0x0020), VR::LO),
        (Tag(0x0020, 0x000d), VR::UI),
        (Tag(0x0020, 0x000e), VR::UI),
    ];
    for (tag, vr) in required_text {
        let element = object
            .element(tag)
            .with_context(|| format!("sanitized DICOM omitted required Type 1 attribute {tag}"))?;
        if element.vr() != vr
            || element
                .to_str()
                .ok()
                .is_none_or(|value| value.trim_matches([' ', '\0']).is_empty())
        {
            bail!("sanitized DICOM has an invalid required Type 1 attribute {tag}");
        }
    }
    let sop_class = root_text(object, Tag(0x0008, 0x0016), VR::UI)
        .context("sanitized DICOM has no valid SOP Class UID")?;
    if !supported_mr_image_sop_class(&sop_class) {
        bail!("sanitized DICOM has an unsupported MR SOP Class UID");
    }
    validate_reference_semantics(object, 0, ReferenceValidationStage::Sanitized, false)?;
    validate_context_uid_placement(object, 0, &mut Vec::new())?;
    validate_metabolite_map_placement(object, 0, false)?;
    let expected_pixel_value_len = validate_pixel_module(object, &sop_class)?;
    if root_text(object, Tag(0x0008, 0x0060), VR::CS).as_deref() != Some("MR")
        || root_text(object, Tag(0x0010, 0x0010), VR::PN).as_deref() != Some(subject_id)
        || root_text(object, Tag(0x0010, 0x0020), VR::LO).as_deref() != Some(subject_id)
    {
        bail!("sanitized DICOM violates the supported MR identity contract");
    }
    for tag in [
        Tag(0x0008, 0x0018),
        Tag(0x0020, 0x000d),
        Tag(0x0020, 0x000e),
        Tag(0x0020, 0x0052),
    ] {
        if root_text(object, tag, VR::UI).is_none_or(|value| {
            !value.starts_with("2.25.") || value.len() > 64 || !valid_uid(&value)
        }) {
            bail!("sanitized DICOM has a non-pseudonymous required UID");
        }
    }
    let frame_of_reference_uid = object
        .element(Tag(0x0020, 0x0052))
        .context("sanitized DICOM omitted required Frame of Reference UID")?;
    if frame_of_reference_uid.vr() != VR::UI {
        bail!("sanitized DICOM has an invalid Frame of Reference UID");
    }
    let enhanced = sop_class != MR_IMAGE_STORAGE_UID;
    if !enhanced {
        for tag in [
            Tag(0x0018, 0x0020), // Scanning Sequence, Type 1, VM 1-n
            Tag(0x0018, 0x0021), // Sequence Variant, Type 1, VM 1-n
        ] {
            if !required_root_code_list(object, tag) {
                bail!("sanitized classic MR omitted required Type 1 acquisition metadata");
            }
        }
    } else if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID {
        for (tag, vr) in [
            (Tag(0x0008, 0x0070), VR::LO), // Manufacturer
            (Tag(0x0008, 0x1090), VR::LO), // Manufacturer Model Name
            (Tag(0x0018, 0x1000), VR::LO), // Device Serial Number
        ] {
            if root_text(object, tag, vr).is_none() {
                bail!("sanitized Enhanced MR omitted required Type 1 equipment metadata");
            }
        }
        let software = object
            .element(Tag(0x0018, 0x1020))
            .context("sanitized Enhanced MR omitted required Software Versions")?;
        if software.vr() != VR::LO
            || software.to_multi_str().ok().is_none_or(|values| {
                values.is_empty()
                    || values
                        .iter()
                        .any(|value| value.trim_matches([' ', '\0']).is_empty())
            })
        {
            bail!("sanitized Enhanced MR omitted required Type 1 equipment metadata");
        }
        let serial = root_text(object, Tag(0x0018, 0x1000), VR::LO).unwrap_or_default();
        if !serial.starts_with("SN-")
            || serial.len() != 27
            || !serial[3..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("sanitized Enhanced MR retained a non-pseudonymous device serial number");
        }
    }
    if enhanced {
        validate_enhanced_mr_iod_contract(object, &sop_class)?;
    }
    for &(tag, vr) in PRIVACY_TYPE_TWO_ATTRIBUTES {
        let element = object
            .element(tag)
            .with_context(|| format!("sanitized DICOM omitted required Type 2 attribute {tag}"))?;
        if enhanced && tag == Tag(0x0008, 0x0023) {
            if root_text(object, tag, vr).as_deref() != Some(ENHANCED_CONTENT_DATE_SENTINEL) {
                bail!("sanitized Enhanced MR has an invalid de-identified Content Date");
            }
            continue;
        }
        if enhanced && tag == Tag(0x0008, 0x0033) {
            if root_text(object, tag, vr).as_deref() != Some(ENHANCED_CONTENT_TIME_SENTINEL) {
                bail!("sanitized Enhanced MR has an invalid de-identified Content Time");
            }
            continue;
        }
        if element.vr() != vr || !matches!(element.value(), Value::Primitive(PrimitiveValue::Empty))
        {
            bail!("sanitized DICOM retained a non-empty privacy-sensitive Type 2 attribute");
        }
    }
    for &(tag, vr) in PRESERVED_TYPE_TWO_ATTRIBUTES {
        if enhanced && tag == Tag(0x0018, 0x0081) {
            continue;
        }
        let element = object
            .element(tag)
            .with_context(|| format!("sanitized DICOM omitted required Type 2 attribute {tag}"))?;
        if element.vr() != vr {
            bail!("sanitized DICOM has an invalid required Type 2 attribute {tag}");
        }
        if matches!(element.value(), Value::Primitive(PrimitiveValue::Empty)) {
            continue;
        }
        if tag == Tag(0x0008, 0x0070) {
            let value = element.to_str()?;
            if canonical_manufacturer(value.as_ref()).as_deref()
                != Some(value.trim_matches([' ', '\0']))
            {
                bail!("sanitized DICOM retained unsafe Manufacturer text");
            }
        } else {
            let source = element.to_str()?;
            let source = source.trim_matches([' ', '\0']);
            let valid = match vr {
                VR::IS => canonical_numeric_text(source, true).is_some(),
                VR::DS => canonical_numeric_text(source, false).is_some(),
                VR::CS => canonical_code_string(tag, source).as_deref() == Some(source),
                _ => false,
            };
            if !valid {
                bail!("sanitized DICOM retained an invalid Type 2 attribute");
            }
        }
    }
    Ok(expected_pixel_value_len)
}

fn validate_enhanced_mr_iod_contract(object: &InMemDicomObject, sop_class: &str) -> Result<()> {
    let invalid = || {
        anyhow::anyhow!("sanitized Enhanced MR omitted or invalidated a mandatory IOD attribute")
    };

    let instance_number = root_text(object, Tag(0x0020, 0x0013), VR::IS)
        .context("sanitized Enhanced MR omitted mandatory Instance Number")?;
    if canonical_numeric_text(&instance_number, true).as_deref() != Some(instance_number.as_str()) {
        bail!("sanitized Enhanced MR has an invalid mandatory Instance Number");
    }

    let pixel_presentation = required_root_code_value(object, Tag(0x0008, 0x9205))
        .context("sanitized Enhanced MR omitted mandatory Pixel Presentation")?;
    let volumetric_properties = required_root_code_value(object, Tag(0x0008, 0x9206))
        .context("sanitized Enhanced MR omitted mandatory Volumetric Properties")?;
    let volume_calculation = required_root_code_value(object, Tag(0x0008, 0x9207))
        .context("sanitized Enhanced MR omitted mandatory Volume Based Calculation Technique")?;
    if pixel_presentation != "MONOCHROME"
        || !matches!(
            volumetric_properties.as_str(),
            "VOLUME" | "SAMPLED" | "DISTORTED" | "MIXED"
        )
        || root_text(object, Tag(0x2050, 0x0020), VR::CS).as_deref() != Some("IDENTITY")
    {
        bail!("sanitized Enhanced MR has invalid mandatory presentation metadata");
    }
    let image_type_element = object
        .element(Tag(0x0008, 0x0008))
        .context("sanitized Enhanced MR omitted mandatory Image Type")?;
    if image_type_element.vr() != VR::CS {
        bail!("sanitized Enhanced MR has invalid mandatory Image Type");
    }
    let image_type = image_type_element.to_str()?;
    let image_type = image_type.trim_matches([' ', '\0']);
    if image_type.split('\\').next() == Some("ORIGINAL") && volume_calculation != "NONE" {
        bail!("sanitized Enhanced MR has inconsistent volume calculation metadata");
    }

    let frames = optional_number_of_frames(object)
        .context("sanitized Enhanced MR has invalid Number of Frames")?
        .context("sanitized Enhanced MR omitted Number of Frames")?;
    let shared = exact_sequence_items(object, SHARED_FUNCTIONAL_GROUPS_SEQUENCE)
        .context("sanitized Enhanced MR omitted Shared Functional Groups Sequence")?;
    let per_frame = exact_sequence_items(object, PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE)
        .context("sanitized Enhanced MR omitted Per-frame Functional Groups Sequence")?;
    if shared.len() != 1 || per_frame.len() != usize::try_from(frames).map_err(|_| invalid())? {
        bail!("sanitized Enhanced MR has invalid mandatory functional-group modules");
    }

    let context = exact_sequence_items(object, ACQUISITION_CONTEXT_SEQUENCE)
        .context("sanitized Enhanced MR omitted Acquisition Context Sequence")?;
    if !context.is_empty() {
        bail!("sanitized Enhanced MR retained a non-empty Acquisition Context Sequence");
    }

    for tag in [
        PIXEL_MEASURES_SEQUENCE,
        PLANE_POSITION_SEQUENCE,
        PLANE_ORIENTATION_SEQUENCE,
        MR_IMAGE_FRAME_TYPE_SEQUENCE,
    ] {
        required_functional_group_items(&shared[0], per_frame, tag).with_context(|| {
            format!("sanitized Enhanced MR omitted mandatory functional-group macro {tag}")
        })?;
    }
    for item in required_functional_group_items(&shared[0], per_frame, PIXEL_MEASURES_SEQUENCE)
        .ok_or_else(invalid)?
    {
        validate_pixel_measures_item(item)?;
    }
    for item in required_functional_group_items(&shared[0], per_frame, PLANE_POSITION_SEQUENCE)
        .ok_or_else(invalid)?
    {
        validate_plane_position_item(item)?;
    }
    for item in required_functional_group_items(&shared[0], per_frame, PLANE_ORIENTATION_SEQUENCE)
        .ok_or_else(invalid)?
    {
        validate_plane_orientation_item(item)?;
    }
    let frame_type_items =
        required_functional_group_items(&shared[0], per_frame, MR_IMAGE_FRAME_TYPE_SEQUENCE)
            .ok_or_else(invalid)?;
    validate_frame_type_items(
        &frame_type_items,
        image_type,
        &pixel_presentation,
        &volumetric_properties,
        &volume_calculation,
        sop_class,
    )?;
    if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID {
        for item in required_functional_group_items(&shared[0], per_frame, FRAME_ANATOMY_SEQUENCE)
            .context("sanitized Enhanced MR omitted mandatory Frame Anatomy macro")?
        {
            validate_frame_anatomy_item(item)?;
        }
        required_functional_group_items(&shared[0], per_frame, PIXEL_VALUE_TRANSFORMATION_SEQUENCE)
            .context("sanitized Enhanced MR omitted mandatory Pixel Value Transformation macro")?;
    } else {
        validate_legacy_converted_macro_shells(&shared[0], per_frame)?;
    }

    let frame_contents =
        required_per_frame_functional_group_items(&shared[0], per_frame, FRAME_CONTENT_SEQUENCE)
            .context("sanitized Enhanced MR frame omitted Frame Content Sequence")?;
    for item in &frame_contents {
        validate_frame_content_item(item, sop_class == ENHANCED_MR_IMAGE_STORAGE_UID)?;
    }

    if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID
        && matches!(image_type.split('\\').next(), Some("ORIGINAL" | "MIXED"))
    {
        validate_mr_pulse_sequence_module(object)?;
        for (tag, validator) in [
            (
                Tag(0x0018, 0x9112),
                validate_mr_timing_item as fn(&InMemDicomObject) -> Result<()>,
            ),
            (Tag(0x0018, 0x9114), validate_mr_echo_item),
            (Tag(0x0018, 0x9115), validate_mr_modifier_item),
            (Tag(0x0018, 0x9006), validate_mr_imaging_modifier_item),
            (Tag(0x0018, 0x9042), validate_mr_receive_coil_item),
            (Tag(0x0018, 0x9049), validate_mr_transmit_coil_item),
            (Tag(0x0018, 0x9119), validate_mr_averages_item),
        ] {
            for item in
                required_functional_group_items(&shared[0], per_frame, tag).with_context(|| {
                    format!("sanitized Enhanced MR omitted mandatory MR macro {tag}")
                })?
            {
                validator(item)?;
            }
        }
        if root_text(object, Tag(0x0018, 0x9032), VR::CS).as_deref() == Some("RECTILINEAR") {
            for item in required_functional_group_items(&shared[0], per_frame, Tag(0x0018, 0x9125))
                .context("sanitized Enhanced MR omitted conditional MR FOV/Geometry macro")?
            {
                validate_mr_fov_item(item)?;
            }
        }
    }

    let dimension_organizations = exact_sequence_items(object, Tag(0x0020, 0x9221));
    let dimension_indexes = exact_sequence_items(object, DIMENSION_INDEX_SEQUENCE);
    let dimensions_required = sop_class == ENHANCED_MR_IMAGE_STORAGE_UID;
    let dimensions_present = dimension_organizations.is_some()
        || dimension_indexes.is_some()
        || frame_contents
            .iter()
            .any(|item| item.get(DIMENSION_INDEX_VALUES).is_some());
    if dimensions_required || dimensions_present {
        let dimension_organizations = dimension_organizations
            .filter(|items| !items.is_empty())
            .context("sanitized Enhanced MR has an incomplete Dimension Organization Sequence")?;
        let dimension_indexes = dimension_indexes
            .filter(|items| !items.is_empty())
            .context("sanitized Enhanced MR has an incomplete Dimension Index Sequence")?;
        let organization_uids = dimension_organizations
            .iter()
            .map(|item| root_text(item, DIMENSION_ORGANIZATION_UID, VR::UI))
            .collect::<Option<HashSet<_>>>()
            .context("sanitized Enhanced MR has an invalid Dimension Organization Sequence")?;
        if organization_uids.len() != dimension_organizations.len()
            || organization_uids
                .iter()
                .any(|uid| !uid.starts_with("2.25.") || !valid_uid(uid))
            || dimension_indexes.iter().any(|item| {
                !valid_dimension_index_item(object, &shared[0], per_frame, item, &organization_uids)
            })
        {
            bail!("sanitized Enhanced MR has invalid mandatory dimension-index attributes");
        }
        for frame_content in &frame_contents {
            let dimension_values = frame_content
                .element(DIMENSION_INDEX_VALUES)
                .context("sanitized Enhanced MR frame omitted Dimension Index Values")?;
            let valid_values = matches!(
                dimension_values.value(),
                Value::Primitive(PrimitiveValue::U32(values))
                    if values.len() == dimension_indexes.len()
                        && values.iter().all(|value| *value > 0)
            );
            if dimension_values.vr() != VR::UL || !valid_values {
                bail!("sanitized Enhanced MR frame has invalid Dimension Index Values");
            }
        }
    } else if frame_contents
        .iter()
        .any(|item| item.get(DIMENSION_INDEX_VALUES).is_some())
    {
        bail!("Legacy Converted MR retained Dimension Index Values without dimensions");
    }

    if sop_class == ENHANCED_MR_IMAGE_STORAGE_UID {
        if root_text(object, Tag(0x0028, 0x0301), VR::CS).as_deref() != Some("NO")
            || required_root_code_value(object, Tag(0x0008, 0x9208)).is_none()
            || required_root_code_value(object, Tag(0x0008, 0x9209)).is_none()
        {
            bail!("sanitized Enhanced MR has invalid mandatory image-description metadata");
        }
        let lossy = required_root_code_value(object, Tag(0x0028, 0x2110))
            .context("sanitized Enhanced MR omitted Lossy Image Compression")?;
        if lossy == "01" {
            let ratios = object.element(Tag(0x0028, 0x2112)).map_err(|_| invalid())?;
            let methods = object.element(Tag(0x0028, 0x2114)).map_err(|_| invalid())?;
            if ratios.vr() != VR::DS || methods.vr() != VR::CS {
                bail!("sanitized Enhanced MR has invalid lossy-compression metadata");
            }
            let ratios = ratios.to_multi_str().map_err(|_| invalid())?;
            let methods = methods.to_multi_str().map_err(|_| invalid())?;
            if ratios.is_empty()
                || ratios.len() != methods.len()
                || ratios.iter().any(|value| {
                    value
                        .trim_matches([' ', '\0'])
                        .parse::<f64>()
                        .ok()
                        .is_none_or(|value| !value.is_finite() || value <= 0.0)
                })
                || methods.iter().any(|value| {
                    let value = value.trim_matches([' ', '\0']);
                    canonical_code_string(Tag(0x0028, 0x2114), value).as_deref() != Some(value)
                })
            {
                bail!("sanitized Enhanced MR has inconsistent lossy-compression metadata");
            }
        }
    }
    Ok(())
}

fn required_root_code_value(object: &InMemDicomObject, tag: Tag) -> Option<String> {
    let value = root_text(object, tag, VR::CS)?;
    (canonical_code_string(tag, &value).as_deref() == Some(value.as_str())).then_some(value)
}

fn required_code_value(object: &InMemDicomObject, tag: Tag) -> Option<String> {
    required_root_code_value(object, tag)
}

fn exact_fd_values(object: &InMemDicomObject, tag: Tag, vm: usize) -> Option<Vec<f64>> {
    let element = object.get(tag)?;
    if element.vr() != VR::FD {
        return None;
    }
    match element.value() {
        Value::Primitive(PrimitiveValue::F64(values))
            if values.len() == vm && values.iter().all(|value| value.is_finite()) =>
        {
            Some(values.iter().copied().collect())
        }
        _ => None,
    }
}

fn valid_type_two_empty(object: &InMemDicomObject, tag: Tag, vr: VR) -> bool {
    object.get(tag).is_some_and(|element| {
        element.vr() == vr && matches!(element.value(), Value::Primitive(PrimitiveValue::Empty))
    })
}

fn validate_pixel_measures_item(item: &InMemDicomObject) -> Result<()> {
    let spacing = decimal_values(item, Tag(0x0028, 0x0030), 2, 2)
        .context("Enhanced MR Pixel Measures omitted Pixel Spacing")?;
    let thickness = decimal_values(item, Tag(0x0018, 0x0050), 1, 1)
        .context("Enhanced MR Pixel Measures omitted Slice Thickness")?;
    if spacing.iter().any(|value| *value <= 0.0 || *value > 1.0e6)
        || thickness[0] <= 0.0
        || thickness[0] > 1.0e6
    {
        bail!("Enhanced MR Pixel Measures contains invalid physical spacing");
    }
    Ok(())
}

fn validate_plane_position_item(item: &InMemDicomObject) -> Result<()> {
    let position = decimal_values(item, Tag(0x0020, 0x0032), 3, 3)
        .context("Enhanced MR Plane Position omitted Image Position Patient")?;
    if position.iter().any(|value| value.abs() > 1.0e9) {
        bail!("Enhanced MR Plane Position is outside the supported finite range");
    }
    Ok(())
}

fn validate_plane_orientation_item(item: &InMemDicomObject) -> Result<()> {
    let values = decimal_values(item, Tag(0x0020, 0x0037), 6, 6)
        .context("Enhanced MR Plane Orientation omitted Image Orientation Patient")?;
    let first = &values[..3];
    let second = &values[3..];
    let first_norm = first.iter().map(|value| value * value).sum::<f64>();
    let second_norm = second.iter().map(|value| value * value).sum::<f64>();
    let dot = first
        .iter()
        .zip(second)
        .map(|(left, right)| left * right)
        .sum::<f64>();
    if (first_norm - 1.0).abs() > 1.0e-3 || (second_norm - 1.0).abs() > 1.0e-3 || dot.abs() > 1.0e-3
    {
        bail!("Enhanced MR Plane Orientation is not orthonormal");
    }
    Ok(())
}

fn validate_frame_anatomy_item(item: &InMemDicomObject) -> Result<()> {
    required_code_value(item, Tag(0x0020, 0x9072))
        .context("Enhanced MR Frame Anatomy omitted Frame Laterality")?;
    let anatomy = exact_sequence_items(item, Tag(0x0008, 0x2218))
        .context("Enhanced MR Frame Anatomy omitted Anatomic Region Sequence")?;
    if anatomy.len() != 1
        || root_text(&anatomy[0], Tag(0x0008, 0x0100), VR::SH)
            .filter(|value| canonical_code_identifier(value).as_deref() == Some(value.as_str()))
            .is_none()
        || root_text(&anatomy[0], Tag(0x0008, 0x0102), VR::SH)
            .filter(|value| canonical_code_identifier(value).as_deref() == Some(value.as_str()))
            .is_none()
        || root_text(&anatomy[0], Tag(0x0008, 0x0104), VR::LO).as_deref() != Some("ANATOMY")
    {
        bail!("Enhanced MR Frame Anatomy contains an invalid coded anatomy macro");
    }
    Ok(())
}

fn validate_frame_type_items(
    items: &[&InMemDicomObject],
    root_image_type: &str,
    root_pixel_presentation: &str,
    root_volumetric_properties: &str,
    root_volume_calculation: &str,
    sop_class: &str,
) -> Result<()> {
    let legacy = sop_class == LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID;
    let root_origin = root_image_type.split('\\').next().unwrap_or_default();
    let mut origins = HashSet::new();
    for item in items {
        let frame_type_element = item
            .element(Tag(0x0008, 0x9007))
            .context("Enhanced MR Image Frame Type omitted Frame Type")?;
        if frame_type_element.vr() != VR::CS {
            bail!("Enhanced MR Image Frame Type has an invalid Frame Type VR");
        }
        let frame_type = frame_type_element
            .to_str()?
            .trim_matches([' ', '\0'])
            .to_owned();
        if canonical_enhanced_mr_type_for_scientific_contract(&frame_type, true, legacy).as_deref()
            != Some(frame_type.as_str())
        {
            bail!("Enhanced MR Image Frame Type has invalid positional values");
        }
        let origin = frame_type.split('\\').next().unwrap_or_default();
        origins.insert(origin.to_owned());
        let pixel = required_code_value(item, Tag(0x0008, 0x9205))
            .context("Enhanced MR Image Frame Type omitted Pixel Presentation")?;
        let volumetric = required_code_value(item, Tag(0x0008, 0x9206))
            .context("Enhanced MR Image Frame Type omitted Volumetric Properties")?;
        let calculation = required_code_value(item, Tag(0x0008, 0x9207))
            .context("Enhanced MR Image Frame Type omitted Volume Calculation")?;
        if (root_pixel_presentation != "MIXED" && pixel != root_pixel_presentation)
            || (root_volumetric_properties != "MIXED" && volumetric != root_volumetric_properties)
            || (root_volume_calculation != "MIXED" && calculation != root_volume_calculation)
            || origin == "ORIGINAL" && calculation != "NONE"
        {
            bail!("Enhanced MR root and frame image-description metadata disagree");
        }
    }
    let origin_summary_valid = match root_origin {
        "ORIGINAL" => origins.len() == 1 && origins.contains("ORIGINAL"),
        "DERIVED" => origins.len() == 1 && origins.contains("DERIVED"),
        "MIXED" if legacy && origins.len() == 1 && origins.contains("MIXED") => true,
        "MIXED" => origins.contains("ORIGINAL") && origins.contains("DERIVED"),
        _ => false,
    };
    if !origin_summary_valid {
        bail!("Enhanced MR root Image Type does not summarize frame origins");
    }
    Ok(())
}

fn validate_frame_content_item(item: &InMemDicomObject, current_enhanced: bool) -> Result<()> {
    if current_enhanced {
        for tag in [Tag(0x0018, 0x9074), Tag(0x0018, 0x9151)] {
            if root_text(item, tag, VR::DT).as_deref() != Some(ENHANCED_FRAME_DATETIME_SENTINEL) {
                bail!("Enhanced MR Frame Content omitted a de-identified mandatory DateTime");
            }
        }
        let duration = exact_fd_values(item, Tag(0x0018, 0x9220), 1)
            .context("Enhanced MR Frame Content omitted Frame Acquisition Duration")?;
        if duration[0] < 0.0 || duration[0] > 1.0e12 {
            bail!("Enhanced MR Frame Acquisition Duration is invalid");
        }
    }
    Ok(())
}

fn validate_mr_pulse_sequence_module(object: &InMemDicomObject) -> Result<()> {
    let name = root_text(object, Tag(0x0018, 0x9005), VR::SH)
        .context("Enhanced MR omitted Pulse Sequence Name")?;
    if canonical_pulse_sequence_name(&name).as_deref() != Some(name.as_str()) {
        bail!("Enhanced MR Pulse Sequence Name is not safely canonicalized");
    }
    required_code_value(object, Tag(0x0018, 0x0023))
        .context("Enhanced MR omitted MR Acquisition Type")?;
    let echo = required_code_value(object, Tag(0x0018, 0x9008))
        .context("Enhanced MR omitted Echo Pulse Sequence")?;
    if matches!(echo.as_str(), "SPIN" | "BOTH") {
        required_code_value(object, Tag(0x0018, 0x9011))
            .context("Enhanced MR omitted Multiple Spin Echo")?;
    }
    for tag in [
        Tag(0x0018, 0x9012),
        Tag(0x0018, 0x9014),
        Tag(0x0018, 0x9015),
        Tag(0x0018, 0x9017),
        Tag(0x0018, 0x9018),
        Tag(0x0018, 0x9024),
        Tag(0x0018, 0x9025),
        Tag(0x0018, 0x9029),
        Tag(0x0018, 0x9032),
        Tag(0x0018, 0x9033),
    ] {
        required_code_value(object, tag)
            .with_context(|| format!("Enhanced MR pulse-sequence module omitted {tag}"))?;
    }
    if root_text(object, Tag(0x0018, 0x9032), VR::CS).as_deref() == Some("RECTILINEAR") {
        required_code_value(object, Tag(0x0018, 0x9034))
            .context("Enhanced MR omitted rectilinear phase-encode reordering")?;
    }
    if root_us(object, Tag(0x0018, 0x9093)).is_none() {
        bail!("Enhanced MR omitted Number of K-space Trajectories");
    }
    Ok(())
}

fn validate_mr_timing_item(item: &InMemDicomObject) -> Result<()> {
    let repetition = decimal_values(item, Tag(0x0018, 0x0080), 1, 1)
        .context("Enhanced MR Timing omitted Repetition Time")?;
    let flip = decimal_values(item, Tag(0x0018, 0x1314), 1, 1)
        .context("Enhanced MR Timing omitted Flip Angle")?;
    let echo_train = root_text(item, Tag(0x0018, 0x0091), VR::IS)
        .and_then(|value| value.parse::<u32>().ok())
        .context("Enhanced MR Timing omitted Echo Train Length")?;
    if repetition[0] <= 0.0
        || repetition[0] > 1.0e9
        || !(0.0..=360.0).contains(&flip[0])
        || echo_train > 1_000_000
        || root_us(item, Tag(0x0018, 0x9240)).is_none()
        || root_us(item, Tag(0x0018, 0x9241)).is_none()
    {
        bail!("Enhanced MR Timing contains invalid required values");
    }
    Ok(())
}

fn validate_mr_echo_item(item: &InMemDicomObject) -> Result<()> {
    let echo = exact_fd_values(item, Tag(0x0018, 0x9082), 1)
        .context("Enhanced MR Echo omitted Effective Echo Time")?;
    if echo[0] < 0.0 || echo[0] > 1.0e9 {
        bail!("Enhanced MR Effective Echo Time is invalid");
    }
    Ok(())
}

fn validate_mr_modifier_item(item: &InMemDicomObject) -> Result<()> {
    for tag in [
        Tag(0x0018, 0x9009),
        Tag(0x0018, 0x9010),
        Tag(0x0018, 0x9016),
        Tag(0x0018, 0x9021),
        Tag(0x0018, 0x9026),
        Tag(0x0018, 0x9027),
        Tag(0x0018, 0x9077),
        Tag(0x0018, 0x9081),
    ] {
        required_code_value(item, tag)
            .with_context(|| format!("Enhanced MR Modifier omitted {tag}"))?;
    }
    if root_text(item, Tag(0x0018, 0x9077), VR::CS).as_deref() == Some("YES") {
        required_code_value(item, Tag(0x0018, 0x9078))
            .context("Enhanced MR parallel acquisition omitted its technique")?;
    }
    if root_text(item, Tag(0x0018, 0x9081), VR::CS).as_deref() == Some("YES") {
        required_code_value(item, Tag(0x0018, 0x9036))
            .context("Enhanced MR partial Fourier omitted its direction")?;
    }
    Ok(())
}

fn validate_mr_imaging_modifier_item(item: &InMemDicomObject) -> Result<()> {
    for tag in [
        Tag(0x0018, 0x9020),
        Tag(0x0018, 0x9022),
        Tag(0x0018, 0x9028),
    ] {
        required_code_value(item, tag)
            .with_context(|| format!("Enhanced MR Imaging Modifier omitted {tag}"))?;
    }
    let transmitter = exact_fd_values(item, Tag(0x0018, 0x9098), 1)
        .context("Enhanced MR Imaging Modifier omitted Transmitter Frequency")?;
    let bandwidth = decimal_values(item, Tag(0x0018, 0x0095), 1, 1)
        .context("Enhanced MR Imaging Modifier omitted Pixel Bandwidth")?;
    if transmitter[0] <= 0.0 || bandwidth[0] <= 0.0 {
        bail!("Enhanced MR Imaging Modifier contains invalid quantitative values");
    }
    Ok(())
}

fn validate_mr_receive_coil_item(item: &InMemDicomObject) -> Result<()> {
    let name = root_text(item, RECEIVE_COIL_NAME, VR::SH)
        .context("Enhanced MR Receive Coil omitted Receive Coil Name")?;
    if canonical_coil_name(&name).as_deref() != Some(name.as_str())
        || !valid_type_two_empty(item, RECEIVE_COIL_MANUFACTURER_NAME, VR::LO)
    {
        bail!("Enhanced MR Receive Coil contains unsafe identity text");
    }
    let coil_type = required_code_value(item, RECEIVE_COIL_TYPE)
        .context("Enhanced MR Receive Coil omitted Receive Coil Type")?;
    required_code_value(item, QUADRATURE_RECEIVE_COIL)
        .context("Enhanced MR Receive Coil omitted Quadrature Receive Coil")?;
    if coil_type == "MULTICOIL" {
        if name != "MULTI_COIL" || item.iter().count() != 5 {
            bail!("Enhanced MR multi-coil receive macro is not atomically canonicalized");
        }
        let definitions = exact_sequence_items(item, MULTI_COIL_DEFINITION_SEQUENCE)
            .filter(|items| !items.is_empty() && items.len() <= MAX_MULTI_COIL_ELEMENTS)
            .context("Enhanced MR multi-coil definition is missing or unbounded")?;
        for definition in definitions {
            if definition.iter().count() != 2
                || root_text(definition, MULTI_COIL_ELEMENT_NAME, VR::SH).as_deref()
                    != Some("MULTI_ELEMENT")
                || required_code_value(definition, MULTI_COIL_ELEMENT_USED).is_none()
                || definition.iter().any(|element| {
                    !matches!(
                        element.tag(),
                        MULTI_COIL_ELEMENT_NAME | MULTI_COIL_ELEMENT_USED
                    )
                })
            {
                bail!("Enhanced MR multi-coil definition is not atomically canonicalized");
            }
        }
    } else if item.get(MULTI_COIL_DEFINITION_SEQUENCE).is_some()
        || item.get(MULTI_COIL_CONFIGURATION).is_some()
    {
        bail!("Enhanced MR non-multi receive coil retained multi-coil metadata");
    }
    Ok(())
}

fn validate_mr_transmit_coil_item(item: &InMemDicomObject) -> Result<()> {
    let name = root_text(item, TRANSMIT_COIL_NAME, VR::SH)
        .context("Enhanced MR Transmit Coil omitted Transmit Coil Name")?;
    if canonical_coil_name(&name).as_deref() != Some(name.as_str())
        || !valid_type_two_empty(item, TRANSMIT_COIL_MANUFACTURER_NAME, VR::LO)
    {
        bail!("Enhanced MR Transmit Coil contains unsafe identity text");
    }
    let coil_type = required_code_value(item, TRANSMIT_COIL_TYPE)
        .context("Enhanced MR Transmit Coil omitted Transmit Coil Type")?;
    if name == "SURFACE"
        && (coil_type != "SURFACE"
            || item.iter().count() != 3
            || item.iter().any(|element| {
                !matches!(
                    element.tag(),
                    TRANSMIT_COIL_NAME | TRANSMIT_COIL_MANUFACTURER_NAME | TRANSMIT_COIL_TYPE
                )
            }))
    {
        bail!("Enhanced MR surface transmit-coil macro is not atomically canonicalized");
    }
    Ok(())
}

fn validate_mr_averages_item(item: &InMemDicomObject) -> Result<()> {
    let averages = decimal_values(item, Tag(0x0018, 0x0083), 1, 1)
        .context("Enhanced MR Averages omitted Number of Averages")?;
    if averages[0] <= 0.0 || averages[0] > 1.0e9 {
        bail!("Enhanced MR Number of Averages is invalid");
    }
    Ok(())
}

fn validate_mr_fov_item(item: &InMemDicomObject) -> Result<()> {
    let direction = required_code_value(item, Tag(0x0018, 0x1312))
        .context("Enhanced MR FOV omitted In-plane Phase Encoding Direction")?;
    let sampling = decimal_values(item, Tag(0x0018, 0x0093), 1, 1)
        .context("Enhanced MR FOV omitted Percent Sampling")?;
    let phase_fov = decimal_values(item, Tag(0x0018, 0x0094), 1, 1)
        .context("Enhanced MR FOV omitted Percent Phase FOV")?;
    if !matches!(direction.as_str(), "ROW" | "COLUMN" | "OTHER")
        || root_us(item, Tag(0x0018, 0x9058)).is_none()
        || root_us(item, Tag(0x0018, 0x9231)).is_none()
        || !(0.0..=100.0).contains(&sampling[0])
        || sampling[0] == 0.0
        || !(0.0..=100.0).contains(&phase_fov[0])
        || phase_fov[0] == 0.0
    {
        bail!("Enhanced MR FOV/Geometry contains invalid required values");
    }
    Ok(())
}

fn required_functional_group_items<'a>(
    shared: &'a InMemDicomObject,
    per_frame: &'a [InMemDicomObject],
    tag: Tag,
) -> Option<Vec<&'a InMemDicomObject>> {
    if shared.get(tag).is_some() {
        let items = exact_sequence_items(shared, tag)?;
        if items.len() != 1 || per_frame.iter().any(|frame| frame.get(tag).is_some()) {
            return None;
        }
        return Some(vec![&items[0]]);
    }
    let mut output = Vec::with_capacity(per_frame.len());
    for frame in per_frame {
        let items = exact_sequence_items(frame, tag)?;
        if items.len() != 1 {
            return None;
        }
        output.push(&items[0]);
    }
    (!output.is_empty()).then_some(output)
}

fn required_per_frame_functional_group_items<'a>(
    shared: &InMemDicomObject,
    per_frame: &'a [InMemDicomObject],
    tag: Tag,
) -> Option<Vec<&'a InMemDicomObject>> {
    if shared.get(tag).is_some() {
        return None;
    }
    let mut output = Vec::with_capacity(per_frame.len());
    for frame in per_frame {
        let items = exact_sequence_items(frame, tag)?;
        if items.len() != 1 {
            return None;
        }
        output.push(&items[0]);
    }
    (!output.is_empty()).then_some(output)
}

fn validate_legacy_converted_macro_shells(
    shared: &InMemDicomObject,
    per_frame: &[InMemDicomObject],
) -> Result<()> {
    let shared_items =
        exact_sequence_items(shared, UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE)
            .context("Legacy Converted MR omitted Unassigned Shared Converted Attributes")?;
    if shared_items.len() != 1 || shared_items[0].iter().next().is_some() {
        bail!("Legacy Converted MR has unsupported non-empty shared converted attributes");
    }
    if shared
        .get(UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE)
        .is_some()
        || shared.get(CONVERSION_SOURCE_ATTRIBUTES_SEQUENCE).is_some()
    {
        bail!("Legacy Converted MR has a converted-attribute macro in the wrong context");
    }
    for frame in per_frame {
        let items = exact_sequence_items(frame, UNASSIGNED_PER_FRAME_CONVERTED_ATTRIBUTES_SEQUENCE)
            .context("Legacy Converted MR omitted Unassigned Per-frame Converted Attributes")?;
        if items.len() != 1 || items[0].iter().next().is_some() {
            bail!("Legacy Converted MR has unsupported non-empty per-frame converted attributes");
        }
        if frame
            .get(UNASSIGNED_SHARED_CONVERTED_ATTRIBUTES_SEQUENCE)
            .is_some()
            || frame.get(CONVERSION_SOURCE_ATTRIBUTES_SEQUENCE).is_some()
        {
            bail!("Legacy Converted MR has unsupported conversion-source metadata");
        }
    }
    Ok(())
}

fn exact_sequence_items(object: &InMemDicomObject, tag: Tag) -> Option<&[InMemDicomObject]> {
    let element = object.get(tag)?;
    (element.vr() == VR::SQ)
        .then(|| element.value().items())
        .flatten()
}

fn valid_dimension_index_item(
    root: &InMemDicomObject,
    shared: &InMemDicomObject,
    per_frame: &[InMemDicomObject],
    item: &InMemDicomObject,
    organization_uids: &HashSet<String>,
) -> bool {
    if item.iter().any(|element| {
        !matches!(
            element.tag(),
            DIMENSION_ORGANIZATION_UID | DIMENSION_INDEX_POINTER | FUNCTIONAL_GROUP_POINTER
        )
    }) || root_text(item, DIMENSION_ORGANIZATION_UID, VR::UI)
        .is_none_or(|uid| !organization_uids.contains(&uid))
    {
        return false;
    }
    let Some(index_pointer) = root_at(item, DIMENSION_INDEX_POINTER) else {
        return false;
    };
    if index_pointer.group() % 2 == 1
        || matches!(
            index_pointer,
            FRAME_CONTENT_SEQUENCE | DIMENSION_INDEX_VALUES
        )
    {
        return false;
    }
    let group_pointer_present = item.get(FUNCTIONAL_GROUP_POINTER).is_some();
    let group_pointer = root_at(item, FUNCTIONAL_GROUP_POINTER);
    if group_pointer_present && group_pointer.is_none() {
        return false;
    }
    if let Some(group_pointer) = group_pointer {
        return group_pointer.group() % 2 == 0
            && valid_functional_group_reference(
                shared,
                per_frame,
                group_pointer,
                Some(index_pointer),
            );
    }

    // Functional Group Sequence pointers identify the macro itself and shall
    // not carry FunctionalGroupPointer. A public retained root attribute is
    // the other standard no-group-pointer form.
    valid_functional_group_reference(shared, per_frame, index_pointer, None)
        || root
            .get(index_pointer)
            .is_some_and(|element| element.vr() != VR::SQ)
}

fn valid_functional_group_reference(
    shared: &InMemDicomObject,
    per_frame: &[InMemDicomObject],
    group_tag: Tag,
    target_tag: Option<Tag>,
) -> bool {
    let valid_group = |container: &InMemDicomObject| {
        exact_sequence_items(container, group_tag).is_some_and(|items| {
            items.len() == 1 && target_tag.is_none_or(|target| items[0].get(target).is_some())
        })
    };
    let shared_has_group = shared.get(group_tag).is_some();
    let per_frame_has_group = |frame: &InMemDicomObject| frame.get(group_tag).is_some();
    if shared_has_group {
        valid_group(shared) && per_frame.iter().all(|frame| !per_frame_has_group(frame))
    } else {
        !per_frame.is_empty() && per_frame.iter().all(valid_group)
    }
}

fn required_root_code_list(object: &InMemDicomObject, tag: Tag) -> bool {
    let Ok(element) = object.element(tag) else {
        return false;
    };
    if element.vr() != VR::CS {
        return false;
    }
    let Ok(value) = element.to_str() else {
        return false;
    };
    let value = value.trim_matches([' ', '\0']);
    !value.is_empty() && canonical_code_string(tag, value).as_deref() == Some(value)
}

fn validate_pixel_module(object: &InMemDicomObject, sop_class: &str) -> Result<u64> {
    let invalid =
        || anyhow::anyhow!("DICOM pixel module is missing or inconsistent with its MR SOP Class");
    let rows = root_us(object, Tag(0x0028, 0x0010)).ok_or_else(invalid)?;
    let columns = root_us(object, Tag(0x0028, 0x0011)).ok_or_else(invalid)?;
    let samples = root_us(object, Tag(0x0028, 0x0002)).ok_or_else(invalid)?;
    let photometric = root_text(object, Tag(0x0028, 0x0004), VR::CS).ok_or_else(invalid)?;
    let bits_allocated = root_us(object, Tag(0x0028, 0x0100)).ok_or_else(invalid)?;
    let bits_stored = root_us(object, Tag(0x0028, 0x0101)).ok_or_else(invalid)?;
    let high_bit = root_us(object, Tag(0x0028, 0x0102)).ok_or_else(invalid)?;
    let pixel_representation = root_us(object, Tag(0x0028, 0x0103)).ok_or_else(invalid)?;
    if rows == 0
        || columns == 0
        || samples != 1
        || !matches!(pixel_representation, 0 | 1)
        || high_bit.checked_add(1) != Some(bits_stored)
        || object.get(Tag(0x0028, 0x0006)).is_some()
    {
        return Err(invalid());
    }
    let number_of_frames = optional_number_of_frames(object).ok_or_else(invalid)?;
    let frames = match sop_class {
        MR_IMAGE_STORAGE_UID => {
            if !matches!(photometric.as_str(), "MONOCHROME1" | "MONOCHROME2")
                || bits_allocated != 16
                || !(1..=16).contains(&bits_stored)
                || number_of_frames.is_some_and(|frames| frames != 1)
            {
                return Err(invalid());
            }
            number_of_frames.unwrap_or(1)
        }
        ENHANCED_MR_IMAGE_STORAGE_UID | LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID => {
            if photometric != "MONOCHROME2"
                || !matches!((bits_allocated, bits_stored), (8, 8) | (16, 12) | (16, 16))
                || number_of_frames.is_none()
            {
                return Err(invalid());
            }
            number_of_frames.ok_or_else(invalid)?
        }
        _ => return Err(invalid()),
    };
    let raw_bytes = u64::from(rows)
        .checked_mul(u64::from(columns))
        .and_then(|value| value.checked_mul(u64::from(samples)))
        .and_then(|value| value.checked_mul(frames))
        .and_then(|value| value.checked_mul(u64::from(bits_allocated / 8)))
        .ok_or_else(invalid)?;
    raw_bytes.checked_add(raw_bytes % 2).ok_or_else(invalid)
}

fn root_us(object: &InMemDicomObject, tag: Tag) -> Option<u16> {
    let element = object.get(tag)?;
    if element.vr() != VR::US {
        return None;
    }
    match element.value() {
        Value::Primitive(PrimitiveValue::U16(values)) if values.len() == 1 => Some(values[0]),
        _ => None,
    }
}

fn root_at(object: &InMemDicomObject, tag: Tag) -> Option<Tag> {
    let element = object.get(tag)?;
    if element.vr() != VR::AT {
        return None;
    }
    match element.value() {
        Value::Primitive(PrimitiveValue::Tags(values)) if values.len() == 1 => Some(values[0]),
        _ => None,
    }
}

fn root_text(object: &InMemDicomObject, tag: Tag, vr: VR) -> Option<String> {
    let element = object.get(tag)?;
    if element.vr() != vr {
        return None;
    }
    let values = element.to_multi_str().ok()?;
    if values.len() != 1 {
        return None;
    }
    let value = values[0].trim_matches([' ', '\0']);
    (!value.is_empty()).then(|| value.to_owned())
}

fn optional_number_of_frames(object: &InMemDicomObject) -> Option<Option<u64>> {
    let element = match object.element(Tag(0x0028, 0x0008)) {
        Ok(element) => element,
        Err(_) => return Some(None),
    };
    if element.vr() != VR::IS {
        return None;
    }
    let values = element.to_multi_str().ok()?;
    if values.len() != 1 {
        return None;
    }
    let value = values[0].trim_matches([' ', '\0']);
    let frames = value.parse::<u64>().ok()?;
    (frames > 0 && frames <= MAX_DICOM_INSTANCES_PER_SERIES as u64).then_some(Some(frames))
}

fn audit_dataset(object: &InMemDicomObject, subject_id: &str, depth: usize) -> Result<()> {
    if depth == 0 && object.get(Tag(0x0008, 0x0008)).is_none() {
        bail!("sanitized DICOM omitted required ImageType");
    }
    if depth == 0 {
        validate_supported_mr_iod_contract(object, subject_id)?;
        validate_pixel_transforms(
            object,
            0,
            PixelTransformValidationStage::Sanitized,
            PixelTransformContext::root(),
        )?;
        validate_source_asl_conditionals(object, 0)?;
    }
    let mut sequence_items = 0_usize;
    let image_type_profile = object_mr_image_type_profile(object);
    audit_dataset_inner(
        object,
        subject_id,
        depth,
        &mut sequence_items,
        image_type_profile,
    )
}

fn audit_dataset_inner(
    object: &InMemDicomObject,
    subject_id: &str,
    depth: usize,
    sequence_items: &mut usize,
    image_type_profile: MrImageTypeProfile,
) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("sanitized DICOM exceeded sequence-depth policy");
    }
    let creators = private_creators(object);
    for element in object.iter() {
        let tag = element.tag();
        if depth == 0
            && PRIVACY_TYPE_TWO_ATTRIBUTES
                .iter()
                .chain(PRESERVED_TYPE_TWO_ATTRIBUTES)
                .any(|(required, _)| tag == *required)
        {
            continue;
        }
        if image_type_profile != MrImageTypeProfile::Classic
            && element.vr() == VR::DT
            && matches!(tag, Tag(0x0018, 0x9074) | Tag(0x0018, 0x9151))
            && root_text(object, tag, VR::DT).as_deref() == Some(ENHANCED_FRAME_DATETIME_SENTINEL)
        {
            continue;
        }
        if tag.group() % 2 == 1 {
            if (0x0010..=0x00ff).contains(&tag.element()) {
                if !safe_private_creator(element.to_str()?.as_ref()) {
                    bail!("sanitized DICOM retained an unknown private creator");
                }
            } else {
                let canonical_siemens_csa = tag == Tag(0x0029, 0x1010)
                    && element.vr() == VR::OB
                    && creators_match(&creators, Tag(0x0029, 0x0010), "SIEMENS CSA HEADER")
                    && element.to_bytes().ok().is_some_and(|bytes| {
                        sanitize_siemens_csa_image_header(bytes.as_ref())
                            .is_some_and(|sanitized| sanitized.as_slice() == bytes.as_ref())
                    });
                let creator_tag = Tag(tag.group(), tag.element() >> 8);
                let canonical_ps315_safe_private = canonical_ps315_safe_private_attribute(
                    tag,
                    element.vr(),
                    element.value(),
                    &creators,
                )
                .is_some();
                let canonical_philips_scaling = tag.group() == 0x2005
                    && matches!(tag.element() & 0x00ff, 0x000d | 0x000e)
                    && element.vr() == VR::FL
                    && creators_match(&creators, creator_tag, "Philips MR Imaging DD 001")
                    && match tag.element() & 0x00ff {
                        0x000d => bounded_float32_vm1(element.value(), |v| v.abs() <= 1.0e9),
                        0x000e => bounded_float32_vm1(element.value(), |v| v > 0.0 && v <= 1.0e9),
                        _ => false,
                    };
                let canonical_philips_number_of_slices = tag.group() == 0x2001
                    && tag.element() & 0x00ff == 0x0018
                    && element.vr() == VR::SL
                    && creators_match(&creators, creator_tag, "Philips Imaging DD 001")
                    && positive_i32_vm1(element.value(), 1..=4096);
                let canonical_philips_water_fat_shift = tag.group() == 0x2001
                    && tag.element() & 0x00ff == 0x0022
                    && element.vr() == VR::FL
                    && creators_match(&creators, creator_tag, "Philips Imaging DD 001")
                    && bounded_float32_vm1(element.value(), |v| (0.0..=1.0e6).contains(&v));
                let canonical_philips_per_frame_scale = tag.group() == 0x2005
                    && tag.element() & 0x00ff == 0x000f
                    && element.vr() == VR::SQ
                    && creators_match(&creators, creator_tag, "Philips MR Imaging DD 005")
                    && canonical_philips_per_frame_scale_sequence(element.value());
                if !canonical_ps315_safe_private
                    && !canonical_siemens_csa
                    && !canonical_philips_scaling
                    && !canonical_philips_number_of_slices
                    && !canonical_philips_water_fat_shift
                    && !canonical_philips_per_frame_scale
                {
                    bail!("sanitized DICOM retained unsafe private data");
                }
            }
        } else if !matches!(
            tag,
            Tag(0x0010, 0x0010) | Tag(0x0010, 0x0020) | Tag(0x0012, 0x0062) | Tag(0x0012, 0x0063)
        ) && !public_attribute_allowed(tag, element.vr())
        {
            bail!("sanitized DICOM retained a non-allowlisted public attribute");
        }
        if is_date_or_time_vr(element.vr()) {
            bail!("sanitized DICOM retained a date or time value");
        }
        if tag == Tag(0x0008, 0x0008) {
            if depth != 0 {
                bail!("sanitized DICOM retained a nested ImageType");
            }
            let value = element.to_str()?;
            let canonical = canonical_image_type(value.as_ref(), image_type_profile)
                .context("sanitized DICOM retained an invalid positional ImageType")?;
            if canonical != value.trim_matches([' ', '\0']) {
                bail!("sanitized DICOM retained a non-canonical positional ImageType");
            }
        }
        if tag == Tag(0x0008, 0x9007) {
            let value = element.to_str()?;
            let canonical = canonical_frame_type(value.as_ref(), image_type_profile)
                .context("sanitized DICOM retained an invalid positional FrameType")?;
            if canonical != value.trim_matches([' ', '\0']) {
                bail!("sanitized DICOM retained a non-canonical positional FrameType");
            }
        }
        if element.vr() == VR::UI && semantic_uid_constant(tag) {
            let value = element.to_str()?;
            let canonical = canonical_semantic_uid(tag, value.as_ref(), depth)
                .context("sanitized DICOM retained an unsupported semantic UID")?;
            if canonical != value.trim_matches([' ', '\0']) {
                bail!("sanitized DICOM retained a non-canonical semantic UID");
            }
        }
        if tag == Tag(0x0020, 0x9056)
            && !canonical_pseudonymous_stack_id(element.to_str()?.as_ref())
        {
            bail!("sanitized DICOM retained a non-pseudonymous StackID");
        }
        if (tag == Tag(0x0010, 0x0010) || tag == Tag(0x0010, 0x0020))
            && element.to_str()?.trim_matches([' ', '\0']) != subject_id
        {
            bail!("sanitized DICOM patient identity was not pseudonymized");
        }
        if tag == Tag(0x0028, 0x0303) && element.to_str()?.trim_matches([' ', '\0']) != "REMOVED" {
            bail!("sanitized DICOM did not declare longitudinal temporal information removal");
        }
        if tag == Tag(0x0018, 0x9252)
            && (element.vr() != VR::LO
                || !matches!(element.value(), Value::Primitive(PrimitiveValue::Empty)))
        {
            bail!("sanitized DICOM retained a non-empty ASL Technique Description");
        }
        if tag == Tag(0x0018, 0x925b)
            && (element.vr() != VR::LO || element.to_str()?.trim_matches([' ', '\0']) != "REDACTED")
        {
            bail!("sanitized DICOM retained a non-canonical ASL crusher description");
        }
        if tag == Tag(0x0018, 0x925e)
            && (element.vr() != VR::LO
                || !matches!(element.value(), Value::Primitive(PrimitiveValue::Empty)))
        {
            bail!("sanitized DICOM retained a non-empty ASL bolus cut-off technique");
        }
        if let Some(items) = element.value().items() {
            *sequence_items = sequence_items
                .checked_add(items.len())
                .context("sanitized DICOM sequence-item count overflow")?;
            if *sequence_items > MAX_SEQUENCE_ITEMS {
                bail!("sanitized DICOM contains more than 100000 aggregate sequence items");
            }
            for item in items {
                audit_dataset_inner(
                    item,
                    subject_id,
                    depth + 1,
                    sequence_items,
                    image_type_profile,
                )?;
            }
        }
    }
    for creator_tag in creators.keys() {
        if !object.iter().any(|element| {
            let tag = element.tag();
            tag.group() == creator_tag.group()
                && tag.element() >= 0x1000
                && tag.element() >> 8 == creator_tag.element()
        }) {
            bail!("sanitized DICOM retained an orphan private creator");
        }
    }
    Ok(())
}

impl UidRemapper<'_> {
    fn map(&mut self, original: &str) -> Result<String> {
        let original = original.trim_matches([' ', '\0']);
        if original.is_empty()
            || original.len() > 64
            || original.starts_with('.')
            || original.ends_with('.')
            || original
                .bytes()
                .any(|byte| !byte.is_ascii_digit() && byte != b'.')
        {
            bail!("DICOM contains an invalid UID");
        }
        if let Some(mapped) = self.mapped.get(original) {
            return Ok(mapped.clone());
        }
        let digest = self.pseudonymizer.id("dicom-uid-v1", original);
        let bytes = hex::decode(digest)?;
        let mut integer = [0_u8; 16];
        integer[16 - bytes.len()..].copy_from_slice(&bytes);
        let mapped = format!("2.25.{}", u128::from_be_bytes(integer));
        self.mapped.insert(original.to_owned(), mapped.clone());
        Ok(mapped)
    }

    fn map_stack_id(&self, original: &str) -> Option<String> {
        let original = original.trim_matches([' ', '\0']);
        if original.is_empty() || original.len() > 16 || original.contains('\\') {
            return None;
        }
        Some(
            self.pseudonymizer
                .id("dicom-stack-id-v1", original)
                .chars()
                .take(16)
                .collect(),
        )
    }

    fn map_device_serial(&self, original: &str) -> Option<String> {
        let original = original.trim_matches([' ', '\0']);
        if original.is_empty() || original.len() > 64 || original.contains('\\') {
            return None;
        }
        Some(format!(
            "SN-{}",
            self.pseudonymizer.id("dicom-device-serial-v1", original)
        ))
    }
}

fn canonical_pseudonymous_stack_id(value: &str) -> bool {
    let value = value.trim_matches([' ', '\0']);
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn append_bytes<W: Write>(archive: &mut tar::Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
    let header = deterministic_tar_header(path, bytes.len() as u64)?;
    archive.append(&header, bytes)?;
    Ok(())
}

fn deterministic_tar_header(path: &str, size: u64) -> Result<tar::Header> {
    let mut header = tar::Header::new_gnu();
    header.set_path(path)?;
    header.set_size(size);
    header.set_mode(0o600);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    Ok(header)
}

fn safe_source_metadata(group: &SeriesGroup) -> SourceMetadata {
    let header = &group.representative;
    let manufacturer = if group.manufacturer_missing || group.manufacturers.is_empty() {
        None
    } else {
        let canonical = group
            .manufacturers
            .iter()
            .filter_map(|value| canonical_manufacturer(value))
            .collect::<Vec<_>>();
        (canonical.len() == group.manufacturers.len())
            .then(|| unique_value(canonical))
            .flatten()
    };
    let model = if group.model_missing || group.models.is_empty() {
        None
    } else {
        let canonical = group
            .models
            .iter()
            .filter_map(|value| canonical_model(value))
            .collect::<Vec<_>>();
        (canonical.len() == group.models.len())
            .then(|| unique_value(canonical))
            .flatten()
    };
    let software_versions =
        if group.software_versions_missing || group.software_version_values.is_empty() {
            Vec::new()
        } else {
            let mut values = group
                .software_version_values
                .iter()
                .map(|value| canonical_software_versions(value, manufacturer.as_deref()));
            let first = values.next().unwrap_or_default();
            if !first.is_empty() && values.all(|value| value == first) {
                first
            } else {
                Vec::new()
            }
        };
    SourceMetadata {
        dicom_count: group.files.len() as u64,
        manufacturer: manufacturer.clone(),
        model,
        patient_position: header.patient_position.as_deref().and_then(|value| {
            safe_enum(
                value,
                &["HFP", "HFS", "FFP", "FFS", "HFDR", "HFDL", "FFDR", "FFDL"],
            )
        }),
        software_versions,
        magnetic_field_strength: header
            .magnetic_field_strength
            .filter(|value| value.is_finite() && (0.01..=15.0).contains(value)),
        receive_coil_name: acquisition_string(header, "receive_coil_name")
            .and_then(canonical_coil_name),
        transmit_coil_name: acquisition_string(header, "transmit_coil_name")
            .and_then(canonical_coil_name),
        sequence_name: header
            .sequence_name
            .as_deref()
            .and_then(canonical_sequence_name),
        scanning_sequence: safe_code_list(
            &group.scanning_sequences,
            &["SE", "IR", "GR", "EP", "RM"],
        ),
        sequence_variant: safe_code_list(
            &group.sequence_variants,
            &["SK", "MTC", "SS", "TRSS", "SP", "MP", "OSP", "NONE"],
        ),
        scan_options: safe_code_list(
            &group.scan_options,
            &["PER", "RG", "CG", "PPG", "FC", "PFF", "PFP", "SP", "FS"],
        ),
        mr_acquisition_type: header
            .mr_acquisition_type
            .as_deref()
            .and_then(|value| safe_enum(value, &["2D", "3D"])),
        image_type: safe_code_list(
            &group.image_types,
            &[
                "ORIGINAL",
                "PRIMARY",
                "M",
                "MAGNITUDE",
                "P",
                "PHASE",
                "R",
                "REAL",
                "I",
                "IMAGINARY",
                "MIXED",
                "ND",
                "NORM",
                "MOSAIC",
                "GRID",
                "VFRAME",
                "DIS2D",
                "FMRI",
                "BOLD",
                "EPI",
                "T1",
                "T1W",
                "T2",
                "T2W",
                "T2_STAR",
                "T2STAR",
                "FLAIR",
                "DIFFUSION",
                "DWI",
                "ADC",
                "TRACEW",
                "FA",
                "DTI",
                "ASL",
                "PERFUSION",
                "FIELD_MAP",
                "FIELDMAP",
                "PHASEDIFF",
                "SBREF",
                "LOCALIZER",
                "SCOUT",
                "SURVEY",
                "REF",
                "REFERENCE",
                "DERIVED",
                "SECONDARY",
                "NONE",
            ],
        ),
        series_number: header
            .series_number
            .filter(|value| (0..=i64::from(i32::MAX)).contains(value)),
        acquisition_number: header
            .acquisition_number
            .filter(|value| (0..=i64::from(i32::MAX)).contains(value)),
    }
}

fn unique_value(values: impl IntoIterator<Item = String>) -> Option<String> {
    let mut values = values.into_iter();
    let first = values.next()?;
    values.all(|value| value == first).then_some(first)
}

fn acquisition_string<'a>(header: &'a DicomHeader, key: &str) -> Option<&'a str> {
    header.acquisition.get(key).and_then(JsonValue::as_str)
}

fn safe_enum(value: &str, allowed: &[&str]) -> Option<String> {
    let value = value.trim().to_ascii_uppercase();
    allowed.contains(&value.as_str()).then_some(value)
}

fn safe_code_list(values: &[String], allowed: &[&str]) -> Vec<String> {
    let mut output = Vec::new();
    for value in values.iter().flat_map(|value| value.split('\\')) {
        if let Some(value) = safe_enum(value, allowed) {
            if !output.contains(&value) {
                output.push(value);
            }
        }
    }
    output
}

fn protocol_group_input(group: &SeriesGroup) -> String {
    let local = group.representative.local_protocol_text();
    if !local.trim().is_empty() {
        return local;
    }
    serde_json::json!({
        "manufacturer": group.representative.manufacturer.as_deref().and_then(canonical_manufacturer),
        "model": group.representative.model.as_deref().and_then(canonical_model),
        "scanning_sequence": safe_code_list(&group.scanning_sequences, &["SE", "IR", "GR", "EP", "RM"]),
        "tr_ms": group.representative.repetition_time_ms,
        "te_ms": group.representative.echo_time_ms,
        "acquisition": safe_numeric_acquisition(&group.representative),
    })
    .to_string()
}

fn safe_numeric_acquisition(header: &DicomHeader) -> BTreeMap<String, JsonValue> {
    header
        .acquisition
        .iter()
        .filter(|(_, value)| {
            value.is_number()
                || value
                    .as_array()
                    .is_some_and(|values| values.iter().all(JsonValue::is_number))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn pass(code: &str) -> QcCheck {
    QcCheck {
        code: code.into(),
        status: QcStatus::Pass,
    }
}

pub fn metadata_policy() -> MetadataPolicy {
    MetadataPolicy {
        policy_id: DICOM_METADATA_POLICY_ID.into(),
        policy_version: DICOM_METADATA_POLICY_VERSION.into(),
    }
}

fn archive_writer_contract() -> ArchiveWriterContract {
    ArchiveWriterContract {
        name: "neuro-sync",
        version: DICOM_ARCHIVE_CONTRACT_VERSION.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClassificationDecision, ClassificationEvidence};

    #[test]
    fn archive_identity_is_stable_across_binary_patches_but_changes_with_contract() {
        let pseudonymizer =
            Pseudonymizer::from_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap();
        let audit = DeidentificationAudit {
            policy_id: DICOM_METADATA_POLICY_ID,
            policy_version: DICOM_METADATA_POLICY_VERSION,
            method: "scaling-neuro-recursive-allowlist-v2",
            recursive: true,
            private_text_removed: true,
            unknown_private_removed: true,
            uids_remapped: true,
            pixel_data_retained: true,
            defacing_performed: false,
            recognizable_visual_features: "may_be_present",
            burned_in_annotation_status: "verified_no",
            safe_private_exceptions: Vec::new(),
            metadata_transformations: Vec::new(),
        };
        let source = SourceMetadata {
            dicom_count: 1,
            manufacturer: Some("SIEMENS".into()),
            ..Default::default()
        };
        let classification = Classification {
            decision: ClassificationDecision::Accepted,
            kind: "functional_epi".into(),
            confidence: 0.95,
            evidence: vec![ClassificationEvidence {
                code: "functional_image_type".into(),
                source: "dicom_header".into(),
                effect: "supports".into(),
            }],
        };
        let instances = vec![ArchiveInstance {
            path: "dicom/000001.dcm".into(),
            size_bytes: 1024,
            sha256: "a".repeat(64),
            sop_instance_uid: "2.25.1".into(),
        }];
        let writer_contract = archive_writer_contract();
        let first = derive_series_archive_id(
            &pseudonymizer,
            "1".repeat(24).as_str(),
            "2".repeat(24).as_str(),
            "3".repeat(24).as_str(),
            "4".repeat(24).as_str(),
            "functional_epi",
            FUNCTIONAL_EPI_ARCHIVE_ROUTE,
            &writer_contract,
            &audit,
            &source,
            &classification,
            &instances,
        )
        .unwrap();
        let repeat = derive_series_archive_id(
            &pseudonymizer,
            "1".repeat(24).as_str(),
            "2".repeat(24).as_str(),
            "3".repeat(24).as_str(),
            "4".repeat(24).as_str(),
            "functional_epi",
            FUNCTIONAL_EPI_ARCHIVE_ROUTE,
            &writer_contract,
            &audit,
            &source,
            &classification,
            &instances,
        )
        .unwrap();
        // A 0.4.1 binary uses this same explicit archive contract; patch
        // provenance belongs to the outer receipt, not deterministic bytes.
        let next_patch_contract = archive_writer_contract();
        let next_patch = derive_series_archive_id(
            &pseudonymizer,
            "1".repeat(24).as_str(),
            "2".repeat(24).as_str(),
            "3".repeat(24).as_str(),
            "4".repeat(24).as_str(),
            "functional_epi",
            FUNCTIONAL_EPI_ARCHIVE_ROUTE,
            &next_patch_contract,
            &audit,
            &source,
            &classification,
            &instances,
        )
        .unwrap();
        let changed_contract = ArchiveWriterContract {
            name: "neuro-sync",
            version: "2.0.1".into(),
        };
        let changed = derive_series_archive_id(
            &pseudonymizer,
            "1".repeat(24).as_str(),
            "2".repeat(24).as_str(),
            "3".repeat(24).as_str(),
            "4".repeat(24).as_str(),
            "functional_epi",
            FUNCTIONAL_EPI_ARCHIVE_ROUTE,
            &changed_contract,
            &audit,
            &source,
            &classification,
            &instances,
        )
        .unwrap();
        assert_eq!(first, repeat);
        assert_eq!(first, next_patch);
        assert_ne!(first, changed);
    }

    #[test]
    fn archive_expansion_and_sequence_limits_accept_exact_boundaries_only() {
        let exact = DICOM_ARCHIVE_EXPANSION_FLOOR_BYTES * MAX_DICOM_ARCHIVE_EXPANSION_RATIO;
        assert!(dicom_archive_expansion_supported(exact, 1));
        assert!(!dicom_archive_expansion_supported(exact + 1, 1));

        let mut stats = SanitizationStats::default();
        reserve_sequence_items(&mut stats, MAX_SEQUENCE_ITEMS).unwrap();
        assert!(reserve_sequence_items(&mut stats, 1).is_err());
    }

    #[test]
    fn philips_numeric_scientific_bounds_are_closed_and_finite() {
        let value = |number: f32| {
            Value::<InMemDicomObject, Vec<u8>>::Primitive(PrimitiveValue::from(number))
        };
        assert!(bounded_float32_vm1(&value(-1.0e9), |v| v.abs() <= 1.0e9));
        assert!(!bounded_float32_vm1(&value(f32::INFINITY), |v| {
            v.abs() <= 1.0e9
        }));
        assert!(bounded_float32_vm1(&value(1.0e9), |v| {
            v > 0.0 && v <= 1.0e9
        }));
        assert!(!bounded_float32_vm1(&value(0.0), |v| {
            v > 0.0 && v <= 1.0e9
        }));
        assert!(bounded_float32_vm1(&value(1.0e6), |v| {
            (0.0..=1.0e6).contains(&v)
        }));
        assert!(!bounded_float32_vm1(&value(-1.0), |v| {
            (0.0..=1.0e6).contains(&v)
        }));
    }

    #[test]
    fn enhanced_frame_type_and_asl_codes_are_position_and_tag_specific() {
        assert_eq!(
            canonical_frame_type(
                "original\\primary\\fmri\\none",
                MrImageTypeProfile::Enhanced,
            )
            .as_deref(),
            Some("ORIGINAL\\PRIMARY\\FMRI\\NONE")
        );
        assert!(
            canonical_frame_type(
                "PRIMARY\\ORIGINAL\\FMRI\\NONE",
                MrImageTypeProfile::Enhanced,
            )
            .is_none()
        );
        assert!(
            canonical_frame_type(
                "ORIGINAL\\PRIMARY\\NONE\\FMRI",
                MrImageTypeProfile::Enhanced,
            )
            .is_none()
        );
        assert!(
            canonical_frame_type("ORIGINAL\\PRIMARY\\FMRI", MrImageTypeProfile::Enhanced).is_none()
        );
        assert!(
            canonical_frame_type(
                "ORIGINAL\\PRIMARY\\FMRI\\RESAMPLED",
                MrImageTypeProfile::Enhanced,
            )
            .is_none()
        );
        assert!(canonical_code_string(Tag(0x0018, 0x9255), "LABEL").is_none());
        assert_eq!(
            canonical_code_string(Tag(0x0018, 0x9257), "label").as_deref(),
            Some("LABEL")
        );
        assert_eq!(
            canonical_code_string(Tag(0x0018, 0x9259), "yes").as_deref(),
            Some("YES")
        );
        assert_eq!(
            canonical_code_string(Tag(0x0018, 0x925C), "no").as_deref(),
            Some("NO")
        );
        assert!(canonical_code_string(Tag(0x0018, 0x9257), "YES").is_none());
    }

    #[test]
    fn classic_required_acquisition_codes_accept_standard_vm_one_to_many() {
        let mut object = InMemDicomObject::new_empty();
        object.put_str(Tag(0x0018, 0x0020), VR::CS, "EP");
        object.put_str(Tag(0x0018, 0x0021), VR::CS, "SK\\SS");
        assert!(required_root_code_list(&object, Tag(0x0018, 0x0020)));
        assert!(required_root_code_list(&object, Tag(0x0018, 0x0021)));

        object.put_str(Tag(0x0018, 0x0021), VR::CS, "SK\\PATIENT_NAME");
        assert!(!required_root_code_list(&object, Tag(0x0018, 0x0021)));
    }

    #[test]
    fn image_type_canonicalization_preserves_positions_vm_and_duplicates() {
        assert_eq!(
            canonical_image_type(
                "original\\primary\\m\\ffe\\m\\ffe",
                MrImageTypeProfile::Classic,
            )
            .as_deref(),
            Some("ORIGINAL\\PRIMARY\\M\\FFE\\M\\FFE")
        );
        assert_eq!(
            canonical_image_type("derived\\secondary", MrImageTypeProfile::Classic).as_deref(),
            Some("DERIVED\\SECONDARY")
        );
        assert_eq!(
            canonical_image_type(
                "ORIGINAL\\PRIMARY\\UNKNOWN_PATIENT\\EPI",
                MrImageTypeProfile::Classic,
            )
            .as_deref(),
            Some("ORIGINAL\\PRIMARY\\OTHER\\EPI")
        );
        assert_eq!(
            classic_image_type_replacement_count(
                "ORIGINAL\\PRIMARY\\UNKNOWN_PATIENT\\OTHER\\EPI",
                "ORIGINAL\\PRIMARY\\OTHER\\OTHER\\EPI",
            ),
            1
        );
        assert!(
            canonical_image_type("PRIMARY\\ORIGINAL\\M", MrImageTypeProfile::Classic).is_none()
        );
        assert!(
            canonical_image_type(
                "ORIGINAL\\UNKNOWN_PATIENT\\M\\EPI",
                MrImageTypeProfile::Classic,
            )
            .is_none()
        );
        assert!(canonical_image_type("ORIGINAL", MrImageTypeProfile::Classic).is_none());
        assert_eq!(
            canonical_image_type(
                "original\\primary\\fmri\\none",
                MrImageTypeProfile::Enhanced,
            )
            .as_deref(),
            Some("ORIGINAL\\PRIMARY\\FMRI\\NONE")
        );
        assert!(
            canonical_image_type("ORIGINAL\\PRIMARY\\FMRI", MrImageTypeProfile::Enhanced).is_none()
        );
        assert!(
            canonical_image_type(
                "ORIGINAL\\PRIMARY\\FMRI\\NONE\\M",
                MrImageTypeProfile::Enhanced,
            )
            .is_none()
        );
    }

    #[test]
    fn enhanced_image_and_frame_type_profiles_preserve_defined_terms_and_mixed_rules() {
        for contrast in [
            "ADC",
            "FA",
            "DIFFUSION_ANISO",
            "DIFFUSION_ISO",
            "ATTNTD",
            "VELOCITY",
        ] {
            let value = format!("DERIVED\\PRIMARY\\DIFFUSION\\{contrast}");
            assert_eq!(
                canonical_image_type(&value, MrImageTypeProfile::Enhanced).as_deref(),
                Some(value.as_str())
            );
            assert_eq!(
                canonical_frame_type(&value, MrImageTypeProfile::Enhanced).as_deref(),
                Some(value.as_str())
            );
        }
        for flavor in ["T1", "T2", "VELOCITY"] {
            let value = format!("ORIGINAL\\PRIMARY\\{flavor}\\NONE");
            assert_eq!(
                canonical_image_type(&value, MrImageTypeProfile::Enhanced).as_deref(),
                Some(value.as_str())
            );
        }

        assert_eq!(
            canonical_image_type(
                "MIXED\\PRIMARY\\VOLUME\\MIXED",
                MrImageTypeProfile::Enhanced,
            )
            .as_deref(),
            Some("MIXED\\PRIMARY\\VOLUME\\MIXED")
        );
        assert!(
            canonical_frame_type("MIXED\\PRIMARY\\VOLUME\\NONE", MrImageTypeProfile::Enhanced,)
                .is_none()
        );
        assert!(
            canonical_frame_type(
                "DERIVED\\PRIMARY\\VOLUME\\MIXED",
                MrImageTypeProfile::Enhanced,
            )
            .is_none()
        );

        let legacy_empty = "DERIVED\\PRIMARY\\DIFFUSION\\";
        assert_eq!(
            canonical_image_type(legacy_empty, MrImageTypeProfile::LegacyConvertedEnhanced)
                .as_deref(),
            Some(legacy_empty)
        );
        assert_eq!(
            canonical_frame_type(legacy_empty, MrImageTypeProfile::LegacyConvertedEnhanced)
                .as_deref(),
            Some(legacy_empty)
        );
        assert!(canonical_image_type(legacy_empty, MrImageTypeProfile::Enhanced).is_none());
        assert!(canonical_frame_type(legacy_empty, MrImageTypeProfile::Enhanced).is_none());
    }

    #[test]
    fn siemens_csa_ignores_empty_optional_diffusion_placeholders() {
        fn append_tag(
            output: &mut Vec<u8>,
            name: &str,
            vr: [u8; 4],
            declared_vm: i32,
            values: &[&str],
        ) {
            let mut name_bytes = [0_u8; 64];
            name_bytes[..name.len()].copy_from_slice(name.as_bytes());
            output.extend_from_slice(&name_bytes);
            output.extend_from_slice(&declared_vm.to_le_bytes());
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
        source.extend_from_slice(&4_u32.to_le_bytes());
        source.extend_from_slice(&77_u32.to_le_bytes());
        append_tag(
            &mut source,
            "NumberOfImagesInMosaic",
            [b'U', b'S', 0, 0],
            1,
            &["51"],
        );
        append_tag(&mut source, "B_value", [b'I', b'S', 0, 0], 1, &[]);
        append_tag(
            &mut source,
            "DiffusionGradientDirection",
            [b'F', b'D', 0, 0],
            3,
            &[],
        );
        append_tag(&mut source, "B_matrix", [b'F', b'D', 0, 0], 6, &[]);

        let parsed = parse_siemens_csa_numeric_fields(&source).unwrap();
        assert_eq!(parsed.get("NumberOfImagesInMosaic").unwrap(), &["51"]);
        assert!(!parsed.contains_key("B_value"));
        assert!(!parsed.contains_key("DiffusionGradientDirection"));
        assert!(!parsed.contains_key("B_matrix"));
        assert!(sanitize_siemens_csa_image_header(&source).is_some());
    }

    #[test]
    fn enhanced_dimension_and_concatenation_tags_require_their_standard_vrs() {
        for (element, vr) in [
            (0x0242, VR::UI),
            (0x9056, VR::SH),
            (0x9057, VR::UL),
            (0x9153, VR::FD),
            (0x9157, VR::UL),
            (0x9161, VR::UI),
            (0x9162, VR::US),
            (0x9163, VR::US),
            (0x9164, VR::UI),
            (0x9165, VR::AT),
            (0x9167, VR::AT),
            (0x9221, VR::SQ),
            (0x9222, VR::SQ),
            (0x9228, VR::UL),
        ] {
            assert!(geometry_attribute(element, vr));
            assert!(!geometry_attribute(element, VR::OB));
        }
    }

    #[test]
    fn scanner_identity_is_vendor_neutral_but_rejects_identity_and_path_text() {
        assert_eq!(
            canonical_manufacturer("UNITEDIMAGING"),
            Some("United Imaging".into())
        );
        assert_eq!(canonical_manufacturer("UIH"), Some("United Imaging".into()));
        assert_eq!(
            canonical_manufacturer("Future Scanner Works"),
            Some("Future Scanner Works".into())
        );
        assert_eq!(
            canonical_model("FutureMR Research 9000"),
            Some("FutureMR Research 9000".into())
        );
        assert!(canonical_model("/home/paul/scanner").is_none());
        assert!(canonical_model("C:\\Users\\paul\\scanner").is_none());
        assert!(canonical_model("https://scanner.invalid/model").is_none());
        assert!(canonical_model("MRN1234567 PATIENT").is_none());
        assert!(canonical_model("Participant Scanner").is_none());
        assert!(canonical_model("Scanner 1234567").is_none());
    }
}
