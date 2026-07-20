#![cfg(unix)]

mod support;

use std::{fs, fs::File, io::Read, process::Command};

use tempfile::tempdir;

#[test]
fn dry_run_prepares_privacy_cleared_dicom_archives_without_network() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("dicoms");
    let state = directory.path().join("state");
    fs::create_dir(&source).unwrap();
    support::write_functional_epi(&source.join("image-1.dcm"), 1);
    support::write_functional_epi(&source.join("image-2.dcm"), 2);

    let output = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "upload",
            "--dry-run",
            source.to_str().unwrap(),
        ])
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
    assert!(progress_output.contains("Confirming source stability"));
    assert!(progress_output.contains("Preparing privacy-cleared MR DICOM archives"));
    assert!(progress_output.contains("Final source stability check"));
    assert!(progress_output.contains("Privacy preparation complete"));
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
    assert!(report.get("errors").is_none());
    assert!(report["bundles"][0]["archive"].is_object());
    assert!(report["bundles"][0]["nifti"].is_null());
    assert!(report["bundles"][0]["metadata"].is_null());
    let archive_path = walkdir::WalkDir::new(state.join("bundles"))
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .find(|path| path.file_name().and_then(|value| value.to_str()) == Some("dicom.tar.zst"))
        .unwrap();
    let decoder = zstd::stream::read::Decoder::new(File::open(archive_path).unwrap()).unwrap();
    let mut archive = tar::Archive::new(decoder);
    let mut expanded = Vec::new();
    for entry in archive.entries().unwrap() {
        entry.unwrap().read_to_end(&mut expanded).unwrap();
    }
    let expanded = String::from_utf8_lossy(&expanded);
    assert!(!expanded.contains("FIXTURE-SUBJECT"));
    assert!(!expanded.contains("FIXTURE^SUBJECT"));
    assert!(!expanded.contains("task_fixture"));
    assert!(!expanded.contains("1.2.826.0.1.3680043"));
    assert!(!expanded.contains("SYNTHETIC TEXT LEAK"));
    assert!(expanded.contains("scaling-neuro-recursive-allowlist-v2"));
}

#[test]
fn dry_run_fails_closed_when_a_dicom_like_file_is_unreadable() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("dicoms");
    let state = directory.path().join("state");
    fs::create_dir(&source).unwrap();
    support::write_functional_epi(&source.join("image-1.dcm"), 1);
    fs::write(source.join("corrupt-export.dcm"), b"DICM truncated fixture").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "upload",
            "--dry-run",
            source.to_str().unwrap(),
        ])
        .env_remove("RUST_LOG")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("DICOM-like file"), "stderr={stderr}");
    assert!(
        stderr.contains("nothing new was uploaded"),
        "stderr={stderr}"
    );
    assert!(stderr.contains("Re-export or repair"), "stderr={stderr}");
    assert!(
        !walkdir::WalkDir::new(&state)
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name() == "dicom.tar.zst")
    );
}
