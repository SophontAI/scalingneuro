mod support;

use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

use dicom_core::{
    DataElement, Length, Tag, VR,
    header::{HasLength, Header},
    value::{DataSetSequence, PrimitiveValue, Value},
};
use dicom_object::{InMemDicomObject, open_file};
use neuro_sync::{
    archive::{
        ARCHIVE_VERIFY_PROCESSING_ROUTE, ArchiveRequest, DICOM_ARCHIVE_FORMAT,
        DICOM_METADATA_POLICY_ID, DICOM_METADATA_POLICY_VERSION,
        SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY, create_dicom_archive,
    },
    classify::classify_header,
    dicom::discover,
    model::{Classification, ClassificationDecision},
    pseudonym::Pseudonymizer,
};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

use support::{
    DimensionIndexFixture, FixturePurpose, FixtureVendor, FrameVoiFixture, FunctionalDicomOptions,
};

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
    assert_eq!(manifest["schema_version"], "2.0.0");
    assert_eq!(manifest["series_archive_id"], first.bundle_id);
    assert_eq!(manifest["modality"], "mr");
    assert_eq!(manifest["series_kind"], "functional_epi");
    assert_eq!(manifest["processing_route"], "functional-epi-v1");
    assert_eq!(manifest["pixel_data_policy"], "scanner-native-not-defaced");
    assert_eq!(manifest["deidentification"]["defacing_performed"], false);
    assert_eq!(
        manifest["deidentification"]["recognizable_visual_features"],
        "may_be_present"
    );
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
        "scaling-neuro-recursive-allowlist-v2"
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
fn structural_mr_archive_is_native_pixel_exact_and_uses_archive_only_routing() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let options = FunctionalDicomOptions {
        purpose: FixturePurpose::StructuralT1w,
        include_privacy_leaks: true,
        siemens_mosaic: false,
        pixel_bytes: 384 * 1024,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source.join("t1w.dcm"), 1, &options);

    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
    assert_eq!(classification.kind, "structural_t1w");

    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    assert_eq!(bundle.series_kind, "structural_t1w");
    assert_eq!(bundle.processing_route, ARCHIVE_VERIFY_PROCESSING_ROUTE);
    assert_eq!(
        bundle.pixel_data_policy,
        SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY
    );

    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest["modality"], "mr");
    assert_eq!(manifest["series_kind"], "structural_t1w");
    assert_eq!(
        manifest["processing_route"],
        ARCHIVE_VERIFY_PROCESSING_ROUTE
    );
    assert_eq!(
        manifest["pixel_data_policy"],
        SCANNER_NATIVE_NOT_DEFACED_PIXEL_POLICY
    );
    assert_eq!(manifest["deidentification"]["pixel_data_retained"], true);
    assert_eq!(manifest["deidentification"]["defacing_performed"], false);
    assert_eq!(
        manifest["deidentification"]["recognizable_visual_features"],
        "may_be_present"
    );

    let sanitized_path = directory.path().join("sanitized-t1w.dcm");
    std::fs::write(&sanitized_path, &entries["dicom/000001.dcm"]).unwrap();
    let sanitized = open_file(&sanitized_path).unwrap();
    assert_eq!(
        sanitized
            .element(Tag(0x7FE0, 0x0010))
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        support::fixture_pixel_bytes(options.pixel_bytes)
    );
    assert_no_fixture_identity_leaks(&entries["dicom/000001.dcm"]);
}

#[test]
fn enhanced_color_is_held_while_legacy_converted_mr_remains_pixel_exact() {
    let color_directory = tempdir().unwrap();
    let color_source = color_directory.path().join("source");
    std::fs::create_dir(&color_source).unwrap();
    support::write_functional_epi_fixture(
        &color_source.join("enhanced-color.dcm"),
        1,
        &FunctionalDicomOptions {
            purpose: FixturePurpose::OtherMr,
            sop_class_override: Some("1.2.840.10008.5.1.4.1.1.4.3"),
            siemens_mosaic: false,
            ..Default::default()
        },
    );
    let color_discovery = discover(&color_source).unwrap();
    let color = classify_header(&color_discovery.series[0]);
    assert_eq!(color.decision, ClassificationDecision::Held);
    assert_eq!(color.kind, "unsupported_sop_class");

    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let sop_class = "1.2.840.10008.5.1.4.1.1.4.4";
    let options = FunctionalDicomOptions {
        purpose: FixturePurpose::OtherMr,
        sop_class_override: Some(sop_class),
        siemens_mosaic: false,
        pixel_bytes: 96 * 1024,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source.join("legacy.dcm"), 1, &options);
    let discovery = discover(&source).unwrap();
    let classification = classify_header(&discovery.series[0]);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
    let bundle = create_dicom_archive(ArchiveRequest {
        group: &discovery.series[0],
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let sanitized_path = directory.path().join("legacy-sanitized.dcm");
    std::fs::write(&sanitized_path, &entries["dicom/000001.dcm"]).unwrap();
    let sanitized = open_file(&sanitized_path).unwrap();
    assert_eq!(
        sanitized
            .element(Tag(0x0008, 0x0016))
            .unwrap()
            .to_str()
            .unwrap()
            .trim_matches([' ', '\0']),
        sop_class
    );
    assert_eq!(
        sanitized
            .element(Tag(0x7FE0, 0x0010))
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        support::fixture_pixel_bytes(options.pixel_bytes)
    );
}

#[test]
fn enhanced_adc_and_fa_image_and_frame_types_are_preserved_positionally() {
    for (case, image_type) in [
        ("adc", "DERIVED\\PRIMARY\\DIFFUSION\\ADC"),
        ("fa", "DERIVED\\PRIMARY\\DIFFUSION\\FA"),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join(format!("enhanced-{case}.dcm")),
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                purpose: FixturePurpose::DerivedMr,
                enhanced_image_type_override: Some(image_type),
                enhanced_frame_type_override: Some(image_type),
                ..Default::default()
            },
        );

        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let classification = classify_header(group);
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
        assert_eq!(classification.kind, "derived_mr");
        let bundle = create_dicom_archive(ArchiveRequest {
            group,
            classification,
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap();
        let entries = read_archive(&bundle.archive.unwrap().object.local_path);
        let extracted = directory
            .path()
            .join(format!("enhanced-{case}-sanitized.dcm"));
        std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
        let object = open_file(&extracted).unwrap();
        assert_eq!(
            object
                .element(Tag(0x0008, 0x0008))
                .unwrap()
                .to_str()
                .unwrap(),
            image_type
        );
        let frames = object
            .element(Tag(0x5200, 0x9230))
            .unwrap()
            .items()
            .unwrap();
        for frame in frames {
            let frame_type = frame.element(Tag(0x0018, 0x9226)).unwrap().items().unwrap()[0]
                .element(Tag(0x0008, 0x9007))
                .unwrap()
                .to_str()
                .unwrap();
            assert_eq!(frame_type, image_type);
        }
    }
}

#[test]
fn legacy_converted_mr_preserves_standard_empty_fourth_image_and_frame_type_value() {
    const LEGACY_MR_UID: &str = "1.2.840.10008.5.1.4.1.1.4.4";
    const LEGACY_EMPTY_V4: &str = "DERIVED\\PRIMARY\\DIFFUSION\\";
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("legacy-empty-v4.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            purpose: FixturePurpose::DerivedMr,
            sop_class_override: Some(LEGACY_MR_UID),
            enhanced_image_type_override: Some(LEGACY_EMPTY_V4),
            enhanced_frame_type_override: Some(LEGACY_EMPTY_V4),
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
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let extracted = directory.path().join("legacy-empty-v4-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    assert_eq!(
        object
            .element(Tag(0x0008, 0x0008))
            .unwrap()
            .to_str()
            .unwrap(),
        LEGACY_EMPTY_V4
    );
    let frames = object
        .element(Tag(0x5200, 0x9230))
        .unwrap()
        .items()
        .unwrap();
    assert!(!frames.is_empty());
    for frame in frames {
        let frame_type = frame.element(Tag(0x0018, 0x9226)).unwrap().items().unwrap()[0]
            .element(Tag(0x0008, 0x9007))
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(frame_type, LEGACY_EMPTY_V4);
    }
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
            pixel_bytes: 12 * 64 * 64 * 2,
            ..Default::default()
        },
    );
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(group.has_per_frame_functional_groups);
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
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
    assert_eq!(frames.len(), 12);
    let shared = object
        .element(Tag(0x5200, 0x9229))
        .unwrap()
        .items()
        .unwrap();
    let timing = shared[0]
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
    let frame_type = frames[0]
        .element(Tag(0x0018, 0x9226))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(
        frame_type[0]
            .element(Tag(0x0008, 0x9007))
            .unwrap()
            .to_str()
            .unwrap(),
        "ORIGINAL\\PRIMARY\\FMRI\\NONE"
    );
    let frame_content = frames[0]
        .element(Tag(0x0020, 0x9111))
        .unwrap()
        .items()
        .unwrap();
    let expected_stack_id = pseudonymizer
        .id("dicom-stack-id-v1", "ORIGINAL_STACK")
        .chars()
        .take(16)
        .collect::<String>();
    assert_eq!(
        frame_content[0]
            .element(Tag(0x0020, 0x9056))
            .unwrap()
            .to_str()
            .unwrap(),
        expected_stack_id
    );
    assert_eq!(
        frame_content[0]
            .element(Tag(0x0020, 0x9057))
            .unwrap()
            .to_int::<u32>()
            .unwrap(),
        1
    );
    assert_eq!(
        frame_content[0]
            .element(Tag(0x0020, 0x9153))
            .unwrap()
            .to_float64()
            .unwrap(),
        0.0
    );
    assert_eq!(
        frame_content[0]
            .element(Tag(0x0020, 0x9157))
            .unwrap()
            .to_multi_int::<u32>()
            .unwrap(),
        vec![1]
    );
    for tag in [
        Tag(0x0020, 0x0242),
        Tag(0x0020, 0x9161),
        Tag(0x0020, 0x9162),
        Tag(0x0020, 0x9163),
        Tag(0x0020, 0x9228),
    ] {
        assert!(object.element(tag).is_err());
    }
    let organizations = object
        .element(Tag(0x0020, 0x9221))
        .unwrap()
        .items()
        .unwrap();
    let indexes = object
        .element(Tag(0x0020, 0x9222))
        .unwrap()
        .items()
        .unwrap();
    let root_dimension_uid = object
        .element(Tag(0x0020, 0x9164))
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        organizations[0]
            .element(Tag(0x0020, 0x9164))
            .unwrap()
            .to_str()
            .unwrap(),
        root_dimension_uid
    );
    assert_eq!(
        indexes[0]
            .element(Tag(0x0020, 0x9164))
            .unwrap()
            .to_str()
            .unwrap(),
        root_dimension_uid
    );
    assert!(matches!(
        indexes[0]
            .element(Tag(0x0020, 0x9165))
            .unwrap()
            .value()
            .primitive(),
        Some(PrimitiveValue::Tags(values)) if values.as_slice() == [Tag(0x0020, 0x9057)]
    ));
    assert!(matches!(
        indexes[0]
            .element(Tag(0x0020, 0x9167))
            .unwrap()
            .value()
            .primitive(),
        Some(PrimitiveValue::Tags(values)) if values.as_slice() == [Tag(0x0020, 0x9111)]
    ));
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
fn enhanced_asl_retains_exact_standard_context_orientation_and_flags() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("enhanced-asl.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            purpose: FixturePurpose::AslPerfusion,
            ..Default::default()
        },
    );
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
    assert!(
        bundle
            .qc
            .warnings
            .iter()
            .any(|warning| warning == "emptied_asl_technique_descriptions:12")
    );
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert!(
        manifest["deidentification"]["metadata_transformations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "emptied_asl_technique_description")
    );
    let extracted = directory.path().join("enhanced-asl-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    let frames = object
        .element(Tag(0x5200, 0x9230))
        .unwrap()
        .items()
        .unwrap();
    let asl = frames[0]
        .element(Tag(0x0018, 0x9251))
        .unwrap()
        .items()
        .unwrap();
    let description = asl[0].element(Tag(0x0018, 0x9252)).unwrap();
    assert_eq!(description.vr(), VR::LO);
    assert_eq!(description.to_str().unwrap(), "");
    let slabs = asl[0]
        .element(Tag(0x0018, 0x9260))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(
        slabs[0]
            .element(Tag(0x0018, 0x9253))
            .unwrap()
            .to_int::<u16>()
            .unwrap(),
        1
    );
    assert_eq!(
        slabs[0]
            .element(Tag(0x0018, 0x9254))
            .unwrap()
            .to_float64()
            .unwrap(),
        120.0
    );
    assert_eq!(
        slabs[0]
            .element(Tag(0x0018, 0x9255))
            .unwrap()
            .to_multi_float64()
            .unwrap(),
        vec![0.0, 0.0, 1.0]
    );
    assert_eq!(
        slabs[0]
            .element(Tag(0x0018, 0x9256))
            .unwrap()
            .to_multi_float64()
            .unwrap(),
        vec![0.0, 0.0, 0.0]
    );
    assert_eq!(
        slabs[0]
            .element(Tag(0x0018, 0x9258))
            .unwrap()
            .to_int::<u32>()
            .unwrap(),
        1800
    );
    assert_eq!(
        asl[0]
            .element(Tag(0x0018, 0x9257))
            .unwrap()
            .to_str()
            .unwrap(),
        "LABEL"
    );
    assert_eq!(
        asl[0]
            .element(Tag(0x0018, 0x9259))
            .unwrap()
            .to_str()
            .unwrap(),
        "NO"
    );
    assert_eq!(
        asl[0]
            .element(Tag(0x0018, 0x925C))
            .unwrap()
            .to_str()
            .unwrap(),
        "NO"
    );
    assert!(
        !String::from_utf8_lossy(&entries["dicom/000001.dcm"])
            .contains("FIXTURE ASL TECHNIQUE PHI")
    );
}

#[test]
fn enhanced_asl_positive_groups_are_rewritten_atomically_without_source_text() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let source_path = source.join("enhanced-asl-positive.dcm");
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::PhilipsEnhanced,
        purpose: FixturePurpose::AslPerfusion,
        asl_crusher: true,
        asl_bolus_cutoff: true,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source_path, 1, &options);
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
            .contains(&"redacted_asl_crusher_descriptions:12".to_owned())
    );
    assert!(
        bundle
            .qc
            .warnings
            .contains(&"emptied_asl_bolus_cutoff_techniques:12".to_owned())
    );
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let output = &entries["dicom/000001.dcm"];
    let text = String::from_utf8_lossy(output);
    assert!(!text.contains("FIXTURE CRUSHER FREE TEXT"));
    assert!(!text.contains("FIXTURE BOLUS TECHNIQUE"));
    let extracted = directory.path().join("enhanced-asl-positive-sanitized.dcm");
    std::fs::write(&extracted, output).unwrap();
    let object = open_file(&extracted).unwrap();
    let frames = object
        .element(Tag(0x5200, 0x9230))
        .unwrap()
        .items()
        .unwrap();
    for frame in frames {
        let context = &frame.element(Tag(0x0018, 0x9251)).unwrap().items().unwrap()[0];
        assert_eq!(
            context
                .element(Tag(0x0018, 0x925a))
                .unwrap()
                .to_float64()
                .unwrap(),
            0.0
        );
        assert_eq!(
            context
                .element(Tag(0x0018, 0x925b))
                .unwrap()
                .to_str()
                .unwrap(),
            "REDACTED"
        );
        let bolus = context
            .element(Tag(0x0018, 0x925d))
            .unwrap()
            .items()
            .unwrap();
        assert_eq!(bolus.len(), 1);
        assert!(bolus[0].element(Tag(0x0018, 0x925e)).unwrap().is_empty());
        assert_eq!(
            bolus[0]
                .element(Tag(0x0018, 0x925f))
                .unwrap()
                .to_int::<u32>()
                .unwrap(),
            450
        );
    }

    let malformed_directory = tempdir().unwrap();
    let malformed_source = malformed_directory.path().join("source");
    let malformed_bundles = malformed_directory.path().join("bundles");
    std::fs::create_dir(&malformed_source).unwrap();
    std::fs::create_dir(&malformed_bundles).unwrap();
    let malformed_path = malformed_source.join("malformed-asl.dcm");
    support::write_functional_epi_fixture(
        &malformed_path,
        1,
        &FunctionalDicomOptions {
            omit_asl_crusher_description: true,
            ..options.clone()
        },
    );
    let malformed_discovery = discover(&malformed_source).unwrap();
    let malformed_group = &malformed_discovery.series[0];
    let error = create_dicom_archive(ArchiveRequest {
        group: malformed_group,
        classification: Classification {
            decision: ClassificationDecision::Accepted,
            kind: "asl_perfusion".into(),
            confidence: 1.0,
            evidence: Vec::new(),
        },
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &malformed_bundles,
        progress: |_| {},
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "DICOM contains an incomplete or invalid ASL crusher group"
    );
}

