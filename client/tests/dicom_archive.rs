mod support;

use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use dicom_core::Tag;
use dicom_object::open_file;
use neuro_sync::{
    archive::{
        ArchiveRequest, DICOM_ARCHIVE_FORMAT, DICOM_METADATA_POLICY_ID,
        DICOM_METADATA_POLICY_VERSION, create_dicom_archive,
    },
    classify::classify_header,
    dicom::discover,
    model::ClassificationDecision,
    pseudonym::Pseudonymizer,
};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use support::{FixtureVendor, FunctionalDicomOptions};

const TEST_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

#[test]
fn siemens_style_archive_is_deterministic_private_and_pixel_exact() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let first_root = directory.path().join("first");
    let second_root = directory.path().join("second");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&first_root).unwrap();
    std::fs::create_dir(&second_root).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::Siemens,
        include_privacy_leaks: true,
        pixel_bytes: 2 * 1024 * 1024,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source.join("one.dcm"), 1, &options);

    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let classification = classify_header(group);
    assert_eq!(
        classification.decision,
        ClassificationDecision::Accepted,
        "classification: {classification:?}"
    );
    assert!(classification.confidence >= 0.9);
    let pseudonymizer = Pseudonymizer::from_base64(TEST_KEY).unwrap();
    let mut streamed_bytes = 0_u64;
    let mut progress_callbacks = 0_u64;
    let first = create_dicom_archive(ArchiveRequest {
        group,
        classification: classification.clone(),
        pseudonymizer: &pseudonymizer,
        bundle_root: &first_root,
        progress: |bytes| {
            streamed_bytes += bytes;
            progress_callbacks += 1;
        },
    })
    .unwrap();
    let second = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &pseudonymizer,
        bundle_root: &second_root,
        progress: |_| {},
    })
    .unwrap();

    assert_eq!(first.bundle_id, second.bundle_id);
    let first_archive = first.archive.as_ref().unwrap();
    let second_archive = second.archive.as_ref().unwrap();
    assert_eq!(first_archive.format, DICOM_ARCHIVE_FORMAT);
    assert_eq!(
        first_archive.deidentification_profile,
        DICOM_METADATA_POLICY_ID
    );
    assert_eq!(
        first_archive.deidentification_profile_version,
        DICOM_METADATA_POLICY_VERSION
    );
    let first_bytes = std::fs::read(&first_archive.object.local_path).unwrap();
    let second_bytes = std::fs::read(&second_archive.object.local_path).unwrap();
    assert_eq!(first_bytes, second_bytes);
    assert_eq!(
        hex::encode(Sha256::digest(&first_bytes)),
        first_archive.object.sha256
    );
    assert!(streamed_bytes >= options.pixel_bytes as u64);
    assert!(
        progress_callbacks > 16,
        "large PixelData should report chunk progress"
    );

    let entries = read_archive(&first_archive.object.local_path);
    assert_eq!(
        entries.keys().cloned().collect::<Vec<_>>(),
        ["dicom/000001.dcm".to_owned(), "manifest.json".to_owned(),]
    );
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest["schema_version"], "1.0.0");
    assert_eq!(manifest["series_archive_id"], first.bundle_id);
    assert_eq!(manifest["dicom_instance_count"], 1);
    assert_eq!(manifest["source"]["model"], "MAGNETOM Prisma_fit");
    assert_eq!(manifest["source"]["software_versions"][0], "Siemens E11");
    assert_eq!(
        manifest["deidentification"]["policy_id"],
        DICOM_METADATA_POLICY_ID
    );
    assert_eq!(
        manifest["deidentification"]["policy_version"],
        DICOM_METADATA_POLICY_VERSION
    );
    assert_eq!(
        manifest["deidentification"]["method"],
        "scaling-neuro-recursive-allowlist-v1"
    );
    let dicom = &entries["dicom/000001.dcm"];
    assert_eq!(manifest["instances"][0]["size_bytes"], dicom.len());
    assert_eq!(
        manifest["instances"][0]["sha256"],
        hex::encode(Sha256::digest(dicom))
    );
    assert_no_fixture_identity_leaks(dicom);

    let extracted = directory.path().join("sanitized.dcm");
    std::fs::write(&extracted, dicom).unwrap();
    let object = open_file(&extracted).unwrap();
    let subject = object
        .element(Tag(0x0010, 0x0020))
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(subject.len(), 24);
    assert_ne!(subject.as_ref(), "FIXTURE-SUBJECT-001");
    assert_eq!(
        object
            .element(Tag(0x0010, 0x0010))
            .unwrap()
            .to_str()
            .unwrap(),
        subject
    );
    assert_eq!(
        object
            .element(Tag(0x0012, 0x0062))
            .unwrap()
            .to_str()
            .unwrap(),
        "YES"
    );
    assert_eq!(
        object
            .element(Tag(0x0028, 0x0301))
            .unwrap()
            .to_str()
            .unwrap(),
        "NO"
    );
    assert_eq!(
        object
            .element(Tag(0x0028, 0x0303))
            .unwrap()
            .to_str()
            .unwrap(),
        "REMOVED"
    );
    assert_eq!(
        object
            .element(Tag(0x0008, 0x0070))
            .unwrap()
            .to_str()
            .unwrap(),
        "SIEMENS"
    );
    assert_eq!(
        object
            .element(Tag(0x0008, 0x1090))
            .unwrap()
            .to_str()
            .unwrap(),
        "MAGNETOM Prisma_fit"
    );
    assert_eq!(
        object
            .element(Tag(0x0018, 0x1020))
            .unwrap()
            .to_str()
            .unwrap(),
        "Siemens E11"
    );
    assert_eq!(
        object
            .element(Tag(0x0018, 0x0024))
            .unwrap()
            .to_str()
            .unwrap(),
        "ep2d_bold"
    );
    assert_eq!(
        object
            .element(Tag(0x0018, 0x1250))
            .unwrap()
            .to_str()
            .unwrap(),
        "HEAD_NECK_64"
    );
    let remapped_sop = object
        .element(Tag(0x0008, 0x0018))
        .unwrap()
        .to_str()
        .unwrap();
    assert!(remapped_sop.starts_with("2.25."));
    assert_eq!(
        object
            .element(Tag(0x0008, 0x0016))
            .unwrap()
            .to_str()
            .unwrap(),
        "1.2.840.10008.5.1.4.1.1.4"
    );
    assert_eq!(
        object
            .meta()
            .media_storage_sop_instance_uid
            .trim_end_matches('\0'),
        remapped_sop
    );
    assert_eq!(
        object
            .meta()
            .media_storage_sop_class_uid
            .trim_end_matches('\0'),
        "1.2.840.10008.5.1.4.1.1.4"
    );
    let referenced = object
        .element(Tag(0x0008, 0x1140))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(
        referenced[0]
            .element(Tag(0x0008, 0x1150))
            .unwrap()
            .to_str()
            .unwrap(),
        "1.2.840.10008.5.1.4.1.1.4"
    );
    assert_eq!(
        referenced[0]
            .element(Tag(0x0008, 0x1155))
            .unwrap()
            .to_str()
            .unwrap(),
        remapped_sop
    );
    assert!(referenced[0].element(Tag(0x0008, 0x0090)).is_err());
    assert!(object.element(Tag(0x0019, 0x100A)).is_err());
    assert!(object.element(Tag(0x0019, 0x1010)).is_err());
    assert_eq!(
        object
            .element(Tag(0x0029, 0x0010))
            .unwrap()
            .to_str()
            .unwrap(),
        "SIEMENS CSA HEADER"
    );
    assert!(object.element(Tag(0x0029, 0x1010)).is_ok());
    assert!(object.element(Tag(0x0029, 0x1001)).is_err());
    assert_eq!(
        object
            .element(Tag(0x7FE0, 0x0010))
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        support::fixture_pixel_bytes(options.pixel_bytes)
    );
}

