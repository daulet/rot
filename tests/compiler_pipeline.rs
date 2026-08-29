use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

fn repository() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn driver() -> PathBuf {
    let path = repository().join("compiler/rot-rustc-driver/target/debug/rot-rustc-driver");
    assert!(path.is_file(), "build {path:?} before running this test");
    path
}

fn fixture(name: &str) -> PathBuf {
    repository().join("tests/fixtures").join(name)
}

fn run_audit(path: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .args([
            "--driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .args(extra)
        .arg(path)
        .args(["--format", "json"])
        .output()
        .expect("run rot-audit")
}

fn parse_report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "parse rot-audit JSON: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn complete_report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "rot-audit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = parse_report(output);
    assert_eq!(report["status"], "complete", "{report:#}");
    report
}

fn incomplete_report(output: &Output) -> Value {
    assert!(
        !output.status.success(),
        "incomplete rot-audit unexpectedly succeeded"
    );
    let report = parse_report(output);
    assert_ne!(report["status"], "complete", "{report:#}");
    report
}

fn pinned_host() -> String {
    let output = Command::new("rustup")
        .args(["run", "nightly-2026-08-27", "rustc", "-vV"])
        .output()
        .expect("query pinned rustc host");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap()
        .to_owned()
}

fn required_paths(report: &Value) -> BTreeSet<&str> {
    report["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect()
}

fn findings(report: &Value) -> &[Value] {
    report["closed_world"]["findings"].as_array().unwrap()
}

#[test]
fn custom_cfg_refuses_ambient_rustflags_before_driver_execution() {
    for (variable, value) in [
        ("RUSTFLAGS", "-Cdebuginfo=0"),
        ("CARGO_TARGET_FAKE_TARGET_RUSTFLAGS", ""),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
            .env(variable, value)
            .args([
                "--driver",
                "/definitely/not/a/rot-driver",
                "--cfg",
                "rot_requested",
            ])
            .arg(fixture("workspace"))
            .args(["--format", "json"])
            .output()
            .expect("run rot-audit");
        let report = incomplete_report(&output);

        assert_eq!(report["status"], "unavailable");
        assert!(
            report["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| {
                    diagnostic["message"].as_str().is_some_and(|message| {
                        message.contains("cannot be composed")
                            && message.contains("rustflag environment")
                    })
                })
        );
        assert!(
            !report["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| {
                    diagnostic["message"].as_str().is_some_and(|message| {
                        message.contains("driver") && message.contains("does not exist")
                    })
                }),
            "{variable}: {}",
            report["diagnostics"]
        );
    }
}

