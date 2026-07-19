use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
use flate2::{Compression, GzBuilder};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    SIDECAR_SCHEMA_VERSION,
    classify::ConversionSignals,
    convert::{ConvertedImage, ConvertedSeries},
    dicom::SeriesGroup,
    model::{
        BundleFiles, Classification, FileDigest, ImageMetadata, ManifestBundle, ManifestObject,
        MetadataPolicy, QcCheck, QcResult, QcStatus, ScanSidecar, SourceMetadata,
    },
    pseudonym::Pseudonymizer,
};

pub const METADATA_POLICY_ID: &str = "scaling-neuro-epi-default-deny";
pub const METADATA_POLICY_VERSION: &str = "1.1.0";

#[derive(Debug, Clone)]
pub struct AnalyzedImage {
    pub info: NiftiInfo,
    pub uncompressed_sha256: String,
    pub signals: ConversionSignals,
    pub qc: QcResult,
}

#[derive(Debug, Clone)]
pub struct NiftiInfo {
    pub dimensions: Vec<u64>,
    pub voxel_size_mm: Vec<f64>,
    pub datatype_code: i16,
    pub datatype: String,
    pub bits_per_voxel: u16,
    pub affine: [[f64; 4]; 4],
    pub orientation: String,
    pub volume_count: u64,
    pub tr_seconds: Option<f64>,
    pub voxel_offset: u64,
    pub expected_size: u64,
    pub actual_size: u64,
    pub geometry_transform_present: bool,
    pub spatial_units_mm: bool,
    pub temporal_units_seconds: bool,
    pub scale_slope: f64,
    pub scale_intercept: f64,
    endian: Endian,
}

#[derive(Debug, Clone, Copy)]
enum Endian {
    Little,
    Big,
}