#[test]
fn philips_enhanced_sequences_are_sanitized_recursively() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("enhanced.dcm"),
        7,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            include_privacy_leaks: true,
            pixel_bytes: 128 * 1024,
            ..Default::default()
        },
    );
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(group.has_per_frame_functional_groups);
    let mut classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Held);
    assert_eq!(
        classification.kind,
        "enhanced_mr_pending_verified_metadata_contract"
    );
    // Exercise the recursive sanitizer directly while production
    // classification conservatively holds Enhanced MR.
    classification.decision = ClassificationDecision::Accepted;
    let pseudonymizer = Pseudonymizer::from_base64(TEST_KEY).unwrap();
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &pseudonymizer,
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let dicom = &entries["dicom/000001.dcm"];
    assert_no_fixture_identity_leaks(dicom);
    let extracted = directory.path().join("enhanced-sanitized.dcm");
    std::fs::write(&extracted, dicom).unwrap();
    let object = open_file(&extracted).unwrap();
    let frames = object
        .element(Tag(0x5200, 0x9230))
        .unwrap()
        .items()
        .unwrap();
    let timing = frames[0]
        .element(Tag(0x0018, 0x9112))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(
        timing[0]
            .element(Tag(0x0018, 0x0080))
            .unwrap()
            .to_float64()
            .unwrap(),
        800.0
    );
    assert!(timing[0].element(Tag(0x0008, 0x0090)).is_err());
    assert!(object.element(Tag(0x0019, 0x100A)).is_err());
    assert!(object.element(Tag(0x0019, 0x1010)).is_err());
    assert_eq!(
        object
            .element(Tag(0x2005, 0x100D))
            .unwrap()
            .to_float32()
            .unwrap(),
        0.0
    );
    assert!(
        (object
            .element(Tag(0x2005, 0x100E))
            .unwrap()
            .to_float32()
            .unwrap()
            - 0.00363177)
            .abs()
            < 1e-7
    );
    assert!(object.element(Tag(0x2005, 0x100F)).is_err());
    assert!(object.element(Tag(0x2005, 0x10A0)).is_err());
    assert!(object.element(Tag(0x0018, 0x1060)).is_err());
    assert_eq!(
        object
            .element(Tag(0x2001, 0x1018))
            .unwrap()
            .to_int::<i32>()
            .unwrap(),
        32
    );
    assert!(
        (object
            .element(Tag(0x2001, 0x1022))
            .unwrap()
            .to_float32()
            .unwrap()
            - 0.75)
            .abs()
            < f32::EPSILON
    );
    assert!(object.element(Tag(0x2001, 0x1019)).is_err());
    let private_frames = object
        .element(Tag(0x2005, 0x140F))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(private_frames.len(), 1);
    assert_eq!(
        object
            .element(Tag(0x2005, 0x0014))
            .unwrap()
            .to_str()
            .unwrap(),
        "Philips MR Imaging DD 005"
    );
    assert_eq!(private_frames[0].iter().count(), 2);
    assert_eq!(
        private_frames[0]
            .element(Tag(0x2005, 0x0010))
            .unwrap()
            .to_str()
            .unwrap(),
        "Philips MR Imaging DD 001"
    );
    assert!(private_frames[0].element(Tag(0x2005, 0x100E)).is_ok());
    assert!(private_frames[0].element(Tag(0x2005, 0x100F)).is_err());
    assert!(private_frames[0].element(Tag(0x0010, 0x0010)).is_err());
}