#[test]
fn enhanced_diffusion_retains_one_atomic_per_frame_diffusion_contract() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("enhanced-diffusion.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            purpose: FixturePurpose::Diffusion,
            ..Default::default()
        },
    );
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(group.representative.public_diffusion_metadata_present);
    assert!(group.all_diffusion_metadata_contracts_verified);
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification: classify_header(group),
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let extracted = directory.path().join("enhanced-diffusion-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(extracted).unwrap();
    assert!(object.element(Tag(0x0018, 0x9087)).is_err());
    let frames = object
        .element(Tag(0x5200, 0x9230))
        .unwrap()
        .items()
        .unwrap();
    let diffusion = frames[0]
        .element(Tag(0x0018, 0x9117))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(diffusion.len(), 1);
    assert_eq!(
        diffusion[0]
            .element(Tag(0x0018, 0x9087))
            .unwrap()
            .to_float64()
            .unwrap(),
        1000.0
    );
    assert_eq!(
        diffusion[0]
            .element(Tag(0x0018, 0x9075))
            .unwrap()
            .to_str()
            .unwrap(),
        "DIRECTIONAL"
    );
    let gradient = diffusion[0]
        .element(Tag(0x0018, 0x9076))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(gradient.len(), 1);
    assert_eq!(
        gradient[0]
            .element(Tag(0x0018, 0x9089))
            .unwrap()
            .to_multi_float64()
            .unwrap(),
        vec![1.0, 0.0, 0.0]
    );
}

#[test]
fn philips_private_asl_label_is_canonical_and_asl_trigger_time_survives() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::PhilipsClassic,
        purpose: FixturePurpose::AslPerfusion,
        philips_dynamic_timing: true,
        philips_temporal_positions: 10,
        philips_slices: 1,
        ..Default::default()
    };
    for instance in 1..=10 {
        support::write_functional_epi_fixture(
            &source.join(format!("asl-{instance}.dcm")),
            instance,
            &options,
        );
    }
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert!(group.philips_dynamic_timing_contract_verified);
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
    assert_eq!(classification.kind, "asl_perfusion");
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();
    assert!(bundle.classification.evidence.iter().any(|item| item.code
        == "asl_scientific_metadata_contract_verified"
        && item.source == "dicom_header"
        && item.effect == "supports"));
    assert!(
        bundle
            .qc
            .warnings
            .iter()
            .any(|warning| { warning == "retained_philips_dd005_asl_label_attributes:10" })
    );
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert!(
        manifest["deidentification"]["safe_private_exceptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "philips_mr_imaging_dd_005_asl_label_code_v1")
    );
    let extracted = directory.path().join("philips-asl-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000010.dcm"]).unwrap();
    let object = open_file(extracted).unwrap();
    let label = object.element(Tag(0x2005, 0x1429)).unwrap();
    assert_eq!(label.vr(), VR::CS);
    assert_eq!(label.to_str().unwrap(), "LABEL");
    assert!(object.element(Tag(0x2005, 0x142A)).is_err());
    assert_eq!(
        object
            .element(Tag(0x0018, 0x1060))
            .unwrap()
            .to_float64()
            .unwrap(),
        7200.0
    );
}

#[test]
fn ge_asl_supplemental_private_fields_are_retained_but_not_a_completeness_claim() {
    let directory = tempdir().unwrap();
    let held_source = directory.path().join("held-source");
    std::fs::create_dir(&held_source).unwrap();
    support::write_functional_epi_fixture(
        &held_source.join("ge-asl.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::Ge,
            purpose: FixturePurpose::AslPerfusion,
            ge_asl_private_metadata: true,
            ..Default::default()
        },
    );
    let held_discovery = discover(&held_source).unwrap();
    assert_eq!(
        classify_header(&held_discovery.series[0]).decision,
        ClassificationDecision::Held
    );

    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("ge-functional-with-asl-supplement.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::Ge,
            ge_asl_private_metadata: true,
            ..Default::default()
        },
    );
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert_eq!(
        classify_header(group).decision,
        ClassificationDecision::Held
    );
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification: Classification {
            decision: ClassificationDecision::Accepted,
            kind: "functional_epi".into(),
            confidence: 1.0,
            evidence: Vec::new(),
        },
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
            .any(|warning| warning == "retained_ge_parm_asl_attributes:2")
    );
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert!(
        manifest["deidentification"]["safe_private_exceptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "ge_gems_parm_01_asl_technique_duration_v1")
    );
    let extracted = directory.path().join("ge-asl-supplement-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(extracted).unwrap();
    let technique = object.element(Tag(0x0043, 0x10A3)).unwrap();
    assert_eq!(technique.vr(), VR::CS);
    assert_eq!(technique.to_str().unwrap(), "PSEUDOCONTINUOUS");
    let duration = object.element(Tag(0x0043, 0x10A5)).unwrap();
    assert_eq!(duration.vr(), VR::IS);
    assert_eq!(duration.to_int::<i64>().unwrap(), 1800);
    assert!(object.element(Tag(0x0043, 0x10A4)).is_err());
    assert!(object.element(Tag(0x0043, 0x10A6)).is_err());
}

#[test]
fn invalid_required_image_type_component_rejects_the_instance() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("invalid-image-type.dcm"),
        1,
        &FunctionalDicomOptions {
            invalid_image_type: true,
            ..Default::default()
        },
    );
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let error = create_dicom_archive(ArchiveRequest {
        group,
        classification: classify_header(group),
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "DICOM ImageType failed positional validation"
    );
}

#[test]
fn unknown_optional_image_type_is_replaced_in_place_and_reported() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("unknown-optional-image-type.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::Generic,
            unknown_optional_image_type: true,
            ..Default::default()
        },
    );
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
    assert_eq!(
        bundle.qc.warnings,
        vec!["classic_image_type_supplemental_metadata_incomplete_replaced_with_other:1"]
    );

    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(
        manifest["deidentification"]["metadata_transformations"],
        serde_json::json!(["replaced_unknown_classic_image_type_components_with_other"])
    );
    let extracted = directory.path().join("sanitized-image-type.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    assert_eq!(
        open_file(extracted)
            .unwrap()
            .element(Tag(0x0008, 0x0008))
            .unwrap()
            .to_str()
            .unwrap(),
        "ORIGINAL\\PRIMARY\\OTHER\\EPI"
    );
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
fn philips_reviewed_per_frame_scale_accepts_only_a_complete_nested_rescale() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("enhanced-philips-scale.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            include_pixel_value_transform: true,
            philips_per_frame_standard_rescale: true,
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
    assert!(bundle.qc.warnings.iter().any(|warning| {
        warning.starts_with("rebuilt_ps315_philips_per_frame_scale_sequences:")
    }));

    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert!(
        manifest["deidentification"]["safe_private_exceptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("dicom_ps3.15_philips_per_frame_scale_slope"))
    );
    let extracted = directory
        .path()
        .join("enhanced-philips-scale-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    let private_frames = object
        .element(Tag(0x2005, 0x140F))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(private_frames.len(), 1);
    assert!(private_frames[0].element(Tag(0x2005, 0x100E)).is_ok());
    for tag in [
        Tag(0x0028, 0x1052),
        Tag(0x0028, 0x1053),
        Tag(0x0028, 0x1054),
    ] {
        assert!(private_frames[0].element(tag).is_err());
    }
    assert!(object.element(Tag(0x0028, 0x9145)).is_ok());
}

#[test]
fn philips_nested_rescale_remains_fail_closed_without_the_exact_rebuilt_contract() {
    for (case, options) in [
        (
            "incomplete-reviewed-scale",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                philips_per_frame_standard_rescale: true,
                philips_per_frame_incomplete_standard_rescale: true,
                ..Default::default()
            },
        ),
        (
            "complete-non-scale-container",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                philips_non_scaling_per_frame_container: true,
                philips_per_frame_standard_rescale: true,
                ..Default::default()
            },
        ),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(&source.join("image.dcm"), 1, &options);
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "DICOM contains an incomplete or invalid rescale transform",
            "case {case}"
        );
    }
}

