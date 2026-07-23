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

use support::{FixturePurpose, FixtureVendor, FunctionalDicomOptions};

const TEST_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

#[test]
fn functional_epi_archive_is_deterministic_private_and_pixel_exact() {
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
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
    assert_eq!(classification.kind, "functional_epi");

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

    let first_archive = first.archive.as_ref().unwrap();
    let second_archive = second.archive.as_ref().unwrap();
    assert_eq!(first.bundle_id, second.bundle_id);
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
    assert!(progress_callbacks > 16);

    let entries = read_archive(&first_archive.object.local_path);
    assert_eq!(
        entries.keys().cloned().collect::<Vec<_>>(),
        ["dicom/000001.dcm".to_owned(), "manifest.json".to_owned()]
    );
    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest["series_kind"], "functional_epi");
    assert_eq!(manifest["archive_route"], "functional-epi-v1");
    assert_eq!(manifest["deidentification"]["pixel_data_retained"], true);
    assert_eq!(manifest["deidentification"]["defacing_performed"], false);

    let sanitized_path = directory.path().join("sanitized.dcm");
    std::fs::write(&sanitized_path, &entries["dicom/000001.dcm"]).unwrap();
    let object = open_file(&sanitized_path).unwrap();
    let subject = object
        .element(Tag(0x0010, 0x0020))
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(subject.len(), 24);
    assert_ne!(subject.as_ref(), "FIXTURE-SUBJECT-001");
    assert_eq!(
        object
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
fn enhanced_functional_epi_is_supported_without_source_identity() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let bundles = directory.path().join("bundles");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&bundles).unwrap();
    support::write_functional_epi_fixture(
        &source.join("enhanced.dcm"),
        1,
        &FunctionalDicomOptions {
            vendor: FixtureVendor::PhilipsEnhanced,
            include_privacy_leaks: true,
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
    assert_no_fixture_identity_leaks(&entries["dicom/000001.dcm"]);
}

#[test]
fn structural_and_diffusion_series_cannot_be_archived() {
    for purpose in [FixturePurpose::StructuralT1w, FixturePurpose::Diffusion] {
        let directory = tempdir().unwrap();
        let source = directory.path().join("source");
        let bundles = directory.path().join("bundles");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bundles).unwrap();
        support::write_functional_epi_fixture(
            &source.join("non-epi.dcm"),
            1,
            &FunctionalDicomOptions {
                purpose,
                siemens_mosaic: false,
                ..Default::default()
            },
        );
        let discovery = discover(&source).unwrap();
        let group = &discovery.series[0];
        let classification = classify_header(group);
        assert_ne!(classification.decision, ClassificationDecision::Accepted);
        assert!(
            create_dicom_archive(ArchiveRequest {
                group,
                classification,
                pseudonymizer: &Pseudonymizer::from_base64(TEST_KEY).unwrap(),
                bundle_root: &bundles,
                progress: |_| {},
            })
            .is_err()
        );
    }
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
        "FIXTURE-SERIAL-001",
        "UNKNOWN PRIVATE CREATOR",
        "FIXTURE UNKNOWN PRIVATE TEXT LEAK",
        "1.2.826.0.1.3680043.10.999.1",
    ] {
        assert!(!text.contains(leak), "sanitized DICOM leaked {leak}");
    }
}