#[test]
fn complete_philips_classic_dynamic_series_suppresses_only_redundant_trigger_time() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::PhilipsClassic,
        philips_dynamic_timing: true,
        philips_temporal_positions: 10,
        philips_slices: 1,
        include_privacy_leaks: true,
        ..Default::default()
    };
    for instance in 1..=10 {
        support::write_functional_epi_fixture(
            &source.join(format!("dynamic-{instance}.dcm")),
            instance,
            &options,
        );
    }

    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(group.philips_dynamic_timing_detected);
    assert!(group.philips_dynamic_timing_contract_verified);
    let classification = classify_header(group);
    assert_eq!(
        classification.decision,
        ClassificationDecision::Accepted,
        "classification: {classification:?}"
    );

    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    assert!(
        bundle
            .qc
            .warnings
            .contains(&"suppressed_redundant_philips_dynamic_trigger_times:10".to_owned())
    );
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(
        manifest["deidentification"]["metadata_transformations"],
        serde_json::json!(["suppressed_redundant_philips_dynamic_trigger_time"])
    );
    assert_eq!(manifest["source"]["model"], "Achieva dStream");
    for instance in 1..=10 {
        let path = format!("dicom/{instance:06}.dcm");
        let extracted = directory.path().join(format!("sanitized-{instance}.dcm"));
        std::fs::write(&extracted, &entries[&path]).unwrap();
        let object = open_file(&extracted).unwrap();
        assert_eq!(
            object
                .element(Tag(0x0008, 0x1090))
                .unwrap()
                .to_str()
                .unwrap(),
            "Achieva dStream"
        );
        assert!(object.element(Tag(0x0018, 0x1060)).is_err());
        assert!(object.element(Tag(0x2005, 0x10A0)).is_err());
        assert!(object.element(Tag(0x2005, 0x100D)).is_ok());
        assert!(object.element(Tag(0x2005, 0x100E)).is_ok());
        assert!(object.element(Tag(0x2001, 0x1018)).is_ok());
        assert!(object.element(Tag(0x2001, 0x1022)).is_ok());
        assert_no_fixture_identity_leaks(&entries[&path]);
    }
}

