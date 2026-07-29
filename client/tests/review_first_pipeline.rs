#![cfg(unix)]

mod support;

use std::{fs, process::Command};

use neuro_sync::{
    config::{AppPaths, ClientConfig},
    pseudonym::Pseudonymizer,
};
use tempfile::tempdir;
use walkdir::WalkDir;

#[test]
fn prepare_creates_editable_deidentified_dicoms_and_dry_run_uses_current_files() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("source");
    let state = directory.path().join("state");
    let review = directory.path().join("source-review");
    fs::create_dir(&source).unwrap();
    let options = support::FunctionalDicomOptions {
        omit_burned_in_annotation: true,
        ..Default::default()
    };
    support::write_functional_epi_fixture(&source.join("image-1.dcm"), 1, &options);
    support::write_functional_epi_fixture(&source.join("image-2.dcm"), 2, &options);

    let paths = AppPaths::discover(Some(&state)).unwrap();
    paths.initialize().unwrap();
    ClientConfig {
        api_url: "http://127.0.0.1:9".into(),
        device_token: "test-device".into(),
        site_id: "test-site".into(),
        project_id: "test-project".into(),
        project_name: "Test project".into(),
        consent_policy_version: "open-epi-test".into(),
        pseudonym_key_b64: Pseudonymizer::generate_base64(),
    }
    .save(&paths)
    .unwrap();

    let prepared = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "prepare",
            source.to_str().unwrap(),
        ])
        .current_dir(directory.path())
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(
        prepared.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&prepared.stdout),
        String::from_utf8_lossy(&prepared.stderr)
    );
    let stdout = String::from_utf8_lossy(&prepared.stdout);
    assert!(stdout.contains("Nothing was uploaded."));
    assert!(stdout.contains("2 deidentified DICOM files"));
    assert!(review.join(".neuro-sync/review-package.json").is_file());
    assert!(review.join("README.txt").is_file());
    assert!(review.join("series-index.tsv").is_file());
    assert!(review.join("preparation-report.json").is_file());

    let dicoms = WalkDir::new(review.join("series"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("dcm")
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    assert_eq!(dicoms.len(), 2);
    let joined = dicoms
        .iter()
        .flat_map(|path| fs::read(path).unwrap())
        .collect::<Vec<_>>();
    let expanded = String::from_utf8_lossy(&joined);
    assert!(!expanded.contains("FIXTURE-SUBJECT"));
    assert!(!expanded.contains("FIXTURE^SUBJECT"));
    assert!(!expanded.contains("task_fixture"));

    let series_index = fs::read_to_string(review.join("series-index.tsv")).unwrap();
    assert!(
        series_index
            .starts_with("folder\tseries_number\tdicom_files\trows\tcolumns\tnumber_of_frames\t")
    );
    assert_eq!(series_index.lines().count(), 2);
    assert!(series_index.contains("\t2\t"));
    assert!(series_index.contains("functional_tr_range"));
    assert!(series_index.contains("burned_in_annotation_not_declared"));
    assert!(!series_index.contains("FIXTURE-SUBJECT"));
    assert!(!series_index.contains("FIXTURE^SUBJECT"));
    assert!(!series_index.contains("task_fixture"));

    let readme = fs::read_to_string(review.join("README.txt")).unwrap();
    assert!(readme.contains("Start with series-index.tsv."));
    assert!(readme.contains("usual DICOM tools"));
    assert!(readme.contains("move its entire series/<series-id>/ directory outside"));

    assert!(
        !WalkDir::new(review.join("series"))
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == "manifest.json")
    );

    let status = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["--state-dir", state.to_str().unwrap(), "status", "--json"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("\"status\": \"ready_for_review\""));

    let saved_report = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["--state-dir", state.to_str().unwrap(), "report", "--json"])
        .output()
        .unwrap();
    assert!(saved_report.status.success());
    assert!(
        String::from_utf8_lossy(&saved_report.stdout).contains("\"status\": \"ready_for_review\"")
    );

    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(review.join("preparation-report.json")).unwrap()).unwrap();
    let subject = report["bundles"][0]["subject_id"].as_str().unwrap();
    assert_eq!(subject.len(), 24);
    let replacement_prefix = if subject.as_bytes()[0] == b'a' {
        'b'
    } else {
        'a'
    };
    let replacement = format!(
        "{replacement_prefix}{}",
        subject.get(1..).expect("ASCII pseudonymous subject")
    );
    let mut edited = fs::read(&dicoms[0]).unwrap();
    let mut replacements = 0;
    for offset in 0..=edited.len() - subject.len() {
        if &edited[offset..offset + subject.len()] == subject.as_bytes() {
            edited[offset..offset + subject.len()].copy_from_slice(replacement.as_bytes());
            replacements += 1;
        }
    }
    assert!(replacements > 0);
    fs::write(&dicoms[0], edited).unwrap();

    let checked = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "upload",
            review.to_str().unwrap(),
            "--dry-run",
        ])
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let stdout = String::from_utf8_lossy(&checked.stdout);
    assert!(stdout.contains("2 current DICOM files"));
    assert!(stdout.contains("Status: dry_run_complete"));
}
