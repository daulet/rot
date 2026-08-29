use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::Value;

static NEXT_SOURCE: AtomicU64 = AtomicU64::new(0);

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

fn temporary_source(source: &str) -> (PathBuf, PathBuf) {
    let directory = std::env::temp_dir().join(format!(
        "rot-cli-test-{}-{}",
        std::process::id(),
        NEXT_SOURCE.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&directory).expect("create source fixture directory");
    let path = directory.join("fixture.rs");
    fs::write(&path, source).expect("write source fixture");
    (directory, path)
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
fn cargo_roles_cfg_modules_and_declared_visibility_are_distinct() {
    let (_, report) = run_json(&[]);

    assert_eq!(report["schema_version"], 2);
    assert!(report.get("complexity").is_none());
    assert!(report["lexical_complexity"].as_u64().is_some());
    assert!(report["cyclomatic_authored"].as_u64().is_some());
    assert!(report["cognitive_authored"].as_u64().is_some());

    let unit_module = file(&report, "src/arbitrary_name.rs");
    assert!(bucket(unit_module, "production").is_none());
    assert!(bucket(unit_module, "test").is_some());
    assert!(bucket(file(&report, "src/nested_fixture/deep.rs"), "test").is_some());
    assert!(bucket(file(&report, "src/nested_fixture/chosen.rs"), "test").is_some());
    assert!(bucket(file(&report, "src/chosen.rs"), "orphan").is_some());

    assert!(bucket(file(&report, "integration/check.rs"), "test").is_some());
    let example = file(&report, "demo/demo.rs");
    assert!(bucket(example, "example").is_some());
    assert!(bucket(example, "test").is_some());
    assert!(bucket(file(&report, "perf/bench.rs"), "bench").is_some());
    assert!(bucket(file(&report, "build/custom.rs"), "build").is_some());
    assert!(bucket(file(&report, "src/feature_only.rs"), "inactive").is_some());

    assert!(report.get("surface").is_none());
    let private_module = file(&report, "src/private_mod.rs");
    assert!(private_module.get("surface").is_none());
    assert!(
        bucket(private_module, "production").unwrap()["declared_public"]
            .as_u64()
            .unwrap()
            > 0
    );
    let public_module = file(&report, "src/public_mod.rs");
    assert_eq!(
        bucket(public_module, "production").unwrap()["declared_public"],
        6
    );
    assert_eq!(bucket(public_module, "test").unwrap()["declared_public"], 1);

    let bucket_lines = report["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|bucket| bucket["physical"].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(bucket_lines, report["total"]["physical"]);
    let project_public = report["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|bucket| bucket["declared_public"].as_u64().unwrap())
        .sum::<u64>();
    let file_public = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|file| file["buckets"].as_array().unwrap())
        .map(|bucket| bucket["declared_public"].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(project_public, file_public);
    for metric in [
        "lexical_complexity",
        "cyclomatic_authored",
        "cognitive_authored",
    ] {
        let bucket_total = report["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|bucket| bucket[metric].as_u64().unwrap())
            .sum::<u64>();
        let file_total = report["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file[metric].as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(bucket_total, report[metric]);
        assert_eq!(file_total, report[metric]);
    }
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
    let (with_files, _) = run_json(&["--files"]);
    assert_eq!(single, parallel);
    let (default, _) = run_json(&[]);
    assert_eq!(default, with_files);
}

#[test]
fn authored_complexity_is_explicit_in_json_and_human_file_reports() {
    let (directory, path) = temporary_source(
        r#"
fn outer() {
    let _first = || {
        if true {
            let _second = || { while true {} };
        }
    };
}
"#,
    );
    let path = path.to_str().unwrap();
    let json_output = run(&[path, "--format", "json"]);
    assert!(json_output.status.success());
    let report: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["lexical_complexity"], 2);
    assert_eq!(report["cyclomatic_authored"], 5);
    assert_eq!(report["cognitive_authored"], 2);
    assert!(report.get("complexity").is_none());

    let source = &report["files"][0];
    assert_eq!(source["lexical_complexity"], 2);
    assert_eq!(source["cyclomatic_authored"], 5);
    assert_eq!(source["cognitive_authored"], 2);
    assert!(source.get("complexity").is_none());

    let production = bucket(source, "production").unwrap();
    assert_eq!(production["lexical_complexity"], 2);
    assert_eq!(production["cyclomatic_authored"], 5);
    assert_eq!(production["cognitive_authored"], 2);
    assert!(production.get("complexity").is_none());

    let summary = run(&[path]);
    assert!(summary.status.success());
    let summary = String::from_utf8(summary.stdout).unwrap();
    assert!(summary.contains("Lexical"));
    assert!(summary.contains("Cyclomatic"));
    assert!(summary.contains("Cognitive"));
    assert!(summary.contains("Declared pub"));
    assert!(!summary.contains("Exported surface"));
    assert!(!summary.contains("Prod LOC"));

    let files = run(&[path, "--files"]);
    assert!(files.status.success());
    let files = String::from_utf8(files.stdout).unwrap();
    assert!(files.contains("Prod LOC"));
    assert!(files.contains("Cyclomatic"));
    assert!(files.contains("Cognitive"));
    assert!(files.contains("Prod pub"));
    assert!(!files.contains("Surface"));

    fs::remove_dir_all(directory).expect("remove source fixture directory");
}

#[test]
fn source_only_profile_never_launches_the_compiler_driver() {
    let output = run(&[
        "--compiler",
        "--compiler-driver",
        "/definitely/not/a/rot-driver",
        "--unset-cfg",
        "debug_assertions",
        fixture().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["file_count"].as_u64().unwrap() > 0);
    assert!(report["compiler"].get("status").is_none());
    assert!(report["compiler"].get("effective_api").is_none());
    assert!(report["compiler"].get("required_visibility").is_none());
    assert!(report["compiler"].get("closed_world").is_none());
    assert!(
        report["compiler"]
            .get("macro_expansion_complexity")
            .is_none()
    );
    assert!(
        report["compiler"]["products"]
            .as_array()
            .unwrap()
            .iter()
            .all(|product| product["status"] == "unavailable")
    );
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("--unset-cfg")))
    );
}

#[test]
fn missing_compiler_driver_preserves_the_source_report() {
    let output = run(&[
        "--compiler",
        "--compiler-driver",
        "/definitely/not/a/rot-driver",
        fixture().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["file_count"].as_u64().unwrap() > 0);
    assert!(report["compiler"].get("status").is_none());
    assert!(report["compiler"].get("effective_api").is_none());
    assert!(report["compiler"].get("required_visibility").is_none());
    assert!(report["compiler"].get("closed_world").is_none());
    assert!(
        report["compiler"]
            .get("macro_expansion_complexity")
            .is_none()
    );
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("does not exist")))
    );

    let table = run(&[
        "--compiler",
        "--compiler-driver",
        "/definitely/not/a/rot-driver",
        fixture().to_str().unwrap(),
    ]);
    assert!(table.status.success());
    let table = String::from_utf8(table.stdout).unwrap();
    assert!(table.contains("Compiler: 0/0"));
    assert!(table.contains("macro delta Unavailable"));
    assert!(!table.contains("Macro expansion delta: +0"));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn pinned_compiler_pipeline_correlates_all_cargo_target_roles() {
    let driver = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("compiler/rot-rustc-driver/target/debug/rot-rustc-driver");
    assert!(
        driver.is_file(),
        "build {driver:?} before running this test"
    );
    let output = run(&[
        "--compiler",
        "--compiler-driver",
        driver.to_str().unwrap(),
        "--offline",
        "--locked",
        fixture().to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["compiler"].get("status").is_none());
    assert_eq!(
        report["compiler"]["products"]
            .as_array()
            .unwrap()
            .iter()
            .find(|product| product["product"] == "hir_bodies")
            .unwrap()["status"],
        "complete"
    );
    assert!(report["compiler"]["effective_api"].is_object());
    for product in ["required_visibility", "closed_world_liveness"] {
        assert_eq!(
            report["compiler"]["products"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["product"] == product)
                .unwrap()["status"],
            "complete"
        );
    }
    assert_eq!(
        report["compiler"]["closed_world"]["scope"],
        "selected-workspace compiled-target closed world"
    );
    assert_eq!(
        report["compiler"]["closed_world"]["evidence_exclusions"],
        serde_json::json!([
            "doctests",
            "Cargo targets skipped by the active feature profile",
        ])
    );
    assert_eq!(
        report["compiler"]["required_visibility"]["scope"],
        "selected-workspace compiled-target closed world"
    );
    assert_eq!(
        report["compiler"]["required_visibility"]["evidence_exclusions"],
        report["compiler"]["closed_world"]["evidence_exclusions"]
    );
    let findings = report["compiler"]["closed_world"]["findings"]
        .as_array()
        .unwrap();
    let finding = |path: &str| {
        findings
            .iter()
            .find(|finding| finding["definition_path"] == path)
            .unwrap_or_else(|| panic!("missing closed-world finding for {path}"))
    };
    assert_eq!(
        finding("reachable_public_helper")["kind"],
        "unnecessary_public"
    );
    assert_eq!(finding("dead_public_for_graph")["kind"], "dead_public");
    assert!(
        !findings
            .iter()
            .any(|finding| { finding["definition_path"] == "macro_generated_decision" })
    );
    assert!(findings.iter().all(|finding| {
        finding["representative_invocation"].is_string()
            && finding["representative_id"]["stable_crate_id"].is_string()
    }));
    assert_eq!(
        report["compiler"]["expected_invocations"],
        report["compiler"]["correlated_invocations"]
    );
    let roles = report["compiler"]["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|invocation| invocation["target"]["role"].as_str())
        .collect::<std::collections::HashSet<_>>();
    for expected in [
        "production",
        "unit_test",
        "test",
        "bench",
        "example",
        "build",
    ] {
        assert!(
            roles.contains(expected),
            "missing role {expected}: {roles:?}"
        );
    }
    assert!(
        report["compiler"]["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|invocation| invocation["products"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |product| product["product"] == "hir_bodies" && product["status"] == "complete"
                ))
    );
    let generated = report["compiler"]["generated_files"].as_array().unwrap();
    assert!(!generated.is_empty());
    assert!(generated.iter().all(|file| {
        file["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("<generated>/"))
    }));
    assert!(generated.iter().any(|file| {
        file["cyclomatic_authored"]
            .as_u64()
            .is_some_and(|value| value >= 2)
    }));
}