#[test]
fn philips_broad_non_scaling_per_frame_container_is_safely_dropped() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::PhilipsClassic,
        philips_dynamic_timing: true,
        philips_temporal_positions: 10,
        philips_slices: 1,
        philips_non_scaling_per_frame_container: true,
        ..Default::default()
    };
    for instance in 1..=10 {
        support::write_functional_epi_fixture(
            &source.join(format!("broad-container-{instance}.dcm")),
            instance,
            &options,
        );
    }

    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    assert!(!bundle.qc.warnings.iter().any(|warning| {
        warning.starts_with("rebuilt_ps315_philips_per_frame_scale_sequences:")
    }));
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert!(
        !manifest["deidentification"]["safe_private_exceptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| { value.as_str() == Some("dicom_ps3.15_philips_per_frame_scale_slope") })
    );
    let extracted = directory.path().join("broad-container-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    assert!(object.element(Tag(0x2005, 0x140F)).is_err());
    assert!(object.element(Tag(0x2005, 0x100D)).is_ok());
    assert!(object.element(Tag(0x2005, 0x100E)).is_ok());
    assert!(!String::from_utf8_lossy(&entries["dicom/000001.dcm"]).contains("20000101120000"));
}

#[test]
fn incomplete_philips_dynamic_series_is_held_without_trigger_suppression() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::PhilipsClassic,
        philips_dynamic_timing: true,
        philips_temporal_positions: 10,
        philips_slices: 1,
        ..Default::default()
    };
    for instance in 1..=2 {
        support::write_functional_epi_fixture(
            &source.join(format!("incomplete-{instance}.dcm")),
            instance,
            &options,
        );
    }
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(group.philips_dynamic_timing_detected);
    assert!(!group.philips_dynamic_timing_contract_verified);
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Held);
    assert_eq!(
        classification.kind,
        "philips_dynamic_timing_contract_unverified"
    );
}

