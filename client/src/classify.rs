use std::collections::HashSet;

use crate::{
    dicom::{
        ENHANCED_MR_IMAGE_STORAGE_UID, LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID, SeriesGroup,
        dicom_instance_count_supported, dicom_instance_size_supported,
        dicom_series_uncompressed_size_supported, supported_mr_image_sop_class,
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
            "dicom_instance_exceeds_64_gib",
            1.0,
            [(
                "dicom_instance_exceeds_64_gib",
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
        .any(|value| !supported_mr_image_sop_class(value))
    {
        return hold(
            "unsupported_sop_class",
            1.0,
            [("unsupported_sop_class", "dicom_header", "contradicts")],
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
    // Scanner identity is provenance, not eligibility. Unknown, absent, or
    // previously unmeasured manufacturer/model/software values must never
    // prevent standards-conformant functional MR from reaching the archive.
    let all_philips = !group.manufacturers.is_empty()
        && group
            .manufacturers
            .iter()
            .all(|value| is_philips_manufacturer(value));
    let classic_philips = all_philips
        && group.sop_class_uids.len() == 1
        && group.sop_class_uids[0] == crate::dicom::MR_IMAGE_STORAGE_UID;
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
    let image_type = lower_join(&group.image_types);
    let scanning = lower_join(&group.scanning_sequences);
    let sequence = header
        .sequence_name
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let local_text = group.local_protocol_texts.join(" ").to_ascii_lowercase();
    let all_text = format!("{image_type} {scanning} {sequence} {local_text}");

    if group.burned_in_annotation_missing
        && group.sop_class_uids.iter().any(|uid| {
            matches!(
                uid.as_str(),
                ENHANCED_MR_IMAGE_STORAGE_UID | LEGACY_CONVERTED_ENHANCED_MR_IMAGE_STORAGE_UID
            )
        })
    {
        return hold(
            "enhanced_mr_burned_in_annotation_not_declared",
            1.0,
            [(
                "enhanced_mr_missing_required_burned_in_annotation_no",
                "dicom_header",
                "contradicts",
            )],
        );
    }
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

    if image_type.contains("mosaic")
        && (!group.siemens_csa_image_header_present
            || !group.all_siemens_csa_image_headers_sanitizable)
    {
        return hold(
            "classic_mosaic_requires_safe_csa",
            1.0,
            [(
                "mosaic_private_geometry_not_exported",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if group.uih_grid_or_vframe
        && (!group.uih_grid_slice_count_present || !group.all_uih_grid_slice_counts_verified)
    {
        let evidence_code = if group.uih_grid_slice_count_present {
            "uih_grid_slice_count_malformed"
        } else {
            "uih_grid_slice_count_missing"
        };
        return hold(
            "uih_grid_slice_count_missing_or_invalid",
            1.0,
            [(evidence_code, "dicom_private_header", "contradicts")],
        );
    }

    let derived = contains_any(
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
    ) || contains_any(&all_text, &["screensave", "secondary capture", "derived"]);
    let diffusion = group.diffusion_context
        || contains_any(&all_text, &["diffusion", " dwi", "dti", "b0 map", "tracew"]);
    // ASL has a dedicated scientific-metadata contract. DSC/DCE and other
    // contrast perfusion acquisitions do not carry ASL label/control macros,
    // so a generic "perfusion" protocol label must remain independently
    // archiveable instead of being failed against the ASL contract.
    let asl_perfusion =
        group.asl_context || contains_any(&all_text, &[" arterial spin", " asl", "pcasl", "pasl"]);
    let perfusion = !asl_perfusion && contains_any(&all_text, &["perfusion", " dsc", " dce"]);
    // ORIGINAL or explicitly MIXED data remain acquired scans even when a
    // vendor also emits a derived-looking component. Pure DERIVED/SECONDARY
    // ADC, FA, and trace-weighted products are archival derivatives and do
    // not need to masquerade as reconstructable acquired diffusion.
    let acquired_or_mixed = contains_any(&image_type, &["original", "mixed"]);
    let derived_diffusion = diffusion && derived && !acquired_or_mixed;
    let acquired_diffusion = diffusion && !derived_diffusion;
    if acquired_diffusion
        && (!group.diffusion_metadata_present || !group.all_diffusion_metadata_contracts_verified)
    {
        return hold(
            "diffusion_scientific_metadata_incomplete",
            1.0,
            [(
                "diffusion_b_value_or_direction_contract_missing_or_invalid",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if asl_perfusion && (!group.asl_metadata_present || !group.all_asl_metadata_contracts_verified)
    {
        return hold(
            "asl_scientific_metadata_incomplete",
            1.0,
            [(
                if classic_philips && group.philips_private_asl_label_type_present {
                    "philips_private_asl_label_contract_missing_or_invalid"
                } else {
                    "asl_technique_or_label_context_contract_missing_or_invalid"
                },
                "dicom_header",
                "contradicts",
            )],
        );
    }
    // A series cannot safely be routed as both acquired diffusion and ASL.
    // Both contracts can be individually valid (for example after a scanner
    // exports stale supplemental fields), but selecting one route would then
    // discard a contradictory scientific interpretation. Keep that ambiguity
    // local instead of producing an archive the processor must reject.
    if diffusion && asl_perfusion {
        return hold(
            "ambiguous_diffusion_and_asl_scientific_context",
            1.0,
            [(
                "diffusion_and_asl_scientific_context_conflict",
                "dicom_header",
                "contradicts",
            )],
        );
    }
    if acquired_diffusion {
        evidence.push(ev(
            "diffusion_scientific_metadata_contract_verified",
            "dicom_header",
            "supports",
        ));
    }
    if asl_perfusion {
        evidence.push(ev(
            "asl_scientific_metadata_contract_verified",
            "dicom_header",
            "supports",
        ));
    }
    let fieldmap = contains_any(
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
    );
    let sbref = contains_any(&all_text, &["sbref", "single band ref", "single-band ref"])
        || image_type.contains("sbref");
    let localizer = contains_any(
        &all_text,
        &[
            "localizer",
            "scout",
            "survey",
            "locator",
            "three plane",
            "3-plane",
        ],
    );
    let structural_t1w = contains_any(&all_text, &["mprage", "mp-rage", "t1w", "spgr", "bravo"])
        || has_token(&all_text, "t1");
    let structural_t2w = contains_any(&all_text, &["t2w", "flair"])
        || has_token(&all_text, "t2")
        || has_token(&all_text, "space");
    let structural_other = contains_any(&all_text, &["structural", "anatomical"]);

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
        .is_some_and(|count| count >= 2)
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
    let repeated_slice_positions = repeated_slice_time_series(group);
    let classic_mosaic_time_series = image_type.contains("mosaic") && group.files.len() >= 2;
    let temporal_evidence = header
        .number_of_temporal_positions
        .is_some_and(|count| count >= 2)
        || group.temporal_position_identifiers.len() >= 2
        || group.acquisition_numbers.len() >= 2
        || repeated_slice_positions
        || classic_mosaic_time_series;

    if temporal_evidence
        && !evidence
            .iter()
            .any(|item| item.code == "multiple_temporal_positions")
    {
        evidence.push(ev(
            "multiple_temporal_positions",
            "dicom_header",
            "supports",
        ));
    }

    let functional_timing = if strong_functional_evidence && temporal_evidence {
        series_timing_contract(group).err()
    } else {
        None
    };
    if let Some((_, evidence_code)) = functional_timing {
        evidence.push(ev(evidence_code, "dicom_header", "limits_processing"));
    }

    let (kind, confidence, kind_evidence) = if derived_diffusion {
        ("derived_mr", 0.99, "derived_or_secondary")
    } else if diffusion {
        ("diffusion", 0.99, "diffusion_detected")
    } else if asl_perfusion {
        ("asl_perfusion", 0.99, "asl_or_perfusion_detected")
    } else if perfusion {
        ("perfusion", 0.98, "perfusion_detected")
    } else if fieldmap {
        ("fieldmap", 0.98, "fieldmap_detected")
    } else if sbref {
        ("sbref", 0.99, "sbref_detected")
    } else if localizer {
        ("localizer", 0.99, "localizer_detected")
    } else if structural_t1w {
        ("structural_t1w", 0.98, "structural_t1w_detected")
    } else if structural_t2w {
        ("structural_t2w", 0.98, "structural_t2w_detected")
    } else if structural_other {
        ("structural_other", 0.95, "structural_detected")
    } else if derived {
        ("derived_mr", 0.99, "derived_or_secondary")
    } else if strong_functional_evidence && temporal_evidence && functional_timing.is_none() {
        (
            "functional_epi",
            (0.90 + f64::from(score.min(5)) * 0.01).min(0.95),
            "functional_epi_confirmed",
        )
    } else {
        ("other_mr", 0.90, "supported_mr_image")
    };
    if classic_philips
        && group.philips_private_pixel_scaling_present
        && !group.all_philips_pixel_scaling_contracts_verified
    {
        return hold(
            "philips_private_scientific_metadata_incomplete",
            1.0,
            [(
                "philips_private_scaling_malformed_without_public_fallback",
                "dicom_private_header",
                "contradicts",
            )],
        );
    }
    if classic_philips
        && group.philips_private_pixel_scaling_incomplete
        && group.all_philips_pixel_scaling_contracts_verified
    {
        evidence.push(ev(
            "philips_private_metadata_dropped_public_pixel_scaling_retained",
            "dicom_header",
            "limits_processing",
        ));
    }
    if kind != "functional_epi" && !evidence.iter().any(|item| item.code == kind_evidence) {
        evidence.push(ev(kind_evidence, "dicom_header", "supports"));
    }
    Classification {
        decision: ClassificationDecision::Accepted,
        kind: kind.into(),
        confidence,
        evidence,
    }
}

fn repeated_slice_time_series(group: &SeriesGroup) -> bool {
    repeated_positions(
        group.instances.iter().map(|instance| {
            if instance.image_position_patient.len() != 3
                || instance
                    .image_position_patient
                    .iter()
                    .any(|value| !value.is_finite())
            {
                return None;
            }
            Some([
                (instance.image_position_patient[0] * 1_000_000.0).round() as i64,
                (instance.image_position_patient[1] * 1_000_000.0).round() as i64,
                (instance.image_position_patient[2] * 1_000_000.0).round() as i64,
            ])
        }),
        group.instances.len(),
    )
}

fn repeated_positions(
    positions: impl Iterator<Item = Option<[i64; 3]>>,
    total_instances: usize,
) -> bool {
    let mut unique_positions = HashSet::<[i64; 3]>::new();
    let mut measured_instances = 0_usize;
    for position in positions {
        let Some(position) = position else {
            continue;
        };
        measured_instances += 1;
        unique_positions.insert(position);
    }
    !unique_positions.is_empty()
        && measured_instances == total_instances
        && measured_instances / unique_positions.len() >= 2
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
    Ok(())
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

fn is_philips_manufacturer(value: &str) -> bool {
    let value = normalized_family_text(value);
    value == "PHILIPS" || value == "PHILIPS MEDICAL SYSTEMS" || value.starts_with("PHILIPS ")
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
        let diffusion_context = header.diffusion_b_value.is_some_and(|value| value > 1.0)
            || header.public_diffusion_semantic_evidence
            || header.reviewed_private_diffusion_semantic_evidence;
        let asl_context = header.asl_technique.is_some()
            || header.reviewed_private_asl_metadata_present
            || header.ge_asl_supplemental_metadata_present;
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
            philips_private_pixel_scaling_present: header.philips_private_pixel_scaling_present,
            philips_private_pixel_scaling_incomplete: header.philips_private_pixel_scaling_present
                && !header.philips_private_pixel_scaling_usable,
            philips_private_asl_label_type_present: header.philips_private_asl_label_type_present,
            all_philips_pixel_scaling_contracts_verified: header
                .philips_private_pixel_scaling_usable
                || header.public_pixel_scaling_contract_verified,
            uih_grid_or_vframe: header.uih_grid_or_vframe,
            uih_grid_slice_count_present: header.uih_grid_slice_count_present,
            all_uih_grid_slice_counts_verified: !header.uih_grid_or_vframe
                || header.uih_grid_slice_count_verified,
            diffusion_metadata_present: header.public_diffusion_metadata_present
                || header.reviewed_private_diffusion_metadata_present,
            all_diffusion_metadata_contracts_verified: header.diffusion_metadata_contract_verified,
            asl_metadata_present: header.public_asl_metadata_present
                || header.reviewed_private_asl_metadata_present,
            all_asl_metadata_contracts_verified: header.asl_metadata_contract_verified,
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
    fn packed_geometry_is_required_without_trusting_manufacturer_identity() {
        let mut mosaic = group(DicomHeader {
            modality: Some("MR".into()),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        });
        mosaic.manufacturers.clear();
        mosaic.manufacturer_missing = true;
        mosaic.representative.manufacturer = None;
        mosaic.siemens_csa_image_header_present = false;
        mosaic.all_siemens_csa_image_headers_sanitizable = false;

        let classification = classify_header(&mosaic);
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(classification.kind, "classic_mosaic_requires_safe_csa");

        let mut grid = group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into(), "GRID".into()],
            burned_in_annotation: Some("NO".into()),
            uih_grid_or_vframe: true,
            uih_grid_slice_count_present: false,
            uih_grid_slice_count_verified: false,
            ..Default::default()
        });
        grid.image_types.retain(|value| value != "MOSAIC");
        grid.representative
            .image_type
            .retain(|value| value != "MOSAIC");
        grid.manufacturers.clear();
        grid.manufacturer_missing = true;
        grid.representative.manufacturer = None;

        let classification = classify_header(&grid);
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(
            classification.kind,
            "uih_grid_slice_count_missing_or_invalid"
        );
    }

    #[test]
    fn generic_epi_without_temporal_evidence_is_archived_as_other_mr() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            repetition_time_ms: Some(800.0),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
        assert_eq!(classification.kind, "other_mr");
    }

    #[test]
    fn diffusion_is_accepted_for_archive_verification() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_diff".into()),
            diffusion_b_value: Some(1_000.0),
            public_diffusion_metadata_present: true,
            diffusion_metadata_contract_verified: true,
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.kind, "diffusion");
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
    }

    #[test]
    fn diffusion_and_asl_fail_closed_without_scientific_metadata_contracts() {
        let diffusion = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            sequence_name: Some("ep2d_diff".into()),
            diffusion_b_value: Some(1_000.0),
            public_diffusion_metadata_present: true,
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(diffusion.decision, ClassificationDecision::Held);
        assert_eq!(diffusion.kind, "diffusion_scientific_metadata_incomplete");

        let asl = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            sequence_name: Some("pcasl".into()),
            asl_technique: Some("PSEUDOCONTINUOUS".into()),
            public_asl_metadata_present: true,
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(asl.decision, ClassificationDecision::Held);
        assert_eq!(asl.kind, "asl_scientific_metadata_incomplete");
    }

    #[test]
    fn diffusion_and_asl_with_complete_contracts_are_held_as_ambiguous() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            sequence_name: Some("ep2d_diff_pcasl".into()),
            diffusion_b_value: Some(1_000.0),
            public_diffusion_metadata_present: true,
            public_diffusion_semantic_evidence: true,
            diffusion_metadata_contract_verified: true,
            asl_technique: Some("PSEUDOCONTINUOUS".into()),
            public_asl_metadata_present: true,
            asl_metadata_contract_verified: true,
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));

        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(
            classification.kind,
            "ambiguous_diffusion_and_asl_scientific_context"
        );
        assert_eq!(
            classification.evidence[0].code,
            "diffusion_and_asl_scientific_context_conflict"
        );
    }

    #[test]
    fn non_asl_perfusion_is_archived_without_an_asl_label_contract() {
        for sequence_name in ["ep2d_DSC_perfusion", "T1_DCE_perfusion"] {
            let classification = classify_header(&group(DicomHeader {
                modality: Some("MR".into()),
                sequence_name: Some(sequence_name.into()),
                burned_in_annotation: Some("NO".into()),
                ..Default::default()
            }));
            assert_eq!(classification.decision, ClassificationDecision::Accepted);
            assert_eq!(classification.kind, "perfusion");
            assert!(
                classification
                    .evidence
                    .iter()
                    .any(|item| { item.code == "perfusion_detected" && item.effect == "supports" })
            );
            assert!(
                !classification
                    .evidence
                    .iter()
                    .any(|item| item.code == "asl_scientific_metadata_contract_verified")
            );
        }
    }

    #[test]
    fn derived_diffusion_products_do_not_require_acquired_diffusion_metadata() {
        for derived_term in ["ADC", "FA", "TRACEW"] {
            let classification = classify_header(&group(DicomHeader {
                modality: Some("MR".into()),
                image_type: vec!["DERIVED".into(), "SECONDARY".into(), derived_term.into()],
                sequence_name: Some(format!("derived_{derived_term}")),
                burned_in_annotation: Some("NO".into()),
                ..Default::default()
            }));
            assert_eq!(classification.decision, ClassificationDecision::Accepted);
            assert_eq!(classification.kind, "derived_mr");
            assert!(
                !classification
                    .evidence
                    .iter()
                    .any(|item| { item.code == "diffusion_scientific_metadata_contract_verified" })
            );
        }
    }

    #[test]
    fn mixed_original_diffusion_still_requires_acquired_metadata() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "MIXED".into(), "ADC".into()],
            sequence_name: Some("diffusion_adc_mixed".into()),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(
            classification.kind,
            "diffusion_scientific_metadata_incomplete"
        );
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
    fn private_b0_none_metadata_does_not_reclassify_functional_epi_as_diffusion() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            repetition_time_ms: Some(800.0),
            echo_time_ms: Some(30.0),
            number_of_temporal_positions: Some(120),
            reviewed_private_diffusion_metadata_present: true,
            diffusion_metadata_contract_verified: true,
            reviewed_private_diffusion_semantic_evidence: false,
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
        assert_eq!(classification.kind, "functional_epi");
    }

    #[test]
    fn two_position_short_functional_epi_is_accepted() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into()],
            echo_planar_pulse_sequence: Some("YES".into()),
            repetition_time_ms: Some(1_500.0),
            echo_time_ms: Some(23.0),
            number_of_temporal_positions: Some(2),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
    }

    #[test]
    fn functional_route_requires_consistent_tr_and_valid_te_without_blocking_archive() {
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
        let missing = classify_header(&missing);
        assert_eq!(missing.decision, ClassificationDecision::Accepted);
        assert_eq!(missing.kind, "other_mr");
        assert!(
            missing
                .evidence
                .iter()
                .any(|item| item.code == "missing_te_in_series_instance")
        );

        let mut inconsistent_tr = group(header.clone());
        inconsistent_tr.instances = vec![
            timing_instance(Some(2_000.0), Some(30.0)),
            timing_instance(Some(2_000.01), Some(30.0)),
        ];
        let inconsistent_tr = classify_header(&inconsistent_tr);
        assert_eq!(inconsistent_tr.decision, ClassificationDecision::Accepted);
        assert_eq!(inconsistent_tr.kind, "other_mr");
        assert!(
            inconsistent_tr
                .evidence
                .iter()
                .any(|item| item.code == "tr_inconsistent_across_series_instances")
        );

        let mut multi_echo = group(header);
        multi_echo.instances = vec![
            timing_instance(Some(2_000.0), Some(30.0)),
            timing_instance(Some(2_000.0), Some(12.0)),
        ];
        assert_eq!(
            classify_header(&multi_echo).decision,
            ClassificationDecision::Accepted
        );
    }

    #[test]
    fn protocol_label_alone_cannot_route_a_series_to_functional_processing() {
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
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
        assert_eq!(classification.kind, "other_mr");
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
    fn scanner_manufacturer_is_optional_provenance() {
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
            classify_header(&unknown).decision,
            ClassificationDecision::Accepted
        );

        let mut missing = group(header);
        missing.representative.manufacturer = None;
        missing.manufacturers.clear();
        missing.manufacturer_missing = true;
        assert_eq!(
            classify_header(&missing).decision,
            ClassificationDecision::Accepted
        );
    }

    #[test]
    fn scanner_model_and_software_are_optional_provenance() {
        let header = DicomHeader {
            modality: Some("MR".into()),
            image_type: vec!["ORIGINAL".into(), "PRIMARY".into(), "BOLD".into()],
            scanning_sequence: vec!["EP".into()],
            repetition_time_ms: Some(800.0),
            number_of_temporal_positions: Some(120),
            burned_in_annotation: Some("NO".into()),
            ..Default::default()
        };
        let mut missing_model = group(header.clone());
        missing_model.models.clear();
        missing_model.model_missing = true;
        assert_eq!(
            classify_header(&missing_model).decision,
            ClassificationDecision::Accepted
        );

        let mut missing_software = group(header);
        missing_software.software_version_values.clear();
        missing_software.software_versions_missing = true;
        assert_eq!(
            classify_header(&missing_software).decision,
            ClassificationDecision::Accepted
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

    #[test]
    fn repeated_position_detection_scales_to_the_series_instance_limit() {
        let count = crate::dicom::MAX_DICOM_INSTANCES_PER_SERIES;
        assert!(repeated_positions(
            (0..count).map(|index| Some([(index % (count / 2)) as i64, 0, 0])),
            count,
        ));
        assert!(!repeated_positions(
            (0..count).map(|index| Some([index as i64, 0, 0])),
            count,
        ));
        assert!(!repeated_positions(
            (0..count).map(|index| (index != count - 1).then_some([index as i64, 0, 0])),
            count,
        ));
    }
}