pub fn analyze_converted(
    group: &SeriesGroup,
    image: &ConvertedImage,
    converted_name_count: usize,
) -> Result<AnalyzedImage> {
    scrub_nifti_text_header(&image.nifti_path)?;
    let info = read_nifti_info(&image.nifti_path)?;
    let (uncompressed_sha256, signal) = inspect_signal_and_hash(&image.nifti_path, &info)?;
    let metadata = &image.metadata;

    let tr_seconds = json_float(metadata, "RepetitionTime").or(info.tr_seconds);
    let echo_time_seconds = json_float(metadata, "EchoTime").or_else(|| {
        group
            .representative
            .echo_time_ms
            .map(|value| value / 1_000.0)
    });
    let local_evidence = [
        json_string(metadata, "SequenceName"),
        json_string(metadata, "PulseSequenceDetails"),
        json_string(metadata, "SeriesDescription"),
        json_string(metadata, "ProtocolName"),
        json_string(metadata, "ScanningSequence"),
        json_string(metadata, "ImageType"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();
    let header_supports_epi = group
        .representative
        .scanning_sequence
        .iter()
        .any(|v| v.eq_ignore_ascii_case("EP"))
        || group
            .representative
            .image_type
            .iter()
            .any(|v| contains_any(&v.to_ascii_lowercase(), &["epi", "bold", "fmri"]))
        || group
            .representative
            .sequence_name
            .as_deref()
            .is_some_and(|v| {
                contains_any(&v.to_ascii_lowercase(), &["epi", "ep2d", "epfid", "bold"])
            });
    let functional_epi_evidence = header_supports_epi
        || contains_any(&local_evidence, &["epi", "ep2d", "epfid", "bold", "fmri"]);
    let bids_has_diffusion = image.nifti_path.with_extension("bval").exists()
        || image.nifti_path.with_extension("bvec").exists()
        || ["DiffusionBValue", "DiffusionGradientDirection", "BValue"]
            .iter()
            .any(|key| metadata.get(*key).is_some());
    let bids_has_asl = [
        "ArterialSpinLabelingType",
        "PostLabelingDelay",
        "LabelingDuration",
        "BackgroundSuppression",
    ]
    .iter()
    .any(|key| metadata.get(*key).is_some());

    let mut checks = vec![
        check("nifti_header_valid", true),
        check("nifti_extensions_absent", true),
        check(
            "native_geometry_present",
            info.geometry_transform_present && info.orientation != "unknown",
        ),
        check("spatial_units_millimeters", info.spatial_units_mm),
        check("temporal_units_time", info.temporal_units_seconds),
        check(
            "intensity_scaling_finite",
            info.scale_slope.is_finite() && info.scale_intercept.is_finite(),
        ),
        check(
            "spatial_dimensions_plausible",
            spatial_dimensions_valid(&info.dimensions),
        ),
        check("voxel_sizes_valid", voxel_sizes_valid(&info.voxel_size_mm)),
        check("affine_finite", affine_finite(&info.affine)),
        check("affine_nondegenerate", affine_nondegenerate(&info.affine)),
        check(
            "datatype_bit_depth_consistent",
            datatype_bit_depth(info.datatype_code) == Some(info.bits_per_voxel),
        ),
        check(
            "file_size_consistent",
            info.actual_size == info.expected_size,
        ),
        check("signal_nonconstant", signal.nonconstant),
        check("signal_finite", signal.finite),
        check("metadata_default_deny_policy", true),
    ];
    let time_series = info.dimensions.len() >= 4 && info.volume_count >= 10;
    checks.push(check("functional_time_series", time_series));
    checks.push(check(
        "repetition_time_valid",
        tr_seconds.is_some_and(|tr| (0.1..=20.0).contains(&tr)),
    ));
    checks.push(check(
        "echo_time_valid",
        echo_time_seconds.is_some_and(|te| 0.0 < te && te <= 2.0),
    ));
    checks.push(check(
        "dicom_volume_count_consistent",
        group
            .representative
            .number_of_temporal_positions
            .is_none_or(|count| count > 0 && count as u64 == info.volume_count),
    ));
    let passed = checks
        .iter()
        .all(|item| !matches!(item.status, QcStatus::Fail));
    let mut warnings = Vec::new();
    if group.representative.patient_id.is_none() {
        warnings.push("subject_linkage_uses_session_fallback".into());
    }
    Ok(AnalyzedImage {
        signals: ConversionSignals {
            dimensions: info.dimensions.clone(),
            volume_count: info.volume_count,
            repetition_time_seconds: tr_seconds,
            echo_time_seconds,
            bids_has_diffusion,
            bids_has_asl,
            functional_epi_evidence,
            converted_name_count,
        },
        uncompressed_sha256,
        info,
        qc: QcResult {
            passed,
            checks,
            warnings,
        },
    })
}

pub struct BundleRequest<'a> {
    pub group: &'a SeriesGroup,
    pub converted: &'a ConvertedSeries,
    pub image: &'a ConvertedImage,
    pub analyzed: &'a AnalyzedImage,
    pub classification: Classification,
    pub pseudonymizer: &'a Pseudonymizer,
    pub bundle_root: &'a Path,
    pub echo_label: Option<&'a str>,
}

pub fn create_bundle(request: BundleRequest<'_>) -> Result<ManifestBundle> {
    let BundleRequest {
        group,
        converted,
        image,
        analyzed,
        classification,
        pseudonymizer,
        bundle_root,
        echo_label,
    } = request;
    if !analyzed.qc.passed {
        bail!("series failed local NIfTI quality control");
    }
    let study_uid = &group.study_uid;
    let series_uid = &group.series_uid;
    let subject_id = match group.representative.patient_id.as_deref() {
        Some(patient_id) => pseudonymizer.subject_id(
            patient_id,
            group.representative.issuer_of_patient_id.as_deref(),
        ),
        None => pseudonymizer.id("subject-session-fallback", study_uid),
    };
    let session_id = pseudonymizer.id("session", study_uid);
    let series_id = pseudonymizer.id("series", series_uid);
    let protocol_group_id = pseudonymizer.protocol_group_id(&protocol_group_input(
        group,
        &image.metadata,
        &analyzed.info,
    ));
    let bundle_id = pseudonymizer.id(
        "bundle",
        &format!(
            "{series_uid}\0{}\0{}",
            analyzed.uncompressed_sha256,
            echo_label.unwrap_or("single")
        ),
    );
    let echo_suffix = echo_label
        .map(|label| format!("_echo-{label}"))
        .unwrap_or_default();
    let basename = format!("sub-{subject_id}_ses-{session_id}_ser-{series_id}{echo_suffix}_bold");
    let directory = bundle_root.join(&bundle_id);
    fs::create_dir_all(&directory)?;
    let nifti_filename = format!("{basename}.nii.gz");
    let nifti_path = directory.join(&nifti_filename);
    let compressed_sha256 = deterministic_gzip(&image.nifti_path, &nifti_path)?;
    let nifti_size = fs::metadata(&nifti_path)?.len();

    let source = build_source_metadata(group, &image.metadata);
    let image_metadata =
        build_image_metadata(group, &image.metadata, &analyzed.info, echo_label.is_none());
    let sidecar = ScanSidecar {
        schema_version: SIDECAR_SCHEMA_VERSION.into(),
        bundle_id: bundle_id.clone(),
        subject_id: subject_id.clone(),
        session_id: session_id.clone(),
        series_id: series_id.clone(),
        protocol_group_id: protocol_group_id.clone(),
        modality: "bold".into(),
        source,
        image: image_metadata,
        files: BundleFiles {
            nifti: FileDigest {
                filename: nifti_filename.clone(),
                size_bytes: nifti_size,
                sha256: compressed_sha256.clone(),
                uncompressed_sha256: Some(analyzed.uncompressed_sha256.clone()),
            },
        },
        metadata_policy: MetadataPolicy {
            policy_id: METADATA_POLICY_ID.into(),
            policy_version: METADATA_POLICY_VERSION.into(),
        },
        conversion: converted.provenance.clone(),
        classification: classification.clone(),
        qc: analyzed.qc.clone(),
    };
    let metadata_filename = format!("{basename}.json");
    let metadata_path = directory.join(&metadata_filename);
    write_json_atomic(&metadata_path, &sidecar)?;
    let metadata_size = fs::metadata(&metadata_path)?.len();
    let metadata_sha256 = sha256_file(&metadata_path)?;

    Ok(ManifestBundle {
        bundle_id: bundle_id.clone(),
        series_id,
        subject_id,
        session_id,
        protocol_group_id,
        nifti: Some(ManifestObject {
            relative_key: format!("{bundle_id}/{nifti_filename}"),
            local_path: nifti_path.to_string_lossy().into_owned(),
            size: nifti_size,
            sha256: compressed_sha256,
            uncompressed_sha256: Some(analyzed.uncompressed_sha256.clone()),
        }),
        metadata: Some(ManifestObject {
            relative_key: format!("{bundle_id}/{metadata_filename}"),
            local_path: metadata_path.to_string_lossy().into_owned(),
            size: metadata_size,
            sha256: metadata_sha256,
            uncompressed_sha256: None,
        }),
        archive: None,
        source_dicom_count: group.files.len() as u64,
        classification,
        qc: analyzed.qc.clone(),
    })
}

fn build_source_metadata(group: &SeriesGroup, json: &Value) -> SourceMetadata {
    let header = &group.representative;
    let manufacturer = header
        .manufacturer
        .as_deref()
        .and_then(canonical_manufacturer)
        .or_else(|| {
            json_string(json, "Manufacturer")
                .as_deref()
                .and_then(canonical_manufacturer)
        });
    let vendor = manufacturer.as_deref().and_then(ScannerVendor::from_name);
    let mut software_versions = BTreeSet::new();
    if let Some(value) = header.software_versions.as_deref() {
        for part in value.split(['\\', ',']) {
            if let Some(value) = canonical_software_version(vendor, part) {
                software_versions.insert(value);
            }
        }
    }
    for value in json_strings(json, "SoftwareVersions") {
        if let Some(value) = canonical_software_version(vendor, &value) {
            software_versions.insert(value);
        }
    }
    SourceMetadata {
        dicom_count: group.files.len() as u64,
        manufacturer,
        model: header
            .model
            .as_deref()
            .and_then(|value| canonical_scanner_model(vendor, value))
            .or_else(|| {
                json_string(json, "ManufacturersModelName")
                    .as_deref()
                    .and_then(|value| canonical_scanner_model(vendor, value))
            }),
        patient_position: header
            .patient_position
            .as_deref()
            .and_then(safe_patient_position),
        software_versions: software_versions.into_iter().take(16).collect(),
        magnetic_field_strength: header
            .magnetic_field_strength
            .or_else(|| json_float(json, "MagneticFieldStrength"))
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 15.0)),
        receive_coil_name: acquisition_equipment_string(header, "receive_coil_name")
            .as_deref()
            .and_then(canonical_coil)
            .or_else(|| {
                json_string(json, "ReceiveCoilName")
                    .as_deref()
                    .and_then(canonical_coil)
            }),
        transmit_coil_name: acquisition_equipment_string(header, "transmit_coil_name")
            .as_deref()
            .and_then(canonical_coil)
            .or_else(|| {
                json_string(json, "TransmitCoilName")
                    .as_deref()
                    .and_then(canonical_coil)
            }),
        sequence_name: header
            .sequence_name
            .as_deref()
            .and_then(canonical_sequence_family)
            .or_else(|| {
                json_string(json, "SequenceName")
                    .as_deref()
                    .and_then(canonical_sequence_family)
            }),
        scanning_sequence: allowlisted_code_list(
            &group.scanning_sequences,
            &["SE", "IR", "GR", "EP", "RM"],
        ),
        sequence_variant: allowlisted_code_list(
            &group.sequence_variants,
            &["SK", "MTC", "SS", "TRSS", "SP", "MP", "OSP", "NONE"],
        ),
        scan_options: allowlisted_code_list(
            &group.scan_options,
            &["PER", "RG", "CG", "PPG", "FC", "PFF", "PFP", "SP", "FS"],
        ),
        mr_acquisition_type: header
            .mr_acquisition_type
            .as_deref()
            .and_then(|value| safe_enum(value, &["2D", "3D"])),
        image_type: allowlisted_code_list(
            &group.image_types,
            &[
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
        ),
        series_number: header.series_number.filter(|value| *value >= 0),
        acquisition_number: header.acquisition_number.filter(|value| *value >= 0),
    }
}