#[test]
fn nonredundant_or_malformed_philips_dynamic_timing_is_held() {
    for (case, malformed, trigger_offset_ms) in [
        ("malformed-a0", true, 0.0),
        ("offset-trigger", false, 100.0),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let options = FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsClassic,
            philips_dynamic_timing: true,
            philips_dynamic_timing_malformed: malformed,
            philips_trigger_offset_ms: trigger_offset_ms,
            philips_temporal_positions: 10,
            philips_slices: 1,
            ..Default::default()
        };
        for instance in 1..=10 {
            support::write_functional_epi_fixture(
                &source.join(format!("{case}-{instance}.dcm")),
                instance,
                &options,
            );
        }
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        assert!(group.philips_dynamic_timing_detected, "case {case}");
        assert!(
            !group.philips_dynamic_timing_contract_verified,
            "case {case}"
        );
        let classification = classify_header(group);
        assert_eq!(classification.decision, ClassificationDecision::Held);
        assert_eq!(
            classification.kind,
            "philips_dynamic_timing_contract_unverified"
        );
    }
}

#[test]
fn malformed_philips_private_scientific_metadata_holds_the_whole_series() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for instance in 1..=10 {
        support::write_functional_epi_fixture(
            &source.join(format!("private-contract-{instance}.dcm")),
            instance,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                philips_dynamic_timing: true,
                philips_temporal_positions: 10,
                philips_slices: 1,
                philips_private_metadata_malformed: instance == 6,
                ..Default::default()
            },
        );
    }
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(!group.all_philips_classic_private_metadata_contract_verified);
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Held);
    assert_eq!(
        classification.kind,
        "philips_classic_private_metadata_contract_unverified"
    );
}

#[test]
fn equipment_fields_are_canonicalized_and_hostile_free_text_is_dropped() {
    let directory = tempdir().unwrap();
    let pseudonymizer = Pseudonymizer::from_base64(TEST_KEY).unwrap();

    let ge_source = directory.path().join("ge-source");
    let ge_bundles = directory.path().join("ge-bundles");
    std::fs::create_dir(&ge_source).unwrap();
    std::fs::create_dir(&ge_bundles).unwrap();
    support::write_functional_epi_fixture(
        &ge_source.join("ge.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::Ge,
            ..Default::default()
        },
    );
    let ge_discovery = discover(&ge_source).unwrap();
    let mut ge_classification = classify_header(&ge_discovery.series[0]);
    assert_eq!(ge_classification.decision, ClassificationDecision::Held);
    assert_eq!(
        ge_classification.kind,
        "ge_classic_requires_verified_private_metadata_reconstruction"
    );
    // Exercise canonical public equipment metadata directly while production
    // holds legacy GE pending private scientific-metadata reconstruction.
    ge_classification.decision = ClassificationDecision::Accepted;
    let ge_bundle = create_dicom_archive(ArchiveRequest {
        group: &ge_discovery.series[0],
        classification: ge_classification,
        pseudonymizer: &pseudonymizer,
        bundle_root: &ge_bundles,
        progress: |_| {},
    })
    .unwrap();
    let ge_entries = read_archive(&ge_bundle.archive.unwrap().object.local_path);
    let ge_manifest: serde_json::Value =
        serde_json::from_slice(&ge_entries["manifest.json"]).unwrap();
    assert_eq!(ge_manifest["source"]["manufacturer"], "GE MEDICAL SYSTEMS");
    assert_eq!(ge_manifest["source"]["model"], "Discovery MR750");
    assert_eq!(ge_manifest["source"]["software_versions"][0], "GE DV26.0");
    assert_eq!(ge_manifest["source"]["receive_coil_name"], "HEAD_32");

    let hostile_source = directory.path().join("hostile-source");
    let hostile_bundles = directory.path().join("hostile-bundles");
    std::fs::create_dir(&hostile_source).unwrap();
    std::fs::create_dir(&hostile_bundles).unwrap();
    support::write_functional_epi_fixture(
        &hostile_source.join("hostile.dcm"),
        2,
        &FunctionalDicomOptions {
            hostile_free_text: true,
            include_privacy_leaks: true,
            ..Default::default()
        },
    );
    let hostile_discovery = discover(&hostile_source).unwrap();
    let mut hostile_classification = classify_header(&hostile_discovery.series[0]);
    assert_eq!(
        hostile_classification.decision,
        ClassificationDecision::Held
    );
    assert_eq!(
        hostile_classification.kind,
        "unsupported_scanner_manufacturer"
    );
    // Exercise the privacy writer directly after the production classifier
    // has proved that hostile provenance is held locally.
    hostile_classification.decision = ClassificationDecision::Accepted;
    let hostile_bundle = create_dicom_archive(ArchiveRequest {
        group: &hostile_discovery.series[0],
        classification: hostile_classification,
        pseudonymizer: &pseudonymizer,
        bundle_root: &hostile_bundles,
        progress: |_| {},
    })
    .unwrap();
    let hostile_entries = read_archive(&hostile_bundle.archive.unwrap().object.local_path);
    let hostile_dicom = &hostile_entries["dicom/000001.dcm"];
    let hostile_text = String::from_utf8_lossy(hostile_dicom);
    assert!(!hostile_text.contains("PAUL"));
    assert!(!hostile_text.contains("Paul"));
    assert!(!hostile_text.contains("MRN"));
    let extracted = directory.path().join("hostile-sanitized.dcm");
    std::fs::write(&extracted, hostile_dicom).unwrap();
    let hostile_object = open_file(&extracted).unwrap();
    for tag in [
        Tag(0x0008, 0x0070),
        Tag(0x0008, 0x1090),
        Tag(0x0018, 0x0024),
        Tag(0x0018, 0x1020),
        Tag(0x0018, 0x1250),
    ] {
        assert!(
            hostile_object.element(tag).is_err(),
            "hostile tag {tag} survived"
        );
    }
    let hostile_manifest: serde_json::Value =
        serde_json::from_slice(&hostile_entries["manifest.json"]).unwrap();
    for key in [
        "manufacturer",
        "model",
        "sequence_name",
        "software_versions",
        "receive_coil_name",
    ] {
        assert!(
            hostile_manifest["source"].get(key).is_none(),
            "hostile source field {key} survived"
        );
    }
}

