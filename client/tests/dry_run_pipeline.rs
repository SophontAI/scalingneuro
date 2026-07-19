#![cfg(unix)]

mod support;

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use tempfile::tempdir;

#[test]
fn dry_run_converts_scrubs_and_bundles_without_network() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("dicoms");
    let state = directory.path().join("state");
    fs::create_dir(&source).unwrap();
    support::write_functional_epi(&source.join("image-1.dcm"), 1);
    support::write_functional_epi(&source.join("image-2.dcm"), 2);
    let nifti = directory.path().join("fixture.nii");
    let metadata = directory.path().join("fixture.json");
    support::write_nifti_epi(&nifti);
    fs::write(
        &metadata,
        r#"{
          "Manufacturer":"FIXTURE_VENDOR",
          "SequenceName":"ep2d_bold",
          "RepetitionTime":0.8,
          "EchoTime":0.03,
          "FlipAngle":52,
          "PhaseEncodingDirection":"j-"
        }"#,
    )
    .unwrap();
    let converter = directory.path().join("dcm2niix");
    fs::write(
        &converter,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "Chris Rorden's dcm2niix version v1.0.20260416 fixture build"
  exit 0
fi
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then out="$2"; shift 2; else shift; fi
done
cp "$NEURO_SYNC_FAKE_NIFTI" "$out/series.nii"
cp "$NEURO_SYNC_FAKE_JSON" "$out/series.json"
"#,
    )
    .unwrap();
    fs::set_permissions(&converter, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "upload",
            "--dry-run",
            source.to_str().unwrap(),
        ])
        .env("NEURO_SYNC_DCM2NIIX", &converter)
        .env("NEURO_SYNC_FAKE_NIFTI", &nifti)
        .env("NEURO_SYNC_FAKE_JSON", &metadata)
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let progress_output = format!("{stdout}\n{stderr}");
    assert!(stdout.contains("Syncing"));
    assert!(
        progress_output.contains("DICOM discovery complete"),
        "output={progress_output}"
    );
    assert!(progress_output.contains("Source stability check progress"));
    assert!(progress_output.contains("Local validation progress"));
    assert!(progress_output.contains("Local validation complete"));
    let reports = fs::read_dir(state.join("reports"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|value| value.to_str()) == Some("json")
                && !path.to_string_lossy().contains("manifest")
        })
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 1);
    let report_bytes = fs::read(&reports[0]).unwrap();
    let report_text = String::from_utf8(report_bytes.clone()).unwrap();
    assert!(!report_text.contains("local_path"));
    assert!(!report_text.contains("source_path"));
    assert!(!report_text.contains(source.to_string_lossy().as_ref()));
    let report: serde_json::Value = serde_json::from_slice(&report_bytes).unwrap();
    assert_eq!(report["status"], "dry_run_complete");
    assert_eq!(report["source_summary"]["accepted"], 1);
    assert_eq!(report["bundles"].as_array().unwrap().len(), 1);
    let sidecar_path = walkdir::WalkDir::new(state.join("bundles"))
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .unwrap();
    let sidecar = fs::read_to_string(sidecar_path).unwrap();
    assert!(!sidecar.contains("FIXTURE-SUBJECT"));
    assert!(!sidecar.contains("FIXTURE^SUBJECT"));
    assert!(!sidecar.contains("task_fixture"));
    assert!(!sidecar.contains("1.2.826.0.1.3680043"));
    assert!(!sidecar.contains("SYNTHETIC TEXT LEAK"));
}