fn build_image_metadata(
    group: &SeriesGroup,
    json: &Value,
    info: &NiftiInfo,
    allow_representative_echo_number: bool,
) -> ImageMetadata {
    let header = &group.representative;
    ImageMetadata {
        dimensions: info.dimensions.clone(),
        voxel_size_mm: info.voxel_size_mm.clone(),
        datatype: info.datatype.clone(),
        bits_per_voxel: info.bits_per_voxel,
        affine: info.affine,
        orientation: info.orientation.clone(),
        volume_count: info.volume_count,
        echo_number: json_integer(json, "EchoNumber")
            .or_else(|| {
                allow_representative_echo_number
                    .then_some(header.echo_number)
                    .flatten()
            })
            .filter(|value| *value >= 1),
        tr_seconds: json_float(json, "RepetitionTime")
            .or(info.tr_seconds)
            .or_else(|| header.repetition_time_ms.map(|value| value / 1_000.0))
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 60.0)),
        te_seconds: json_float(json, "EchoTime")
            .or_else(|| header.echo_time_ms.map(|value| value / 1_000.0))
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 2.0)),
        inversion_time_seconds: json_float(json, "InversionTime")
            .or_else(|| header.inversion_time_ms.map(|value| value / 1_000.0))
            .and_then(|value| number_in_range(value, 0.0, 30.0)),
        flip_angle_degrees: json_float(json, "FlipAngle")
            .or(header.flip_angle_degrees)
            .and_then(|value| number_in_range(value, 0.0, 360.0)),
        slice_thickness_mm: json_float(json, "SliceThickness")
            .or_else(|| acquisition_float(header, "slice_thickness_mm"))
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 100.0)),
        spacing_between_slices_mm: json_float(json, "SpacingBetweenSlices")
            .or_else(|| acquisition_float(header, "spacing_between_slices_mm"))
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 100.0)),
        pixel_bandwidth_hz: json_float(json, "PixelBandwidth")
            .or_else(|| acquisition_float(header, "pixel_bandwidth_hz"))
            .filter(|value| *value > 0.0),
        dwell_time_seconds: json_float(json, "DwellTime")
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 1.0)),
        effective_echo_spacing_seconds: json_float(json, "EffectiveEchoSpacing")
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 1.0)),
        total_readout_time_seconds: json_float(json, "TotalReadoutTime")
            .and_then(|value| number_in_range(value, 0.0, 10.0)),
        phase_encoding_direction: json_string(json, "PhaseEncodingDirection")
            .and_then(|value| safe_enum(&value, &["i", "i-", "j", "j-", "k", "k-"])),
        slice_timing_seconds: bounded_float_array(json, "SliceTiming", 4096, 0.0, 60.0),
        acquisition_matrix: acquisition_u64_array(header, "acquisition_matrix"),
        recon_matrix: info.dimensions.iter().take(3).copied().collect(),
        multiband_acceleration_factor: json_float(json, "MultibandAccelerationFactor")
            .and_then(|value| number_in_range(value, 1.0, 64.0)),
        parallel_reduction_factor_in_plane: json_float(json, "ParallelReductionFactorInPlane")
            .or_else(|| acquisition_float(header, "parallel_reduction_factor_in_plane"))
            .and_then(|value| number_in_range(value, 1.0, 64.0)),
        partial_fourier: json_float(json, "PartialFourier")
            .and_then(|value| number_in_range(value, f64::MIN_POSITIVE, 1.0)),
        echo_train_length: json_integer(json, "EchoTrainLength")
            .or_else(|| acquisition_integer(header, "echo_train_length"))
            .filter(|value| *value >= 1),
        number_of_averages: json_float(json, "NumberOfAverages")
            .or_else(|| acquisition_float(header, "number_of_averages"))
            .filter(|value| *value > 0.0),
        imaging_frequency_mhz: json_float(json, "ImagingFrequency")
            .or_else(|| acquisition_float(header, "imaging_frequency_mhz"))
            .filter(|value| *value > 0.0),
        imaged_nucleus: json_string(json, "ImagedNucleus")
            .as_deref()
            .and_then(canonical_nucleus)
            .or_else(|| {
                acquisition_code_string(header, "imaged_nucleus")
                    .as_deref()
                    .and_then(canonical_nucleus)
            }),
    }
}

fn protocol_group_input(group: &SeriesGroup, json: &Value, info: &NiftiInfo) -> String {
    let local_protocol = group.representative.local_protocol_text();
    if !local_protocol.trim().is_empty() {
        return format!("protocol:{local_protocol}");
    }
    // Some enhanced/vendor DICOMs omit protocol labels. A deterministic,
    // acquisition-level signature avoids collapsing every such series into the
    // same protocol group without exposing any raw identifier.
    let header = &group.representative;
    serde_json::json!({
        "manufacturer": header.manufacturer.as_deref().and_then(|v| safe_equipment_text(v, 96)),
        "sequence_name": header.sequence_name.as_deref().and_then(|v| safe_code_text(v, 96)),
        "scanning_sequence": safe_code_list(&header.scanning_sequence),
        "tr_seconds": json_float(json, "RepetitionTime").or(info.tr_seconds),
        "te_seconds": json_float(json, "EchoTime").or_else(|| header.echo_time_ms.map(|v| v / 1000.0)),
        "matrix": acquisition_u64_array(header, "acquisition_matrix"),
        "dimensions": info.dimensions.iter().take(3).collect::<Vec<_>>(),
        "voxel_size_mm": &info.voxel_size_mm,
        "phase_encoding_direction": json_string(json, "PhaseEncodingDirection")
            .and_then(|v| safe_enum(&v, &["i", "i-", "j", "j-", "k", "k-"])),
        "multiband": json_float(json, "MultibandAccelerationFactor"),
    })
    .to_string()
}