#[test]
fn enhanced_frame_voi_lut_preserves_exact_bounded_per_frame_windows() {
    for mode in [
        FrameVoiFixture::Valid,
        FrameVoiFixture::ValidWithExplanation,
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join("enhanced-frame-voi.dcm"),
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                frame_voi_lut: mode,
                ..Default::default()
            },
        );

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
        let extracted = directory.path().join("enhanced-frame-voi-sanitized.dcm");
        std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
        let object = open_file(&extracted).unwrap();
        let frames = object
            .element(Tag(0x5200, 0x9230))
            .unwrap()
            .items()
            .unwrap();
        assert_eq!(frames.len(), 12);
        for (index, frame) in frames.iter().enumerate() {
            let sequence = frame.element(Tag(0x0028, 0x9132)).unwrap().items().unwrap();
            assert_eq!(sequence.len(), 1);
            let item = &sequence[0];
            assert_eq!(
                item.iter().map(|element| element.tag()).collect::<Vec<_>>(),
                [Tag(0x0028, 0x1050), Tag(0x0028, 0x1051)]
            );
            assert_eq!(
                item.element(Tag(0x0028, 0x1050))
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .trim(),
                (1019 + index).to_string()
            );
            assert_eq!(
                item.element(Tag(0x0028, 0x1051))
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .trim(),
                "1772"
            );
            assert!(item.element(Tag(0x0028, 0x1055)).is_err());
        }
        assert_no_fixture_identity_leaks(&entries["dicom/000001.dcm"]);
    }
}

#[test]
fn frame_voi_lut_remains_cardinality_context_and_semantics_bound() {
    for (case, mode, expected) in [
        (
            "partial-pair",
            FrameVoiFixture::MissingWidth,
            "incomplete or invalid window transform",
        ),
        (
            "extra-attribute",
            FrameVoiFixture::ExtraAttribute,
            "unsupported FrameVOILUTSequence item",
        ),
        (
            "multiple-items",
            FrameVoiFixture::MultipleItems,
            "invalid or off-context FrameVOILUTSequence",
        ),
        (
            "off-context-wrapper",
            FrameVoiFixture::OffContext,
            "invalid or off-context FrameVOILUTSequence",
        ),
        (
            "direct-nested-window",
            FrameVoiFixture::DirectNestedWindow,
            "incomplete or invalid window transform",
        ),
        (
            "voi-lut-function",
            FrameVoiFixture::VoiLutFunction,
            "unsupported FrameVOILUTSequence item",
        ),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join("image.dcm"),
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                frame_voi_lut: mode,
                ..Default::default()
            },
        );
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "case {case}: {error:#}"
        );
    }
}

#[test]
fn every_quantitative_mapping_and_generic_palette_root_is_rejected_before_rewrite() {
    let unsupported = [
        Tag(0x0028, 0x1100),
        Tag(0x0028, 0x1200),
        Tag(0x0040, 0x9094),
        Tag(0x0040, 0x9096),
        Tag(0x0040, 0x9098),
        Tag(0x0040, 0x9210),
        Tag(0x0040, 0x9211),
        Tag(0x0040, 0x9212),
        Tag(0x0040, 0x9213),
        Tag(0x0040, 0x9214),
        Tag(0x0040, 0x9216),
        Tag(0x0040, 0x9220),
        Tag(0x0040, 0x9224),
        Tag(0x0040, 0x9225),
    ];
    for tag in unsupported {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let source_path = source.join("image.dcm");
        support::write_functional_epi_fixture(&source_path, 1, &FunctionalDicomOptions::default());
        let mut object = open_file(&source_path).unwrap();
        object.put(DataElement::new(tag, VR::UN, PrimitiveValue::Empty));
        object.write_to_file(&source_path).unwrap();

        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("not supported")
                || error.to_string().contains("unsupported pixel transform"),
            "tag {tag}: {error:#}"
        );
    }
}

#[test]
fn enhanced_dimension_indexes_resolve_only_to_retained_public_attributes_or_macros() {
    for mode in [
        DimensionIndexFixture::ValidAttributePointer,
        DimensionIndexFixture::FunctionalGroupSequencePointer,
        DimensionIndexFixture::RootAttributePointer,
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join("image.dcm"),
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                dimension_index: mode,
                ..Default::default()
            },
        );
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap();
    }
}

#[test]
fn enhanced_dimension_indexes_reject_private_broken_and_nonpositive_contracts() {
    for mode in [
        DimensionIndexFixture::PrivateIndexPointer,
        DimensionIndexFixture::PrivateGroupPointer,
        DimensionIndexFixture::PrivateCreator,
        DimensionIndexFixture::MissingTarget,
        DimensionIndexFixture::ZeroIndexValue,
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join("image.dcm"),
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                dimension_index: mode,
                ..Default::default()
            },
        );
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        assert!(
            create_dicom_archive(ArchiveRequest {
                group,
                classification: classify_header(group),
                pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
                bundle_root: &bundles,
                progress: |_| {},
            })
            .is_err(),
            "mode {mode:?}"
        );
    }
}

#[test]
fn unsupported_derived_reference_semantics_are_rejected_before_rewrite() {
    for tag in [
        Tag(0x0008, 0x9124),
        Tag(0x0008, 0x9215),
        Tag(0x0040, 0xa170),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("derived.dcm");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                purpose: FixturePurpose::DerivedMr,
                ..Default::default()
            },
        );
        let mut object = open_file(&path).unwrap();
        object.put(DataElement::new(
            tag,
            VR::SQ,
            Value::Sequence(DataSetSequence::new(
                vec![InMemDicomObject::new_empty()],
                Length::UNDEFINED,
            )),
        ));
        object.write_to_file(&path).unwrap();

        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot yet be preserved atomically"),
            "tag {tag}: {error:#}"
        );
    }
}

#[test]
fn source_image_sequence_preserves_exact_pseudonymous_sop_references() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let path = source.join("source-reference.dcm");
    support::write_functional_epi_fixture(&path, 1, &FunctionalDicomOptions::default());
    let source_uids = (1..=51)
        .map(|index| format!("1.2.826.0.1.3680043.10.543.12345.{index}"))
        .collect::<Vec<_>>();
    let mut object = open_file(&path).unwrap();
    let items: Vec<_> = source_uids
        .iter()
        .map(|uid| {
            let mut item = InMemDicomObject::new_empty();
            item.put_str(Tag(0x0008, 0x1150), VR::UI, "1.2.840.10008.5.1.4.1.1.4");
            item.put_str(Tag(0x0008, 0x1155), VR::UI, uid.as_str());
            item
        })
        .collect();
    object.put(DataElement::new(
        Tag(0x0008, 0x2112),
        VR::SQ,
        Value::Sequence(DataSetSequence::new(items, Length::UNDEFINED)),
    ));
    object.write_to_file(&path).unwrap();

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
    let rewritten = &entries["dicom/000001.dcm"];
    for source_uid in &source_uids {
        assert!(!String::from_utf8_lossy(rewritten).contains(source_uid.as_str()));
    }
    let extracted = directory.path().join("source-reference-sanitized.dcm");
    std::fs::write(&extracted, rewritten).unwrap();
    let object = open_file(&extracted).unwrap();
    let references = object
        .element(Tag(0x0008, 0x2112))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(references.len(), 51);
    for reference in references {
        assert_eq!(reference.iter().count(), 2);
        assert_eq!(
            reference
                .element(Tag(0x0008, 0x1150))
                .unwrap()
                .to_str()
                .unwrap(),
            "1.2.840.10008.5.1.4.1.1.4"
        );
        assert!(
            reference
                .element(Tag(0x0008, 0x1155))
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("2.25.")
        );
    }
}

#[test]
fn source_image_sequence_rejects_non_atomic_reference_items() {
    for case in [
        "empty",
        "missing-instance",
        "extra-frame",
        "nonstandard-class",
    ] {
        let options = FunctionalDicomOptions::default();
        let error = archive_error_after_mutation(options, |bytes| {
            let directory = tempdir().unwrap();
            let path = directory.path().join("mutated.dcm");
            std::fs::write(&path, bytes.as_slice()).unwrap();
            let mut object = open_file(&path).unwrap();
            let mut item = InMemDicomObject::new_empty();
            item.put_str(
                Tag(0x0008, 0x1150),
                VR::UI,
                if case == "nonstandard-class" {
                    "2.25.100000000000000000000000000000000098"
                } else {
                    "1.2.840.10008.5.1.4.1.1.4"
                },
            );
            if case != "missing-instance" {
                item.put_str(
                    Tag(0x0008, 0x1155),
                    VR::UI,
                    "1.3.12.2.1107.5.2.43.67060.2019042614130880159442511",
                );
            }
            if case == "extra-frame" {
                item.put_str(Tag(0x0008, 0x1160), VR::IS, "1");
            }
            object.put(DataElement::new(
                Tag(0x0008, 0x2112),
                VR::SQ,
                Value::Sequence(DataSetSequence::new(
                    if case == "empty" {
                        Vec::new()
                    } else {
                        vec![item]
                    },
                    Length::UNDEFINED,
                )),
            ));
            object.write_to_file(&path).unwrap();
            *bytes = std::fs::read(path).unwrap();
        });
        assert!(
            error.contains("Source Image Sequence"),
            "case {case}: {error}"
        );
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
fn philips_private_lut_label_exception_is_exact_source_only_and_context_bound() {
    for case in [
        "wrong-value",
        "wrong-vr",
        "wrong-creator",
        "wrong-sequence-offset",
        "root-label",
        "other-rwvm-member",
        "reviewed-scale-sequence",
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("image.dcm");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                philips_non_scaling_per_frame_container: case != "reviewed-scale-sequence",
                ..Default::default()
            },
        );
        let mut object = open_file(&path).unwrap();
        match case {
            "wrong-value" | "wrong-vr" | "other-rwvm-member" | "reviewed-scale-sequence" => {
                let mut sequence = object.take_element(Tag(0x2005, 0x140F)).unwrap();
                let item = &mut sequence.items_mut().unwrap()[0];
                match case {
                    "wrong-value" => {
                        item.put_str(Tag(0x0040, 0x9210), VR::SH, "PHILIPS");
                    }
                    "wrong-vr" => {
                        item.put_str(Tag(0x0040, 0x9210), VR::LO, "Philips");
                    }
                    "other-rwvm-member" => {
                        item.put(DataElement::new(
                            Tag(0x0040, 0x9211),
                            VR::US,
                            PrimitiveValue::from(1_u16),
                        ));
                    }
                    "reviewed-scale-sequence" => {
                        item.put_str(Tag(0x0040, 0x9210), VR::SH, "Philips");
                    }
                    _ => unreachable!(),
                }
                object.put(sequence);
            }
            "wrong-creator" => {
                object.put_str(Tag(0x2005, 0x0014), VR::LO, "Philips MR Imaging DD 001");
            }
            "wrong-sequence-offset" => {
                let sequence = object.take_element(Tag(0x2005, 0x140F)).unwrap();
                let (_, value) = sequence.into_parts();
                object.put(DataElement::new(Tag(0x2005, 0x141F), VR::SQ, value));
            }
            "root-label" => {
                object.put_str(Tag(0x0040, 0x9210), VR::SH, "Philips");
            }
            _ => unreachable!(),
        }
        object.write_to_file(&path).unwrap();

        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "DICOM contains an unsupported pixel transform",
            "case {case}"
        );
    }
}

