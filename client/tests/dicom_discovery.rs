mod support;

use std::{fs::OpenOptions, io::Write};

use neuro_sync::{classify::classify_header, dicom::discover, model::ClassificationDecision};
use tempfile::tempdir;

#[test]
fn synthetic_part10_files_group_and_classify_as_functional_epi() {
    let directory = tempdir().unwrap();
    support::write_functional_epi(&directory.path().join("image-1.dcm"), 1);
    support::write_functional_epi(&directory.path().join("image-2.dcm"), 2);
    let discovery = discover(directory.path()).unwrap();
    assert_eq!(discovery.summary.files_seen, 2);
    assert_eq!(discovery.summary.dicom_files, 2);
    assert_eq!(discovery.summary.series_found, 1);
    assert_eq!(discovery.series[0].files.len(), 2);
    let classification = classify_header(&discovery.series[0]);
    assert_eq!(classification.decision, ClassificationDecision::Accepted);
}

#[test]
fn unsafe_nonrepresentative_instance_holds_the_entire_series() {
    let directory = tempdir().unwrap();
    support::write_functional_epi(&directory.path().join("first.dcm"), 1);
    support::write_functional_epi_with_burned_annotation(&directory.path().join("second.dcm"), 2);

    let discovery = discover(directory.path()).unwrap();
    assert_eq!(discovery.series.len(), 1);
    let classification = classify_header(&discovery.series[0]);
    assert_eq!(classification.decision, ClassificationDecision::Held);
    assert_eq!(classification.kind, "burned_in_annotation");
}

#[test]
fn source_snapshot_detects_dicom_changed_after_discovery() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("image.dcm");
    support::write_functional_epi(&path, 1);
    let first = discover(directory.path()).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let second = discover(directory.path()).unwrap();
    assert!(
        !first
            .source_snapshot
            .is_stable_with(&second.source_snapshot)
    );
}
