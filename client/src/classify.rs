use crate::{
    dicom::SeriesGroup,
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
    if group
        .burned_in_annotations
        .iter()
        .any(|value| value.eq_ignore_ascii_case("YES"))
    {
        return hold(
            "burned_in_annotation",
            1.0,
            [("burned_in_annotation", "dicom_header", "contradicts")],
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

    if contains_any(
        &image_type,
        &["derived", "secondary", "adc", "tracew", "fa map"],
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

    if score >= 4 {
        Classification {
            decision: ClassificationDecision::Accepted,
            kind: "functional_epi_candidate".into(),
            confidence: (0.50 + f64::from(score) * 0.05).min(0.95),
            evidence,
        }
    } else {
        if evidence.is_empty() {
            evidence.push(ev("needs_conversion_evidence", "derived", "contradicts"));
        }
        Classification {
            decision: ClassificationDecision::Accepted,
            kind: "ambiguous_mr_candidate".into(),
            confidence: 0.35,
            evidence,
        }
    }
}

fn is_supported_mr_sop(value: &str) -> bool {
    matches!(
        value,
        "1.2.840.10008.5.1.4.1.1.4" | "1.2.840.10008.5.1.4.1.1.4.1" | "1.2.840.10008.5.1.4.1.1.4.4"
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
    use crate::dicom::DicomHeader;

    fn group(header: DicomHeader) -> SeriesGroup {
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
        let burned_in_annotations = header.burned_in_annotation.iter().cloned().collect();
        let diffusion_context = header.diffusion_b_value.is_some();
        let asl_context = header.asl_technique.is_some();
        SeriesGroup {
            study_uid: "1.2.3".into(),
            series_uid: "1.2.3.4".into(),
            representative: header,
            files: vec![],
            inconsistent_subject: false,
            inconsistent_metadata: false,
            sop_class_uids: vec![sop_class_uid],
            modalities: vec![modality],
            image_types,
            scanning_sequences,
            sequence_variants,
            scan_options,
            local_protocol_texts,
            burned_in_annotations,
            diffusion_context,
            asl_context,
        }
    }

    #[test]
    fn accepts_vendor_neutral_functional_epi_candidate() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_bold".into()),
            repetition_time_ms: Some(800.0),
            ..Default::default()
        }));
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
    }

    #[test]
    fn diffusion_never_passes_even_when_epi() {
        let classification = classify_header(&group(DicomHeader {
            modality: Some("MR".into()),
            scanning_sequence: vec!["EP".into()],
            sequence_name: Some("ep2d_diff".into()),
            diffusion_b_value: Some(1_000.0),
            ..Default::default()
        }));
        assert_eq!(classification.kind, "diffusion");
        assert_eq!(classification.decision, ClassificationDecision::Held);
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
}