fn scrub_nifti_text_header(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut header = [0_u8; 352];
    file.read_exact(&mut header)
        .context("NIfTI file is shorter than its header")?;
    let header_size = i32::from_le_bytes(header[0..4].try_into().unwrap());
    let header_size_be = i32::from_be_bytes(header[0..4].try_into().unwrap());
    if header_size != 348 && header_size_be != 348 {
        bail!("only valid NIfTI-1 output is accepted");
    }
    if header[348..352].iter().any(|byte| *byte != 0) {
        bail!("NIfTI extensions are not accepted by the metadata policy");
    }
    for range in [4..32, 148..228, 228..252, 328..344] {
        header[range].fill(0);
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.flush()?;
    Ok(())
}

fn read_nifti_info(path: &Path) -> Result<NiftiInfo> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; 352];
    file.read_exact(&mut header)?;
    let endian = match i32::from_le_bytes(header[0..4].try_into().unwrap()) {
        348 => Endian::Little,
        _ if i32::from_be_bytes(header[0..4].try_into().unwrap()) == 348 => Endian::Big,
        _ => bail!("invalid NIfTI-1 header size"),
    };
    if &header[344..348] != b"n+1\0" {
        bail!("only single-file NIfTI-1 output is accepted");
    }
    let dimension_count = read_i16(&header, 40, endian);
    if !(3..=7).contains(&dimension_count) {
        bail!("invalid NIfTI dimension count");
    }
    let mut dimensions = Vec::with_capacity(dimension_count as usize);
    for index in 1..=dimension_count as usize {
        let value = read_i16(&header, 40 + index * 2, endian);
        if value <= 0 {
            bail!("invalid non-positive NIfTI dimension");
        }
        dimensions.push(value as u64);
    }
    let mut pixdim = [0_f64; 8];
    for (index, value) in pixdim.iter_mut().enumerate() {
        *value = f64::from(read_f32(&header, 76 + index * 4, endian));
    }
    let voxel_size_mm = pixdim[1..=3].iter().map(|value| value.abs()).collect();
    let datatype_code = read_i16(&header, 70, endian);
    let bits_per_voxel = read_i16(&header, 72, endian);
    if bits_per_voxel <= 0 || bits_per_voxel % 8 != 0 {
        bail!("unsupported NIfTI bit depth");
    }
    let datatype = datatype_name(datatype_code).context("unsupported NIfTI datatype")?;
    let voxel_offset_float = f64::from(read_f32(&header, 108, endian));
    if !voxel_offset_float.is_finite() || voxel_offset_float < 352.0 {
        bail!("invalid NIfTI voxel offset");
    }
    let voxel_offset = voxel_offset_float.round() as u64;
    let voxel_count = dimensions
        .iter()
        .try_fold(1_u64, |acc, value| acc.checked_mul(*value))
        .context("NIfTI dimensions overflow")?;
    let data_size = voxel_count
        .checked_mul((bits_per_voxel / 8) as u64)
        .context("NIfTI byte size overflow")?;
    let expected_size = voxel_offset
        .checked_add(data_size)
        .context("NIfTI size overflow")?;
    let actual_size = file.metadata()?.len();
    if actual_size < expected_size {
        bail!("NIfTI voxel payload is truncated");
    }
    let geometry_transform_present =
        read_i16(&header, 252, endian) > 0 || read_i16(&header, 254, endian) > 0;
    let affine = read_affine(&header, &pixdim, endian);
    let orientation = orientation_code(&affine);
    let volume_count = dimensions.iter().skip(3).copied().product::<u64>().max(1);
    let temporal_units = header[123] & 0x38;
    let spatial_units_mm = header[123] & 0x07 == 2;
    let temporal_units_seconds = matches!(temporal_units, 8 | 16 | 24);
    let scale_slope = f64::from(read_f32(&header, 112, endian));
    let scale_intercept = f64::from(read_f32(&header, 116, endian));
    let tr_raw = dimensions
        .get(3)
        .map(|_| pixdim[4].abs())
        .filter(|value| *value > 0.0);
    let tr_seconds = tr_raw.map(|value| match temporal_units {
        16 => value / 1_000.0,
        24 => value / 1_000_000.0,
        _ => value,
    });
    Ok(NiftiInfo {
        dimensions,
        voxel_size_mm,
        datatype_code,
        datatype: datatype.into(),
        bits_per_voxel: bits_per_voxel as u16,
        affine,
        orientation,
        volume_count,
        tr_seconds,
        voxel_offset,
        expected_size,
        actual_size,
        geometry_transform_present,
        spatial_units_mm,
        temporal_units_seconds,
        scale_slope,
        scale_intercept,
        endian,
    })
}

#[derive(Debug)]
struct SignalInspection {
    finite: bool,
    nonconstant: bool,
}

fn inspect_signal_and_hash(path: &Path, info: &NiftiInfo) -> Result<(String, SignalInspection)> {
    let mut file = BufReader::with_capacity(1024 * 1024, File::open(path)?);
    let bytes_per_voxel = usize::from(info.bits_per_voxel / 8);
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut carry = Vec::with_capacity(bytes_per_voxel);
    let mut first_voxel: Option<Vec<u8>> = None;
    let mut finite = true;
    let mut nonconstant = false;
    let mut absolute_offset = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = &buffer[..read];
        digest.update(bytes);
        let end_offset = absolute_offset + read as u64;
        if end_offset > info.voxel_offset {
            let data_start = if absolute_offset < info.voxel_offset {
                (info.voxel_offset - absolute_offset) as usize
            } else {
                0
            };
            carry.extend_from_slice(&bytes[data_start..]);
            let complete_bytes = (carry.len() / bytes_per_voxel) * bytes_per_voxel;
            for voxel in carry[..complete_bytes].chunks_exact(bytes_per_voxel) {
                if let Some(first) = &first_voxel {
                    nonconstant |= voxel != first;
                } else {
                    first_voxel = Some(voxel.to_vec());
                }
                finite &= voxel_is_finite(voxel, info.datatype_code, info.endian);
            }
            carry = carry.split_off(complete_bytes);
        }
        absolute_offset = end_offset;
    }
    if !carry.is_empty() || first_voxel.is_none() {
        bail!("NIfTI voxel payload is not aligned to its datatype");
    }
    Ok((
        hex::encode(digest.finalize()),
        SignalInspection {
            finite,
            nonconstant,
        },
    ))
}

fn voxel_is_finite(bytes: &[u8], datatype: i16, endian: Endian) -> bool {
    match datatype {
        16 => {
            let value = match endian {
                Endian::Little => f32::from_le_bytes(bytes.try_into().unwrap()),
                Endian::Big => f32::from_be_bytes(bytes.try_into().unwrap()),
            };
            value.is_finite()
        }
        64 => {
            let value = match endian {
                Endian::Little => f64::from_le_bytes(bytes.try_into().unwrap()),
                Endian::Big => f64::from_be_bytes(bytes.try_into().unwrap()),
            };
            value.is_finite()
        }
        _ => true,
    }
}

