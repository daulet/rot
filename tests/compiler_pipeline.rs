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

fn run_compiler(path: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rot"))
        .args([
            "--compiler",
            "--compiler-driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .args(extra)
        .arg(path)
        .args(["--format", "json"])
        .output()
        .expect("run rot compiler mode")
}

fn report(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse rot JSON")
}

fn product_status<'a>(report: &'a Value, product: &str) -> &'a str {
    report["compiler"]["products"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["product"] == product)
        .unwrap()["status"]
        .as_str()
        .unwrap()
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

#[test]
fn custom_cfg_refuses_ambient_rustflags_before_driver_execution() {
    for (variable, value) in [
        ("RUSTFLAGS", "-Cdebuginfo=0"),
        ("CARGO_TARGET_FAKE_TARGET_RUSTFLAGS", ""),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rot"))
            .env(variable, value)
            .args([
                "--compiler",
                "--compiler-driver",
                "/definitely/not/a/rot-driver",
                "--cfg",
                "rot_requested",
            ])
            .arg(fixture("workspace"))
            .args(["--format", "json"])
            .output()
            .expect("run rot compiler mode");
        let report = report(&output);

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
                }),
            "{variable}: {}",
            report["diagnostics"]
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
                })
        );
    }
}

#[test]
fn rejected_rustc_override_preserves_the_cargo_aware_source_report() {
    for rustc in ["", "/definitely/not-rustc"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rot"))
            .env("RUSTC", rustc)
            .args(["--compiler", "--format", "json"])
            .arg(fixture("workspace"))
            .output()
            .expect("run rot compiler mode");
        let report = report(&output);

        assert!(report["file_count"].as_u64().unwrap() > 0);
        assert!(report["buckets"].as_array().unwrap().iter().any(|bucket| {
            bucket["role"] == "production" && bucket["code"].as_u64().unwrap() > 0
        }));
        assert_eq!(product_status(&report, "hir_bodies"), "unavailable");
        assert!(
            report["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("rejects RUSTC")))
        );
    }
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn keep_going_retains_good_facts_and_marks_the_workspace_partial() {
    let report = report(&run_compiler(&fixture("partial-workspace"), &[]));

    assert_eq!(product_status(&report, "hir_bodies"), "partial");
    assert_eq!(product_status(&report, "effective_api"), "partial");
    assert_eq!(product_status(&report, "required_visibility"), "partial");
    assert_eq!(product_status(&report, "closed_world_liveness"), "partial");
    assert_eq!(
        product_status(&report, "macro_expansion_cyclomatic_delta"),
        "partial"
    );
    assert!(report["compiler"].get("effective_api").is_none());
    assert!(report["compiler"].get("required_visibility").is_none());
    assert!(report["compiler"].get("closed_world").is_none());
    let expansion = report["compiler"]["macro_expansion_complexity"]
        .as_object()
        .expect("invocation-local macro facts remain inspectable");
    assert!(expansion.get("totals").is_none());
    assert!(expansion["invocations"].as_array().unwrap().iter().all(
        |invocation| invocation["status"] == "complete" || invocation.get("metrics").is_none()
    ));
    assert!(
        expansion["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|invocation| invocation["status"] == "complete"
                && invocation["metrics"].is_object())
    );
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
        report["compiler"]["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|invocation| {
                invocation["crate_name"] == "rot_good_fixture"
                    && invocation["products"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|product| {
                            product["product"] == "hir_bodies" && product["status"] == "complete"
                        })
            })
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn isolated_runs_are_deterministic_and_leave_no_artifacts() {
    let parent = tempfile::tempdir().unwrap();
    let parent_path = parent.path().to_string_lossy().into_owned();
    let arguments = ["--compiler-target-dir", parent_path.as_str()];

    let first = report(&run_compiler(&fixture("workspace"), &arguments));
    assert!(fs::read_dir(parent.path()).unwrap().next().is_none());
    let second = report(&run_compiler(&fixture("workspace"), &arguments));
    assert!(fs::read_dir(parent.path()).unwrap().next().is_none());

    assert_eq!(first["compiler"], second["compiler"]);
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn explicit_target_cfg_does_not_require_target_flags_on_host_build_scripts() {
    let host = pinned_host();
    let arguments = ["--target", host.as_str(), "--cfg", "rot_requested"];
    let report = report(&run_compiler(&fixture("workspace"), &arguments));

    assert_eq!(product_status(&report, "hir_bodies"), "complete");
    for invocation in report["compiler"]["invocations"].as_array().unwrap() {
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
fn effective_api_contains_only_complete_production_library_facts() {
    let report = report(&run_compiler(&fixture("workspace"), &[]));

    assert_eq!(product_status(&report, "effective_api"), "complete");
    let api = report["compiler"]["effective_api"]
        .as_object()
        .expect("complete effective API object");
    let summary = &api["summary"];
    assert_eq!(summary["production_library_invocations"], 1);
    assert_eq!(summary["effective_definitions"], 45);
    assert_eq!(summary["public_bindings"], 30);
    assert_eq!(
        summary["bindings_by_namespace"],
        serde_json::json!({"macro": 1, "type": 14, "value": 15})
    );
    assert_eq!(
        summary["bindings_by_exposure"],
        serde_json::json!({
            "direct": 20,
            "glob_reexport": 4,
            "macro_export": 1,
            "single_reexport": 5,
        })
    );

    let definitions = api["definitions"].as_array().unwrap();
    assert!(definitions.iter().all(|definition| {
        definition["effective_public_at"].is_string()
            && definition["id"]["stable_crate_id"].is_string()
            && definition["id"]["local_hash"].is_string()
    }));
    assert!(!definitions.iter().any(|definition| {
        definition["definition_path"]
            .as_str()
            .is_some_and(|path| path.contains("unit_test_helper"))
    }));
    for (path, kind) in [
        ("public_mod::Choice", "enum"),
        ("public_mod::Choice::Unit", "variant"),
        ("public_mod::Choice::Unit", "constructor"),
        ("public_mod::Choice::Tuple", "constructor"),
        ("public_mod::Choice::Record::visible", "field"),
        ("public_mod::Contract", "trait"),
        ("public_mod::Contract::Item", "associated_type"),
        ("public_mod::Contract::VALUE", "associated_constant"),
        ("public_mod::Contract::call", "associated_function"),
        ("public_mod::PublicType::field", "associated_function"),
    ] {
        assert!(
            definitions.iter().any(|definition| {
                definition["definition_path"] == path && definition["kind"] == kind
            }),
            "missing effective API definition {path} ({kind})"
        );
    }
    let macro_generated = definitions
        .iter()
        .find(|definition| definition["definition_path"] == "macro_generated_decision")
        .expect("local macro-generated API definition");
    assert_eq!(macro_generated["expansion_origin"], "local_macro");
    assert!(macro_generated.get("span").is_none());
    assert_eq!(
        macro_generated["attribution_callsite"]["path"],
        "src/lib.rs"
    );
    let generated_file = definitions
        .iter()
        .find(|definition| definition["definition_path"] == "generated_decision")
        .expect("build-script generated API definition");
    assert_eq!(generated_file["span"]["generated"], true);
    assert!(
        generated_file["span"]["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("<generated>/"))
    );

    let bindings = api["public_bindings"].as_array().unwrap();
    assert!(bindings.iter().any(|binding| {
        binding["name"] == "Reexported" && binding["exposure"] == "single_reexport"
    }));
    assert!(!bindings.iter().any(|binding| {
        binding["name"] == "declared_but_not_exported"
            || binding["parent_definition_path"]
                .as_str()
                .is_some_and(|path| path.contains("hidden_reexports"))
    }));
    assert!(
        bindings
            .iter()
            .all(|binding| binding.get("segments").is_none())
    );
    assert_eq!(
        bindings
            .iter()
            .filter(|binding| binding["exposure"] == "glob_reexport")
            .count(),
        4
    );
    assert_eq!(
        bindings
            .iter()
            .filter(|binding| {
                matches!(
                    binding["parent_definition_path"].as_str(),
                    Some("cycle_a" | "cycle_b")
                ) && matches!(binding["name"].as_str(), Some("A" | "B"))
            })
            .count(),
        8,
        "cyclic reexports must remain a finite set of type/value bindings"
    );
    assert!(bindings.iter().any(|binding| {
        binding["name"] == "exported_fixture_macro"
            && binding["namespace"] == "macro"
            && binding["exposure"] == "macro_export"
    }));

    for source in report["compiler"]["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|invocation| invocation["sources"].as_array().unwrap())
        .filter(|source| source["generated"] == true)
    {
        let path = source["path"].as_str().unwrap();
        assert!(path.starts_with("<generated>/"), "unstable path: {path}");
        assert!(!path.contains("rot-compiler"), "temporary path: {path}");
    }
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_patterns_preserve_required_field_visibility() {
    let report = report(&run_compiler(&fixture("compiler-field-visibility"), &[]));

    assert_eq!(product_status(&report, "required_visibility"), "complete");
    assert_eq!(product_status(&report, "closed_world_liveness"), "complete");

    let required_fields = report["compiler"]["required_visibility"]["definitions"]
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

    let findings = report["compiler"]["closed_world"]["findings"]
        .as_array()
        .unwrap();
    assert!(
        findings
            .iter()
            .any(|finding| finding["definition_path"] == "Named::rest")
    );
    assert!(findings.iter().all(|finding| {
        !required_fields.contains(finding["definition_path"].as_str().unwrap_or_default())
    }));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_pathless_self_constructor_preserves_field_visibility() {
    let report = report(&run_compiler(&fixture("compiler-field-visibility"), &[]));

    assert_eq!(product_status(&report, "required_visibility"), "complete");
    assert_eq!(product_status(&report, "closed_world_liveness"), "complete");

    let required_definitions = report["compiler"]["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(required_definitions.contains("SelfConstructed::0"));
    assert!(
        report["compiler"]["closed_world"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["definition_path"] != "SelfConstructed::0")
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_type_system_const_preserves_visibility() {
    let report = report(&run_compiler(&fixture("compiler-field-visibility"), &[]));

    assert_eq!(product_status(&report, "required_visibility"), "complete");
    assert_eq!(product_status(&report, "closed_world_liveness"), "complete");

    let required_definitions = report["compiler"]["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(required_definitions.contains("SIGNATURE_WIDTH"));
    assert!(
        report["compiler"]["closed_world"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["definition_path"] != "SIGNATURE_WIDTH")
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn global_asm_symbol_target_is_production_live() {
    let report = report(&run_compiler(&fixture("compiler-field-visibility"), &[]));

    assert_eq!(product_status(&report, "closed_world_liveness"), "complete");

    let finding = report["compiler"]["closed_world"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["definition_path"] == "global_asm_target")
        .expect("global asm target visibility finding");
    assert_eq!(finding["kind"], "unnecessary_public");
    assert_eq!(finding["production_live"], true);
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn closed_world_scope_discloses_uncompiled_doctests() {
    let report = report(&run_compiler(&fixture("compiler-field-visibility"), &[]));

    let closed_world = &report["compiler"]["closed_world"];
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
    let report = report(&run_compiler(&fixture("compiler-field-visibility"), &[]));

    assert_eq!(product_status(&report, "required_visibility"), "complete");

    let required_definitions = report["compiler"]["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(required_definitions.contains("foreign_api::imported"));
    assert!(required_definitions.contains("foreign_api"));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn direct_decl_macro_requires_namespace_but_macro_export_does_not() {
    let report = report(&run_compiler(&fixture("compiler-decl-macro"), &[]));

    assert_eq!(product_status(&report, "required_visibility"), "complete");

    let required_definitions = report["compiler"]["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(required_definitions.contains("namespaced_macros"));
    assert!(!required_definitions.contains("legacy_macro_home"));

    assert!(
        report["compiler"]["closed_world"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["definition_path"] == "legacy_macro_home")
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn nested_public_extern_crate_preserves_containing_namespace_visibility() {
    let report = report(&run_compiler(&fixture("compiler-field-visibility"), &[]));

    assert_eq!(product_status(&report, "required_visibility"), "complete");

    let required_definitions = report["compiler"]["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(required_definitions.contains("extern_facade"));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn cross_crate_inherent_associated_type_preserves_alias_and_rhs_visibility() {
    let report = report(&run_compiler(
        &fixture("compiler-inherent-associated-type"),
        &[],
    ));

    assert_eq!(product_status(&report, "required_visibility"), "complete");
    assert_eq!(product_status(&report, "closed_world_liveness"), "complete");

    let required_definitions = report["compiler"]["required_visibility"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|definition| definition["definition_path"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert!(required_definitions.contains("S::Assoc"));
    assert!(required_definitions.contains("Value"));

    let findings = report["compiler"]["closed_world"]["findings"]
        .as_array()
        .unwrap();
    assert!(findings.iter().all(|finding| {
        !matches!(
            finding["definition_path"].as_str(),
            Some("S::Assoc" | "Value")
        )
    }));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn host_and_target_build_script_cfg_stay_with_their_cargo_units() {
    let report = report(&run_compiler(&fixture("compiler-host-target"), &[]));

    assert_eq!(product_status(&report, "hir_bodies"), "complete");
    assert_eq!(product_status(&report, "effective_api"), "complete");

    let shared = report["compiler"]["invocations"]
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

    let definitions = report["compiler"]["effective_api"]["definitions"]
        .as_array()
        .unwrap();
    assert_eq!(
        report["compiler"]["effective_api"]["summary"]["production_library_invocations"],
        2
    );
    assert!(definitions.iter().any(|definition| {
        definition["crate_name"] == "shared_core"
            && definition["definition_path"]
                .as_str()
                .is_some_and(|path| path.ends_with("target_mode_value"))
    }));
    assert!(!definitions.iter().any(|definition| {
        definition["crate_name"] == "shared_core"
            && definition["definition_path"]
                .as_str()
                .is_some_and(|path| path.ends_with("host_mode_value"))
    }));
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn macro_expansion_complexity_is_a_separate_weighted_delta() {
    let report = report(&run_compiler(&fixture("workspace"), &[]));

    assert_eq!(
        product_status(&report, "macro_expansion_cyclomatic_delta"),
        "complete"
    );
    let expansion = &report["compiler"]["macro_expansion_complexity"];
    assert_eq!(expansion["metric"], "macro_expansion_cyclomatic_delta");
    assert!(
        expansion["baseline"]
            .as_str()
            .unwrap()
            .contains("source-authored")
    );
    let production = expansion["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|invocation| invocation["target"]["role"] == "production")
        .expect("production compiler invocation");
    assert_eq!(production["status"], "complete");
    let totals = &production["metrics"]["totals"];
    assert!(totals["macro_body_bases"].as_u64().unwrap() >= 1);
    assert!(totals["decision_delta"].as_u64().unwrap() >= 1);
    assert_eq!(
        totals["cyclomatic_delta"].as_u64().unwrap(),
        totals["macro_body_bases"].as_u64().unwrap() + totals["decision_delta"].as_u64().unwrap()
    );
    assert!(
        production["metrics"]["decisions_by_kind"]["conditional"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[test]
#[ignore = "requires the pinned nightly rustc-dev helper"]
fn procedural_attribute_deltas_keep_normal_and_test_invocations_distinct() {
    let report = report(&run_compiler(&fixture("compiler-macros"), &[]));

    assert_eq!(
        product_status(&report, "macro_expansion_cyclomatic_delta"),
        "complete"
    );
    let consumer = report["compiler"]["macro_expansion_complexity"]["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|invocation| invocation["crate_name"] == "rot_macro_consumer")
        .collect::<Vec<_>>();
    let invocation = |role: &str| {
        consumer
            .iter()
            .copied()
            .find(|invocation| invocation["target"]["role"] == role)
            .unwrap_or_else(|| panic!("missing {role} consumer invocation: {consumer:#?}"))
    };
    let production = invocation("production");
    let unit_test = invocation("unit_test");

    assert_ne!(production["key"], unit_test["key"]);
    assert_eq!(production["status"], "complete");
    assert_eq!(unit_test["status"], "complete");
    assert!(
        production["metrics"]["by_origin"]["external_macro"]["cyclomatic_delta"]
            .as_u64()
            .unwrap()
            >= 2
    );
    assert!(
        production["metrics"]["by_macro_kind"]["attribute"]["cyclomatic_delta"]
            .as_u64()
            .unwrap()
            >= 2
    );
    assert!(
        unit_test["metrics"]["totals"]["cyclomatic_delta"]
            .as_u64()
            .unwrap()
            > production["metrics"]["totals"]["cyclomatic_delta"]
                .as_u64()
                .unwrap()
    );

    let output = Command::new(env!("CARGO_BIN_EXE_rot"))
        .args([
            "--compiler",
            "--compiler-driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .arg(fixture("compiler-macros"))
        .output()
        .expect("run rot compiler table output");
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let table = String::from_utf8(output.stdout).unwrap();
    assert!(table.contains("macro delta Complete"));
    assert!(table.contains("selected-workspace compiled-target closed world"));
    assert!(
        table.contains("excludes doctests, Cargo targets skipped by the active feature profile")
    );
    assert!(table.contains("Macro expansion delta: invocation-local sum +"));
    assert!(table.contains("body bases +"));
    assert!(table.contains("decision weight;"));
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

    let output = Command::new(env!("CARGO_BIN_EXE_rot"))
        .env("RUSTC_WRAPPER", &wrapper)
        .args([
            "--compiler",
            "--compiler-driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .arg(fixture("workspace"))
        .args(["--format", "json"])
        .output()
        .expect("run rot compiler mode");
    let report = report(&output);

    assert_eq!(
        product_status(&report, "hir_bodies"),
        "complete",
        "{}",
        report["diagnostics"]
    );
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

    let output = Command::new(env!("CARGO_BIN_EXE_rot"))
        .env("RUSTC_WRAPPER", &wrapper)
        .args([
            "--compiler",
            "--compiler-driver",
            driver().to_str().unwrap(),
            "--locked",
            "--offline",
        ])
        .arg(fixture("partial-workspace/bad"))
        .args(["--format", "json"])
        .output()
        .expect("run rot compiler mode");
    let report = report(&output);

    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("retried once")))
    );
    assert!(
        report["compiler"]["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|invocation| invocation["crate_name"] == "rot_bad_fixture")
    );
}