#[test]
fn large_pixel_payload_is_streamed_in_many_progress_chunks() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let pixel_bytes = 16 * 1024 * 1024;
    support::write_functional_epi_fixture(
        &source.join("large.dcm"),
        1,
        &FunctionalDicomOptions {
            pixel_bytes,
            ..Default::default()
        },
    );
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let classification = classify_header(group);
    let pseudonymizer = Pseudonymizer::from_base64(TEST_KEY).unwrap();
    let mut chunks = 0_u64;
    let mut bytes = 0_u64;
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &pseudonymizer,
        bundle_root: &bundles,
        progress: |count| {
            chunks += 1;
            bytes += count;
        },
    })
    .unwrap();
    assert!(chunks > 100, "PixelData was not copied incrementally");
    assert!(bytes >= pixel_bytes as u64);
    assert!(bytes < pixel_bytes as u64 + 1024 * 1024);
    assert!(Path::new(&bundle.archive.unwrap().object.local_path).is_file());
}

#[test]
fn siemens_classic_mosaic_keeps_only_canonical_numeric_csa_geometry() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::Siemens,
        siemens_mosaic: true,
        include_privacy_leaks: true,
        pixel_bytes: 256 * 1024,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source.join("mosaic.dcm"), 1, &options);
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(group.all_siemens_csa_image_headers_sanitizable);
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);

    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    assert!(
        bundle
            .qc
            .warnings
            .iter()
            .any(|warning| { warning == "rewritten_numeric_siemens_csa_image_headers:1" })
    );
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let dicom = &entries["dicom/000001.dcm"];
    assert_no_fixture_identity_leaks(dicom);
    let extracted = directory.path().join("mosaic-sanitized.dcm");
    std::fs::write(&extracted, dicom).unwrap();
    let object = open_file(&extracted).unwrap();
    let csa = object
        .element(Tag(0x0029, 0x1010))
        .unwrap()
        .to_bytes()
        .unwrap();
    let csa_text = String::from_utf8_lossy(csa.as_ref());
    assert!(csa_text.contains("NumberOfImagesInMosaic"));
    assert!(csa_text.contains("MosaicRefAcqTimes"));
    assert!(csa_text.contains("PhaseEncodingDirectionPositive"));
    assert!(!csa_text.contains("MrPhoenixProtocol"));
    assert!(!csa_text.contains("ASCCONV"));
    assert!(!csa_text.contains("Paul"));
    assert_eq!(
        object
            .element(Tag(0x7FE0, 0x0010))
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        support::fixture_pixel_bytes(options.pixel_bytes)
    );
}

