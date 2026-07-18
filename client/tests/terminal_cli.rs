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
    let register = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["register", "--help"])
        .output()
        .unwrap();
    assert!(register.status.success());
    assert!(String::from_utf8_lossy(&register.stdout).contains("--accept-policy"));

    let upload = Command::new(env!("CARGO_BIN_EXE_neuro-sync"))
        .args(["upload", "--help"])
        .output()
        .unwrap();
    assert!(upload.status.success());
    assert!(String::from_utf8_lossy(&upload.stdout).contains("--confirm-authorized"));
}