#[test]
fn rejected_rustc_override_is_a_structured_unavailable_audit() {
    // A resolvable override lets metadata establish the report root/profile;
    // the audit must then reject the override as structured unavailable evidence.
    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .env("RUSTC", "rustc")
        .args(["--driver", "/definitely/not/a/rot-driver"])
        .arg(fixture("workspace"))
        .args(["--format", "json"])
        .output()
        .expect("run rot-audit");
    let report = incomplete_report(&output);

    assert_eq!(report["status"], "unavailable");
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("rejects RUSTC"))
            })
    );
    assert!(
        !report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("does not exist")))
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn keep_going_retains_good_facts_but_fails_the_partial_audit() {
    let output = run_audit(&fixture("partial-workspace"), &[]);
    let report = incomplete_report(&output);

    assert_eq!(report["status"], "partial");
    assert!(report["reason"].as_str().is_some_and(|reason| {
        reason.contains("complete visibility facts") || reason.contains("selected Cargo")
    }));
    assert!(report.get("required_visibility").is_none());
    assert!(report.get("closed_world").is_none());
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("compiler Cargo pass was incomplete"))
            })
    );
    assert!(
        report["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|invocation| {
                invocation["crate_name"] == "rot_good_fixture" && invocation["status"] == "complete"
            })
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn visibility_audit_correlates_all_cargo_target_roles() {
    let report = complete_report(&run_audit(&fixture("workspace"), &[]));

    assert_eq!(
        report["expected_invocations"],
        report["correlated_invocations"]
    );
    assert!(
        report["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|invocation| invocation["status"] == "complete")
    );
    let roles = report["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|invocation| invocation["target"]["role"].as_str())
        .collect::<BTreeSet<_>>();
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

    assert_eq!(
        report["required_visibility"]["scope"],
        "selected-workspace compiled-target closed world"
    );
    assert_eq!(
        report["required_visibility"]["evidence_exclusions"],
        report["closed_world"]["evidence_exclusions"]
    );
    let finding = |path: &str| {
        findings(&report)
            .iter()
            .find(|finding| finding["definition_path"] == path)
            .unwrap_or_else(|| panic!("missing closed-world finding for {path}"))
    };
    assert_eq!(
        finding("reachable_public_helper")["kind"],
        "unnecessary_public"
    );
    assert_eq!(finding("dead_public_for_graph")["kind"], "dead_public");
    assert!(!required_paths(&report).contains("generated_decision"));
    assert!(
        findings(&report)
            .iter()
            .all(|finding| finding["definition_path"] != "generated_decision")
    );
    assert!(findings(&report).iter().all(|finding| {
        finding["reason"].is_string()
            && finding["representative_invocation"].is_string()
            && finding["representative_id"]["stable_crate_id"].is_string()
    }));

    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .args([
            "--driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .arg(fixture("workspace"))
        .output()
        .expect("run rot-audit table output");
    assert!(output.status.success());
    let table = String::from_utf8(output.stdout).unwrap();
    assert!(table.contains("reachable_public_helper"));
    assert!(table.contains("dead_public_for_graph"));
    assert!(table.contains("unnecessary_public"));
    assert!(table.contains("dead_public"));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn isolated_runs_are_deterministic_and_leave_no_artifacts() {
    let parent = tempfile::tempdir().unwrap();
    let parent_path = parent.path().to_string_lossy().into_owned();
    let arguments = ["--scratch-dir", parent_path.as_str()];

    let first = complete_report(&run_audit(&fixture("workspace"), &arguments));
    assert!(fs::read_dir(parent.path()).unwrap().next().is_none());
    let second = complete_report(&run_audit(&fixture("workspace"), &arguments));
    assert!(fs::read_dir(parent.path()).unwrap().next().is_none());

    assert_eq!(first, second);
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn explicit_target_cfg_does_not_require_target_flags_on_host_build_scripts() {
    let host = pinned_host();
    let arguments = ["--target", host.as_str(), "--cfg", "rot_requested"];
    let report = complete_report(&run_audit(&fixture("workspace"), &arguments));

    for invocation in report["invocations"].as_array().unwrap() {
        let role = invocation["target"]["role"].as_str().unwrap();
        let cfg = invocation["cfg"].as_array().unwrap();
        if role == "build" {
            assert!(!cfg.iter().any(|value| value == "rot_requested"));
        } else {
            assert!(cfg.iter().any(|value| value == "rot_requested"));
        }
    }
    assert!(
        !report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["message"].as_str().is_some_and(|message| {
                    message.contains("requested cfg") && message.contains("build")
                })
            })
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_patterns_preserve_required_field_visibility() {
    let report = complete_report(&run_audit(&fixture("compiler-field-visibility"), &[]));
    let required_fields = report["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|definition| definition["kind"] == "field")
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        required_fields,
        BTreeSet::from([
            "Named::used",
            "Named::wildcard",
            "PrivateRest::used",
            "SelfConstructed::0",
            "Spread::0",
            "Spread::1",
            "Spread::2",
            "Tuple::0",
            "Tuple::1",
        ])
    );
    assert!(!required_fields.contains("Named::rest"));
    assert!(
        findings(&report)
            .iter()
            .any(|finding| finding["definition_path"] == "Named::rest")
    );
    assert!(findings(&report).iter().all(|finding| {
        !required_fields.contains(finding["definition_path"].as_str().unwrap_or_default())
    }));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_pathless_self_constructor_preserves_field_visibility() {
    let report = complete_report(&run_audit(&fixture("compiler-field-visibility"), &[]));
    let required = required_paths(&report);

    assert!(required.contains("SelfConstructed::0"));
    assert!(
        findings(&report)
            .iter()
            .all(|finding| finding["definition_path"] != "SelfConstructed::0")
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_type_system_const_preserves_visibility() {
    let report = complete_report(&run_audit(&fixture("compiler-field-visibility"), &[]));
    let required = required_paths(&report);

    assert!(required.contains("SIGNATURE_WIDTH"));
    assert!(
        findings(&report)
            .iter()
            .all(|finding| finding["definition_path"] != "SIGNATURE_WIDTH")
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn global_asm_symbol_target_is_production_live() {
    let report = complete_report(&run_audit(&fixture("compiler-field-visibility"), &[]));
    let finding = findings(&report)
        .iter()
        .find(|finding| finding["definition_path"] == "global_asm_target")
        .expect("global asm target visibility finding");

    assert_eq!(finding["kind"], "unnecessary_public");
    assert_eq!(finding["production_live"], true);
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn closed_world_scope_discloses_uncompiled_doctests() {
    let report = complete_report(&run_audit(&fixture("compiler-field-visibility"), &[]));
    let closed_world = &report["closed_world"];

    assert_eq!(
        closed_world["scope"],
        "selected-workspace compiled-target closed world"
    );
    assert_eq!(
        closed_world["evidence_exclusions"],
        serde_json::json!([
            "doctests",
            "Cargo targets skipped by the active feature profile",
        ])
    );
    assert!(
        closed_world["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["definition_path"] == "doctest_only" && finding["kind"] == "dead_public"
            })
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_foreign_item_preserves_containing_module_visibility() {
    let report = complete_report(&run_audit(&fixture("compiler-field-visibility"), &[]));
    let required = required_paths(&report);

    assert!(required.contains("foreign_api::imported"));
    assert!(required.contains("foreign_api"));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn direct_decl_macro_requires_namespace_but_macro_export_does_not() {
    let report = complete_report(&run_audit(&fixture("compiler-decl-macro"), &[]));
    let required = required_paths(&report);

    assert!(required.contains("namespaced_macros"));
    assert!(!required.contains("legacy_macro_home"));
    assert!(
        findings(&report)
            .iter()
            .any(|finding| finding["definition_path"] == "legacy_macro_home")
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn nested_public_extern_crate_preserves_containing_namespace_visibility() {
    let report = complete_report(&run_audit(&fixture("compiler-field-visibility"), &[]));
    assert!(required_paths(&report).contains("extern_facade"));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_inherent_associated_type_preserves_alias_and_rhs_visibility() {
    let report = complete_report(&run_audit(
        &fixture("compiler-inherent-associated-type"),
        &[],
    ));
    let required = required_paths(&report);

    assert!(required.contains("S::Assoc"));
    assert!(required.contains("Value"));
    assert!(findings(&report).iter().all(|finding| {
        !matches!(
            finding["definition_path"].as_str(),
            Some("S::Assoc" | "Value")
        )
    }));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn host_and_target_build_script_cfg_stay_with_their_cargo_units() {
    let report = complete_report(&run_audit(&fixture("compiler-host-target"), &[]));
    let shared = report["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|invocation| {
            invocation["crate_name"] == "shared_core"
                && invocation["target"]["role"] == "production"
        })
        .collect::<Vec<_>>();
    assert_eq!(shared.len(), 2, "shared-core invocations: {shared:#?}");

    for (context, feature, present_cfg, absent_cfg) in [
        ("host", "host-mode", "rot_host_mode", "rot_target_mode"),
        ("target", "target-mode", "rot_target_mode", "rot_host_mode"),
    ] {
        let invocation = shared
            .iter()
            .find(|invocation| invocation["compilation_context"] == context)
            .unwrap_or_else(|| panic!("missing shared-core {context} invocation: {shared:#?}"));
        assert_eq!(invocation["target"]["compilation_context"], context);
        assert_eq!(invocation["features"], serde_json::json!([feature]));
        let cfg = invocation["cfg"].as_array().unwrap();
        assert!(cfg.iter().any(|value| value == present_cfg), "{cfg:#?}");
        assert!(!cfg.iter().any(|value| value == absent_cfg), "{cfg:#?}");
    }

    assert!(
        !report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["message"].as_str().is_some_and(|message| {
                    message.contains("Cargo build-script cfg")
                        || message.contains("build-script OUT_DIR")
                })
            }),
        "{}",
        report["diagnostics"]
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn an_outer_wrapper_that_skips_the_driver_is_retried_once() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let wrapper = directory.path().join("skip-workspace-wrapper.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"{}\" ]; then shift; fi\nexec \"$@\"\n",
            driver().display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .env("RUSTC_WRAPPER", &wrapper)
        .args([
            "--driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .arg(fixture("workspace"))
        .args(["--format", "json"])
        .output()
        .expect("run rot-audit");
    let report = complete_report(&output);

    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["message"].as_str().is_some_and(|message| {
                    message.contains("suppressed compiler sidecars")
                        && message.contains("retried once")
                })
            })
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn an_outer_wrapper_is_retried_for_an_invoked_failing_target() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let wrapper = directory.path().join("skip-workspace-wrapper.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"{}\" ]; then shift; fi\nexec \"$@\"\n",
            driver().display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_rot-audit"))
        .env("RUSTC_WRAPPER", &wrapper)
        .args([
            "--driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .arg(fixture("partial-workspace/bad"))
        .args(["--format", "json"])
        .output()
        .expect("run rot-audit");
    let report = incomplete_report(&output);

    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("retried once"))
            })
    );
    assert!(
        report["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|invocation| invocation["crate_name"] == "rot_bad_fixture")
    );
}
