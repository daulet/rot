use std::{path::PathBuf, process::Command};

use serde_json::Value;

#[test]
fn dependency_enabled_exclusion_is_rejected_before_driver_execution() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compiler-features");
    let output = Command::new(env!("CARGO_BIN_EXE_rot"))
        .args([
            "--compiler",
            "--compiler-driver",
            "/definitely/not/a/rot-driver",
            "--locked",
            "--offline",
            "--exclude-feature",
            "b/foo",
        ])
        .arg(fixture)
        .args(["--format", "json"])
        .output()
        .expect("run rot");
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("parse rot JSON");
    assert_eq!(report["profile"]["compiler_compatible"], false);
    assert!(
        report["profile"]["compiler_unavailable_reasons"]
            .as_array()
            .expect("compiler reasons array")
            .iter()
            .any(|reason| reason
                .as_str()
                .is_some_and(|reason| reason.contains("Cargo's resolved dependency graph")))
    );
    let diagnostics = report["diagnostics"].as_array().expect("diagnostics array");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["message"].as_str().is_some_and(|message| {
            message.contains("package b")
                && message.contains("Cargo's resolved dependency graph")
                && message.contains("feature \"foo\"")
        })
    }));
    assert!(!diagnostics.iter().any(|diagnostic| {
        diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("driver") && message.contains("does not exist"))
    }));
}

#[test]
fn disabled_exclusion_remains_compiler_compatible() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compiler-features");
    let output = Command::new(env!("CARGO_BIN_EXE_rot"))
        .args([
            "--compiler",
            "--compiler-driver",
            "/definitely/not/a/rot-driver",
            "--locked",
            "--offline",
            "--exclude-feature",
            "b/bar",
        ])
        .arg(fixture)
        .args(["--format", "json"])
        .output()
        .expect("run rot");
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("parse rot JSON");
    assert_eq!(report["profile"]["compiler_compatible"], true);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |diagnostic| diagnostic["message"].as_str().is_some_and(|message| message
                    .contains("driver")
                    && message.contains("does not exist"))
            )
    );
}