fn deterministic_gzip(source: &Path, destination: &Path) -> Result<String> {
    let temporary = destination.with_extension("gz.tmp");
    let input = File::open(source)?;
    let output = File::create(&temporary)?;
    let mut reader = BufReader::with_capacity(1024 * 1024, input);
    let writer = BufWriter::with_capacity(1024 * 1024, output);
    let mut encoder = GzBuilder::new().mtime(0).write(writer, Compression::new(6));
    std::io::copy(&mut reader, &mut encoder)?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    drop(writer);
    fs::rename(&temporary, destination)?;
    sha256_file(destination)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = BufReader::with_capacity(1024 * 1024, File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn write_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn check(code: &str, passed: bool) -> QcCheck {
    QcCheck {
        code: code.into(),
        status: if passed {
            QcStatus::Pass
        } else {
            QcStatus::Fail
        },
    }
}

fn datatype_name(code: i16) -> Option<&'static str> {
    Some(match code {
        2 => "uint8",
        4 => "int16",
        8 => "int32",
        16 => "float32",
        64 => "float64",
        256 => "int8",
        512 => "uint16",
        768 => "uint32",
        1024 => "int64",
        1280 => "uint64",
        _ => return None,
    })
}

fn read_affine(header: &[u8], pixdim: &[f64; 8], endian: Endian) -> [[f64; 4]; 4] {
    let sform_code = read_i16(header, 254, endian);
    if sform_code > 0 {
        let mut affine = [[0.0; 4]; 4];
        for (row, values) in affine.iter_mut().take(3).enumerate() {
            for (column, value) in values.iter_mut().enumerate() {
                *value = f64::from(read_f32(header, 280 + (row * 4 + column) * 4, endian));
            }
        }
        affine[3][3] = 1.0;
        return affine;
    }
    let qform_code = read_i16(header, 252, endian);
    if qform_code <= 0 {
        return [
            [pixdim[1].abs(), 0.0, 0.0, 0.0],
            [0.0, pixdim[2].abs(), 0.0, 0.0],
            [0.0, 0.0, pixdim[3].abs(), 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
    }
    let b = f64::from(read_f32(header, 256, endian));
    let c = f64::from(read_f32(header, 260, endian));
    let d = f64::from(read_f32(header, 264, endian));
    let a_squared = 1.0 - b * b - c * c - d * d;
    let a = if a_squared > 1e-7 {
        a_squared.sqrt()
    } else {
        0.0
    };
    let dx = pixdim[1].abs();
    let dy = pixdim[2].abs();
    let dz = pixdim[3].abs() * if pixdim[0] < 0.0 { -1.0 } else { 1.0 };
    let rotation = [
        [
            a * a + b * b - c * c - d * d,
            2.0 * (b * c - a * d),
            2.0 * (b * d + a * c),
        ],
        [
            2.0 * (b * c + a * d),
            a * a + c * c - b * b - d * d,
            2.0 * (c * d - a * b),
        ],
        [
            2.0 * (b * d - a * c),
            2.0 * (c * d + a * b),
            a * a + d * d - c * c - b * b,
        ],
    ];
    let offsets = [
        f64::from(read_f32(header, 268, endian)),
        f64::from(read_f32(header, 272, endian)),
        f64::from(read_f32(header, 276, endian)),
    ];
    let scales = [dx, dy, dz];
    let mut affine = [[0.0; 4]; 4];
    for (row, values) in affine.iter_mut().take(3).enumerate() {
        for (column, value) in values.iter_mut().take(3).enumerate() {
            *value = rotation[row][column] * scales[column];
        }
        values[3] = offsets[row];
    }
    affine[3][3] = 1.0;
    affine
}

fn orientation_code(affine: &[[f64; 4]; 4]) -> String {
    let mut output = String::with_capacity(3);
    let mut used = [false; 3];
    let columns = [
        [affine[0][0], affine[1][0], affine[2][0]],
        [affine[0][1], affine[1][1], affine[2][1]],
        [affine[0][2], affine[1][2], affine[2][2]],
    ];
    for column in columns {
        let Some((axis, value)) = (0..3)
            .filter(|axis| !used[*axis])
            .map(|axis| (axis, column[axis]))
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        else {
            return "unknown".into();
        };
        if value.abs() < 1e-8 {
            return "unknown".into();
        }
        used[axis] = true;
        output.push(match (axis, value >= 0.0) {
            (0, true) => 'R',
            (0, false) => 'L',
            (1, true) => 'A',
            (1, false) => 'P',
            (2, true) => 'S',
            (2, false) => 'I',
            _ => unreachable!(),
        });
    }
    output
}

fn read_i16(bytes: &[u8], offset: usize, endian: Endian) -> i16 {
    let value = bytes[offset..offset + 2].try_into().unwrap();
    match endian {
        Endian::Little => i16::from_le_bytes(value),
        Endian::Big => i16::from_be_bytes(value),
    }
}

fn read_f32(bytes: &[u8], offset: usize, endian: Endian) -> f32 {
    let value = bytes[offset..offset + 4].try_into().unwrap();
    match endian {
        Endian::Little => f32::from_le_bytes(value),
        Endian::Big => f32::from_be_bytes(value),
    }
}

fn json_float(json: &Value, key: &str) -> Option<f64> {
    json.get(key)
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|value| value.is_finite())
}

fn json_integer(json: &Value, key: &str) -> Option<i64> {
    json.get(key)
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn json_string(json: &Value, key: &str) -> Option<String> {
    let value = json.get(key)?;
    if let Some(value) = value.as_str() {
        return Some(value.to_owned());
    }
    if let Some(values) = value.as_array() {
        return Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    None
}

fn json_strings(json: &Value, key: &str) -> Vec<String> {
    let Some(value) = json.get(key) else {
        return Vec::new();
    };
    if let Some(value) = value.as_str() {
        return value
            .split(['\\', ','])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn json_float_array(json: &Value, key: &str) -> Vec<f64> {
    json.get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_f64)
                .filter(|v| v.is_finite())
                .collect()
        })
        .unwrap_or_default()
}

fn bounded_float_array(
    json: &Value,
    key: &str,
    max_items: usize,
    minimum: f64,
    maximum: f64,
) -> Vec<f64> {
    let values = json_float_array(json, key);
    if values.len() > max_items
        || values
            .iter()
            .any(|value| *value < minimum || *value > maximum)
    {
        Vec::new()
    } else {
        values
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScannerVendor {
    Siemens,
    Philips,
    Ge,
    Canon,
    UnitedImaging,
    Bruker,
}

impl ScannerVendor {
    fn from_name(value: &str) -> Option<Self> {
        match value {
            "Siemens" => Some(Self::Siemens),
            "Philips" => Some(Self::Philips),
            "GE" => Some(Self::Ge),
            "Canon/Toshiba" => Some(Self::Canon),
            "United Imaging" => Some(Self::UnitedImaging),
            "Bruker" => Some(Self::Bruker),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Siemens => "Siemens",
            Self::Philips => "Philips",
            Self::Ge => "GE",
            Self::Canon => "Canon/Toshiba",
            Self::UnitedImaging => "United Imaging",
            Self::Bruker => "Bruker",
        }
    }
}

fn semantic_words(value: &str, max_characters: usize) -> Option<Vec<String>> {
    if !value.is_ascii() || value.chars().count() > max_characters {
        return None;
    }
    let words = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>();
    (!words.is_empty()).then_some(words)
}

fn canonical_manufacturer(value: &str) -> Option<String> {
    let words = semantic_words(value, 128)?;
    let has = |candidate: &str| words.iter().any(|word| word == candidate);
    let vendor = if has("SIEMENS") {
        ScannerVendor::Siemens
    } else if has("PHILIPS") {
        ScannerVendor::Philips
    } else if has("GE") || (has("GENERAL") && has("ELECTRIC")) {
        ScannerVendor::Ge
    } else if has("CANON") || has("TOSHIBA") {
        ScannerVendor::Canon
    } else if has("UIH") || (has("UNITED") && has("IMAGING")) {
        ScannerVendor::UnitedImaging
    } else if has("BRUKER") {
        ScannerVendor::Bruker
    } else {
        return None;
    };
    Some(vendor.name().into())
}

fn normalized_word_string(value: &str) -> Option<String> {
    semantic_words(value, 128).map(|words| words.join(" "))
}

fn model_from_patterns(value: &str, patterns: &[(&str, &str)]) -> Option<String> {
    let words = normalized_word_string(value)?;
    let padded = format!(" {words} ");
    patterns
        .iter()
        .find(|(needle, _)| padded.contains(&format!(" {needle} ")))
        .map(|(_, canonical)| (*canonical).into())
}

fn canonical_scanner_model(vendor: Option<ScannerVendor>, value: &str) -> Option<String> {
    match vendor? {
        ScannerVendor::Siemens => model_from_patterns(
            value,
            &[
                ("PRISMA FIT", "MAGNETOM Prisma_fit"),
                ("BIOGRAPH MMR", "Biograph mMR"),
                ("FREE MAX", "MAGNETOM Free.Max"),
                ("FREE STAR", "MAGNETOM Free.Star"),
                ("TRIO TIM", "MAGNETOM TrioTim"),
                ("CIMA X", "MAGNETOM Cima.X"),
                ("CONNECTOM", "MAGNETOM Connectom"),
                ("PRISMA", "MAGNETOM Prisma"),
                ("SKYRA", "MAGNETOM Skyra"),
                ("TRIOTIM", "MAGNETOM TrioTim"),
                ("TRIO", "MAGNETOM Trio"),
                ("VIDA", "MAGNETOM Vida"),
                ("VERIO", "MAGNETOM Verio"),
                ("TERRA", "MAGNETOM Terra"),
                ("SOLA", "MAGNETOM Sola"),
                ("AERA", "MAGNETOM Aera"),
                ("AVANTO", "MAGNETOM Avanto"),
                ("ALLEGRA", "MAGNETOM Allegra"),
                ("ESPREE", "MAGNETOM Espree"),
                ("SYMPHONY", "MAGNETOM Symphony"),
            ],
        ),
        ScannerVendor::Philips => model_from_patterns(
            value,
            &[
                ("INGENIA ELITION X", "Ingenia Elition X"),
                ("INGENIA AMBITION X", "Ingenia Ambition X"),
                ("INGENIA CX", "Ingenia CX"),
                ("MR 7700", "MR 7700"),
                ("INGENIA", "Ingenia"),
                ("ACHIEVA", "Achieva"),
                ("INTERA", "Intera"),
                ("ELITION", "Elition"),
                ("AMBITION", "Ambition"),
                ("PANORAMA", "Panorama"),
            ],
        ),
        ScannerVendor::Ge => model_from_patterns(
            value,
            &[
                ("DISCOVERY MR750W", "Discovery MR750w"),
                ("DISCOVERY MR750", "Discovery MR750"),
                ("OPTIMA MR450W", "Optima MR450w"),
                ("SIGNA PREMIER", "SIGNA Premier"),
                ("SIGNA ARCHITECT", "SIGNA Architect"),
                ("SIGNA PET MR", "SIGNA PET/MR"),
                ("SIGNA HDXT", "SIGNA HDxt"),
                ("SIGNA VOYAGER", "SIGNA Voyager"),
                ("SIGNA ARTIST", "SIGNA Artist"),
                ("SIGNA HERO", "SIGNA Hero"),
                ("GENESIS SIGNA", "Genesis SIGNA"),
                ("MR750W", "Discovery MR750w"),
                ("MR750", "Discovery MR750"),
            ],
        ),
        ScannerVendor::Canon => model_from_patterns(
            value,
            &[
                ("VANTAGE GALAN", "Vantage Galan"),
                ("VANTAGE TITAN", "Vantage Titan"),
                ("VANTAGE ORIAN", "Vantage Orian"),
                ("VANTAGE ELAN", "Vantage Elan"),
                ("EXCELART VANTAGE", "Excelart Vantage"),
            ],
        ),
        ScannerVendor::UnitedImaging => model_from_patterns(
            value,
            &[
                ("UMR JUPITER", "uMR Jupiter"),
                ("UMR OMEGA", "uMR Omega"),
                ("UMR 790", "uMR 790"),
                ("UMR 780", "uMR 780"),
                ("UMR 770", "uMR 770"),
                ("UMR 670", "uMR 670"),
                ("UMR 570", "uMR 570"),
                ("UMR 560", "uMR 560"),
            ],
        ),
        ScannerVendor::Bruker => model_from_patterns(
            value,
            &[
                ("BIOSPEC", "BioSpec"),
                ("PHARMASCAN", "PharmaScan"),
                ("AVANCE", "Avance"),
                ("ICON", "ICON"),
            ],
        ),
    }
}

fn normalized_numeric_version(value: &str) -> Option<String> {
    let candidate = value
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '.');
    let candidate = candidate
        .strip_prefix(&['R', 'r', 'V', 'v'][..])
        .unwrap_or(candidate);
    let parts = candidate.split('.').collect::<Vec<_>>();
    if !(2..=4).contains(&parts.len()) {
        return None;
    }
    let numbers = parts
        .iter()
        .map(|part| {
            if part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            part.parse::<u8>().ok()
        })
        .collect::<Option<Vec<_>>>()?;
    if numbers[0] == 0 || numbers[0] > 99 {
        return None;
    }
    Some(
        numbers
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join("."),
    )
}

fn siemens_release_token(value: &str) -> Option<String> {
    for token in semantic_words(value, 128)? {
        let (prefix_length, valid_prefix) = if token.len() >= 2 {
            let prefix = &token[..2];
            (
                2,
                matches!(prefix, "VA" | "VB" | "VC" | "VD" | "VE" | "XA" | "XB"),
            )
        } else {
            (0, false)
        };
        let (prefix_length, valid_prefix) = if valid_prefix {
            (prefix_length, true)
        } else {
            (
                1,
                token
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| matches!(byte, b'A' | b'B' | b'C' | b'D' | b'E')),
            )
        };
        if !valid_prefix || token.len() < prefix_length + 2 {
            continue;
        }
        let suffix = &token[prefix_length..];
        let valid_suffix = (suffix.len() == 2 && suffix.bytes().all(|byte| byte.is_ascii_digit()))
            || (suffix.len() == 3
                && suffix[..2].bytes().all(|byte| byte.is_ascii_digit())
                && suffix.as_bytes()[2].is_ascii_uppercase());
        if valid_suffix {
            return Some(token);
        }
    }
    None
}

fn ge_release_token(value: &str) -> Option<String> {
    for raw in
        value.split(|character: char| !(character.is_ascii_alphanumeric() || character == '.'))
    {
        let token = raw.to_ascii_uppercase();
        if let Some(suffix) = token.strip_prefix("DV") {
            let parts = suffix.split('.').collect::<Vec<_>>();
            if (1..=2).contains(&parts.len())
                && parts.iter().all(|part| {
                    !part.is_empty()
                        && part.len() <= 2
                        && part.bytes().all(|byte| byte.is_ascii_digit())
                })
            {
                return Some(format!("DV{}", parts.join(".")));
            }
        }
    }
    None
}

fn numeric_version_from_text(value: &str) -> Option<String> {
    value
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '.'))
        .find_map(normalized_numeric_version)
}

fn canonical_software_version(vendor: Option<ScannerVendor>, value: &str) -> Option<String> {
    if !value.is_ascii() || value.chars().count() > 128 {
        return None;
    }
    let vendor = vendor?;
    let version = match vendor {
        ScannerVendor::Siemens => siemens_release_token(value),
        ScannerVendor::Ge => ge_release_token(value).or_else(|| numeric_version_from_text(value)),
        ScannerVendor::Philips
        | ScannerVendor::Canon
        | ScannerVendor::UnitedImaging
        | ScannerVendor::Bruker => numeric_version_from_text(value),
    }?;
    Some(format!("{} {version}", vendor.name()))
}

fn canonical_coil(value: &str) -> Option<String> {
    if !value.is_ascii() || value.chars().count() > 128 {
        return None;
    }
    let compact = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    let family = if compact.contains("HEADNECK") || compact.contains("HEADANDNECK") {
        "HEAD_NECK"
    } else if compact.contains("HEAD") {
        "HEAD"
    } else if compact.contains("NECK") {
        "NECK"
    } else if compact.contains("BODY") {
        "BODY"
    } else if compact.contains("SPINE") {
        "SPINE"
    } else if compact.contains("KNEE") {
        "KNEE"
    } else if compact.contains("FLEX") {
        "FLEX"
    } else if compact.contains("BREAST") {
        "BREAST"
    } else if compact.contains("CARDIAC") || compact.contains("HEART") {
        "CARDIAC"
    } else if compact.contains("FOOT") {
        "FOOT"
    } else if compact.contains("ANKLE") {
        "ANKLE"
    } else if compact.contains("SHOULDER") {
        "SHOULDER"
    } else if compact.contains("WRIST") {
        "WRIST"
    } else {
        return None;
    };
    let channels = value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u16>().ok())
        .find(|channels| (1..=256).contains(channels));
    Some(match channels {
        Some(channels) => format!("{family}_{channels}"),
        None => family.into(),
    })
}