#[test]
fn philips_enhanced_root_lut_label_is_source_only_and_dropped() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let path = source.join("enhanced.dcm");
    support::write_functional_epi_fixture(
        &path,
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            ..Default::default()
        },
    );
    let mut object = open_file(&path).unwrap();
    object.put_str(Tag(0x0040, 0x9210), VR::SH, "Philips");
    object.write_to_file(&path).unwrap();

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
    let extracted = directory.path().join("enhanced-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let sanitized = open_file(&extracted).unwrap();
    assert!(sanitized.element(Tag(0x0040, 0x9210)).is_err());
}

#[test]
fn philips_enhanced_root_lut_label_exception_rejects_every_lookalike() {
    for case in [
        "wrong-manufacturer",
        "wrong-value",
        "wrong-vr",
        "multiple-values",
        "nested",
        "other-rwvm-member",
        "rwvm-sequence",
        "classic-iod",
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("image.dcm");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                vendor: if case == "classic-iod" {
                    FixtureVendor::PhilipsClassic
                } else {
                    FixtureVendor::PhilipsEnhanced
                },
                ..Default::default()
            },
        );
        let mut object = open_file(&path).unwrap();
        if case != "nested" {
            object.put_str(Tag(0x0040, 0x9210), VR::SH, "Philips");
        }
        match case {
            "wrong-manufacturer" => {
                object.put_str(Tag(0x0008, 0x0070), VR::LO, "Philips Healthcare");
            }
            "wrong-value" => {
                object.put_str(Tag(0x0040, 0x9210), VR::SH, "PHILIPS");
            }
            "wrong-vr" => {
                object.put_str(Tag(0x0040, 0x9210), VR::LO, "Philips");
            }
            "multiple-values" => {
                object.put_str(Tag(0x0040, 0x9210), VR::SH, "Philips\\Philips");
            }
            "nested" => {
                let mut item = InMemDicomObject::new_empty();
                item.put_str(Tag(0x0040, 0x9210), VR::SH, "Philips");
                object.put(DataElement::new(
                    Tag(0x0040, 0x0555),
                    VR::SQ,
                    Value::Sequence(DataSetSequence::new(vec![item], Length::UNDEFINED)),
                ));
            }
            "other-rwvm-member" => {
                object.put(DataElement::new(
                    Tag(0x0040, 0x9211),
                    VR::US,
                    PrimitiveValue::from(1_u16),
                ));
            }
            "rwvm-sequence" => {
                object.put(DataElement::new(
                    Tag(0x0040, 0x9096),
                    VR::SQ,
                    Value::Sequence(DataSetSequence::new(
                        vec![InMemDicomObject::new_empty()],
                        Length::UNDEFINED,
                    )),
                ));
            }
            "classic-iod" => {}
            _ => unreachable!(),
        }
        object.write_to_file(&path).unwrap();

        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("unsupported pixel transform")
                || error
                    .to_string()
                    .contains("RealWorldValueMapping is not supported"),
            "case {case}: {error:#}"
        );
    }
}

#[test]
fn enhanced_shared_localizer_references_are_rebuilt_atomically() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let path = source.join("enhanced.dcm");
    support::write_functional_epi_fixture(
        &path,
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            ..Default::default()
        },
    );
    let source_items = (0..3)
        .map(philips_localizer_reference_item)
        .collect::<Vec<_>>();
    let source_uids = source_items
        .iter()
        .map(|item| {
            item.element(Tag(0x0008, 0x1155))
                .unwrap()
                .to_str()
                .unwrap()
                .into_owned()
        })
        .collect::<Vec<_>>();
    let mut object = open_file(&path).unwrap();
    object.put_str(Tag(0x0040, 0x9210), VR::SH, "Philips");
    put_shared_referenced_images(&mut object, source_items);
    object.write_to_file(&path).unwrap();

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
    let rewritten = &entries["dicom/000001.dcm"];
    for source_uid in &source_uids {
        assert!(!String::from_utf8_lossy(rewritten).contains(source_uid));
    }
    let extracted = directory.path().join("enhanced-sanitized.dcm");
    std::fs::write(&extracted, rewritten).unwrap();
    let sanitized = open_file(&extracted).unwrap();
    assert!(sanitized.element(Tag(0x0040, 0x9210)).is_err());
    let shared = sanitized
        .element(Tag(0x5200, 0x9229))
        .unwrap()
        .items()
        .unwrap();
    let references = shared[0]
        .element(Tag(0x0008, 0x1140))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(references.len(), 3);
    for (index, item) in references.iter().enumerate() {
        assert_eq!(item.iter().count(), 4);
        assert_eq!(
            item.element(Tag(0x0008, 0x1150))
                .unwrap()
                .to_str()
                .unwrap()
                .trim(),
            "1.2.840.10008.5.1.4.1.1.4.1"
        );
        let instance = item.element(Tag(0x0008, 0x1155)).unwrap().to_str().unwrap();
        assert!(instance.starts_with("2.25."));
        assert_ne!(instance.trim(), source_uids[index]);
        assert_eq!(
            item.element(Tag(0x0008, 0x1160))
                .unwrap()
                .to_str()
                .unwrap()
                .trim(),
            ["127", "11", "18"][index]
        );
        assert!(item.element(Tag(0x2005, 0x0014)).is_err());
        assert!(item.element(Tag(0x2005, 0x1411)).is_err());
        let purpose = item.element(Tag(0x0040, 0xa170)).unwrap().items().unwrap();
        assert_eq!(purpose.len(), 1);
        assert_eq!(purpose[0].iter().count(), 4);
        for (tag, value) in [
            (Tag(0x0008, 0x0100), "121311"),
            (Tag(0x0008, 0x0102), "DCM"),
            (Tag(0x0008, 0x0104), "Localizer"),
            (Tag(0x0008, 0x0117), "1.2.840.10008.6.1.508"),
        ] {
            assert_eq!(
                purpose[0].element(tag).unwrap().to_str().unwrap().trim(),
                value
            );
        }
    }
    assert_no_fixture_identity_leaks(rewritten);
}

#[test]
fn enhanced_localizer_reference_contract_rejects_partial_or_lookalike_semantics() {
    for case in [
        "empty",
        "missing-reference-key",
        "extra-reference-key",
        "wrong-sop-class",
        "zero-frame",
        "wrong-frame-vr",
        "missing-purpose-key",
        "extra-purpose-key",
        "wrong-code",
        "wrong-code-vr",
        "wrong-code-vm",
        "wrong-context-uid",
        "missing-private-duplicate",
        "private-creator-lookalike",
        "extra-private-key",
        "off-context-purpose",
        "off-context-reference",
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("enhanced.dcm");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                ..Default::default()
            },
        );
        let mut object = open_file(&path).unwrap();
        let mut item = philips_localizer_reference_item(0);
        match case {
            "missing-reference-key" => {
                assert!(item.remove_element(Tag(0x0008, 0x1160)));
            }
            "extra-reference-key" => {
                item.put_str(Tag(0x0010, 0x0010), VR::PN, "LOOKALIKE^IDENTITY");
            }
            "wrong-sop-class" => {
                item.put_str(Tag(0x0008, 0x1150), VR::UI, "1.2.840.10008.5.1.4.1.1.4");
            }
            "zero-frame" => {
                item.put_str(Tag(0x0008, 0x1160), VR::IS, "0");
            }
            "wrong-frame-vr" => {
                item.put_str(Tag(0x0008, 0x1160), VR::LO, "127");
            }
            "missing-purpose-key"
            | "extra-purpose-key"
            | "wrong-code"
            | "wrong-code-vr"
            | "wrong-code-vm"
            | "wrong-context-uid" => {
                let mut purpose = item.take_element(Tag(0x0040, 0xa170)).unwrap();
                let code = &mut purpose.items_mut().unwrap()[0];
                match case {
                    "missing-purpose-key" => {
                        assert!(code.remove_element(Tag(0x0008, 0x0104)));
                    }
                    "extra-purpose-key" => {
                        code.put_str(Tag(0x0008, 0x0103), VR::SH, "1");
                    }
                    "wrong-code" => {
                        code.put_str(Tag(0x0008, 0x0100), VR::SH, "121322");
                    }
                    "wrong-code-vr" => {
                        code.put_str(Tag(0x0008, 0x0100), VR::LO, "121311");
                    }
                    "wrong-code-vm" => {
                        code.put_str(Tag(0x0008, 0x0100), VR::SH, "121311\\121311");
                    }
                    "wrong-context-uid" => {
                        code.put_str(Tag(0x0008, 0x0117), VR::UI, "1.2.840.10008.6.1.509");
                    }
                    _ => unreachable!(),
                }
                item.put(purpose);
            }
            "missing-private-duplicate" => {
                assert!(item.remove_element(Tag(0x2005, 0x1411)));
            }
            "private-creator-lookalike" => {
                item.put_str(Tag(0x2005, 0x0014), VR::LO, "Philips MR Imaging DD 001");
            }
            "extra-private-key" => {
                item.put_str(Tag(0x2005, 0x1412), VR::IS, "7");
            }
            "empty" | "off-context-purpose" | "off-context-reference" => {}
            _ => unreachable!(),
        }

        match case {
            "empty" => put_shared_referenced_images(&mut object, Vec::new()),
            "off-context-purpose" => {
                object.put(item.take_element(Tag(0x0040, 0xa170)).unwrap());
            }
            "off-context-reference" => {
                object.put(DataElement::new(
                    Tag(0x0008, 0x1140),
                    VR::SQ,
                    Value::Sequence(DataSetSequence::new(vec![item], Length::UNDEFINED)),
                ));
            }
            _ => put_shared_referenced_images(&mut object, vec![item]),
        }
        object.write_to_file(&path).unwrap();
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("Referenced Image Sequence")
                || error.to_string().contains("derived/reference semantics"),
            "case {case}: {error:#}"
        );
    }
}

#[test]
fn classic_root_referenced_image_sequence_remains_supported() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let path = source.join("classic.dcm");
    support::write_functional_epi_fixture(&path, 1, &FunctionalDicomOptions::default());
    let source_uid = "1.3.12.2.1107.5.2.43.67060.2019042614130880159442512";
    let mut reference = InMemDicomObject::new_empty();
    // GE classic exports include the retired standard group-length element in
    // each reference item. It is source-only bookkeeping, not scientific
    // metadata, and must not survive the canonical rewrite.
    reference.put(DataElement::new(
        Tag(0x0008, 0x0000),
        VR::UL,
        PrimitiveValue::from(100_u32),
    ));
    reference.put_str(Tag(0x0008, 0x1150), VR::UI, "1.2.840.10008.5.1.4.1.1.4");
    reference.put_str(Tag(0x0008, 0x1155), VR::UI, source_uid);
    let mut object = open_file(&path).unwrap();
    object.put(DataElement::new(
        Tag(0x0008, 0x1140),
        VR::SQ,
        Value::Sequence(DataSetSequence::new(vec![reference], Length::UNDEFINED)),
    ));
    object.write_to_file(&path).unwrap();

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
    let rewritten = &entries["dicom/000001.dcm"];
    assert!(!String::from_utf8_lossy(rewritten).contains(source_uid));
    let extracted = directory.path().join("classic-sanitized.dcm");
    std::fs::write(&extracted, rewritten).unwrap();
    let sanitized = open_file(&extracted).unwrap();
    let references = sanitized
        .element(Tag(0x0008, 0x1140))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].iter().count(), 2);
    assert!(references[0].element(Tag(0x0008, 0x0000)).is_err());
    assert!(
        references[0]
            .element(Tag(0x0008, 0x1155))
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("2.25.")
    );
}

