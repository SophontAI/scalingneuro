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
    CLIENT_VERSION,
    dicom::{
        DicomHeader, MAX_DICOM_INSTANCES_PER_SERIES, MAX_DICOM_SERIES_UNCOMPRESSED_BYTES,
        SeriesGroup, dicom_instance_size_supported, dicom_series_uncompressed_size_supported,
    },
    model::{
        Classification, ManifestArchiveObject, ManifestBundle, ManifestObject, MetadataPolicy,
        QcCheck, QcResult, QcStatus, SourceMetadata,
    },
    pseudonym::Pseudonymizer,
};

pub const DICOM_ARCHIVE_FORMAT: &str = "dicom-tar-zstd";
pub const DICOM_MANIFEST_SCHEMA_VERSION: &str = "1.0.0";
pub const DICOM_METADATA_POLICY_ID: &str = "scaling-neuro.dicom-deidentification";
pub const DICOM_METADATA_POLICY_VERSION: &str = "1.0.0";
const DICOM_IMPLEMENTATION_CLASS_UID: &str = "2.25.323468694959424494117938985101850441847";
const DICOM_IMPLEMENTATION_VERSION_NAME: &str = "NEUROSYNC_RAW_1";
const MAX_SEQUENCE_DEPTH: usize = 32;
const MAX_SEQUENCE_ITEMS: usize = 100_000;
const DICOM_ARCHIVE_EXPANSION_FLOOR_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DICOM_ARCHIVE_EXPANSION_RATIO: u64 = 20;

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
    dicom_instance_count: u64,
    client: ArchiveClient,
    deidentification: DeidentificationAudit,
    source: SourceMetadata,
    classification: Classification,
    instances: Vec<ArchiveInstance>,
}

#[derive(Debug, Clone, Serialize)]
struct ArchiveClient {
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
    dicom_instance_count: u64,
    client: &'a ArchiveClient,
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
    siemens_csa_headers_rewritten: u64,
    philips_ps315_scaling_attributes_retained: u64,
    philips_ps315_number_of_slices_retained: u64,
    philips_ps315_water_fat_shift_retained: u64,
    philips_ps315_per_frame_scale_sequences_rebuilt: u64,
    philips_redundant_trigger_times_suppressed: u64,
    current_sequence_items: usize,
}