fn canonical_sequence_family(value: &str) -> Option<String> {
    if !value.is_ascii() || value.chars().count() > 128 {
        return None;
    }
    let compact = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    if compact.contains("EPFID") {
        Some("EPFID".into())
    } else if compact.contains("EP2D") {
        Some("EP2D".into())
    } else if compact.contains("BOLD") || compact.contains("FMRI") {
        Some("BOLD_EPI".into())
    } else if compact.contains("EPI") {
        Some("EPI".into())
    } else {
        None
    }
}

fn canonical_nucleus(value: &str) -> Option<String> {
    if !value.is_ascii() || value.chars().count() > 32 {
        return None;
    }
    let compact = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    match compact.as_str() {
        "1H" | "H1" => Some("1H".into()),
        "13C" | "C13" => Some("13C".into()),
        "17O" | "O17" => Some("17O".into()),
        "19F" | "F19" => Some("19F".into()),
        "23NA" | "NA23" => Some("23Na".into()),
        "31P" | "P31" => Some("31P".into()),
        "129XE" | "XE129" => Some("129Xe".into()),
        _ => None,
    }
}

fn safe_equipment_text(value: &str, max_characters: usize) -> Option<String> {
    normalize_restricted_text(value, max_characters, false)
}

fn safe_code_text(value: &str, max_characters: usize) -> Option<String> {
    normalize_restricted_text(value, max_characters, true)
}

