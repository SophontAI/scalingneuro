use std::process::{Command, Stdio};

use tempfile::tempdir;

#[test]
fn no_arguments_fail_actionably_without_a_terminal() {
    let state = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["--state-dir", state.path().to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("interactive setup needs a terminal"),
        "{stderr}"
    );
    assert!(stderr.contains("neuro-sync register --help"), "{stderr}");
}

#[test]
fn automation_flags_are_documented_in_command_help() {
    let primary = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(primary.status.success());
    let primary_help = String::from_utf8_lossy(&primary.stdout);
    assert!(primary_help.contains("[DICOM_FOLDER]"));
    assert!(primary_help.contains("--state-dir"));
    assert!(primary_help.contains("one-series staging archive"));
    assert!(!primary_help.contains("resume"));

    let register = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["register", "--help"])
        .output()
        .unwrap();
    assert!(register.status.success());
    assert!(String::from_utf8_lossy(&register.stdout).contains("--accept-policy-version"));

    let upload = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["upload", "--help"])
        .output()
        .unwrap();
    assert!(upload.status.success());
    let upload_help = String::from_utf8_lossy(&upload.stdout);
    assert!(upload_help.contains("--confirm-authorized"));
    assert!(upload_help.contains("--accept-policy-version"));
    assert!(upload_help.contains("folder created by `neuro-sync prepare`"));

    let prepare = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["prepare", "--help"])
        .output()
        .unwrap();
    assert!(prepare.status.success());
    let prepare_help = String::from_utf8_lossy(&prepare.stdout);
    assert!(prepare_help.contains("--output"));
    assert!(prepare_help.contains("in the current directory"));
    assert!(prepare_help.contains("without uploading anything"));
}

#[test]
fn direct_folder_argument_enters_the_terminal_flow() {
    let directory = tempdir().unwrap();
    let state = directory.path().join("state");
    let dicoms = directory.path().join("new dicoms");
    std::fs::create_dir(&dicoms).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            dicoms.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("interactive setup needs a terminal"),
        "{stderr}"
    );
    assert!(!stderr.contains("unrecognized subcommand"), "{stderr}");
}