#[test]
fn classic_ge_reference_group_length_is_source_only_and_bounded() {
    for case in [
        "wrong-vr",
        "zero",
        "too-large",
        "multiple-values",
        "free-text",
        "extra-field",
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("classic.dcm");
        support::write_functional_epi_fixture(&path, 1, &FunctionalDicomOptions::default());
        let mut reference = InMemDicomObject::new_empty();
        reference.put_str(Tag(0x0008, 0x1150), VR::UI, "1.2.840.10008.5.1.4.1.1.4");
        reference.put_str(
            Tag(0x0008, 0x1155),
            VR::UI,
            "1.3.12.2.1107.5.2.43.67060.2019042614130880159442512",
        );
        match case {
            "wrong-vr" => reference.put_str(Tag(0x0008, 0x0000), VR::US, "100"),
            "zero" => reference.put(DataElement::new(
                Tag(0x0008, 0x0000),
                VR::UL,
                PrimitiveValue::from(0_u32),
            )),
            "too-large" => reference.put(DataElement::new(
                Tag(0x0008, 0x0000),
                VR::UL,
                PrimitiveValue::from(1_048_577_u32),
            )),
            "multiple-values" => reference.put(DataElement::new(
                Tag(0x0008, 0x0000),
                VR::UL,
                PrimitiveValue::from([100_u32, 101_u32]),
            )),
            "free-text" => reference.put_str(Tag(0x0008, 0x0000), VR::LO, "SITE"),
            "extra-field" => reference.put_str(Tag(0x0008, 0x1160), VR::IS, "1"),
            _ => unreachable!(),
        };
        let mut object = open_file(&path).unwrap();
        object.put(DataElement::new(
            Tag(0x0008, 0x1140),
            VR::SQ,
            Value::Sequence(DataSetSequence::new(vec![reference], Length::UNDEFINED)),
        ));
        object.write_to_file(&path).unwrap();
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("Referenced Image Sequence"),
            "case {case}: {error:#}"
        );
    }
}

#[test]
fn incomplete_philips_dynamic_private_timing_does_not_block_standard_dicom() {
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
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
}

#[test]
fn nonredundant_or_malformed_philips_dynamic_timing_uses_public_timing() {
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
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
    }
}

#[test]
fn malformed_philips_private_scientific_metadata_is_dropped() {
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
    assert!(group.philips_private_pixel_scaling_incomplete);
    assert!(group.all_philips_pixel_scaling_contracts_verified);
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
}

#[test]
fn philips_optional_private_metadata_only_requires_an_atomic_scale_pair_when_used() {
    let directory = tempdir().unwrap();
    for (case, options, expected) in [
        (
            "slice-only",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                purpose: FixturePurpose::OtherMr,
                philips_omit_private_scale_intercept: true,
                philips_omit_private_scale_slope: true,
                philips_omit_water_fat_shift: true,
                philips_omit_public_pixel_scaling: true,
                ..Default::default()
            },
            ClassificationDecision::Accepted,
        ),
        (
            "water-fat-shift-only",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                purpose: FixturePurpose::OtherMr,
                philips_omit_private_scale_intercept: true,
                philips_omit_private_scale_slope: true,
                philips_omit_number_of_slices: true,
                philips_omit_public_pixel_scaling: true,
                ..Default::default()
            },
            ClassificationDecision::Accepted,
        ),
        (
            "complete-private-pair",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                purpose: FixturePurpose::OtherMr,
                philips_omit_public_pixel_scaling: true,
                ..Default::default()
            },
            ClassificationDecision::Accepted,
        ),
        (
            "orphan-private-slope",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                purpose: FixturePurpose::OtherMr,
                philips_omit_private_scale_intercept: true,
                philips_omit_public_pixel_scaling: true,
                ..Default::default()
            },
            ClassificationDecision::Held,
        ),
        (
            "malformed-private-slope",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                purpose: FixturePurpose::OtherMr,
                philips_private_metadata_malformed: true,
                philips_omit_public_pixel_scaling: true,
                ..Default::default()
            },
            ClassificationDecision::Held,
        ),
        (
            "malformed-private-with-public-fallback",
            FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsClassic,
                purpose: FixturePurpose::OtherMr,
                philips_private_metadata_malformed: true,
                ..Default::default()
            },
            ClassificationDecision::Accepted,
        ),
    ] {
        let source = directory.path().join(case);
        std::fs::create_dir(&source).unwrap();
        support::write_functional_epi_fixture(&source.join("instance.dcm"), 1, &options);
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let classification = classify_header(group);
        assert_eq!(classification.decision, expected, "case {case}");
        if case.ends_with("only") {
            assert!(!group.philips_private_pixel_scaling_present, "case {case}");
        }
    }
}