pub fn create_dicom_archive<F>(mut request: ArchiveRequest<'_, F>) -> Result<ManifestBundle>
where
    F: FnMut(u64),
{
    let group = request.group;
    if request.classification.decision != crate::model::ClassificationDecision::Accepted {
        bail!("only an accepted functional EPI series can be archived");
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
        bail!("dicom_instance_exceeds_256_mib");
    }
    if !dicom_series_uncompressed_size_supported(source_sizes) {
        bail!("series_exceeds_64_gib_uncompressed_dicom_limit");
    }
    if group
        .burned_in_annotations
        .iter()
        .any(|value| !value.eq_ignore_ascii_case("NO"))
    {
        bail!("functional DICOM series declared possible burned-in annotation");
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
    let suppress_redundant_philips_trigger = group.philips_dynamic_timing_contract_verified;

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
            bail!("dicom_instance_exceeds_256_mib");
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

    let classification = Classification {
        kind: "functional_epi".into(),
        ..request.classification
    };
    let client = ArchiveClient {
        name: "neuro-sync",
        version: CLIENT_VERSION.into(),
    };
    let deidentification = DeidentificationAudit {
        policy_id: DICOM_METADATA_POLICY_ID,
        policy_version: DICOM_METADATA_POLICY_VERSION,
        method: "scaling-neuro-recursive-allowlist-v1",
        recursive: true,
        private_text_removed: true,
        unknown_private_removed: true,
        uids_remapped: true,
        pixel_data_retained: true,
        burned_in_annotation_status: if group.burned_in_annotation_missing {
            "not_declared"
        } else {
            "verified_no"
        },
        safe_private_exceptions: [
            (stats.siemens_csa_headers_rewritten > 0)
                .then_some("siemens_csa_image_header_numeric_v1"),
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
        metadata_transformations: (stats.philips_redundant_trigger_times_suppressed > 0)
            .then_some("suppressed_redundant_philips_dynamic_trigger_time")
            .into_iter()
            .collect(),
    };
    let source = safe_source_metadata(group);
    let series_archive_id = derive_series_archive_id(
        request.pseudonymizer,
        &series_id,
        &subject_id,
        &session_id,
        &protocol_group_id,
        &client,
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
        modality: "functional_epi",
        dicom_instance_count: instances.len() as u64,
        client,
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
        nifti: None,
        metadata: None,
        archive: Some(ManifestArchiveObject {
            object: ManifestObject {
                relative_key: format!("{series_archive_id}/dicom.tar.zst"),
                local_path: archive_path.to_string_lossy().into_owned(),
                size: archive_size,
                sha256: archive_sha256,
                uncompressed_sha256: None,
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
                pass("functional_epi_header_gate"),
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
                (stats.siemens_csa_headers_rewritten > 0).then(|| {
                    format!(
                        "rewritten_numeric_siemens_csa_image_headers:{}",
                        stats.siemens_csa_headers_rewritten
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
    client: &ArchiveClient,
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
        modality: "functional_epi",
        dicom_instance_count: instances.len() as u64,
        client,
        deidentification,
        source,
        classification,
        instances,
    };
    let mut digest = Sha256::new();
    digest.update(b"scaling-neuro-dicom-series-archive-identity-v2\0");
    digest.update(serde_json::to_vec(&preimage)?);
    Ok(pseudonymizer.id("dicom-series-archive-v2", &hex::encode(digest.finalize())))
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
        .read_until(Tag(0x7fe0, 0x0010))
        .open_file(source_snapshot.path())
        .with_context(|| format!("could not read selected DICOM: {}", source_path.display()))?;
    if contains_overlay_or_graphics(&object, 0) {
        bail!("DICOM contains overlay or graphic data and was held locally");
    }
    let burned_in = object
        .element(Tag(0x0028, 0x0301))
        .ok()
        .and_then(|element| element.to_str().ok())
        .map(|value| value.trim_matches([' ', '\0']).to_owned());
    match burned_in.as_deref() {
        Some(value) if value.eq_ignore_ascii_case("NO") => {}
        Some(_) => bail!("DICOM declared possible burned-in annotation"),
        None if declares_original_primary(&object) => {}
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
    stats.current_sequence_items = 0;
    let sanitized = sanitize_dataset(
        object.into_inner(),
        remapper,
        stats,
        0,
        None,
        suppress_redundant_philips_trigger,
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
) -> Result<()> {
    let object = OpenFileOptions::new()
        .read_until(Tag(0x7fe0, 0x0010))
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
    audit_dataset(&object.into_inner(), subject_id, 0)?;
    let final_pixel = locate_pixel_data(path, expected_transfer_syntax, meta_group_length)?;
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
                });
            }
            _ => {}
        }
    }
    bail!("DICOM has no readable PixelData element")
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

fn sanitize_dataset(
    source: InMemDicomObject,
    remapper: &mut UidRemapper<'_>,
    stats: &mut SanitizationStats,
    depth: usize,
    inherited_manufacturer: Option<&str>,
    suppress_redundant_philips_trigger: bool,
) -> Result<InMemDicomObject> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("DICOM sequence nesting exceeds the privacy processor limit");
    }
    let manufacturer = source
        .element(Tag(0x0008, 0x0070))
        .ok()
        .and_then(|element| element.to_str().ok())
        .and_then(|value| canonical_manufacturer(value.as_ref()))
        .or_else(|| inherited_manufacturer.map(str::to_owned));
    let private_creators = private_creators(&source);
    let mut retained_private_creators = HashSet::new();
    let mut output = InMemDicomObject::new_empty();
    for element in source {
        let tag = element.tag();
        let vr = element.vr();
        if tag == Tag(0x0018, 0x1060) && suppress_redundant_philips_trigger {
            stats.philips_redundant_trigger_times_suppressed += 1;
            continue;
        }
        if tag.group() % 2 == 1 {
            let creator_tag = Tag(tag.group(), tag.element() >> 8);
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
        if is_date_or_time_vr(vr) || !public_attribute_allowed(tag, vr) {
            continue;
        }
        let (header, value) = element.into_parts();
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
                            manufacturer.as_deref(),
                            suppress_redundant_philips_trigger,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                Value::Sequence(DataSetSequence::new(items, Length::UNDEFINED))
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
            Value::Primitive(value) => {
                let Some(value) =
                    sanitize_public_primitive(tag, vr, value, manufacturer.as_deref())
                else {
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
    const MAX_CSA_BYTES: usize = 16 * 1024 * 1024;
    const MAX_CSA_ITEMS: usize = 4096;
    const FIELDS: &[(&str, [u8; 4])] = &[
        ("NumberOfImagesInMosaic", [b'U', b'S', 0, 0]),
        ("SliceNormalVector", [b'D', b'S', 0, 0]),
        ("SliceMeasurementDuration", [b'D', b'S', 0, 0]),
        ("BandwidthPerPixelPhaseEncode", [b'D', b'S', 0, 0]),
        ("MosaicRefAcqTimes", [b'D', b'S', 0, 0]),
        ("ProtocolSliceNumber", [b'I', b'S', 0, 0]),
        ("PhaseEncodingDirectionPositive", [b'I', b'S', 0, 0]),
    ];
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
        if item_count > MAX_CSA_ITEMS {
            return None;
        }
        let keep = FIELDS.iter().any(|(allowed, _)| name == *allowed);
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
        if keep && declared_vm > 0 && values.len() != declared_vm as usize {
            return None;
        }
        if keep && retained.insert(name.to_owned(), values).is_some() {
            return None;
        }
    }
    if cursor > source.len() {
        return None;
    }
    validate_csa_values(&mut retained)?;
    let retained_fields = FIELDS
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
    (output.len() <= MAX_CSA_BYTES).then_some(output)
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
        "Philips MR Imaging DD 001",
        "Philips MR Imaging DD 005",
        "Philips Imaging DD 001",
    ]
    .into_iter()
    .find(|known| value.eq_ignore_ascii_case(known))
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
) -> Option<PrimitiveValue> {
    let canonical_text = if tag == Tag(0x0008, 0x0070) {
        canonical_manufacturer(value.to_str().as_ref())
    } else if tag == Tag(0x0008, 0x1090) {
        canonical_model(value.to_str().as_ref())
    } else if tag == Tag(0x0018, 0x0024) {
        canonical_sequence_name(value.to_str().as_ref())
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
            | Tag(0x0018, 0x0024)
            | Tag(0x0018, 0x1020)
            | Tag(0x0018, 0x1250)
            | Tag(0x0018, 0x1251)
    ) {
        return canonical_text.map(PrimitiveValue::from);
    }

    match vr {
        VR::DS => canonical_numeric_text(value.to_str().as_ref(), false).map(PrimitiveValue::from),
        VR::IS => canonical_numeric_text(value.to_str().as_ref(), true).map(PrimitiveValue::from),
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
        Tag(0x0008, 0x0008) => &[
            "ORIGINAL",
            "DERIVED",
            "PRIMARY",
            "SECONDARY",
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
            "DIS2D",
            "FMRI",
            "BOLD",
            "EPI",
            "NONE",
        ],
        Tag(0x0008, 0x0060) => &["MR"],
        Tag(0x0008, 0x9205) => &["COLOR", "MONOCHROME", "MIXED"],
        Tag(0x0008, 0x9206) => &["VOLUME", "SAMPLED", "DISTORTED", "MIXED"],
        Tag(0x0008, 0x9207) => &[
            "NONE",
            "RECON_TOMOGRAPHIC",
            "RECON_PROJECTION",
            "RECON_PLANAR",
        ],
        Tag(0x0008, 0x9208) => &["MAGNITUDE", "PHASE", "REAL", "IMAGINARY", "MIXED"],
        Tag(0x0008, 0x9209) => &[
            "UNKNOWN",
            "NONE",
            "T1",
            "T2",
            "T2_STAR",
            "PROTON_DENSITY",
            "DIFFUSION",
            "FLOW_ENCODED",
            "FLUID_ATTENUATED",
            "PERFUSION",
        ],
        Tag(0x0018, 0x0020) => &["SE", "IR", "GR", "EP", "RM"],
        Tag(0x0018, 0x0021) => &["SK", "MTC", "SS", "TRSS", "SP", "MP", "OSP", "NONE"],
        Tag(0x0018, 0x0022) => &["PER", "RG", "CG", "PPG", "FC", "PFF", "PFP", "SP", "FS"],
        Tag(0x0018, 0x0023) => &["2D", "3D"],
        Tag(0x0018, 0x0025) => &["Y", "N"],
        Tag(0x0018, 0x1312) => &["ROW", "COL"],
        Tag(0x0018, 0x5100) => &["HFP", "HFS", "HFDR", "HFDL", "FFDR", "FFDL", "FFP", "FFS"],
        Tag(0x0018, 0x9036) => &["PHASE", "FREQUENCY", "SLICE", "COMBINATION"],
        Tag(0x0018, 0x9018) => &["YES", "NO"],
        Tag(0x0018, 0x9034) => &["LINEAR", "REVERSE_LINEAR", "CENTRIC", "REVERSE_CENTRIC"],
        Tag(0x0018, 0x9078) => &["SENSE", "GRAPPA", "ASSET", "SMASH", "OTHER", "NONE"],
        Tag(0x0028, 0x0004) => &[
            "MONOCHROME1",
            "MONOCHROME2",
            "PALETTE COLOR",
            "RGB",
            "YBR_FULL",
            "YBR_FULL_422",
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
    let mut output = Vec::new();
    for part in value.split('\\') {
        let part = part.trim().to_ascii_uppercase();
        if allowed.contains(&part.as_str()) && !output.contains(&part) {
            output.push(part);
        }
    }
    (!output.is_empty()).then(|| output.join("\\"))
}

fn canonical_manufacturer(value: &str) -> Option<String> {
    let upper = value
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
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
    } else if upper.contains("UNITED IMAGING") {
        Some("United Imaging".into())
    } else if upper.contains("BRUKER") {
        Some("Bruker".into())
    } else {
        None
    }
}

fn canonical_model(value: &str) -> Option<String> {
    let value = value.trim();
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
        .find(|model| model.eq_ignore_ascii_case(value))
        .map(|model| (*model).to_owned())
}

fn canonical_software_versions(value: &str, manufacturer: Option<&str>) -> Vec<String> {
    let Some(manufacturer) = manufacturer else {
        return Vec::new();
    };
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
    output
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
    } else {
        None
    }
}

fn canonical_coil_name(value: &str) -> Option<String> {
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
                || (tag.element() >= 0x9000 && enhanced_acquisition_vr(vr))
        }
        0x0020 => geometry_attribute(tag.element()),
        0x0028 => pixel_attribute(tag.element(), vr),
        0x0040 => matches!(tag.element(), 0x9094 | 0x9210 | 0x9211 | 0x9212 | 0x9216),
        0x2050 => tag.element() == 0x0020,
        0x5200 => matches!(tag.element(), 0x9229 | 0x9230),
        0x7fe0 => tag.element() == 0x0010,
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
    )
}

fn geometry_attribute(element: u16) -> bool {
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
            | 0x9056
            | 0x9057
            | 0x9111
            | 0x9113
            | 0x9116
            | 0x9128
            | 0x9156
            | 0x9157
            | 0x9161
            | 0x9164
            | 0x9165
            | 0x9221
            | 0x9222
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
            | 0x1050..=0x1055
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
    let valid_uid = |value: &str| {
        value.len() <= 64
            && !value.starts_with('.')
            && !value.ends_with('.')
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
    };
    let supported_top_level_mr = |value: &str| {
        matches!(
            value,
            "1.2.840.10008.5.1.4.1.1.4"
                | "1.2.840.10008.5.1.4.1.1.4.1"
                | "1.2.840.10008.5.1.4.1.1.4.4"
        )
    };
    let allowed = values.iter().all(|value| {
        valid_uid(value)
            && if tag == Tag(0x0008, 0x0016) && depth == 0 {
                supported_top_level_mr(value)
            } else {
                value.starts_with("1.2.840.10008.")
            }
    });
    allowed.then(|| values.join("\\"))
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

fn declares_original_primary(object: &InMemDicomObject) -> bool {
    let values = object
        .element(Tag(0x0008, 0x0008))
        .ok()
        .and_then(|element| element.to_multi_str().ok())
        .unwrap_or_default();
    let has = |expected: &str| {
        values
            .iter()
            .any(|value| value.trim().eq_ignore_ascii_case(expected))
    };
    has("ORIGINAL") && has("PRIMARY") && !has("DERIVED") && !has("SECONDARY")
}

fn audit_dataset(object: &InMemDicomObject, subject_id: &str, depth: usize) -> Result<()> {
    let mut sequence_items = 0_usize;
    audit_dataset_inner(object, subject_id, depth, &mut sequence_items)
}

fn audit_dataset_inner(
    object: &InMemDicomObject,
    subject_id: &str,
    depth: usize,
    sequence_items: &mut usize,
) -> Result<()> {
    if depth > MAX_SEQUENCE_DEPTH {
        bail!("sanitized DICOM exceeded sequence-depth policy");
    }
    let creators = private_creators(object);
    for element in object.iter() {
        let tag = element.tag();
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
                if !canonical_siemens_csa
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
        if element.vr() == VR::UI && semantic_uid_constant(tag) {
            let value = element.to_str()?;
            let canonical = canonical_semantic_uid(tag, value.as_ref(), depth)
                .context("sanitized DICOM retained an unsupported semantic UID")?;
            if canonical != value.trim_matches([' ', '\0']) {
                bail!("sanitized DICOM retained a non-canonical semantic UID");
            }
        }
        if (tag == Tag(0x0010, 0x0010) || tag == Tag(0x0010, 0x0020))
            && element.to_str()?.trim_matches([' ', '\0']) != subject_id
        {
            bail!("sanitized DICOM patient identity was not pseudonymized");
        }
        if tag == Tag(0x0028, 0x0303) && element.to_str()?.trim_matches([' ', '\0']) != "REMOVED" {
            bail!("sanitized DICOM did not declare longitudinal temporal information removal");
        }
        if let Some(items) = element.value().items() {
            *sequence_items = sequence_items
                .checked_add(items.len())
                .context("sanitized DICOM sequence-item count overflow")?;
            if *sequence_items > MAX_SEQUENCE_ITEMS {
                bail!("sanitized DICOM contains more than 100000 aggregate sequence items");
            }
            for item in items {
                audit_dataset_inner(item, subject_id, depth + 1, sequence_items)?;
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
                "DIS2D",
                "FMRI",
                "BOLD",
                "EPI",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClassificationDecision, ClassificationEvidence};

    #[test]
    fn archive_identity_covers_client_and_manifest_fields() {
        let pseudonymizer =
            Pseudonymizer::from_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap();
        let audit = DeidentificationAudit {
            policy_id: DICOM_METADATA_POLICY_ID,
            policy_version: DICOM_METADATA_POLICY_VERSION,
            method: "scaling-neuro-recursive-allowlist-v1",
            recursive: true,
            private_text_removed: true,
            unknown_private_removed: true,
            uids_remapped: true,
            pixel_data_retained: true,
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
        let client = ArchiveClient {
            name: "neuro-sync",
            version: "0.3.1".into(),
        };
        let first = derive_series_archive_id(
            &pseudonymizer,
            "1".repeat(24).as_str(),
            "2".repeat(24).as_str(),
            "3".repeat(24).as_str(),
            "4".repeat(24).as_str(),
            &client,
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
            &client,
            &audit,
            &source,
            &classification,
            &instances,
        )
        .unwrap();
        let changed_client = ArchiveClient {
            name: "neuro-sync",
            version: "0.3.2".into(),
        };
        let changed = derive_series_archive_id(
            &pseudonymizer,
            "1".repeat(24).as_str(),
            "2".repeat(24).as_str(),
            "3".repeat(24).as_str(),
            "4".repeat(24).as_str(),
            &changed_client,
            &audit,
            &source,
            &classification,
            &instances,
        )
        .unwrap();
        assert_eq!(first, repeat);
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
}