#[test]
fn encapsulated_pixel_data_element_is_copied_byte_exact_and_remains_parseable() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::Siemens,
        encapsulated_pixel_data: true,
        pixel_bytes: 128 * 1024,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source.join("jpeg.dcm"), 1, &options);
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification: classify_header(group),
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let dicom = &entries["dicom/000001.dcm"];
    let expected = support::fixture_encapsulated_pixel_element(options.pixel_bytes);
    assert!(
        dicom.ends_with(&expected),
        "the full undefined-length PixelData element must be copied verbatim"
    );
    let extracted = directory.path().join("jpeg-sanitized.dcm");
    std::fs::write(&extracted, dicom).unwrap();
    let object = open_file(&extracted).unwrap();
    assert_eq!(
        object.meta().transfer_syntax.trim_end_matches('\0'),
        "1.2.840.10008.1.2.4.50"
    );
    assert!(object.element(Tag(0x7FE0, 0x0010)).is_ok());
}

#[test]
fn absent_burned_in_annotation_is_recorded_honestly_without_synthesizing_no() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("missing-bia.dcm"),
        1,
        &FunctionalDicomOptions {
            omit_burned_in_annotation: true,
            ..Default::default()
        },
    );
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    assert!(
        bundle
            .qc
            .warnings
            .contains(&"burned_in_annotation_not_declared".to_owned())
    );
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(
        manifest["deidentification"]["burned_in_annotation_status"],
        "not_declared"
    );
    assert!(
        manifest["deidentification"]
            .get("burned_in_annotation_verified_no")
            .is_none()
    );
    let extracted = directory.path().join("missing-bia-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    assert!(object.element(Tag(0x0028, 0x0301)).is_err());
    assert_eq!(
        object
            .element(Tag(0x0028, 0x0303))
            .unwrap()
            .to_str()
            .unwrap(),
        "REMOVED"
    );
}

fn read_archive(path: impl AsRef<Path>) -> BTreeMap<String, Vec<u8>> {
    let decoder = zstd::stream::read::Decoder::new(File::open(path).unwrap()).unwrap();
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (path, bytes)
        })
        .collect()
}

fn assert_no_fixture_identity_leaks(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    for leak in [
        "FIXTURE-SUBJECT-001",
        "FIXTURE^SUBJECT",
        "FIXTURE SECRET HOSPITAL",
        "FIXTURE^PHYSICIAN",
        "NESTED^FIXTURE^PHYSICIAN",
        "DEEPLY^NESTED^PHYSICIAN",
        "20260718",
        "FIXTURE PRIVATE CSA TEXT LEAK",
        "FIXTURE PHILIPS PRIVATE TEXT LEAK",
        "UNKNOWN PRIVATE CREATOR",
        "FIXTURE UNKNOWN PRIVATE TEXT LEAK",
        "1.2.826.0.1.3680043.10.999.1",
    ] {
        assert!(!text.contains(leak), "sanitized DICOM leaked {leak}");
    }
}