#[test]
fn ps315_safe_private_diffusion_attributes_survive_with_exact_creator_vr_and_vm() {
    let directory = tempdir().unwrap();
    let pseudonymizer = Pseudonymizer::from_base64(TEST_KEY).unwrap();

    for (label, vendor, expected_exception) in [
        (
            "siemens",
            FixtureVendor::Siemens,
            "dicom_ps3.15_siemens_mr_header_diffusion",
        ),
        (
            "philips",
            FixtureVendor::PhilipsClassic,
            "dicom_ps3.15_philips_diffusion",
        ),
        ("ge", FixtureVendor::Ge, "dicom_ps3.15_ge_diffusion_b_value"),
        (
            "uih",
            FixtureVendor::Uih,
            "uih_image_private_header_diffusion_numeric_v1",
        ),
    ] {
        let source = directory.path().join(format!("{label}-source"));
        let bundles = directory.path().join(format!("{label}-bundles"));
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join("diffusion.dcm"),
            1,
            &FunctionalDicomOptions {
                vendor,
                purpose: FixturePurpose::Diffusion,
                include_privacy_leaks: true,
                ..Default::default()
            },
        );
        let discovery = discover(&source).unwrap();
        let classification = classify_header(&discovery.series[0]);
        assert_eq!(classification.decision, ClassificationDecision::Accepted);
        assert_eq!(classification.kind, "diffusion");
        let bundle = create_dicom_archive(ArchiveRequest {
            group: &discovery.series[0],
            classification,
            pseudonymizer: &pseudonymizer,
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap();
        assert!(bundle.classification.evidence.iter().any(|item| item.code
            == "diffusion_scientific_metadata_contract_verified"
            && item.source == "dicom_header"
            && item.effect == "supports"));
        assert!(!bundle.qc.warnings.iter().any(|warning| {
            warning == "diffusion_metadata_completeness_requires_server_validation"
        }));
        let expected_vendor_warnings: &[&str] = match vendor {
            FixtureVendor::PhilipsClassic => &[
                "retained_philips_dd001_diffusion_vector_attributes:3",
                "retained_philips_dd005_diffusion_index_attributes:2",
            ],
            FixtureVendor::Ge => &["retained_ge_acqu_diffusion_vector_attributes:3"],
            FixtureVendor::Uih => &[
                "retained_uih_grid_slice_count_attributes:1",
                "retained_uih_diffusion_attributes:2",
            ],
            _ => &[],
        };
        for warning in expected_vendor_warnings {
            assert!(
                bundle.qc.warnings.iter().any(|actual| actual == warning),
                "missing QC warning {warning} for {label}"
            );
        }
        let entries = read_archive(&bundle.archive.unwrap().object.local_path);
        let manifest: serde_json::Value =
            serde_json::from_slice(&entries["manifest.json"]).unwrap();
        assert!(
            manifest["deidentification"]["safe_private_exceptions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == expected_exception),
            "missing {expected_exception} for {label}"
        );
        let expected_vendor_exceptions: &[&str] = match vendor {
            FixtureVendor::PhilipsClassic => &[
                "philips_mr_imaging_dd_001_diffusion_gradient_vector_numeric_v1",
                "philips_mr_imaging_dd_005_diffusion_indices_numeric_v1",
            ],
            FixtureVendor::Ge => &["ge_gems_acqu_01_diffusion_gradient_vector_numeric_v1"],
            FixtureVendor::Uih => &[
                "uih_image_private_header_grid_slice_count_numeric_v1",
                "uih_image_private_header_diffusion_numeric_v1",
            ],
            _ => &[],
        };
        for exception in expected_vendor_exceptions {
            assert!(
                manifest["deidentification"]["safe_private_exceptions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == exception),
                "missing safe exception {exception} for {label}"
            );
        }
        let extracted = directory.path().join(format!("{label}-sanitized.dcm"));
        std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
        let object = open_file(&extracted).unwrap();
        match vendor {
            FixtureVendor::Siemens => {
                assert_eq!(
                    object
                        .element(Tag(0x0019, 0x0010))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "SIEMENS MR HEADER"
                );
                assert_eq!(
                    object
                        .element(Tag(0x0019, 0x100C))
                        .unwrap()
                        .to_int::<i64>()
                        .unwrap(),
                    1000
                );
                assert_eq!(
                    object
                        .element(Tag(0x0019, 0x100D))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "DIRECTIONAL"
                );
                assert_eq!(
                    object
                        .element(Tag(0x0019, 0x100E))
                        .unwrap()
                        .to_multi_float64()
                        .unwrap(),
                    vec![1.0, 0.0, 0.0]
                );
                assert!(object.element(Tag(0x0019, 0x1027)).is_err());
                let csa = object
                    .element(Tag(0x0029, 0x1010))
                    .unwrap()
                    .to_bytes()
                    .unwrap();
                assert_eq!(csa_numeric_field_values(csa.as_ref(), "B_value"), ["1000"]);
                assert_eq!(
                    csa_numeric_field_values(csa.as_ref(), "DiffusionGradientDirection"),
                    ["1", "0", "0"]
                );
                assert!(!String::from_utf8_lossy(csa.as_ref()).contains("B_matrix"));
                assert!(!String::from_utf8_lossy(csa.as_ref()).contains("MrPhoenixProtocol"));
                assert!(object.element(Tag(0x0019, 0x100A)).is_err());
            }
            FixtureVendor::PhilipsClassic => {
                assert_eq!(
                    object
                        .element(Tag(0x2001, 0x0010))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "Philips Imaging DD 001"
                );
                assert_eq!(
                    object
                        .element(Tag(0x2001, 0x1003))
                        .unwrap()
                        .to_float32()
                        .unwrap(),
                    1000.0
                );
                assert_eq!(
                    object
                        .element(Tag(0x2001, 0x1004))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "AP"
                );
                assert_eq!(
                    object
                        .element(Tag(0x2001, 0x1008))
                        .unwrap()
                        .to_int::<i64>()
                        .unwrap(),
                    1
                );
                for (tag, expected) in [
                    (Tag(0x2005, 0x10B0), 1.0),
                    (Tag(0x2005, 0x10B1), 0.0),
                    (Tag(0x2005, 0x10B2), 0.0),
                ] {
                    let element = object.element(tag).unwrap();
                    assert_eq!(element.vr(), VR::FL);
                    assert_eq!(element.to_float32().unwrap(), expected);
                }
                assert_eq!(
                    object
                        .element(Tag(0x2005, 0x0014))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "Philips MR Imaging DD 005"
                );
                for (tag, expected) in [(Tag(0x2005, 0x1412), 7_i64), (Tag(0x2005, 0x1413), 11_i64)]
                {
                    let element = object.element(tag).unwrap();
                    assert_eq!(element.vr(), VR::IS);
                    assert_eq!(element.to_int::<i64>().unwrap(), expected);
                }
                assert!(object.element(Tag(0x2005, 0x14B3)).is_err());
                assert!(object.element(Tag(0x2001, 0x1019)).is_err());
            }
            FixtureVendor::Ge => {
                assert_eq!(
                    object
                        .element(Tag(0x0043, 0x0010))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "GEMS_PARM_01"
                );
                assert_eq!(
                    object
                        .element(Tag(0x0043, 0x1039))
                        .unwrap()
                        .to_multi_int::<i64>()
                        .unwrap(),
                    vec![1000, 0, 0, 0]
                );
                assert_eq!(
                    object
                        .element(Tag(0x0019, 0x0010))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "GEMS_ACQU_01"
                );
                for (tag, expected) in [
                    (Tag(0x0019, 0x10BB), 1.0),
                    (Tag(0x0019, 0x10BC), 0.0),
                    (Tag(0x0019, 0x10BD), 0.0),
                ] {
                    let element = object.element(tag).unwrap();
                    assert_eq!(element.vr(), VR::DS);
                    assert_eq!(element.to_float64().unwrap(), expected);
                }
                assert!(object.element(Tag(0x0019, 0x10BE)).is_err());
                assert!(object.element(Tag(0x0043, 0x1040)).is_err());
            }
            FixtureVendor::Uih => {
                assert_eq!(
                    object
                        .element(Tag(0x0065, 0x0010))
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    "Image Private Header"
                );
                let slices = object.element(Tag(0x0065, 0x1050)).unwrap();
                assert_eq!(slices.vr(), VR::DS);
                assert_eq!(slices.to_str().unwrap(), "42");
                let b_value = object.element(Tag(0x0065, 0x1009)).unwrap();
                assert_eq!(b_value.vr(), VR::FD);
                assert_eq!(b_value.to_float64().unwrap(), 1000.0);
                let gradient = object.element(Tag(0x0065, 0x1037)).unwrap();
                assert_eq!(gradient.vr(), VR::FD);
                assert_eq!(gradient.to_multi_float64().unwrap(), vec![1.0, 0.0, 0.0]);
                assert!(object.element(Tag(0x0065, 0x1051)).is_err());
                assert!(object.element(Tag(0x0065, 0x1038)).is_err());
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn siemens_csa_b_matrix_is_retained_without_gradient_or_text() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("diffusion-b-matrix.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::Siemens,
            purpose: FixturePurpose::Diffusion,
            siemens_csa_b_matrix: true,
            include_privacy_leaks: true,
            ..Default::default()
        },
    );

    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let classification = classify_header(group);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
    assert_eq!(classification.kind, "diffusion");
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        classification,
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();

    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let extracted = directory.path().join("diffusion-b-matrix-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    let csa = object
        .element(Tag(0x0029, 0x1010))
        .unwrap()
        .to_bytes()
        .unwrap();
    assert_eq!(csa_numeric_field_values(csa.as_ref(), "B_value"), ["1000"]);
    assert_eq!(
        csa_numeric_field_values(csa.as_ref(), "B_matrix"),
        ["1000", "0", "0", "0", "0", "0"]
    );
    assert_eq!(
        object
            .element(Tag(0x0019, 0x100D))
            .unwrap()
            .to_str()
            .unwrap(),
        "BMATRIX"
    );
    assert_eq!(
        object
            .element(Tag(0x0019, 0x1027))
            .unwrap()
            .to_multi_float64()
            .unwrap(),
        vec![1000.0, 0.0, 0.0, 0.0, 0.0, 0.0]
    );
    assert!(object.element(Tag(0x0019, 0x100E)).is_err());
    let csa_text = String::from_utf8_lossy(csa.as_ref());
    assert!(!csa_text.contains("DiffusionGradientDirection"));
    assert!(!csa_text.contains("MrPhoenixProtocol"));
    assert!(!csa_text.contains("ASCCONV"));
    assert_no_fixture_identity_leaks(&entries["dicom/000001.dcm"]);
}

#[test]
fn philips_private_diffusion_direction_drops_unreviewed_site_code() {
    const HOSTILE_SITE_CODE: &str = "SITE_SECRET";
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("philips-hostile-direction.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsClassic,
            purpose: FixturePurpose::Diffusion,
            philips_diffusion_direction_override: Some(HOSTILE_SITE_CODE),
            ..Default::default()
        },
    );

    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert_eq!(
        classify_header(group).decision,
        ClassificationDecision::Held
    );
    let bundle = create_dicom_archive(ArchiveRequest {
        group,
        // Isolate archive privacy behavior from the classifier's intentional
        // scientific hold for the malformed private source contract.
        classification: Classification {
            decision: ClassificationDecision::Accepted,
            kind: "diffusion".into(),
            confidence: 1.0,
            evidence: Vec::new(),
        },
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();

    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let extracted = directory
        .path()
        .join("philips-hostile-direction-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    assert!(object.element(Tag(0x2001, 0x1004)).is_err());
    assert!(!String::from_utf8_lossy(&entries["dicom/000001.dcm"]).contains(HOSTILE_SITE_CODE));
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
    let ge_classification = classify_header(&ge_discovery.series[0]);
    assert_eq!(ge_classification.decision, ClassificationDecision::Accepted);
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

    let path_source = directory.path().join("path-source");
    let path_bundles = directory.path().join("path-bundles");
    std::fs::create_dir(&path_source).unwrap();
    std::fs::create_dir(&path_bundles).unwrap();
    support::write_functional_epi_fixture(
        &path_source.join("path.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::Generic,
            model_override: Some("/home/paul/scanner"),
            software_versions_override: Some("https://scanner.invalid/build"),
            ..Default::default()
        },
    );
    let path_discovery = discover(&path_source).unwrap();
    let path_group = &path_discovery.series[0];
    let path_bundle = create_dicom_archive(ArchiveRequest {
        group: path_group,
        classification: classify_header(path_group),
        pseudonymizer: &pseudonymizer,
        bundle_root: &path_bundles,
        progress: |_| {},
    })
    .unwrap();
    let path_entries = read_archive(&path_bundle.archive.unwrap().object.local_path);
    let path_dicom = &path_entries["dicom/000001.dcm"];
    assert!(!String::from_utf8_lossy(path_dicom).contains("/home/paul"));
    assert!(!String::from_utf8_lossy(path_dicom).contains("https://"));
    let path_extracted = directory.path().join("path-sanitized.dcm");
    std::fs::write(&path_extracted, path_dicom).unwrap();
    let path_object = open_file(&path_extracted).unwrap();
    assert!(path_object.element(Tag(0x0008, 0x1090)).is_err());
    assert!(path_object.element(Tag(0x0018, 0x1020)).is_err());

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
    let hostile_classification = classify_header(&hostile_discovery.series[0]);
    assert_eq!(
        hostile_classification.decision,
        ClassificationDecision::Accepted
    );
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
    assert!(
        hostile_object
            .element(Tag(0x0008, 0x0070))
            .unwrap()
            .is_empty(),
        "unsafe Manufacturer should become the required empty Type 2 shell"
    );
    for tag in [
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
fn unknown_vendor_enhanced_mr_retains_bounded_equipment_identity() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("future-enhanced.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::Generic,
            sop_class_override: Some("1.2.840.10008.5.1.4.1.1.4.1"),
            model_override: Some("FutureMR Research 9000"),
            software_versions_override: Some("NextGen 27.4"),
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
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest["source"]["manufacturer"], "FIXTURE_VENDOR");
    assert_eq!(manifest["source"]["model"], "FutureMR Research 9000");
    assert_eq!(
        manifest["source"]["software_versions"],
        serde_json::json!(["NextGen 27.4"])
    );
    let extracted = directory.path().join("future-enhanced-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    assert_eq!(
        object
            .element(Tag(0x0008, 0x0070))
            .unwrap()
            .to_str()
            .unwrap(),
        "FIXTURE_VENDOR"
    );
    assert_eq!(
        object
            .element(Tag(0x0008, 0x1090))
            .unwrap()
            .to_str()
            .unwrap(),
        "FutureMR Research 9000"
    );
    assert_eq!(
        object
            .element(Tag(0x0018, 0x1020))
            .unwrap()
            .to_str()
            .unwrap(),
        "NextGen 27.4"
    );
    let serial = object
        .element(Tag(0x0018, 0x1000))
        .unwrap()
        .to_str()
        .unwrap();
    assert!(serial.starts_with("SN-"));
    assert!(!String::from_utf8_lossy(&entries["dicom/000001.dcm"]).contains("FIXTURE-SERIAL-001"));
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
fn enhanced_multiframe_extended_offset_tables_are_validated_and_retained_byte_exact() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let source_path = source.join("enhanced-eot.dcm");
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::PhilipsEnhanced,
        encapsulated_pixel_data: true,
        extended_offset_table: true,
        pixel_bytes: 128 * 1024,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source_path, 1, &options);
    let source_bytes = std::fs::read(&source_path).unwrap();
    let (expected_offsets, expected_lengths, expected_pixel_data) =
        support::fixture_extended_offset_table_pixel_element(options.pixel_bytes);

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
    assert!(
        dicom.ends_with(&expected_pixel_data),
        "the encapsulated PixelData byte span must remain immutable"
    );
    for tag in [Tag(0x7FE0, 0x0001), Tag(0x7FE0, 0x0002)] {
        assert_eq!(
            explicit_long_vr_value(dicom, tag, b"OV"),
            explicit_long_vr_value(&source_bytes, tag, b"OV"),
            "the numeric OV value bytes must remain exact for {tag}"
        );
    }

    let extracted = directory.path().join("enhanced-eot-sanitized.dcm");
    std::fs::write(&extracted, dicom).unwrap();
    let object = open_file(&extracted).unwrap();
    let offsets = object.element(Tag(0x7FE0, 0x0001)).unwrap();
    let lengths = object.element(Tag(0x7FE0, 0x0002)).unwrap();
    assert_eq!(offsets.vr(), VR::OV);
    assert_eq!(lengths.vr(), VR::OV);
    assert_eq!(offsets.to_multi_int::<u64>().unwrap(), expected_offsets);
    assert_eq!(lengths.to_multi_int::<u64>().unwrap(), expected_lengths);
}

#[test]
fn malformed_extended_offset_table_is_rejected_before_archive_upload() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let source_path = source.join("bad-eot.dcm");
    support::write_functional_epi_fixture(
        &source_path,
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            encapsulated_pixel_data: true,
            extended_offset_table: true,
            ..Default::default()
        },
    );
    let mut bytes = std::fs::read(&source_path).unwrap();
    let range = explicit_long_vr_value_range(&bytes, Tag(0x7FE0, 0x0001), b"OV");
    bytes[range.start + 8..range.start + 16].copy_from_slice(&1_u64.to_le_bytes());
    std::fs::write(&source_path, bytes).unwrap();

    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    let error = create_dicom_archive(ArchiveRequest {
        group,
        classification: classify_header(group),
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "DICOM Extended Offset Table failed structural validation"
    );
    assert!(
        format!("{error:#}").contains("does not point to its frame Item Tag"),
        "the internal rejection should retain a precise structural cause: {error:#}"
    );
}

#[test]
fn pixel_module_and_complete_pvt_round_trip_with_exact_pixel_data_and_type_two_shells() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::Generic,
        model_override: Some("FutureMR Research 9000"),
        software_versions_override: Some("NextGen 27.4"),
        include_pixel_value_transform: true,
        pixel_bytes: 96 * 1024,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source.join("future-scanner.dcm"), 1, &options);
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
    let entries = read_archive(&bundle.archive.unwrap().object.local_path);
    let dicom = &entries["dicom/000001.dcm"];
    let extracted = directory.path().join("future-scanner-sanitized.dcm");
    std::fs::write(&extracted, dicom).unwrap();
    let object = open_file(&extracted).unwrap();
    for (tag, expected) in [
        (Tag(0x0028, 0x0002), 1_u16),
        (Tag(0x0028, 0x0010), 64),
        (Tag(0x0028, 0x0011), 768),
        (Tag(0x0028, 0x0100), 16),
        (Tag(0x0028, 0x0101), 16),
        (Tag(0x0028, 0x0102), 15),
        (Tag(0x0028, 0x0103), 0),
    ] {
        assert_eq!(
            object.element(tag).unwrap().to_int::<u16>().unwrap(),
            expected
        );
    }
    assert_eq!(
        object
            .element(Tag(0x0028, 0x0004))
            .unwrap()
            .to_str()
            .unwrap(),
        "MONOCHROME2"
    );
    let transform = &object
        .element(Tag(0x0028, 0x9145))
        .unwrap()
        .items()
        .unwrap()[0];
    assert_eq!(
        transform
            .element(Tag(0x0028, 0x1052))
            .unwrap()
            .to_str()
            .unwrap(),
        "0"
    );
    assert_eq!(
        transform
            .element(Tag(0x0028, 0x1053))
            .unwrap()
            .to_str()
            .unwrap(),
        "1"
    );
    assert_eq!(
        transform
            .element(Tag(0x0028, 0x1054))
            .unwrap()
            .to_str()
            .unwrap(),
        "US"
    );
    for tag in [
        Tag(0x0008, 0x0020),
        Tag(0x0008, 0x0022),
        Tag(0x0008, 0x0023),
        Tag(0x0008, 0x0030),
        Tag(0x0008, 0x0032),
        Tag(0x0008, 0x0033),
        Tag(0x0008, 0x0050),
        Tag(0x0008, 0x0090),
        Tag(0x0010, 0x0030),
        Tag(0x0010, 0x0040),
        Tag(0x0018, 0x0022),
        Tag(0x0018, 0x0091),
        Tag(0x0020, 0x0010),
        Tag(0x0020, 0x0012),
        Tag(0x0020, 0x1040),
    ] {
        assert!(
            object.element(tag).unwrap().is_empty(),
            "Type 2 shell {tag}"
        );
    }
    assert_eq!(
        object
            .element(Tag(0x7fe0, 0x0010))
            .unwrap()
            .to_bytes()
            .unwrap()
            .as_ref(),
        support::fixture_pixel_bytes(options.pixel_bytes)
    );
    assert_eq!(
        object
            .element(Tag(0x0008, 0x0070))
            .unwrap()
            .to_str()
            .unwrap(),
        "FIXTURE_VENDOR"
    );
    assert_eq!(
        object
            .element(Tag(0x0008, 0x1090))
            .unwrap()
            .to_str()
            .unwrap(),
        "FutureMR Research 9000"
    );
}

#[test]
fn missing_or_inconsistent_pixel_module_is_rejected_before_archive_creation() {
    let missing_photometric =
        archive_error_after_mutation(FunctionalDicomOptions::default(), |bytes| {
            remove_explicit_short_vr_element(bytes, Tag(0x0028, 0x0004), b"CS")
        });
    assert_eq!(
        missing_photometric,
        "DICOM pixel module is missing or inconsistent with its MR SOP Class"
    );

    let bad_bits_stored =
        archive_error_after_mutation(FunctionalDicomOptions::default(), |bytes| {
            set_explicit_us_value(bytes, Tag(0x0028, 0x0101), 17)
        });
    assert_eq!(
        bad_bits_stored,
        "DICOM pixel module is missing or inconsistent with its MR SOP Class"
    );

    let missing_frames = archive_error_after_mutation(
        FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            ..Default::default()
        },
        |bytes| remove_explicit_short_vr_element(bytes, Tag(0x0028, 0x0008), b"IS"),
    );
    assert_eq!(
        missing_frames,
        "DICOM pixel module is missing or inconsistent with its MR SOP Class"
    );
}

#[test]
fn enhanced_mr_requires_explicit_burned_in_annotation_no() {
    for sop_class in ["1.2.840.10008.5.1.4.1.1.4.1", "1.2.840.10008.5.1.4.1.1.4.4"] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join("enhanced-missing-bia.dcm"),
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                sop_class_override: Some(sop_class),
                omit_burned_in_annotation: true,
                ..Default::default()
            },
        );
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        assert_eq!(
            classify_header(group).decision,
            ClassificationDecision::Held,
            "SOP Class {sop_class}"
        );
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: Classification {
                decision: ClassificationDecision::Accepted,
                kind: "functional_epi".to_owned(),
                confidence: 1.0,
                evidence: Vec::new(),
            },
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Enhanced MR omitted required BurnedInAnnotation=NO"
        );
    }
}

