use std::{path::PathBuf, process::Command};

use serde_json::Value;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace")
}

fn run_json(arguments: &[&str]) -> (Vec<u8>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_rot"))
        .args(arguments)
        .arg(fixture())
        .args(["--format", "json"])
        .output()
        .expect("run rot");
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = serde_json::from_slice(&output.stdout).expect("parse rot JSON");
    (output.stdout, json)
}

fn run(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rot"))
        .args(arguments)
        .output()
        .expect("run rot")
}

fn file<'a>(report: &'a Value, suffix: &str) -> &'a Value {
    report["files"]
        .as_array()
        .expect("files array")
        .iter()
        .find(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| path.ends_with(suffix))
        })
        .unwrap_or_else(|| panic!("missing file ending in {suffix}"))
}

fn bucket<'a>(file: &'a Value, role: &str) -> Option<&'a Value> {
    file["buckets"]
        .as_array()
        .expect("bucket array")
        .iter()
        .find(|bucket| bucket["role"] == role)
}

#[test]
fn cargo_roles_cfg_modules_and_exported_surface_are_distinct() {
    let (_, report) = run_json(&[]);

    let unit_module = file(&report, "src/arbitrary_name.rs");
    assert!(bucket(unit_module, "production").is_none());
    assert!(bucket(unit_module, "test").is_some());
    assert!(bucket(file(&report, "src/nested_fixture/deep.rs"), "test").is_some());
    assert!(bucket(file(&report, "src/nested_fixture/chosen.rs"), "test").is_some());
    assert!(bucket(file(&report, "src/chosen.rs"), "orphan").is_some());

    assert!(bucket(file(&report, "integration/check.rs"), "test").is_some());
    assert!(bucket(file(&report, "demo/demo.rs"), "example").is_some());
    assert!(bucket(file(&report, "perf/bench.rs"), "bench").is_some());
    assert!(bucket(file(&report, "build/custom.rs"), "build").is_some());
    assert!(bucket(file(&report, "src/feature_only.rs"), "inactive").is_some());

    let private_module = file(&report, "src/private_mod.rs");
    assert_eq!(private_module["surface"]["signature_lines"], 0);
    assert!(
        private_module["surface"]["production_declared_public"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        file(&report, "src/public_mod.rs")["surface"]["signature_lines"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        file(&report, "src/public_mod.rs")["surface"]["signature_lines"],
        5
    );
    assert_eq!(report["surface"]["unresolved_glob_reexports"], 1);
    assert_eq!(report["surface"]["unresolved_public_uses"], 2);
    assert!(report["surface"]["opaque_macro_calls"].as_u64().unwrap() > 0);
    assert_eq!(report["surface"]["unresolved_inherent_public_items"], 1);
    assert!(bucket(file(&report, "src/public_mod.rs"), "test").is_some());

    let bucket_lines = report["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|bucket| bucket["physical"].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(bucket_lines, report["total"]["physical"]);
}

#[test]
fn feature_exclusion_is_a_visible_synthetic_profile() {
    let (_, default_report) = run_json(&[]);
    let (_, excluded_report) = run_json(&["--all-features", "--exclude-feature", "excluded"]);

    assert_eq!(excluded_report["profile"]["synthetic"], true);
    assert!(bucket(file(&excluded_report, "src/feature_only.rs"), "inactive").is_some());
    let default_prod = bucket(file(&default_report, "src/lib.rs"), "production").unwrap()["code"]
        .as_u64()
        .unwrap();
    let selected_prod = bucket(file(&excluded_report, "src/lib.rs"), "production").unwrap()["code"]
        .as_u64()
        .unwrap();
    assert!(selected_prod > default_prod);
    assert!(
        !excluded_report["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn strong_dependency_features_activate_the_optional_dependency_feature() {
    let (_, report) = run_json(&[
        "--no-default-features",
        "--features",
        "strong_dependency_feature",
    ]);
    let enabled = report["profile"]["enabled_features"]["rot-fixture"]
        .as_array()
        .unwrap();
    assert!(enabled.iter().any(|feature| feature == "fixture-helper"));
    assert!(
        bucket(file(&report, "src/lib.rs"), "production").unwrap()["code"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn unknown_feature_selectors_are_errors() {
    for option in ["--features", "--exclude-feature"] {
        let output = run(&[fixture().to_str().unwrap(), option, "definitely_missing"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("does not match"));
    }
}

#[test]
fn explicit_unset_cfg_overrides_the_host_profile() {
    let (_, report) = run_json(&["--unset-cfg", "debug_assertions"]);
    assert!(
        !report["profile"]["active_cfg"]
            .as_array()
            .unwrap()
            .iter()
            .any(|predicate| predicate == "debug_assertions")
    );
    assert_eq!(
        report["profile"]["forced_unset_cfg"],
        serde_json::json!(["debug_assertions"])
    );
}

#[test]
fn independent_cargo_workspaces_require_separate_reports() {
    let output = run(&[fixture().to_str().unwrap(), env!("CARGO_MANIFEST_DIR")]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("multiple Cargo workspaces"));
}

#[test]
fn a_selected_module_file_keeps_its_parent_cfg_gate() {
    let module = fixture().join("src/arbitrary_name.rs");
    let output = run(&[module.to_str().unwrap(), "--format", "json"]);
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["file_count"], 1);
    assert!(bucket(file(&report, "arbitrary_name.rs"), "production").is_none());
    assert!(bucket(file(&report, "arbitrary_name.rs"), "test").is_some());
}

#[test]
fn json_is_deterministic_across_worker_counts() {
    let (single, _) = run_json(&["--threads", "1"]);
    let (parallel, _) = run_json(&["--threads", "2"]);
    assert_eq!(single, parallel);
}