fn normalize_restricted_text(
    value: &str,
    max_characters: usize,
    code_like: bool,
) -> Option<String> {
    if !value.is_ascii() || value.chars().count() > max_characters {
        return None;
    }
    let allowed = |character: char| {
        character.is_ascii_alphanumeric()
            || matches!(
                character,
                ' ' | '.' | '_' | '+' | '-' | '/' | ':' | ',' | ';' | '(' | ')'
            )
    };
    if !value.chars().all(allowed) || value.contains("  ") || value.contains("..") {
        return None;
    }
    let cleaned = value.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty()
        || !cleaned.as_bytes()[0].is_ascii_alphanumeric()
        || (code_like && cleaned.contains(' '))
    {
        return None;
    }
    Some(cleaned)
}

fn safe_code_list(values: &[String]) -> Vec<String> {
    let mut output = Vec::new();
    for value in values.iter().filter_map(|value| safe_code_text(value, 32)) {
        if value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'))
            && !output.contains(&value)
        {
            output.push(value);
            if output.len() == 32 {
                break;
            }
        }
    }
    output
}

fn allowlisted_code_list(values: &[String], allowed: &[&str]) -> Vec<String> {
    let mut output = Vec::new();
    for value in values.iter().filter_map(|value| safe_enum(value, allowed)) {
        if !output.contains(&value) {
            output.push(value);
        }
    }
    output
}

fn safe_patient_position(value: &str) -> Option<String> {
    safe_enum(
        value,
        &["HFP", "HFS", "HFDR", "HFDL", "FFP", "FFS", "FFDR", "FFDL"],
    )
}

fn safe_enum(value: &str, allowed: &[&str]) -> Option<String> {
    allowed
        .iter()
        .find(|candidate| value.eq_ignore_ascii_case(candidate))
        .map(|value| (*value).into())
}

fn acquisition_code_string(group: &crate::dicom::DicomHeader, key: &str) -> Option<String> {
    group
        .acquisition
        .get(key)?
        .as_str()
        .and_then(|value| safe_code_text(value, 96))
}

fn acquisition_equipment_string(group: &crate::dicom::DicomHeader, key: &str) -> Option<String> {
    group
        .acquisition
        .get(key)?
        .as_str()
        .and_then(|value| safe_equipment_text(value, 96))
}

fn acquisition_float(group: &crate::dicom::DicomHeader, key: &str) -> Option<f64> {
    group
        .acquisition
        .get(key)?
        .as_f64()
        .filter(|value| value.is_finite())
}

fn acquisition_integer(group: &crate::dicom::DicomHeader, key: &str) -> Option<i64> {
    group.acquisition.get(key)?.as_i64()
}

fn acquisition_u64_array(group: &crate::dicom::DicomHeader, key: &str) -> Vec<u64> {
    let values: Vec<u64> = group
        .acquisition
        .get(key)
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    if values.len() == 4 {
        values
    } else {
        Vec::new()
    }
}

fn number_in_range(value: f64, minimum: f64, maximum: f64) -> Option<f64> {
    (value.is_finite() && value >= minimum && value <= maximum).then_some(value)
}

fn spatial_dimensions_valid(dimensions: &[u64]) -> bool {
    dimensions.len() == 4
        && dimensions[..3]
            .iter()
            .all(|value| (8..=4096).contains(value))
        && (10..=10_000_000).contains(&dimensions[3])
}

fn voxel_sizes_valid(values: &[f64]) -> bool {
    values.len() == 3
        && values
            .iter()
            .all(|value| value.is_finite() && 0.0 < *value && *value <= 100.0)
}

fn affine_finite(affine: &[[f64; 4]; 4]) -> bool {
    affine.iter().flatten().all(|value| value.is_finite())
}

fn affine_nondegenerate(affine: &[[f64; 4]; 4]) -> bool {
    let determinant = affine[0][0] * (affine[1][1] * affine[2][2] - affine[1][2] * affine[2][1])
        - affine[0][1] * (affine[1][0] * affine[2][2] - affine[1][2] * affine[2][0])
        + affine[0][2] * (affine[1][0] * affine[2][1] - affine[1][1] * affine[2][0]);
    determinant.is_finite() && determinant.abs() > 1e-8
}

