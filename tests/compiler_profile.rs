use std::{collections::BTreeSet, path::PathBuf, process::Command};

use serde_json::Value;

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    repository().join("tests/fixtures/compiler-features")
}

fn driver() -> PathBuf {
    let path = repository().join("compiler/rot-rustc-driver/target/debug/rot-rustc-driver");
    assert!(path.is_file(), "build {path:?} before running this test");
    path
}

fn run_profile(arguments: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .args([
            "--driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .args(arguments)
        .arg(fixture())
        .args(["--format", "json"])
        .output()
        .expect("run rot-audit");
    assert!(
        output.status.success(),
        "rot-audit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("parse rot-audit JSON");
    assert_eq!(report["status"], "complete", "{report:#}");
    report
}

fn production_features<'a>(report: &'a Value, crate_name: &str) -> BTreeSet<&'a str> {
    report["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|invocation| {
            invocation["crate_name"] == crate_name
                && invocation["target"]["role"] == "production"
        })
        .unwrap_or_else(|| panic!("missing production invocation for {crate_name}: {report:#}"))
        ["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|feature| feature.as_str().unwrap())
        .collect()
}

#[test]
fn audit_cli_rejects_synthetic_feature_exclusion() {
    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .args(["--driver", "/definitely/not/a/rot-driver"])
        .args(["--exclude-feature", "b/foo"])
        .arg(fixture())
        .output()
        .expect("run rot-audit");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unexpected argument"), "{stderr}");
    assert!(stderr.contains("--exclude-feature"), "{stderr}");
}

#[test]
fn audit_cli_requires_an_explicit_driver() {
    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .env("ROT_AUDIT_DRIVER", "/ignored/legacy/fallback")
        .arg(fixture())
        .output()
        .expect("run rot-audit");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--driver <PATH>"), "{stderr}");
    assert!(stderr.contains("required"), "{stderr}");
}

#[test]
fn audit_help_teaches_exact_toolchain_pairing() {
    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .arg("--help")
        .output()
        .expect("run rot-audit help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("help is UTF-8");
    for text in [
        "--toolchain",
        "1.98.0",
        "supported-rustc.toml",
        "exact rustc release, commit, and host",
        "does not guess a path",
        "Missing compiler",
        "evidence fails closed",
    ] {
        assert!(help.contains(text), "audit help omitted {text:?}\n{help}");
    }
}

#[test]
#[ignore = "requires the default matching rustc-dev driver"]
fn cargo_feature_profiles_use_actual_resolved_features() {
    let default = run_profile(&[]);
    assert_eq!(default["profile"]["feature_mode"], "default");
    assert_eq!(production_features(&default, "b"), BTreeSet::from(["foo"]));

    let selected = run_profile(&["--features", "b/bar"]);
    assert_eq!(selected["profile"]["feature_mode"], "default_plus_selected");
    assert_eq!(
        production_features(&selected, "b"),
        BTreeSet::from(["bar", "foo"])
    );

    let all = run_profile(&["--all-features"]);
    assert_eq!(all["profile"]["feature_mode"], "all");
    assert_eq!(
        production_features(&all, "b"),
        BTreeSet::from(["bar", "foo"])
    );

    let no_default = run_profile(&["--no-default-features"]);
    assert_eq!(no_default["profile"]["feature_mode"], "none");
    assert_eq!(
        production_features(&no_default, "b"),
        BTreeSet::new(),
        "disabling a's default feature must leave its optional b dependency inactive"
    );
    assert!(
        [&default, &selected, &all, &no_default]
            .into_iter()
            .all(|report| report["profile"].get("synthetic").is_none())
    );
}