#[test]
fn enhanced_mr_unknown_pulse_sequence_name_uses_safe_other_sentinel() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("enhanced-vendor-pulse.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            enhanced_pulse_sequence_name_override: Some("spiral_research"),
            ..Default::default()
        },
    );
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
    assert!(!String::from_utf8_lossy(&entries["dicom/000001.dcm"]).contains("spiral_research"));
    let extracted = directory.path().join("enhanced-vendor-pulse-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    assert_eq!(
        object
            .element(Tag(0x0018, 0x9005))
            .unwrap()
            .to_str()
            .unwrap(),
        "OTHER"
    );
}

#[test]
fn enhanced_mr_drops_empty_classic_echo_time_shell_and_keeps_effective_echo_time() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let path = source.join("enhanced.dcm");
    support::write_functional_epi_fixture(
        &path,
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            ..Default::default()
        },
    );
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
    let extracted = directory.path().join("enhanced-sanitized.dcm");
    std::fs::write(&extracted, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&extracted).unwrap();
    assert!(object.element(Tag(0x0018, 0x0081)).is_err());
    let shared = object
        .element(Tag(0x5200, 0x9229))
        .unwrap()
        .items()
        .unwrap();
    let echo = shared[0]
        .element(Tag(0x0018, 0x9114))
        .unwrap()
        .items()
        .unwrap();
    assert_eq!(
        echo[0]
            .element(Tag(0x0018, 0x9082))
            .unwrap()
            .to_float64()
            .unwrap(),
        30.0
    );
}

#[test]
fn enhanced_mr_mandatory_root_and_functional_group_modules_are_enforced() {
    let options = FunctionalDicomOptions {
        vendor: FixtureVendor::PhilipsEnhanced,
        ..Default::default()
    };
    let missing_presentation = archive_error_after_mutation(options.clone(), |bytes| {
        remove_explicit_short_vr_element(bytes, Tag(0x0008, 0x9205), b"CS")
    });
    assert_eq!(
        missing_presentation,
        "sanitized Enhanced MR omitted mandatory Pixel Presentation"
    );
    let missing_shared = archive_error_after_mutation(options, |bytes| {
        remove_explicit_long_vr_element(bytes, Tag(0x5200, 0x9229), b"SQ")
    });
    assert_eq!(
        missing_shared,
        "sanitized Enhanced MR omitted Shared Functional Groups Sequence"
    );
}

#[test]
fn enhanced_core_functional_groups_are_mandatory_and_context_exclusive() {
    for tag in [
        Tag(0x0028, 0x9110),
        Tag(0x0020, 0x9113),
        Tag(0x0020, 0x9116),
        Tag(0x0020, 0x9071),
        Tag(0x0018, 0x9226),
        Tag(0x0028, 0x9145),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("enhanced.dcm");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                ..Default::default()
            },
        );
        let mut object = open_file(&path).unwrap();
        if tag == Tag(0x0018, 0x9226) {
            let mut per_frame = object.take_element(Tag(0x5200, 0x9230)).unwrap();
            assert!(per_frame.items_mut().unwrap()[0].remove_element(tag));
            object.put(per_frame);
        } else {
            let mut shared = object.take_element(Tag(0x5200, 0x9229)).unwrap();
            assert!(shared.items_mut().unwrap()[0].remove_element(tag));
            object.put(shared);
        }
        object.write_to_file(&path).unwrap();
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        assert!(
            create_dicom_archive(ArchiveRequest {
                group,
                classification: classify_header(group),
                pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
                bundle_root: &bundles,
                progress: |_| {},
            })
            .is_err(),
            "missing macro {tag}"
        );
    }
}

#[test]
fn current_enhanced_requires_dimensions_while_legacy_dimensions_are_optional() {
    for (sop_class, accepted) in [
        ("1.2.840.10008.5.1.4.1.1.4.1", false),
        ("1.2.840.10008.5.1.4.1.1.4.4", true),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("enhanced.dcm");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                sop_class_override: Some(sop_class),
                ..Default::default()
            },
        );
        remove_enhanced_dimensions(&path);
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let result = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        });
        assert_eq!(result.is_ok(), accepted, "SOP Class {sop_class}");
    }
}

#[test]
fn enhanced_context_concatenation_and_opaque_legacy_macros_fail_closed() {
    for case in [
        "context",
        "concatenation",
        "legacy-opaque",
        "legacy-a36-macro",
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("enhanced.dcm");
        let legacy = case.starts_with("legacy");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                sop_class_override: legacy.then_some("1.2.840.10008.5.1.4.1.1.4.4"),
                ..Default::default()
            },
        );
        let mut object = open_file(&path).unwrap();
        match case {
            "context" => object.put(DataElement::new(
                Tag(0x0040, 0x0555),
                VR::SQ,
                Value::Sequence(DataSetSequence::new(
                    vec![InMemDicomObject::new_empty()],
                    Length::UNDEFINED,
                )),
            )),
            "concatenation" => {
                object.put_str(Tag(0x0020, 0x9161), VR::UI, "1.2.826.0.1.3680043.10.999.8")
            }
            "legacy-opaque" => {
                let mut shared = object.take_element(Tag(0x5200, 0x9229)).unwrap();
                let shared_item = &mut shared.items_mut().unwrap()[0];
                let mut converted = shared_item.take_element(Tag(0x0020, 0x9170)).unwrap();
                converted.items_mut().unwrap()[0].put_str(
                    Tag(0x0010, 0x0010),
                    VR::PN,
                    "IDENTITY^LEAK",
                );
                shared_item.put(converted);
                object.put(shared)
            }
            "legacy-a36-macro" => {
                let mut echo = InMemDicomObject::new_empty();
                echo.put(DataElement::new(
                    Tag(0x0018, 0x9082),
                    VR::FD,
                    PrimitiveValue::from(30.0_f64),
                ));
                let mut shared = object.take_element(Tag(0x5200, 0x9229)).unwrap();
                shared.items_mut().unwrap()[0].put(DataElement::new(
                    Tag(0x0018, 0x9114),
                    VR::SQ,
                    Value::Sequence(DataSetSequence::new(vec![echo], Length::UNDEFINED)),
                ));
                object.put(shared)
            }
            _ => unreachable!(),
        };
        object.write_to_file(&path).unwrap();
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        assert!(
            create_dicom_archive(ArchiveRequest {
                group,
                classification: classify_header(group),
                pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
                bundle_root: &bundles,
                progress: |_| {},
            })
            .is_err(),
            "case {case}"
        );
    }
}

#[test]
fn enhanced_macro_placement_and_original_conditionals_fail_closed() {
    for case in [
        "shared-and-per-frame",
        "frame-content-shared",
        "missing-pulse-sequence-field",
        "missing-frame-datetime",
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        let path = source.join("enhanced.dcm");
        support::write_functional_epi_fixture(
            &path,
            1,
            &FunctionalDicomOptions {
                vendor: FixtureVendor::PhilipsEnhanced,
                ..Default::default()
            },
        );
        let mut object = open_file(&path).unwrap();
        match case {
            "shared-and-per-frame" => {
                let mut shared = object.take_element(Tag(0x5200, 0x9229)).unwrap();
                let measures = shared.items_mut().unwrap()[0]
                    .element(Tag(0x0028, 0x9110))
                    .unwrap()
                    .clone();
                object.put(shared);
                let mut per_frame = object.take_element(Tag(0x5200, 0x9230)).unwrap();
                per_frame.items_mut().unwrap()[0].put(measures);
                object.put(per_frame);
            }
            "frame-content-shared" => {
                let mut per_frame = object.take_element(Tag(0x5200, 0x9230)).unwrap();
                let frame_content = per_frame.items_mut().unwrap()[0]
                    .element(Tag(0x0020, 0x9111))
                    .unwrap()
                    .clone();
                object.put(per_frame);
                let mut shared = object.take_element(Tag(0x5200, 0x9229)).unwrap();
                shared.items_mut().unwrap()[0].put(frame_content);
                object.put(shared);
            }
            "missing-pulse-sequence-field" => {
                object.remove_element(Tag(0x0018, 0x9008));
            }
            "missing-frame-datetime" => {
                let mut per_frame = object.take_element(Tag(0x5200, 0x9230)).unwrap();
                let frame = &mut per_frame.items_mut().unwrap()[0];
                let mut frame_content = frame.take_element(Tag(0x0020, 0x9111)).unwrap();
                frame_content.items_mut().unwrap()[0].remove_element(Tag(0x0018, 0x9074));
                frame.put(frame_content);
                object.put(per_frame);
            }
            _ => unreachable!(),
        };
        object.write_to_file(&path).unwrap();
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        assert!(
            create_dicom_archive(ArchiveRequest {
                group,
                classification: classify_header(group),
                pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
                bundle_root: &bundles,
                progress: |_| {},
            })
            .is_err(),
            "case {case}"
        );
    }
}

