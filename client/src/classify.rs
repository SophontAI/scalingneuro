use crate::{
    dicom::{
        SeriesGroup, dicom_instance_count_supported, dicom_instance_size_supported,
        dicom_series_uncompressed_size_supported,
    },
    model::{Classification, ClassificationDecision, ClassificationEvidence},
};

#[derive(Debug, Clone, Default)]
pub struct ConversionSignals {
    pub dimensions: Vec<u64>,
    pub volume_count: u64,
    pub repetition_time_seconds: Option<f64>,
    pub echo_time_seconds: Option<f64>,
    pub bids_has_diffusion: bool,
    pub bids_has_asl: bool,
    pub functional_epi_evidence: bool,
    pub converted_name_count: usize,
}

pub fn classify_header(group: &SeriesGroup) -> Classification {
    let header = &group.representative;
    let mut evidence = Vec::new();

    if group.study_uid.is_empty() || group.series_uid.is_empty() {
        return hold(
            "missing_required_uid",
            1.0,
            [("missing_required_uid", "dicom_header", "contradicts")],
        );
    }
    if group.duplicate_sop_instance_uid
        || (!group.instances.is_empty()
            && group
                .instances
                .iter()
                .any(|instance| instance.sop_instance_uid.is_empty()))
    {
        return hold(
            "invalid_sop_instance_identity",
            1.0,
            [(
                "missing_or_duplicate_sop_instance_uid",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if !dicom_instance_count_supported(group.instances.len().max(group.files.len())) {
        return hold(
            "series_exceeds_dicom_instance_limit",
            1.0,
            [(
                "dicom_instance_count_exceeds_500000",
                "series_inventory",
                "contradicts",
            )],
        );
    }
    let source_sizes = group
        .files
        .iter()
        .map(std::fs::metadata)
        .collect::<std::io::Result<Vec<_>>>();
    let source_sizes = match source_sizes {
        Ok(metadata) => metadata
            .into_iter()
            .map(|metadata| metadata.len())
            .collect::<Vec<_>>(),
        Err(_) => {
            return hold(
                "source_file_changed_or_unreadable",
                1.0,
                [(
                    "source_file_metadata_unavailable",
                    "series_inventory",
                    "contradicts",
                )],
            );
        }
    };
    if source_sizes
        .iter()
        .any(|size| !dicom_instance_size_supported(*size))
    {
        return hold(
            "dicom_instance_exceeds_256_mib",
            1.0,
            [(
                "dicom_instance_exceeds_256_mib",
                "series_inventory",
                "contradicts",
            )],
        );
    }
    if !dicom_series_uncompressed_size_supported(source_sizes) {
        return hold(
            "series_exceeds_64_gib_uncompressed_dicom_limit",
            1.0,
            [(
                "series_exceeds_64_gib_uncompressed_dicom_limit",
                "series_inventory",
                "contradicts",
            )],
        );
    }
    if group.inconsistent_subject {
        return hold(
            "mixed_subject_series",
            1.0,
            [("mixed_subject_series", "dicom_header", "contradicts")],
        );
    }
    if group.inconsistent_metadata {
        return hold(
            "inconsistent_series_metadata",
            1.0,
            [(
                "required_instance_metadata_conflict",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if group.sop_class_uids.is_empty() {
        return hold(
            "missing_required_sop_class",
            1.0,
            [("missing_sop_class_uid", "dicom_header", "contradicts")],
        );
    }
    if group
        .sop_class_uids
        .iter()
        .any(|value| is_secondary_capture_sop(value))
    {
        return hold(
            "secondary_capture",
            1.0,
            [("secondary_capture_sop_class", "dicom_header", "contradicts")],
        );
    }
    if group
        .sop_class_uids
        .iter()
        .any(|value| !is_supported_mr_sop(value))
    {
        return hold(
            "unsupported_sop_class",
            1.0,
            [("unsupported_sop_class", "dicom_header", "contradicts")],
        );
    }
    if group
        .sop_class_uids
        .iter()
        .any(|value| is_enhanced_or_legacy_converted_mr_sop(value))
    {
        return hold(
            "enhanced_mr_pending_verified_metadata_contract",
            1.0,
            [(
                "enhanced_multiframe_metadata_not_yet_conversion_equivalent",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if group.modalities.is_empty() {
        return hold(
            "missing_required_modality",
            1.0,
            [("missing_modality", "dicom_header", "contradicts")],
        );
    }
    if group
        .modalities
        .iter()
        .any(|value| !value.eq_ignore_ascii_case("MR"))
    {
        return Classification {
            decision: ClassificationDecision::Excluded,
            kind: "not_mr".into(),
            confidence: 1.0,
            evidence: vec![ev("modality_not_mr", "dicom_header", "excludes")],
        };
    }
    if group.manufacturer_missing || group.manufacturers.is_empty() {
        return hold(
            "missing_scanner_manufacturer",
            1.0,
            [(
                "missing_scanner_manufacturer",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    let all_siemens = group
        .manufacturers
        .iter()
        .all(|value| is_siemens_manufacturer(value));
    let all_philips = group
        .manufacturers
        .iter()
        .all(|value| is_philips_manufacturer(value));
    let all_ge = group
        .manufacturers
        .iter()
        .all(|value| is_ge_manufacturer(value));
    if all_ge {
        return hold(
            "ge_classic_requires_verified_private_metadata_reconstruction",
            1.0,
            [(
                "ge_classic_scientific_metadata_not_conversion_equivalent",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if !all_siemens && !all_philips {
        return hold(
            "unsupported_scanner_manufacturer",
            1.0,
            [(
                "scanner_manufacturer_not_release_verified",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if all_siemens && !siemens_release_family_verified(group) {
        return hold(
            "siemens_classic_unverified_model_or_software",
            1.0,
            [(
                "siemens_model_software_not_release_verified",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if all_philips && !philips_release_family_verified(group) {
        return hold(
            "philips_classic_unverified_model_or_software",
            1.0,
            [(
                "philips_model_software_not_release_verified",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if all_philips && !group.all_philips_classic_private_metadata_contract_verified {
        return hold(
            "philips_classic_private_metadata_contract_unverified",
            1.0,
            [(
                "philips_private_scientific_metadata_not_release_verified",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if all_philips && !group.philips_dynamic_timing_contract_verified {
        return hold(
            "philips_dynamic_timing_contract_unverified",
            1.0,
            [(
                "philips_dynamic_timing_series_contract_failed",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if group
        .burned_in_annotations
        .iter()
        .any(|value| !value.eq_ignore_ascii_case("NO"))
    {
        return hold(
            "burned_in_annotation",
            1.0,
            [(
                "burned_in_annotation_declared_or_unknown",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if group.overlay_or_graphics {
        return hold(
            "overlay_or_graphics",
            1.0,
            [("overlay_or_graphics_present", "dicom_header", "contradicts")],
        );
    }
    if group.has_extended_offset_table {
        return hold(
            "encapsulated_extended_offset_table_unsupported",
            1.0,
            [(
                "extended_offset_table_requires_validated_pixel_pairing",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    let image_type = lower_join(&group.image_types);
    let scanning = lower_join(&group.scanning_sequences);
    let sequence = header
        .sequence_name
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let local_text = group.local_protocol_texts.join(" ").to_ascii_lowercase();
    let all_text = format!("{image_type} {scanning} {sequence} {local_text}");

    if group.burned_in_annotation_missing && !group.all_missing_bia_instances_original_primary {
        return hold(
            "burned_in_annotation_not_declared_unsafe_image_type",
            1.0,
            [(
                "burned_in_annotation_not_declared_without_original_primary",
                "dicom_header",
                "contradicts",
            )],
        );
    }

    if all_siemens
        && (!image_type.contains("mosaic")
            || !group.siemens_csa_image_header_present
            || !group.all_siemens_csa_image_headers_sanitizable)
    {
        return hold(
            "siemens_classic_mosaic_requires_safe_csa",
            1.0,
            [(
                "siemens_mosaic_private_geometry_not_exported",
                "dicom_header",
                "contradicts",
            )],
        );
    }

    if contains_any(
        &image_type,
        &[
            "derived",
            "secondary",
            "adc",
            "tracew",
            "fa map",
            "overlay",
            "graphic",
            "presentation",
        ],
    ) || contains_any(&all_text, &["screensave", "secondary capture", "derived"])
    {
        return hold(
            "derived_image",
            0.99,
            [("derived_or_secondary", "dicom_header", "contradicts")],
        );
    }
    if group.diffusion_context
        || contains_any(&all_text, &["diffusion", " dwi", "dti", "b0 map", "tracew"])
    {
        return hold(
            "diffusion",
            0.99,
            [("diffusion_detected", "dicom_header", "contradicts")],
        );
    }
    if group.asl_context
        || contains_any(
            &all_text,
            &[" arterial spin", " asl", "pcasl", "pasl", "perfusion"],
        )
    {
        return hold(
            "asl_or_perfusion",
            0.99,
            [("asl_or_perfusion_detected", "dicom_header", "contradicts")],
        );
    }
    if contains_any(
        &all_text,
        &[
            "fieldmap",
            "field map",
            " fmap",
            "topup",
            "pepolar",
            "blip",
            "gre_field",
            "b0map",
            "se ap",
            "se pa",
        ],
    ) {
        return hold(
            "fieldmap",
            0.98,
            [("fieldmap_detected", "dicom_header", "contradicts")],
        );
    }
    if contains_any(&all_text, &["sbref", "single band ref", "single-band ref"])
        || image_type.contains("sbref")
    {
        return hold(
            "sbref",
            0.99,
            [("sbref_detected", "dicom_header", "contradicts")],
        );
    }
    if contains_any(
        &all_text,
        &[
            "localizer",
            "scout",
            "survey",
            "locator",
            "three plane",
            "3-plane",
        ],
    ) {
        return hold(
            "localizer",
            0.99,
            [("localizer_detected", "dicom_header", "contradicts")],
        );
    }
    if contains_any(
        &all_text,
        &[
            "mprage",
            "mp-rage",
            " t1",
            "t1w",
            " t2",
            "t2w",
            "flair",
            "spgr",
            "bravo",
            "structural",
            "anatomical",
        ],
    ) || has_token(&all_text, "space")
    {
        return hold(
            "structural",
            0.98,
            [("structural_detected", "dicom_header", "contradicts")],
        );
    }
    if contains_any(
        &all_text,
        &[
            "spectro",
            "mrs",
            "angiograph",
            "tof",
            "swi",
            "susceptibility",
        ],
    ) {
        return hold(
            "unsupported_mr",
            0.95,
            [("unsupported_mr_detected", "dicom_header", "contradicts")],
        );
    }

    if let Err((kind, evidence_code)) = series_timing_contract(group) {
        return hold(kind, 1.0, [(evidence_code, "dicom_header", "contradicts")]);
    }

    let mut score = 0_u8;
    if group
        .scanning_sequences
        .iter()
        .any(|value| value.eq_ignore_ascii_case("EP"))
    {
        score += 2;
        evidence.push(ev(
            "echo_planar_scanning_sequence",
            "dicom_header",
            "supports",
        ));
    }
    if header
        .echo_planar_pulse_sequence
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("YES"))
    {
        score += 2;
        evidence.push(ev("echo_planar_pulse_sequence", "dicom_header", "supports"));
    }
    if contains_any(&image_type, &["epi", "bold", "fmri"]) {
        score += 2;
        evidence.push(ev("functional_image_type", "dicom_header", "supports"));
    }
    if contains_any(&sequence, &["ep2d", "epfid", "epi", "bold"]) {
        score += 2;
        evidence.push(ev("echo_planar_sequence", "dicom_header", "supports"));
    }
    if contains_any(
        &local_text,
        &[
            "bold",
            "fmri",
            "functional",
            "rest",
            "task",
            "movie",
            "retinotopy",
        ],
    ) {
        score += 3;
        evidence.push(ev("functional_protocol_label", "dicom_header", "supports"));
    }
    if header
        .repetition_time_ms
        .is_some_and(|tr| (100.0..=20_000.0).contains(&tr))
    {
        score += 1;
        evidence.push(ev("functional_tr_range", "dicom_header", "supports"));
    }
    if header
        .number_of_temporal_positions
        .is_some_and(|count| count >= 10)
    {
        score += 2;
        evidence.push(ev(
            "multiple_temporal_positions",
            "dicom_header",
            "supports",
        ));
    }

    // ProtocolName and SeriesDescription are local classification aids only:
    // the de-identified archive intentionally removes both. Acceptance must
    // therefore have a strong functional signal retained in the uploaded
    // DICOM headers, not only a promising scanner-console label.
    let strong_functional_evidence = group
        .scanning_sequences
        .iter()
        .any(|value| value.eq_ignore_ascii_case("EP"))
        || header
            .echo_planar_pulse_sequence
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("YES"))
        || contains_any(&image_type, &["epi", "bold", "fmri"])
        || contains_any(&sequence, &["ep2d", "epfid", "epi", "bold"]);
    let enhanced_temporal_structure = header
        .sop_class_uid
        .as_deref()
        .is_some_and(|uid| uid == "1.2.840.10008.5.1.4.1.1.4.1")
        && group.has_per_frame_functional_groups
        && header.number_of_frames.is_some_and(|count| count >= 10);
    let temporal_evidence = header
        .number_of_temporal_positions
        .is_some_and(|count| count >= 10)
        || group.temporal_position_identifiers.len() >= 10
        || group.acquisition_numbers.len() >= 10
        || enhanced_temporal_structure;

    if score >= 8 && strong_functional_evidence && temporal_evidence {
        Classification {
            decision: ClassificationDecision::Accepted,
            kind: "functional_epi_candidate".into(),
            confidence: (0.50 + f64::from(score) * 0.05).min(0.95),
            evidence,
        }
    } else {
        evidence.push(ev(
            "insufficient_functional_epi_header_evidence",
            "dicom_header",
            "contradicts",
        ));
        Classification {
            decision: ClassificationDecision::Held,
            kind: "insufficient_functional_epi_header_evidence".into(),
            confidence: 1.0,
            evidence,
        }
    }
}

fn series_timing_contract(
    group: &SeriesGroup,
) -> std::result::Result<(), (&'static str, &'static str)> {
    let repetition_times = if group.instances.is_empty() {
        vec![group.representative.repetition_time_ms]
    } else {
        group
            .instances
            .iter()
            .map(|instance| instance.repetition_time_ms)
            .collect::<Vec<_>>()
    };
    let echo_times = if group.instances.is_empty() {
        vec![group.representative.echo_time_ms]
    } else {
        group
            .instances
            .iter()
            .map(|instance| instance.echo_time_ms)
            .collect::<Vec<_>>()
    };
    if repetition_times.iter().any(Option::is_none) {
        return Err(("missing_repetition_time", "missing_tr_in_series_instance"));
    }
    if echo_times.iter().any(Option::is_none) {
        return Err(("missing_echo_time", "missing_te_in_series_instance"));
    }
    let repetition_times = repetition_times.into_iter().flatten().collect::<Vec<_>>();
    let echo_times = echo_times.into_iter().flatten().collect::<Vec<_>>();
    if repetition_times
        .iter()
        .any(|value| !value.is_finite() || !(100.0..=20_000.0).contains(value))
    {
        return Err((
            "implausible_repetition_time",
            "tr_out_of_range_in_series_instance",
        ));
    }
    if echo_times
        .iter()
        .any(|value| !(value.is_finite() && *value > 0.0 && *value <= 2_000.0))
    {
        return Err((
            "implausible_echo_time",
            "te_out_of_range_in_series_instance",
        ));
    }
    let spread = |values: &[f64]| {
        let minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
        let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        maximum - minimum
    };
    if spread(&repetition_times) > 0.001 {
        return Err((
            "inconsistent_repetition_time",
            "tr_inconsistent_across_series_instances",
        ));
    }
    if spread(&echo_times) > 0.001 {
        return Err((
            "inconsistent_echo_time",
            "te_inconsistent_across_series_instances",
        ));
    }
    Ok(())
}

fn is_supported_mr_sop(value: &str) -> bool {
    matches!(
        value,
        "1.2.840.10008.5.1.4.1.1.4" | "1.2.840.10008.5.1.4.1.1.4.1" | "1.2.840.10008.5.1.4.1.1.4.4"
    )
}

fn is_enhanced_or_legacy_converted_mr_sop(value: &str) -> bool {
    matches!(
        value,
        "1.2.840.10008.5.1.4.1.1.4.1" | "1.2.840.10008.5.1.4.1.1.4.4"
    )
}

fn is_secondary_capture_sop(value: &str) -> bool {
    matches!(
        value,
        "1.2.840.10008.5.1.4.1.1.7"
            | "1.2.840.10008.5.1.4.1.1.7.1"
            | "1.2.840.10008.5.1.4.1.1.7.2"
            | "1.2.840.10008.5.1.4.1.1.7.3"
            | "1.2.840.10008.5.1.4.1.1.7.4"
    )
}

fn is_ge_manufacturer(value: &str) -> bool {
    let value = value.trim().to_ascii_uppercase();
    value == "GE"
        || value.contains("GENERAL ELECTRIC")
        || value.starts_with("GE MEDICAL")
        || value.starts_with("GE HEALTHCARE")
}

fn is_siemens_manufacturer(value: &str) -> bool {
    let value = normalized_family_text(value);
    value == "SIEMENS"
        || value == "SIEMENS HEALTHCARE"
        || value == "SIEMENS HEALTHINEERS"
        || value.starts_with("SIEMENS MEDICAL ")
}

fn is_philips_manufacturer(value: &str) -> bool {
    let value = normalized_family_text(value);
    value == "PHILIPS"
        || value.starts_with("PHILIPS MEDICAL ")
        || value.starts_with("PHILIPS HEALTHCARE ")
}

fn siemens_release_family_verified(group: &SeriesGroup) -> bool {
    !group.model_missing
        && !group.software_versions_missing
        && !group.models.is_empty()
        && !group.software_version_values.is_empty()
        && group.models.iter().all(|value| {
            matches!(
                normalized_family_text(value).as_str(),
                "PRISMA_FIT" | "MAGNETOM PRISMA_FIT"
            )
        })
        && group
            .software_version_values
            .iter()
            .all(|value| normalized_family_text(value) == "SYNGO MR E11")
}

fn philips_release_family_verified(group: &SeriesGroup) -> bool {
    !group.model_missing
        && !group.software_versions_missing
        && !group.models.is_empty()
        && !group.software_version_values.is_empty()
        && group
            .models
            .iter()
            .all(|value| normalized_family_text(value) == "ACHIEVA DSTREAM")
        && group
            .software_version_values
            .iter()
            .all(|value| philips_511_software_versions(value))
}

fn philips_511_software_versions(value: &str) -> bool {
    let versions = value
        .split('\\')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    !versions.is_empty()
        && versions
            .iter()
            .all(|part| matches!(*part, "5.1.1" | "5.1.1.0"))
        && versions.contains(&"5.1.1")
}

fn normalized_family_text(value: &str) -> String {
    value
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

pub fn refine_after_conversion(
    mut classification: Classification,
    signals: &ConversionSignals,
) -> Classification {
    if signals.converted_name_count != 1 {
        return hold(
            "conversion_output_ambiguous",
            1.0,
            [(
                "ambiguous_conversion_output",
                "converter_sidecar",
                "contradicts",
            )],
        );
    }
    if signals.bids_has_diffusion {
        return hold(
            "diffusion",
            1.0,
            [("diffusion_metadata", "converter_sidecar", "contradicts")],
        );
    }
    if signals.bids_has_asl {
        return hold(
            "asl_or_perfusion",
            1.0,
            [("asl_metadata", "converter_sidecar", "contradicts")],
        );
    }
    if !signals.functional_epi_evidence {
        return hold(
            "ambiguous_mr",
            0.95,
            [("missing_echo_planar_evidence", "derived", "contradicts")],
        );
    }
    if signals.dimensions.len() != 4 || signals.volume_count < 10 {
        return hold(
            "not_functional_4d",
            1.0,
            [("insufficient_timepoints", "nifti_header", "contradicts")],
        );
    }
    let Some(tr) = signals.repetition_time_seconds else {
        return hold(
            "missing_repetition_time",
            1.0,
            [("missing_tr", "converter_sidecar", "contradicts")],
        );
    };
    if !(0.1..=20.0).contains(&tr) {
        return hold(
            "implausible_repetition_time",
            1.0,
            [("tr_out_of_range", "converter_sidecar", "contradicts")],
        );
    }
    let Some(te) = signals.echo_time_seconds else {
        return hold(
            "missing_echo_time",
            1.0,
            [("missing_te", "converter_sidecar", "contradicts")],
        );
    };
    if !(0.0 < te && te <= 2.0) {
        return hold(
            "implausible_echo_time",
            1.0,
            [("te_out_of_range", "converter_sidecar", "contradicts")],
        );
    }
    classification.confidence = classification.confidence.max(0.98);
    classification.kind = "functional_epi".into();
    classification
        .evidence
        .push(ev("valid_4d_time_series", "nifti_header", "supports"));
    classification
}

fn hold<I, A, B, C>(kind: &str, confidence: f64, items: I) -> Classification
where
    I: IntoIterator<Item = (A, B, C)>,
    A: AsRef<str>,
    B: AsRef<str>,
    C: AsRef<str>,
{
    Classification {
        decision: ClassificationDecision::Held,
        kind: kind.into(),
        confidence,
        evidence: items
            .into_iter()
            .map(|(code, source, effect)| ev(code.as_ref(), source.as_ref(), effect.as_ref()))
            .collect(),
    }
}

fn ev(code: &str, source: &str, effect: &str) -> ClassificationEvidence {
    ClassificationEvidence {
        code: code.into(),
        source: source.into(),
        effect: effect.into(),
    }
}

fn lower_join(values: &[String]) -> String {
    values.join(" ").to_ascii_lowercase()
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn has_token(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| token == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dicom::{DicomHeader, SeriesInstance};

    fn timing_instance(tr_ms: Option<f64>, te_ms: Option<f64>) -> SeriesInstance {
        SeriesInstance {
            path: std::path::PathBuf::from("fixture.dcm"),
            instance_number: Some(1),
            sop_instance_uid: "1.2.3.4.5".into(),
            trigger_time_ms: None,
            philips_dynamic_scan_begin_time_seconds: None,
            philips_dynamic_timing_tag_present: false,
            temporal_position_identifier: Some(1),
            number_of_temporal_positions: Some(120),
            philips_number_of_slices: None,
            image_position_patient: vec![0.0, 0.0, 0.0],
            acquisition_number: Some(1),
            repetition_time_ms: tr_ms,
            echo_time_ms: te_ms,
        }
    }

    fn group(mut header: DicomHeader) -> SeriesGroup {
        if header.manufacturer.is_none() {
            header.manufacturer = Some("SIEMENS".into());
        }
        if header.model.is_none() {
            header.model = Some("Prisma_fit".into());
        }
        if header.software_versions.is_none() {
            header.software_versions = Some("syngo MR E11".into());
        }
        if header.echo_time_ms.is_none() {
            header.echo_time_ms = Some(30.0);
        }
        if !header
            .image_type
            .iter()
            .any(|value| value.eq_ignore_ascii_case("MOSAIC"))
        {
            header.image_type.push("MOSAIC".into());
        }
        header.siemens_csa_image_header_present = true;
        header.siemens_csa_image_header_sanitizable = true;
        let sop_class_uid = header
            .sop_class_uid
            .clone()
            .unwrap_or_else(|| "1.2.840.10008.5.1.4.1.1.4".into());
        let modality = header.modality.clone().unwrap_or_else(|| "MR".into());
        let image_types = header.image_type.clone();
        let scanning_sequences = header.scanning_sequence.clone();
        let sequence_variants = header.sequence_variant.clone();
        let scan_options = header.scan_options.clone();
        let local_protocol_texts = vec![header.local_protocol_text()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect();
        let burned_in_annotations = header
            .burned_in_annotation
            .iter()
            .cloned()
            .chain(
                header
                    .burned_in_annotation
                    .is_none()
                    .then(|| "NO".to_owned()),
            )
            .collect();
        let diffusion_context = header.diffusion_b_value.is_some_and(|value| value > 1.0);
        let asl_context = header.asl_technique.is_some();
        SeriesGroup {
            study_uid: "1.2.3".into(),
            series_uid: "1.2.3.4".into(),
            representative: header.clone(),
            files: vec![],
            instances: vec![],
            duplicate_sop_instance_uid: false,
            inconsistent_subject: false,
            inconsistent_metadata: false,
            manufacturers: header.manufacturer.iter().cloned().collect(),
            manufacturer_missing: false,
            models: header.model.iter().cloned().collect(),
            model_missing: header.model.is_none(),
            software_version_values: header.software_versions.iter().cloned().collect(),
            software_versions_missing: header.software_versions.is_none(),
            sop_class_uids: vec![sop_class_uid],
            modalities: vec![modality],
            image_types,
            scanning_sequences,
            sequence_variants,
            scan_options,
            local_protocol_texts,
            burned_in_annotations,
            burned_in_annotation_missing: header.burned_in_annotation.is_none(),
            all_missing_bia_instances_original_primary: header.burned_in_annotation.is_some() || {
                let image_type = &header.image_type;
                let has = |expected: &str| {
                    image_type
                        .iter()
                        .any(|value| value.eq_ignore_ascii_case(expected))
                };
                has("ORIGINAL") && has("PRIMARY") && !has("DERIVED") && !has("SECONDARY")
            },
            siemens_csa_image_header_present: header.siemens_csa_image_header_present,
            all_siemens_csa_image_headers_sanitizable: header.siemens_csa_image_header_sanitizable,
            philips_dynamic_timing_detected: false,
            philips_dynamic_timing_contract_verified: false,
            all_philips_classic_private_metadata_contract_verified: header
                .philips_classic_private_metadata_contract_verified,
            overlay_or_graphics: header.overlay_or_graphics,
            has_extended_offset_table: header.has_extended_offset_table,
            temporal_position_identifiers: header
                .temporal_position_identifier
                .into_iter()
                .collect(),
            acquisition_numbers: header.acquisition_number.into_iter().collect(),
            has_per_frame_functional_groups: header.has_per_frame_functional_groups,
            diffusion_context,
            asl_context,
        }
    }

    #[test]
    fn generic_epi_without_functional_or_temporal_evidence_is_held() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            repetition_time_ms: Some(800.0),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(
            classification.kind,
            "insufficient_functional_epi_header_evidence"
        );
    }

    #[test]
    fn diffusion_never_passes_even_when_epi() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_diff".into()),
            diffusion_b_value: Some(1_000.0),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.kind, "diffusion");
        assert_eq!(classification.decision, ClassificationDecision::Held);
    }

    #[test]
    fn zero_diffusion_b_value_does_not_exclude_functional_epi() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec![
                "ORIGINAL".into(),
                "PRIMARY".into(),
                "EPI".into(),
                "BOLD".into(),
            ],
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_bold".into()),
            series_description: Some("rest bold".into()),
            repetition_time_ms: Some(2_000.0),
            number_of_temporal_positions: Some(120),
            diffusion_b_value: Some(0.0),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
    }

    #[test]
    fn every_accepted_instance_requires_consistent_in_range_tr_and_te() {
        let header = DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into(), "BOLD".into()],
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_bold".into()),
            series_description: Some("rest bold".into()),
            repetition_time_ms: Some(2_000.0),
            echo_time_ms: Some(30.0),
            number_of_temporal_positions: Some(120),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        };

        let mut missing = group(header.clone());
        missing.instances = vec![
            timing_instance(Some(2_000.0), Some(30.0)),
            timing_instance(Some(2_000.0), None),
        ];
        assert_eq!(classify_header(&missing).kind, "missing_echo_time");

        let mut inconsistent_tr = group(header.clone());
        inconsistent_tr.instances = vec![
            timing_instance(Some(2_000.0), Some(30.0)),
            timing_instance(Some(2_000.01), Some(30.0)),
        ];
        assert_eq!(
            classify_header(&inconsistent_tr).kind,
            "inconsistent_repetition_time"
        );

        let mut inconsistent_te = group(header);
        inconsistent_te.instances = vec![
            timing_instance(Some(2_000.0), Some(30.0)),
            timing_instance(Some(2_000.0), Some(30.01)),
        ];
        assert_eq!(
            classify_header(&inconsistent_te).kind,
            "inconsistent_echo_time"
        );
    }

    #[test]
    fn protocol_label_alone_cannot_accept_a_series_removed_from_uploaded_headers() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into()],
            series_description: Some("resting state functional bold".into()),
            protocol_name: Some("task movie fmri".into()),
            repetition_time_ms: Some(800.0),
            echo_time_ms: Some(30.0),
            number_of_temporal_positions: Some(300),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert!(
            classification
                .evidence
                .iter()
                .any(|evidence| evidence.code == "functional_protocol_label")
        );
    }

    #[test]
    fn accepted_evidence_codes_match_the_server_contract() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into(), "BOLD".into()],
            scanning_sequence: vec!["EP".into()],
            echo_planar_pulse_sequence: Some("YES".into()),
            sequence_name: Some("ep2d_bold".into()),
            series_description: Some("rest functional".into()),
            repetition_time_ms: Some(800.0),
            echo_time_ms: Some(30.0),
            number_of_temporal_positions: Some(300),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
        assert_eq!(
            classification
                .evidence
                .iter()
                .map(|evidence| evidence.code.as_str())
                .collect::<Vec<_>>(),
            vec![
                "echo_planar_scanning_sequence",
                "echo_planar_pulse_sequence",
                "functional_image_type",
                "echo_planar_sequence",
                "functional_protocol_label",
                "functional_tr_range",
                "multiple_temporal_positions",
            ]
        );
    }

    #[test]
    fn unknown_or_missing_scanner_manufacturer_never_passes_the_release_gate() {
        let header = DicomHeader {
            modality: Some("MR".into()),
            image_type: vec![
                "ORIGINAL".into(),
                "PRIMARY".into(),
                "EPI".into(),
                "BOLD".into(),
            ],
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_bold".into()),
            series_description: Some("rest bold".into()),
            repetition_time_ms: Some(2_000.0),
            number_of_temporal_positions: Some(120),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        };
        let mut unknown = group(header.clone());
        unknown.representative.manufacturer = Some("FIXTURE_VENDOR".into());
        unknown.manufacturers = vec!["FIXTURE_VENDOR".into()];
        assert_eq!(
            classify_header(&unknown).kind,
            "unsupported_scanner_manufacturer"
        );

        let mut missing = group(header);
        missing.representative.manufacturer = None;
        missing.manufacturers.clear();
        missing.manufacturer_missing = true;
        assert_eq!(
            classify_header(&missing).kind,
            "missing_scanner_manufacturer"
        );
    }

    #[test]
    fn measured_manufacturer_without_complete_release_identity_is_held() {
        let header = DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into(), "BOLD".into()],
            scanning_sequence: vec!["EP".into()],
            number_of_temporal_positions: Some(120),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        };
        let mut missing_model = group(header.clone());
        missing_model.models.clear();
        missing_model.model_missing = true;
        assert_eq!(
            classify_header(&missing_model).kind,
            "siemens_classic_unverified_model_or_software"
        );

        let mut missing_software = group(header);
        missing_software.software_version_values.clear();
        missing_software.software_versions_missing = true;
        assert_eq!(
            classify_header(&missing_software).kind,
            "siemens_classic_unverified_model_or_software"
        );
    }

    #[test]
    fn post_conversion_requires_real_time_series() {
        let refined = refine_after_conversion(
            Classification {
                decision: ClassificationDecision::Accepted,
                kind: "candidate".into(),
                confidence: 0.8,
                evidence: vec![],
            },
            &ConversionSignals {
                dimensions: vec![64, 64, 32, 1],
                volume_count: 1,
                repetition_time_seconds: Some(1.0),
                converted_name_count: 1,
                ..Default::default()
            },
        );
        assert_eq!(refined.decision, ClassificationDecision::Held);
    }

    #[test]
    fn secondary_capture_sop_class_is_never_accepted() {
        let classification = classify_header(&group(DicomHeader {
            sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.7".into()),
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            repetition_time_ms: Some(800.0),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(classification.kind, "secondary_capture");
    }

    #[test]
    fn missing_burned_in_annotation_is_allowed_only_after_original_primary_epi_gates() {
        let mut series = group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec![
                "ORIGINAL".into(),
                "PRIMARY".into(),
                "EPI".into(),
                "BOLD".into(),
            ],
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_bold".into()),
            series_description: Some("rest bold".into()),
            repetition_time_ms: Some(800.0),
            number_of_temporal_positions: Some(300),
            ..Default::default()
        });
        series.burned_in_annotations.clear();
        series.burned_in_annotation_missing = true;
        let classification = classify_header(&series);
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
    }

    #[test]
    fn declared_burned_in_annotation_is_always_held() {
        let mut series = group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into(), "BOLD".into()],
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_bold".into()),
            series_description: Some("rest bold".into()),
            repetition_time_ms: Some(800.0),
            number_of_temporal_positions: Some(300),
            burned_in_annotation: Some("YES".into()),
            ..Default::default()
        });
        series.burned_in_annotations = vec!["YES".into()];
        series.burned_in_annotation_missing = false;
        let classification = classify_header(&series);
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(classification.kind, "burned_in_annotation");
    }
}