fn datatype_bit_depth(code: i16) -> Option<u16> {
    Some(match code {
        2 | 256 => 8,
        4 | 512 => 16,
        8 | 16 | 768 => 32,
        64 | 1024 | 1280 => 64,
        _ => return None,
    })
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_test_nifti(path: &Path) {
        let dimensions = [4_i16, 2, 2, 2, 10, 1, 1, 1];
        let mut bytes = vec![0_u8; 352 + 2 * 2 * 2 * 10 * 4];
        bytes[0..4].copy_from_slice(&348_i32.to_le_bytes());
        for (index, value) in dimensions.iter().enumerate() {
            bytes[40 + index * 2..42 + index * 2].copy_from_slice(&value.to_le_bytes());
        }
        bytes[70..72].copy_from_slice(&16_i16.to_le_bytes());
        bytes[72..74].copy_from_slice(&32_i16.to_le_bytes());
        for index in 0..5 {
            let value = if index == 4 { 1.0_f32 } else { 2.0_f32 };
            bytes[76 + index * 4..80 + index * 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[108..112].copy_from_slice(&352_f32.to_le_bytes());
        bytes[112..116].copy_from_slice(&1_f32.to_le_bytes());
        bytes[123] = 10; // mm + seconds
        bytes[254..256].copy_from_slice(&1_i16.to_le_bytes());
        for (index, value) in [
            2.0_f32, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0,
        ]
        .iter()
        .enumerate()
        {
            bytes[280 + index * 4..284 + index * 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[148..158].copy_from_slice(b"patient123");
        bytes[344..348].copy_from_slice(b"n+1\0");
        for (index, chunk) in bytes[352..].chunks_exact_mut(4).enumerate() {
            chunk.copy_from_slice(&(index as f32).to_le_bytes());
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn scrubs_text_and_reads_four_dimensional_geometry() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("test.nii");
        write_test_nifti(&path);
        scrub_nifti_text_header(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes[4..32].iter().all(|byte| *byte == 0));
        assert!(bytes[148..252].iter().all(|byte| *byte == 0));
        let info = read_nifti_info(&path).unwrap();
        assert_eq!(info.dimensions, vec![2, 2, 2, 10]);
        assert_eq!(info.volume_count, 10);
        assert_eq!(info.orientation, "RAS");
    }

    #[test]
    fn deterministic_compression_has_stable_hash() {
        let directory = tempdir().unwrap();
        let input = directory.path().join("test.nii");
        write_test_nifti(&input);
        let one = directory.path().join("one.nii.gz");
        let two = directory.path().join("two.nii.gz");
        let hash_one = deterministic_gzip(&input, &one).unwrap();
        let hash_two = deterministic_gzip(&input, &two).unwrap();
        assert_eq!(hash_one, hash_two);
        assert_eq!(fs::read(one).unwrap(), fs::read(two).unwrap());
    }

    #[test]
    fn safe_code_filter_rejects_free_text() {
        assert_eq!(
            safe_code_list(&["ORIGINAL".into(), "patient name".into()]),
            vec!["ORIGINAL"]
        );
    }

    #[test]
    fn geometry_gate_rejects_malformed_headers() {
        assert!(!voxel_sizes_valid(&[2.0, 0.0, 2.0]));
        assert!(!voxel_sizes_valid(&[2.0, f64::NAN, 2.0]));
        assert!(!spatial_dimensions_valid(&[64, 64, 1, 300]));
        let degenerate = [
            [1.0, 0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        assert!(!affine_nondegenerate(&degenerate));
        let nonfinite = [
            [f64::NAN, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        assert!(!affine_finite(&nonfinite));
    }

    #[test]
    fn nifti_time_units_must_describe_time() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("frequency-units.nii");
        write_test_nifti(&path);
        let mut bytes = fs::read(&path).unwrap();
        bytes[123] = 34; // mm + Hz, which is invalid for a functional time axis.
        fs::write(&path, bytes).unwrap();
        let info = read_nifti_info(&path).unwrap();
        assert!(!info.temporal_units_seconds);
    }

    #[test]
    fn semantic_metadata_normalizer_retains_scanner_context_without_raw_text() {
        assert_eq!(
            canonical_manufacturer("SIEMENS Healthineers").as_deref(),
            Some("Siemens")
        );
        assert_eq!(
            canonical_scanner_model(Some(ScannerVendor::Siemens), "MAGNETOM Prisma_fit").as_deref(),
            Some("MAGNETOM Prisma_fit")
        );
        assert_eq!(
            canonical_software_version(Some(ScannerVendor::Siemens), "syngo MR XA30").as_deref(),
            Some("Siemens XA30")
        );
        assert_eq!(
            canonical_coil("HeadNeck_64").as_deref(),
            Some("HEAD_NECK_64")
        );
        assert_eq!(canonical_coil("SENSE-Head-8").as_deref(), Some("HEAD_8"));
        assert_eq!(
            canonical_sequence_family("*epfid2d1_80").as_deref(),
            Some("EPFID")
        );
        assert_eq!(canonical_nucleus("H1").as_deref(), Some("1H"));

        let vendor_cases = [
            ("Philips Medical Systems", "Philips"),
            ("GE MEDICAL SYSTEMS", "GE"),
            ("TOSHIBA", "Canon/Toshiba"),
            ("United Imaging Healthcare", "United Imaging"),
            ("Bruker BioSpin", "Bruker"),
        ];
        for (raw, canonical) in vendor_cases {
            assert_eq!(canonical_manufacturer(raw).as_deref(), Some(canonical));
        }
        assert_eq!(
            canonical_scanner_model(Some(ScannerVendor::Philips), "Achieva dStream").as_deref(),
            Some("Achieva")
        );
        assert_eq!(
            canonical_scanner_model(Some(ScannerVendor::Ge), "DISCOVERY MR750").as_deref(),
            Some("Discovery MR750")
        );
        assert_eq!(
            canonical_software_version(Some(ScannerVendor::Philips), "Release 5.7.1.0").as_deref(),
            Some("Philips 5.7.1.0")
        );
        assert_eq!(
            canonical_software_version(Some(ScannerVendor::Ge), "DV26.0_R03_1831.a").as_deref(),
            Some("GE DV26.0")
        );
    }

    #[test]
    fn semantic_metadata_normalizer_drops_identifier_shaped_free_text() {
        assert!(canonical_manufacturer("John Doe Lab").is_none());
        assert!(canonical_scanner_model(Some(ScannerVendor::Siemens), "JOHN_DOE").is_none());
        assert!(canonical_software_version(Some(ScannerVendor::Siemens), "JOHN_DOE").is_none());
        assert!(canonical_coil("JOHN_DOE").is_none());
        assert!(canonical_sequence_family("JOHN_DOE").is_none());
        assert!(canonical_nucleus("JOHN_DOE").is_none());
        assert_eq!(
            allowlisted_code_list(&["EP".into(), "JOHN_DOE".into()], &["SE", "EP"]),
            vec!["EP"]
        );
    }
}