#[test]
fn enhanced_standard_code_values_match_the_dicom_defined_terms() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let path = source.join("enhanced-valid-codes.dcm");
    support::write_functional_epi_fixture(
        &path,
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            enhanced_image_type_override: Some("DERIVED\\PRIMARY\\FMRI\\NONE"),
            enhanced_frame_type_override: Some("DERIVED\\PRIMARY\\FMRI\\NONE"),
            ..Default::default()
        },
    );
    let mut bytes = std::fs::read(&path).unwrap();
    replace_all_explicit_short_vr_text(&mut bytes, Tag(0x0008, 0x9207), b"CS", "MPR");
    replace_explicit_short_vr_text(&mut bytes, Tag(0x0008, 0x9209), b"CS", "STIR");
    std::fs::write(&path, bytes).unwrap();
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    create_dicom_archive(ArchiveRequest {
        group,
        classification: classify_header(group),
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap();

    let invalid_volume_technique = archive_error_after_mutation(
        FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            ..Default::default()
        },
        |bytes| replace_explicit_short_vr_text(bytes, Tag(0x0008, 0x9207), b"CS", "RECON_PLANAR"),
    );
    assert_eq!(
        invalid_volume_technique,
        "sanitized Enhanced MR omitted mandatory Volume Based Calculation Technique"
    );
    let invalid_lut = archive_error_after_mutation(
        FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            ..Default::default()
        },
        |bytes| replace_explicit_short_vr_text(bytes, Tag(0x2050, 0x0020), b"CS", "INVERSE"),
    );
    assert_eq!(
        invalid_lut,
        "sanitized Enhanced MR has invalid mandatory presentation metadata"
    );
}

#[test]
fn native_pixel_data_length_must_match_the_declared_matrix() {
    let error = archive_error_after_mutation(FunctionalDicomOptions::default(), |bytes| {
        let value = explicit_long_vr_value_range(bytes, Tag(0x7fe0, 0x0010), b"OW");
        let shorter = u32::try_from(value.len() - 2).unwrap().to_le_bytes();
        bytes[value.start - 4..value.start].copy_from_slice(&shorter);
    });
    assert_eq!(
        error,
        "DICOM native PixelData length does not match its declared pixel matrix"
    );
}

#[test]
fn incomplete_or_opaque_pixel_transforms_fail_closed_atomically() {
    for (case, options, expected) in [
        (
            "rwvm",
            FunctionalDicomOptions {
                include_real_world_value_mapping: true,
                ..Default::default()
            },
            "DICOM RealWorldValueMapping is not supported by the privacy writer",
        ),
        (
            "modality-lut",
            FunctionalDicomOptions {
                include_modality_lut: true,
                ..Default::default()
            },
            "DICOM contains an unsupported pixel transform",
        ),
        (
            "partial-pvt",
            FunctionalDicomOptions {
                include_pixel_value_transform: true,
                incomplete_pixel_value_transform: true,
                ..Default::default()
            },
            "DICOM contains an incomplete or invalid rescale transform",
        ),
    ] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(&source.join("image.dcm"), 1, &options);
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let error = create_dicom_archive(ArchiveRequest {
            group,
            classification: classify_header(group),
            pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
            bundle_root: &bundles,
            progress: |_| {},
        })
        .unwrap_err();
        assert_eq!(error.to_string(), expected, "case {case}");
    }
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

fn philips_localizer_reference_item(index: usize) -> InMemDicomObject {
    let frames = [127_u64, 11, 18];
    let mut purpose = InMemDicomObject::new_empty();
    purpose.put_str(Tag(0x0008, 0x0100), VR::SH, "121311");
    purpose.put_str(Tag(0x0008, 0x0102), VR::SH, "DCM");
    purpose.put_str(Tag(0x0008, 0x0104), VR::LO, "Localizer");
    purpose.put_str(Tag(0x0008, 0x0117), VR::UI, "1.2.840.10008.6.1.508");

    let mut item = InMemDicomObject::new_empty();
    item.put_str(Tag(0x0008, 0x1150), VR::UI, "1.2.840.10008.5.1.4.1.1.4.1");
    item.put_str(
        Tag(0x0008, 0x1155),
        VR::UI,
        format!("1.3.46.670589.11.17240.5.20.1.1.8932.2018052615072283{index:03}"),
    );
    item.put_str(
        Tag(0x0008, 0x1160),
        VR::IS,
        frames[index % frames.len()].to_string(),
    );
    item.put(DataElement::new(
        Tag(0x0040, 0xa170),
        VR::SQ,
        Value::Sequence(DataSetSequence::new(vec![purpose], Length::UNDEFINED)),
    ));
    item.put_str(Tag(0x2005, 0x0014), VR::LO, "Philips MR Imaging DD 005");
    item.put_str(
        Tag(0x2005, 0x1411),
        VR::UI,
        format!("1.3.46.670589.11.17240.5.0.8932.2018052615080497{index:03}"),
    );
    item
}

fn put_shared_referenced_images(object: &mut InMemDicomObject, items: Vec<InMemDicomObject>) {
    let mut shared = object.take_element(Tag(0x5200, 0x9229)).unwrap();
    shared.items_mut().unwrap()[0].put(DataElement::new(
        Tag(0x0008, 0x1140),
        VR::SQ,
        Value::Sequence(DataSetSequence::new(items, Length::UNDEFINED)),
    ));
    object.put(shared);
}

fn archive_error_after_mutation(
    options: FunctionalDicomOptions,
    mutate: impl FnOnce(&mut Vec<u8>),
) -> String {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    let path = source.join("image.dcm");
    support::write_functional_epi_fixture(&path, 1, &options);
    let mut bytes = std::fs::read(&path).unwrap();
    mutate(&mut bytes);
    std::fs::write(&path, bytes).unwrap();
    let discovery = discover(&source).unwrap();
    let group = &discovery.series[0];
    assert_eq!(
        classify_header(group).decision,
        ClassificationDecision::Accepted
    );
    create_dicom_archive(ArchiveRequest {
        group,
        classification: classify_header(group),
        pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
        bundle_root: &bundles,
        progress: |_| {},
    })
    .unwrap_err()
    .to_string()
}

fn explicit_short_vr_element_range(bytes: &[u8], tag: Tag, vr: &[u8; 2]) -> std::ops::Range<usize> {
    let marker = [
        tag.group().to_le_bytes()[0],
        tag.group().to_le_bytes()[1],
        tag.element().to_le_bytes()[0],
        tag.element().to_le_bytes()[1],
        vr[0],
        vr[1],
    ];
    let header = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap_or_else(|| panic!("missing explicit short-VR element {tag}"));
    let length = u16::from_le_bytes(bytes[header + 6..header + 8].try_into().unwrap()) as usize;
    header..header + 8 + length
}

fn remove_explicit_short_vr_element(bytes: &mut Vec<u8>, tag: Tag, vr: &[u8; 2]) {
    let range = explicit_short_vr_element_range(bytes, tag, vr);
    bytes.drain(range);
}

fn replace_explicit_short_vr_text(bytes: &mut Vec<u8>, tag: Tag, vr: &[u8; 2], value: &str) {
    let range = explicit_short_vr_element_range(bytes, tag, vr);
    let mut encoded = value.as_bytes().to_vec();
    if encoded.len() % 2 == 1 {
        encoded.push(b' ');
    }
    let mut replacement = Vec::with_capacity(8 + encoded.len());
    replacement.extend_from_slice(&tag.group().to_le_bytes());
    replacement.extend_from_slice(&tag.element().to_le_bytes());
    replacement.extend_from_slice(vr);
    replacement.extend_from_slice(&u16::try_from(encoded.len()).unwrap().to_le_bytes());
    replacement.extend_from_slice(&encoded);
    bytes.splice(range, replacement);
}

fn replace_all_explicit_short_vr_text(bytes: &mut [u8], tag: Tag, vr: &[u8; 2], value: &str) {
    let marker = [
        tag.group().to_le_bytes()[0],
        tag.group().to_le_bytes()[1],
        tag.element().to_le_bytes()[0],
        tag.element().to_le_bytes()[1],
        vr[0],
        vr[1],
    ];
    let positions = bytes
        .windows(marker.len())
        .enumerate()
        .filter_map(|(index, window)| (window == marker).then_some(index))
        .collect::<Vec<_>>();
    assert!(
        !positions.is_empty(),
        "missing explicit short-VR element {tag}"
    );
    let mut encoded = value.as_bytes().to_vec();
    if encoded.len() % 2 == 1 {
        encoded.push(b' ');
    }
    for header in positions.into_iter().rev() {
        let old_length =
            u16::from_le_bytes(bytes[header + 6..header + 8].try_into().unwrap()) as usize;
        assert_eq!(
            encoded.len(),
            old_length,
            "replacement must preserve SQ lengths"
        );
        bytes[header + 8..header + 8 + old_length].copy_from_slice(&encoded);
    }
}

fn remove_enhanced_dimensions(path: &Path) {
    let mut object = open_file(path).unwrap();
    object.remove_element(Tag(0x0020, 0x9221));
    object.remove_element(Tag(0x0020, 0x9222));
    let mut per_frame = object.take_element(Tag(0x5200, 0x9230)).unwrap();
    for frame in per_frame.items_mut().unwrap() {
        let mut frame_content = frame.take_element(Tag(0x0020, 0x9111)).unwrap();
        frame_content.items_mut().unwrap()[0].remove_element(Tag(0x0020, 0x9157));
        frame.put(frame_content);
    }
    object.put(per_frame);
    object.write_to_file(path).unwrap();
}

fn remove_explicit_long_vr_element(bytes: &mut Vec<u8>, tag: Tag, vr: &[u8; 2]) {
    let value = explicit_long_vr_value_range(bytes, tag, vr);
    bytes.drain(value.start - 12..value.end);
}

fn set_explicit_us_value(bytes: &mut [u8], tag: Tag, value: u16) {
    let range = explicit_short_vr_element_range(bytes, tag, b"US");
    assert_eq!(range.len(), 10);
    bytes[range.start + 8..range.end].copy_from_slice(&value.to_le_bytes());
}

fn explicit_long_vr_value<'a>(bytes: &'a [u8], tag: Tag, vr: &[u8; 2]) -> &'a [u8] {
    let range = explicit_long_vr_value_range(bytes, tag, vr);
    &bytes[range]
}

fn explicit_long_vr_value_range(bytes: &[u8], tag: Tag, vr: &[u8; 2]) -> std::ops::Range<usize> {
    let marker = [
        tag.group().to_le_bytes()[0],
        tag.group().to_le_bytes()[1],
        tag.element().to_le_bytes()[0],
        tag.element().to_le_bytes()[1],
        vr[0],
        vr[1],
        0,
        0,
    ];
    let header = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap_or_else(|| panic!("missing explicit long-VR element {tag}"));
    let length = u32::from_le_bytes(bytes[header + 8..header + 12].try_into().unwrap()) as usize;
    let start = header + 12;
    start..start + length
}

fn csa_numeric_field_values(source: &[u8], expected_name: &str) -> Vec<String> {
    assert_eq!(&source[..4], b"SV10");
    let tag_count = u32::from_le_bytes(source[8..12].try_into().unwrap()) as usize;
    let mut cursor = 16_usize;
    for _ in 0..tag_count {
        let header = &source[cursor..cursor + 84];
        cursor += 84;
        let name_end = header[..64].iter().position(|byte| *byte == 0).unwrap();
        let name = std::str::from_utf8(&header[..name_end]).unwrap();
        let item_count = u32::from_le_bytes(header[76..80].try_into().unwrap()) as usize;
        let mut values = Vec::new();
        for _ in 0..item_count {
            let item = &source[cursor..cursor + 16];
            cursor += 16;
            let length = u32::from_le_bytes(item[4..8].try_into().unwrap()) as usize;
            let value = &source[cursor..cursor + length];
            cursor += (length + 3) & !3;
            if name == expected_name {
                let value = std::str::from_utf8(value)
                    .unwrap()
                    .trim_matches([' ', '\0']);
                if !value.is_empty() {
                    values.push(value.to_owned());
                }
            }
        }
        if name == expected_name {
            return values;
        }
    }
    panic!("missing CSA numeric field {expected_name}");
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
        "FIXTURE-SERIAL-001",
        "UNKNOWN PRIVATE CREATOR",
        "FIXTURE UNKNOWN PRIVATE TEXT LEAK",
        "1.2.826.0.1.3680043.10.999.1",
    ] {
        assert!(!text.contains(leak), "sanitized DICOM leaked {leak}");
    }
}
