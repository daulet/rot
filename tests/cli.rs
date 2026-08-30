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

fn run_path_json(path: &std::path::Path, arguments: &[&str]) -> Value {
    let output = run_path(path, arguments, true);
    assert!(
        output.status.success(),
        "rot failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse rot JSON")
}

fn run_path(path: &std::path::Path, arguments: &[&str], json: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rot"));
    command.arg(path).args(arguments);
    if json {
        command
            .args(["--format", "json"])
            .output()
            .expect("run rot")
    } else {
        command.output().expect("run rot")
    }
}

fn cargo_resolved_features(
    manifest: &std::path::Path,
    arguments: &[&str],
    package: &str,
) -> Vec<String> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--offline"])
        .args(arguments)
        .arg("--manifest-path")
        .arg(manifest)
        .output()
        .expect("run Cargo metadata control");
    assert!(
        output.status.success(),
        "Cargo metadata control failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("parse Cargo metadata");
    let package_id = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["name"] == package)
        .unwrap_or_else(|| panic!("Cargo metadata omitted package {package}"))["id"]
        .as_str()
        .unwrap();
    metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == package_id)
        .unwrap_or_else(|| panic!("Cargo resolve omitted package {package}"))["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|feature| feature.as_str().unwrap().to_owned())
        .collect()
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

fn temporary_git_workspace() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().expect("create Git fixture directory");
    fs::create_dir(directory.path().join("src")).expect("create Git fixture source directory");
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"rot-diff-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("write Git fixture manifest");
    fs::write(directory.path().join("src/lib.rs"), "").expect("write empty baseline source");
    git(directory.path(), &["init", "-q"]);
    git(directory.path(), &["add", "."]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=Rot Test",
            "-c",
            "user.email=rot@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    let commit = git(directory.path(), &["rev-parse", "HEAD"]);
    (directory, commit.trim().to_owned())
}

fn git(directory: &std::path::Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run Git fixture command");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("Git fixture output is UTF-8")
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

fn metric_table_row(output: &str, label: &str) -> [u64; 10] {
    let values = output
        .lines()
        .find(|line| line.split_whitespace().next() == Some(label))
        .unwrap_or_else(|| panic!("missing {label:?} metric row:\n{output}"))
        .split_whitespace()
        .skip(1)
        .map(|value| value.parse().expect("metric table cell is an integer"))
        .collect::<Vec<_>>();
    assert_eq!(
        values.len(),
        10,
        "{label:?} metric row shifted columns: {values:?}"
    );
    values.try_into().unwrap()
}

#[test]
fn cargo_roles_cfg_modules_and_declared_visibility_are_distinct() {
    let (_, report) = run_json(&[]);

    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["report_kind"], "snapshot");
    assert_eq!(report["detail"], "files");
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
    assert_eq!(excluded_report["profile"]["feature_mode"], "all_except");
    assert!(
        excluded_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|diagnostic| !diagnostic["message"].as_str().unwrap().contains("feature"))
    );

    let clean_fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compiler-features");
    let strict = run(&[
        clean_fixture.to_str().unwrap(),
        "--all-features",
        "--exclude-feature",
        "b/foo",
        "--strict",
        "--format",
        "json",
    ]);
    assert!(
        strict.status.success(),
        "intentional exclusion failed strict mode: {}",
        String::from_utf8_lossy(&strict.stderr)
    );
}

#[test]
fn workspace_dependency_features_resolve_before_synthetic_exclusion() {
    let workspace =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compiler-features");
    let ordinary = run_path_json(&workspace, &[]);
    let ordinary_b = ordinary["profile"]["enabled_features"]["b"]
        .as_array()
        .unwrap();
    assert!(ordinary_b.iter().any(|feature| feature == "foo"));

    let ordinary_source = file(&ordinary, "b/src/lib.rs");
    let ordinary_production = bucket(ordinary_source, "production").unwrap()["code"]
        .as_u64()
        .unwrap();
    let ordinary_inactive = bucket(ordinary_source, "inactive").unwrap()["code"]
        .as_u64()
        .unwrap();
    assert!(
        ordinary_production > ordinary_inactive,
        "the dependency-enabled foo branch must be active: {ordinary_source:#}"
    );

    let dependency_root = run_path_json(&workspace.join("a"), &[]);
    assert_eq!(
        dependency_root["profile"]["enabled_features"]["b"],
        serde_json::json!(cargo_resolved_features(
            &workspace.join("a/Cargo.toml"),
            &[],
            "b",
        )),
        "a selected root must match Cargo's optional dependency features"
    );
    let reverse_root = run_path_json(&workspace.join("b"), &[]);
    assert!(
        !reverse_root["profile"]["enabled_features"]["b"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "foo"),
        "an unselected reverse dependent must not activate b/foo"
    );
    let reverse_source = file(&reverse_root, "src/lib.rs");
    assert!(
        bucket(reverse_source, "production").unwrap()["code"]
            .as_u64()
            .unwrap()
            < bucket(reverse_source, "inactive").unwrap()["code"]
                .as_u64()
                .unwrap(),
        "b alone must keep its foo branch inactive: {reverse_source:#}"
    );

    let unqualified_dependency_feature =
        run_path(&workspace.join("a"), &["--features", "bar"], false);
    assert!(!unqualified_dependency_feature.status.success());
    assert!(
        String::from_utf8_lossy(&unqualified_dependency_feature.stderr)
            .contains("selected PATH root feature")
    );
    let qualified_dependency_feature =
        run_path_json(&workspace.join("a"), &["--features", "b/bar"]);
    assert!(
        qualified_dependency_feature["profile"]["enabled_features"]["b"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "bar")
    );

    let qualified_optional_feature = run_path_json(
        &workspace.join("a"),
        &["--no-default-features", "--features", "b/bar"],
    );
    assert_eq!(
        qualified_optional_feature["profile"]["enabled_features"]["b"],
        serde_json::json!(cargo_resolved_features(
            &workspace.join("a/Cargo.toml"),
            &["--no-default-features", "--features", "b/bar"],
            "b",
        )),
        "a workspace package selector must not activate the optional edge leading to that package"
    );
    let renamed_dependency_feature = run_path_json(
        &workspace.join("a"),
        &["--no-default-features", "--features", "renamed_b/bar"],
    );
    assert_eq!(
        renamed_dependency_feature["profile"]["enabled_features"]["b"],
        serde_json::json!(cargo_resolved_features(
            &workspace.join("a/Cargo.toml"),
            &["--no-default-features", "--features", "renamed_b/bar",],
            "b",
        )),
        "a dependency-alias selector must activate the selected root's direct optional edge"
    );
    let inactive_alias_exclusion = run_path(
        &workspace.join("a"),
        &[
            "--no-default-features",
            "--exclude-feature",
            "renamed_b/foo",
        ],
        false,
    );
    assert!(!inactive_alias_exclusion.status.success());
    assert!(
        String::from_utf8_lossy(&inactive_alias_exclusion.stderr)
            .contains("exclusions never activate dependencies")
    );
    let reverse_exclusion = run_path(
        &workspace.join("b"),
        &["--exclude-feature", "a/default"],
        false,
    );
    assert!(!reverse_exclusion.status.success());
    assert!(
        String::from_utf8_lossy(&reverse_exclusion.stderr)
            .contains("not reachable from the selected PATH roots")
    );
    let unqualified_dependency_exclusion =
        run_path(&workspace.join("a"), &["--exclude-feature", "foo"], false);
    assert!(!unqualified_dependency_exclusion.status.success());
    assert!(
        String::from_utf8_lossy(&unqualified_dependency_exclusion.stderr)
            .contains("selected PATH root feature")
    );

    let excluded = run_path_json(&workspace, &["--exclude-feature", "b/foo", "--strict"]);
    assert_eq!(excluded["profile"]["synthetic"], true);
    assert_eq!(
        excluded["profile"]["excluded_features"],
        serde_json::json!(["b/foo"])
    );
    assert_eq!(
        excluded["profile"]["enabled_features"]["a"], ordinary["profile"]["enabled_features"]["a"],
        "forcing b/foo false must not undo the ordinary dependency closure"
    );
    assert!(
        !excluded["profile"]["enabled_features"]["b"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "foo")
    );

    let excluded_source = file(&excluded, "b/src/lib.rs");
    let excluded_production = bucket(excluded_source, "production").unwrap()["code"]
        .as_u64()
        .unwrap();
    let excluded_inactive = bucket(excluded_source, "inactive").unwrap()["code"]
        .as_u64()
        .unwrap();
    assert!(
        excluded_production < excluded_inactive,
        "hard exclusion must flip only the b/foo predicate: {excluded_source:#}"
    );
}

#[test]
fn nonoptional_dependency_feature_does_not_enable_same_named_local_feature() {
    let workspace =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compiler-features");
    let report = run_path_json(
        &workspace.join("c"),
        &["--no-default-features", "--features", "trigger"],
    );

    assert_eq!(
        report["profile"]["enabled_features"]["c"],
        serde_json::json!(cargo_resolved_features(
            &workspace.join("c/Cargo.toml"),
            &["--no-default-features", "--features", "trigger"],
            "c",
        ))
    );
    assert_eq!(
        report["profile"]["enabled_features"]["b"],
        serde_json::json!(cargo_resolved_features(
            &workspace.join("c/Cargo.toml"),
            &["--no-default-features", "--features", "trigger"],
            "b",
        ))
    );
}

#[test]
fn dependency_platforms_use_target_for_normal_edges_and_host_for_build_edges() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/feature-platforms/app");
    let target = "wasm32-unknown-unknown";
    let report = run_path_json(&fixture, &["--target", target]);
    let enabled = &report["profile"]["enabled_features"];

    assert_eq!(enabled["target-on"], serde_json::json!(["marker"]));
    assert_eq!(enabled["target-off"], serde_json::json!([]));
    assert_eq!(enabled["host-build-on"], serde_json::json!(["marker"]));
    assert_eq!(enabled["target-build-off"], serde_json::json!([]));
    assert_eq!(enabled["proc-host-on"], serde_json::json!(["marker"]));
    assert_eq!(enabled["proc-target-off"], serde_json::json!([]));
    assert_eq!(enabled["proc-macro-target-off"], serde_json::json!([]));
    assert_eq!(enabled["debug-on"], serde_json::json!(["marker"]));
    assert_eq!(enabled["release-off"], serde_json::json!([]));
    assert_eq!(enabled["context-host-leaf"], serde_json::json!(["marker"]));
    assert_eq!(enabled["context-target-leaf"], serde_json::json!([]));

    let release = run_path_json(&fixture, &["--target", target, "--release"]);
    assert_eq!(
        release["profile"]["enabled_features"], report["profile"]["enabled_features"],
        "source --release cfg must not change Cargo's dependency-platform resolution"
    );
    assert!(
        !release["profile"]["active_cfg"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cfg| cfg == "debug_assertions"),
        "the release control must still change authored source cfg"
    );

    let cargo_tree = Command::new("cargo")
        .args([
            "tree",
            "--offline",
            "--manifest-path",
            fixture.join("Cargo.toml").to_str().unwrap(),
            "--target",
            target,
            "--edges",
            "normal,build",
            "--prefix",
            "none",
        ])
        .output()
        .expect("run Cargo target-platform control");
    assert!(
        cargo_tree.status.success(),
        "Cargo tree control failed: {}",
        String::from_utf8_lossy(&cargo_tree.stderr)
    );
    let cargo_tree = String::from_utf8(cargo_tree.stdout).unwrap();
    assert!(cargo_tree.contains("target-on v"), "{cargo_tree}");
    assert!(cargo_tree.contains("host-build-on v"), "{cargo_tree}");
    assert!(cargo_tree.contains("proc-host-on v"), "{cargo_tree}");
    assert!(cargo_tree.contains("debug-on v"), "{cargo_tree}");
    assert!(cargo_tree.contains("context-host-leaf v"), "{cargo_tree}");
    assert!(!cargo_tree.contains("target-off v"), "{cargo_tree}");
    assert!(!cargo_tree.contains("target-build-off v"), "{cargo_tree}");
    assert!(!cargo_tree.contains("proc-target-off v"), "{cargo_tree}");
    assert!(
        !cargo_tree.contains("proc-macro-target-off v"),
        "a target-conditioned edge to a proc macro is selected by the parent target before the child switches to host context: {cargo_tree}"
    );
    assert!(!cargo_tree.contains("release-off v"), "{cargo_tree}");
    assert!(
        !cargo_tree.contains("context-target-leaf v"),
        "{cargo_tree}"
    );

    let proc_macro_fixture = fixture.parent().unwrap().join("proc-macro-member");
    let proc_macro_root = run_path_json(&proc_macro_fixture, &["--target", target]);
    assert_eq!(
        proc_macro_root["profile"]["enabled_features"]["context-host-leaf"],
        serde_json::json!(["marker"])
    );
    assert_eq!(
        proc_macro_root["profile"]["enabled_features"]["context-target-leaf"],
        serde_json::json!([]),
        "a selected proc-macro root and its transitive normal dependencies use host context"
    );

    let dual_fixture = fixture.parent().unwrap().join("dual-app");
    let dual = run_path_json(&dual_fixture, &["--target", target]);
    assert_eq!(
        dual["profile"]["enabled_features"]["context-host-leaf"],
        serde_json::json!(["marker"])
    );
    assert_eq!(
        dual["profile"]["enabled_features"]["context-target-leaf"],
        serde_json::json!(["marker"]),
        "one package reached as both a normal and build dependency must union target and host contexts"
    );
    let dual_tree = Command::new("cargo")
        .args([
            "tree",
            "--offline",
            "--manifest-path",
            dual_fixture.join("Cargo.toml").to_str().unwrap(),
            "--target",
            target,
            "--edges",
            "normal,build",
            "--prefix",
            "none",
        ])
        .output()
        .expect("run Cargo dual-context control");
    assert!(
        dual_tree.status.success(),
        "Cargo dual-context control failed: {}",
        String::from_utf8_lossy(&dual_tree.stderr)
    );
    let dual_tree = String::from_utf8(dual_tree.stdout).unwrap();
    assert!(dual_tree.contains("context-host-leaf v"), "{dual_tree}");
    assert!(dual_tree.contains("context-target-leaf v"), "{dual_tree}");
}

#[test]
fn development_dependencies_are_scoped_to_selected_path_roots() {
    let workspace =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/feature-platforms");
    let target = "wasm32-unknown-unknown";

    let transitive = run_path_json(&workspace.join("dev-root"), &["--target", target]);
    assert_eq!(
        transitive["profile"]["enabled_features"]["dev-root"],
        serde_json::json!([]),
        "a dependency's dev edge must not feed features back into the selected root"
    );

    let selected = run_path_json(&workspace.join("dev-child"), &["--target", target]);
    assert_eq!(
        selected["profile"]["enabled_features"]["dev-root"],
        serde_json::json!(["testability"]),
        "the same dev edge is active when its declaring package is a selected PATH root"
    );

    for (member, expects_testability) in [("dev-root", false), ("dev-child", true)] {
        let output = Command::new("cargo")
            .args([
                "tree",
                "--offline",
                "--manifest-path",
                workspace.join(member).join("Cargo.toml").to_str().unwrap(),
                "--target",
                target,
                "--edges",
                "features",
                "--prefix",
                "none",
            ])
            .output()
            .expect("run Cargo dev-dependency control");
        assert!(
            output.status.success(),
            "Cargo dev control failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let tree = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            tree.contains("dev-root feature \"testability\""),
            expects_testability,
            "{member}: {tree}"
        );
    }
}

#[test]
fn standalone_directory_honors_its_own_gitignore() {
    let directory = tempfile::tempdir().expect("create standalone ignore fixture");
    fs::write(directory.path().join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(directory.path().join("visible.rs"), "pub fn visible() {}\n").unwrap();
    fs::write(directory.path().join("ignored.rs"), "pub fn ignored() {}\n").unwrap();

    let report = run_path_json(directory.path(), &[]);
    assert_eq!(report["file_count"], 1);
    assert_eq!(report["files"][0]["path"], "visible.rs");
}

#[test]
fn cargo_module_graph_cannot_bypass_ignore_or_hidden_discovery() {
    let directory = tempfile::tempdir().expect("create Cargo discovery fixture");
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"rot-discovery-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(directory.path().join(".gitignore"), "src/ignored.rs\n").unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        "mod ignored;\n#[path = \".hidden.rs\"] mod hidden;\npub fn visible() {}\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("src/ignored.rs"),
        "pub fn ignored() {}\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("src/.hidden.rs"),
        "pub fn hidden() {}\n",
    )
    .unwrap();

    let default = run_path_json(directory.path(), &[]);
    assert_eq!(default["file_count"], 1);
    assert_eq!(default["files"][0]["path"], "src/lib.rs");

    let no_ignore = run_path_json(directory.path(), &["--no-ignore"]);
    assert_eq!(no_ignore["file_count"], 2);
    assert!(file(&no_ignore, "src/ignored.rs").is_object());
    assert!(
        no_ignore["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != "src/.hidden.rs")
    );

    let hidden = run_path_json(directory.path(), &["--hidden"]);
    assert_eq!(hidden["file_count"], 2);
    assert!(file(&hidden, "src/.hidden.rs").is_object());
    assert!(
        hidden["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != "src/ignored.rs")
    );
}

#[test]
fn ignored_cargo_target_root_requires_no_ignore_for_reporting() {
    let directory = tempfile::tempdir().expect("create ignored target fixture");
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"rot-ignored-target\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(directory.path().join(".gitignore"), "src/lib.rs\n").unwrap();
    fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn ignored_target() {}\n",
    )
    .unwrap();

    let default = run_path_json(directory.path(), &[]);
    assert_eq!(default["file_count"], 0);
    assert_eq!(default["selection"]["respect_ignores"], true);

    let no_ignore = run_path_json(directory.path(), &["--no-ignore"]);
    assert_eq!(no_ignore["file_count"], 1);
    assert_eq!(no_ignore["files"][0]["path"], "src/lib.rs");
    assert_eq!(no_ignore["selection"]["respect_ignores"], false);
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
fn release_cfg_preset_is_explicit_and_disables_debug_assertions() {
    let (_, development) = run_json(&[]);
    let (_, release) = run_json(&["--release"]);

    assert_eq!(development["profile"]["cfg_preset"], "dev");
    assert_eq!(release["profile"]["cfg_preset"], "release");
    assert!(
        development["profile"]["active_cfg"]
            .as_array()
            .unwrap()
            .iter()
            .any(|predicate| predicate == "debug_assertions")
    );
    assert!(
        !release["profile"]["active_cfg"]
            .as_array()
            .unwrap()
            .iter()
            .any(|predicate| predicate == "debug_assertions")
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

#[test]
fn summary_only_omits_only_per_file_json() {
    let (_, detailed) = run_json(&[]);
    let (_, summary) = run_json(&["--summary-only"]);

    assert_eq!(detailed["detail"], "files");
    assert!(detailed.get("files").is_some());
    assert_eq!(summary["detail"], "summary");
    assert!(summary.get("files").is_none());
    for field in [
        "root",
        "selection",
        "profile",
        "file_count",
        "bytes",
        "buckets",
        "total",
        "lexical_complexity",
        "cyclomatic_authored",
        "cognitive_authored",
        "diagnostics",
    ] {
        assert_eq!(summary[field], detailed[field], "aggregate field {field}");
    }
}

#[test]
fn json_records_deterministic_input_and_discovery_provenance() {
    let (directory, _) = temporary_git_workspace();
    let alpha = directory.path().join("alpha");
    let beta = directory.path().join("beta");
    fs::create_dir(&alpha).unwrap();
    fs::create_dir(&beta).unwrap();
    fs::write(alpha.join("same.rs"), "pub fn same() {}\n").unwrap();
    fs::write(beta.join("same.rs"), "pub fn same() {}\n").unwrap();
    git(directory.path(), &["add", "."]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=Rot Test",
            "-c",
            "user.email=rot@example.invalid",
            "commit",
            "-qm",
            "selection fixture",
        ],
    );
    let baseline = git(directory.path(), &["rev-parse", "HEAD"]);
    let alpha = alpha.to_str().unwrap();
    let beta = beta.to_str().unwrap();

    let alpha_beta = run(&[alpha, beta, "--format", "json", "--summary-only"]);
    let beta_alpha = run(&[beta, alpha, "--format", "json", "--summary-only"]);
    assert!(alpha_beta.status.success());
    assert_eq!(alpha_beta.stdout, beta_alpha.stdout);
    let snapshot: Value = serde_json::from_slice(&alpha_beta.stdout).unwrap();
    assert_eq!(
        snapshot["selection"],
        serde_json::json!({
            "paths": [
                {"path": "alpha", "kind": "directory"},
                {"path": "beta", "kind": "directory"},
            ],
            "include_hidden": false,
            "respect_ignores": true,
            "ignore_boundary": "path",
        })
    );

    let alpha_comparison = run(&[
        alpha,
        "--baseline",
        baseline.trim(),
        "--format",
        "json",
        "--summary-only",
    ]);
    let beta_comparison = run(&[
        beta,
        "--baseline",
        baseline.trim(),
        "--format",
        "json",
        "--summary-only",
    ]);
    assert!(alpha_comparison.status.success());
    assert!(beta_comparison.status.success());
    let alpha_comparison: Value = serde_json::from_slice(&alpha_comparison.stdout).unwrap();
    let beta_comparison: Value = serde_json::from_slice(&beta_comparison.stdout).unwrap();
    assert_eq!(alpha_comparison["selection"]["paths"][0]["path"], "alpha");
    assert_eq!(beta_comparison["selection"]["paths"][0]["path"], "beta");
    assert_ne!(alpha_comparison["selection"], beta_comparison["selection"]);

    let discovery = run(&[
        alpha,
        "--hidden",
        "--no-ignore",
        "--format",
        "json",
        "--summary-only",
    ]);
    assert!(discovery.status.success());
    let discovery: Value = serde_json::from_slice(&discovery.stdout).unwrap();
    assert_eq!(discovery["selection"]["include_hidden"], true);
    assert_eq!(discovery["selection"]["respect_ignores"], false);
}

#[test]
fn help_is_agent_oriented_and_output_flags_are_not_silent() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for text in [
        "--baseline",
        "--summary-only",
        "does not select input files",
        "Each directory PATH is an ignore boundary",
        "dirty flag follows repository-wide Git status independently",
        "Role file counts overlap",
        "not a promise of numeric identity with scc",
        "diagnostics to stderr",
    ] {
        assert!(help.contains(text), "help omitted {text:?}\n{help}");
    }

    let conflict = run(&[fixture().to_str().unwrap(), "--files", "--summary-only"]);
    assert_eq!(conflict.status.code(), Some(2));

    let irrelevant_files = run(&[fixture().to_str().unwrap(), "--format", "json", "--files"]);
    assert_eq!(irrelevant_files.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&irrelevant_files.stderr).contains("only valid with table"));

    let irrelevant_summary = run(&[fixture().to_str().unwrap(), "--summary-only"]);
    assert_eq!(irrelevant_summary.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&irrelevant_summary.stderr)
            .contains("only valid with --format json")
    );
}

#[test]
fn baseline_compares_staged_unstaged_and_untracked_rust_metrics() {
    let (directory, baseline) = temporary_git_workspace();
    fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn answer() -> usize { if true { 42 } else { 0 } }\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("src/staged.rs"),
        "pub fn staged() {}\n",
    )
    .unwrap();
    git(directory.path(), &["add", "src/staged.rs"]);
    fs::write(
        directory.path().join("src/untracked.rs"),
        "#[cfg(test)]\npub fn untracked_test_helper() {}\n",
    )
    .unwrap();

    let output = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        &baseline,
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "rot comparison failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        !text.contains("rot-baseline-"),
        "temporary path leaked: {text}"
    );
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["report_kind"], "comparison");
    assert_eq!(report["before"]["revision"], baseline);
    assert_eq!(report["before"]["commit"], baseline);
    assert_eq!(report["after"]["dirty"], true);
    assert_eq!(report["summary"]["code"]["before"], 0);
    assert!(report["summary"]["code"]["after"].as_u64().unwrap() > 0);
    assert!(report["summary"]["code"]["percent_change"].is_null());

    let files = report["files"].as_array().unwrap();
    let statuses = files
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap(),
                file["status"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert!(statuses.contains(&("src/lib.rs", "modified")));
    assert!(statuses.contains(&("src/staged.rs", "added")));
    assert!(statuses.contains(&("src/untracked.rs", "added")));

    let summary = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        &baseline,
        "--format",
        "json",
        "--summary-only",
    ]);
    assert!(summary.status.success());
    let summary: Value = serde_json::from_slice(&summary.stdout).unwrap();
    assert_eq!(summary["detail"], "summary");
    assert!(summary.get("files").is_none());
    assert_eq!(summary["summary"], report["summary"]);

    let human = run(&[directory.path().to_str().unwrap(), "--baseline", &baseline]);
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("Metric-changing files: 2 added, 1 modified, 0 deleted"));
    assert!(human.contains("Largest metric changes"));
    assert!(human.contains("new"));
    assert!(human.contains("src/untracked.rs"));
    assert!(human.contains("Metric diff is not textual Git churn"));

    let parallel = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        &baseline,
        "--format",
        "json",
        "--threads",
        "2",
    ]);
    assert!(parallel.status.success());
    assert_eq!(text.as_bytes(), parallel.stdout);

    fs::write(directory.path().join("src/lib.rs"), "pub fn broken( {\n").unwrap();
    let strict = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        &baseline,
        "--format",
        "json",
        "--summary-only",
        "--strict",
    ]);
    assert!(!strict.status.success());
    assert!(serde_json::from_slice::<Value>(&strict.stdout).is_ok());
    assert!(String::from_utf8_lossy(&strict.stderr).contains("working tree"));
}

#[test]
fn human_baseline_rows_explain_and_rank_non_code_only_changes() {
    let (directory, _) = temporary_git_workspace();
    let comment_path = directory.path().join("src/comment.rs");
    let blank_path = directory.path().join("src/blank.rs");
    fs::write(&comment_path, "fn comment_fixture() {}\n").unwrap();
    fs::write(&blank_path, "fn blank_fixture() {}\n").unwrap();
    git(directory.path(), &["add", "."]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=Rot Test",
            "-c",
            "user.email=rot@example.invalid",
            "commit",
            "-qm",
            "non-code baseline",
        ],
    );
    let baseline = git(directory.path(), &["rev-parse", "HEAD"]);
    let baseline = baseline.trim();

    fs::write(&comment_path, "fn comment_fixture() {}\n// added comment\n").unwrap();
    fs::write(&blank_path, "fn blank_fixture() {}\n\n").unwrap();

    let output = run(&[directory.path().to_str().unwrap(), "--baseline", baseline]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let human = String::from_utf8(output.stdout).unwrap();
    assert!(human.contains("Metric-changing files: 0 added, 2 modified, 0 deleted"));
    assert!(human.contains("Largest metric changes (top 2)"));

    let comment_row = human
        .lines()
        .find(|line| line.contains("src/comment.rs"))
        .expect("comment-only contributor row");
    assert!(comment_row.contains("bytes +"), "{comment_row}");
    assert!(comment_row.contains("lines +1"), "{comment_row}");
    assert!(comment_row.contains("comments +1"), "{comment_row}");
    assert!(!comment_row.contains("code +"), "{comment_row}");

    let blank_row = human
        .lines()
        .find(|line| line.contains("src/blank.rs"))
        .expect("blank-only contributor row");
    assert!(blank_row.contains("bytes +1"), "{blank_row}");
    assert!(blank_row.contains("lines +1"), "{blank_row}");
    assert!(blank_row.contains("blank +1"), "{blank_row}");
    assert!(!blank_row.contains("code +"), "{blank_row}");

    assert!(
        human.find("src/comment.rs").unwrap() < human.find("src/blank.rs").unwrap(),
        "comment churn should deterministically outrank blank-only churn:\n{human}"
    );
}

#[test]
fn human_file_rows_preserve_full_unambiguous_paths() {
    let (directory, baseline) = temporary_git_workspace();
    let common = "this/is/a/deliberately/long/shared/suffix/metrics.rs";
    let alpha = format!("alpha/{common}");
    let beta = format!("beta/{common}");
    for path in [&alpha, &beta] {
        let path = directory.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "pub fn changed() {}\n").unwrap();
    }

    let output = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        &baseline,
        "--files",
    ]);
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.contains(&alpha),
        "missing full alpha path:\n{output}"
    );
    assert!(output.contains(&beta), "missing full beta path:\n{output}");
    assert!(!output.contains('…'), "path was truncated:\n{output}");
}

#[test]
fn baseline_errors_are_actionable() {
    let (directory, baseline) = temporary_git_workspace();
    let missing = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        "definitely-not-a-ref",
    ]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("does not resolve to a commit"));

    let new_file = directory.path().join("src/new.rs");
    fs::write(&new_file, "pub fn new_file() {}\n").unwrap();
    let absent_path = run(&[new_file.to_str().unwrap(), "--baseline", &baseline]);
    assert!(!absent_path.status.success());
    assert!(
        String::from_utf8_lossy(&absent_path.stderr)
            .contains("compare a containing directory to include additions")
    );

    let range = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        "HEAD~1..HEAD",
    ]);
    assert!(!range.status.success());
    assert!(String::from_utf8_lossy(&range.stderr).contains("not a revision range"));

    let changed_kind = directory.path().join("src/lib.rs");
    fs::remove_file(&changed_kind).unwrap();
    fs::create_dir(&changed_kind).unwrap();
    fs::write(changed_kind.join("nested.rs"), "pub fn nested() {}\n").unwrap();
    let changed_kind = run(&[changed_kind.to_str().unwrap(), "--baseline", &baseline]);
    assert!(!changed_kind.status.success());
    assert!(
        String::from_utf8_lossy(&changed_kind.stderr)
            .contains("compare a stable containing directory instead")
    );
}

#[cfg(unix)]
#[test]
fn baseline_maps_a_retargeted_symlink_by_its_lexical_repository_path() {
    use std::os::unix::fs::symlink;

    let backing = tempfile::tempdir().expect("create symlink backing fixture");
    let directory = backing.path().join("repository");
    fs::create_dir(&directory).unwrap();
    let aliases = tempfile::tempdir().expect("create symlink alias fixture");
    let alias = aliases.path().join("repository-alias");
    symlink(&directory, &alias).unwrap();

    fs::create_dir(directory.join("a")).unwrap();
    fs::create_dir(directory.join("b")).unwrap();
    fs::write(
        directory.join("a/metrics.rs"),
        "pub fn before() { if true {} }\n",
    )
    .unwrap();
    fs::write(
        directory.join("b/metrics.rs"),
        "pub fn after() { if true {} if false {} }\n",
    )
    .unwrap();
    symlink("a", directory.join("scope")).unwrap();
    git(&directory, &["init", "-q"]);
    git(&directory, &["add", "."]);
    git(
        &directory,
        &[
            "-c",
            "user.name=Rot Test",
            "-c",
            "user.email=rot@example.invalid",
            "commit",
            "-qm",
            "symlink baseline",
        ],
    );
    let baseline = git(&directory, &["rev-parse", "HEAD"]);

    fs::remove_file(directory.join("scope")).unwrap();
    symlink("b", directory.join("scope")).unwrap();
    let comparison = run_path_json(&alias.join("scope"), &["--baseline", baseline.trim()]);

    assert_eq!(
        comparison["selection"]["paths"],
        serde_json::json!([{ "path": "scope", "kind": "directory" }])
    );
    assert_eq!(comparison["summary"]["cyclomatic_authored"]["delta"], 1);
    assert!(
        comparison["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| { change["path"] == "a/metrics.rs" && change["status"] == "deleted" })
    );
    assert!(
        comparison["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| { change["path"] == "b/metrics.rs" && change["status"] == "added" })
    );
}

#[test]
fn baseline_ignores_inherited_git_repository_environment() {
    let (directory, baseline) = temporary_git_workspace();
    let foreign = tempfile::tempdir().expect("create foreign Git fixture");
    git(foreign.path(), &["init", "-q"]);

    let output = Command::new(env!("CARGO_BIN_EXE_rot"))
        .arg(directory.path())
        .args([
            "--baseline",
            &baseline,
            "--format",
            "json",
            "--summary-only",
        ])
        .env("GIT_DIR", foreign.path().join(".git"))
        .env("GIT_WORK_TREE", foreign.path())
        .env("GIT_COMMON_DIR", foreign.path().join(".git"))
        .env("GIT_INDEX_FILE", foreign.path().join(".git/index"))
        .env("GIT_OBJECT_DIRECTORY", foreign.path().join(".git/objects"))
        .output()
        .expect("run rot with inherited Git environment");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let comparison: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(comparison["before"]["commit"], baseline);
    assert_eq!(comparison["summary"]["files"]["delta"], 0);
}

#[test]
fn baseline_applies_gitignore_rules_on_both_endpoints() {
    let (directory, _) = temporary_git_workspace();
    fs::write(directory.path().join(".gitignore"), "src/generated.rs\n")
        .expect("write ignore file");
    fs::write(
        directory.path().join("src/generated.rs"),
        "pub fn generated() {}\n",
    )
    .expect("write ignored source");
    git(directory.path(), &["add", ".gitignore"]);
    git(directory.path(), &["add", "--force", "src/generated.rs"]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=Rot Test",
            "-c",
            "user.email=rot@example.invalid",
            "commit",
            "-qm",
            "ignored source",
        ],
    );

    let baseline = git(directory.path(), &["rev-parse", "HEAD"]);
    let result = run(&[
        directory.path().to_str().unwrap(),
        "--baseline",
        baseline.trim(),
        "--format",
        "json",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).expect("comparison JSON");
    assert_eq!(report["after"]["commit"], baseline.trim());
    assert_eq!(report["summary"]["files"]["delta"], 0);
    assert_eq!(report["files"], Value::Array(Vec::new()));
}

#[test]
fn ancestor_ignored_selected_directory_includes_untracked_baseline_delta() {
    let directory = tempfile::tempdir().expect("create ignored-directory fixture");
    let selected = directory.path().join("ignored");
    fs::create_dir(&selected).expect("create selected directory");
    fs::write(directory.path().join(".gitignore"), "ignored/\n")
        .expect("write ancestor ignore file");
    let source = selected.join("selected.rs");
    fs::write(&source, "pub fn selected() { if true {} }\n").expect("write baseline source");

    git(directory.path(), &["init", "-q"]);
    git(directory.path(), &["add", ".gitignore"]);
    git(directory.path(), &["add", "--force", "ignored/selected.rs"]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=Rot Test",
            "-c",
            "user.email=rot@example.invalid",
            "commit",
            "-qm",
            "ignored directory baseline",
        ],
    );
    let baseline = git(directory.path(), &["rev-parse", "HEAD"]);

    fs::write(
        selected.join("untracked.rs"),
        "pub fn untracked() { if true {} if false {} }\n",
    )
    .expect("write ignored untracked source");

    let snapshot = run(&[
        selected.to_str().unwrap(),
        "--format",
        "json",
        "--summary-only",
    ]);
    assert!(
        snapshot.status.success(),
        "{}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let snapshot: Value = serde_json::from_slice(&snapshot.stdout).expect("snapshot JSON");
    assert_eq!(snapshot["file_count"], 2);
    assert_eq!(snapshot["cyclomatic_authored"], 5);

    let comparison = run(&[
        selected.to_str().unwrap(),
        "--baseline",
        baseline.trim(),
        "--format",
        "json",
    ]);
    assert!(
        comparison.status.success(),
        "{}",
        String::from_utf8_lossy(&comparison.stderr)
    );
    let comparison: Value = serde_json::from_slice(&comparison.stdout).expect("comparison JSON");
    assert_eq!(comparison["summary"]["files"]["before"], 1);
    assert_eq!(comparison["summary"]["files"]["after"], 2);
    assert_eq!(comparison["summary"]["cyclomatic_authored"]["before"], 2);
    assert_eq!(comparison["summary"]["cyclomatic_authored"]["after"], 5);
    assert_eq!(comparison["summary"]["cyclomatic_authored"]["delta"], 3);
    assert_eq!(comparison["metric_changed_files"]["added"], 1);
    assert_eq!(comparison["metric_changed_files"]["modified"], 0);
    assert_eq!(comparison["files"][0]["path"], "ignored/untracked.rs");
    assert_eq!(comparison["files"][0]["status"], "added");
    assert_eq!(comparison["after"]["dirty"], false);
}

#[test]
fn snapshot_and_comparison_share_explicit_ignore_boundaries() {
    let ambient = tempfile::tempdir().expect("create ambient fixture directory");
    fs::write(ambient.path().join(".ignore"), "ambient.rs\n").expect("write ambient ignore file");

    let repository = ambient.path().join("repository");
    let selected = repository.join("nested/src");
    fs::create_dir_all(repository.join("src")).expect("create Cargo source directory");
    fs::create_dir_all(&selected).expect("create nested selected directory");
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"rot-ignore-boundary\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("write fixture manifest");
    fs::write(repository.join("src/lib.rs"), "").expect("write Cargo target");
    fs::write(repository.join(".ignore"), "repository.rs\n").expect("write repository ignore file");
    fs::write(selected.join(".ignore"), "inside.rs\n").expect("write selected ignore file");
    fs::write(selected.join("ambient.rs"), "pub fn ambient() {\n}\n")
        .expect("write ambient-pattern source");
    fs::write(
        selected.join("repository.rs"),
        "pub fn repository_ignored() {\n    if true {}\n}\n",
    )
    .expect("write repository-pattern source");
    fs::write(
        selected.join("inside.rs"),
        "pub fn selected_root_ignored() {\n    if true {}\n    if false {}\n}\n",
    )
    .expect("write selected-root ignored source");
    fs::write(selected.join("visible.rs"), "pub fn visible() {}\n").expect("write visible source");

    git(&repository, &["init", "-q"]);
    git(&repository, &["add", "."]);
    git(
        &repository,
        &[
            "-c",
            "user.name=Rot Test",
            "-c",
            "user.email=rot@example.invalid",
            "commit",
            "-qm",
            "ignore boundary",
        ],
    );
    let baseline = git(&repository, &["rev-parse", "HEAD"]);

    let snapshot = run(&[selected.to_str().unwrap(), "--format", "json"]);
    assert!(
        snapshot.status.success(),
        "{}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let snapshot: Value = serde_json::from_slice(&snapshot.stdout).expect("snapshot JSON");
    assert_eq!(snapshot["file_count"], 3);
    assert!(file(&snapshot, "ambient.rs").is_object());
    assert!(file(&snapshot, "repository.rs").is_object());
    assert!(file(&snapshot, "visible.rs").is_object());
    assert!(
        snapshot["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["path"] != "inside.rs")
    );

    let without_ignores = run(&[
        selected.to_str().unwrap(),
        "--no-ignore",
        "--format",
        "json",
        "--summary-only",
    ]);
    assert!(without_ignores.status.success());
    let without_ignores: Value =
        serde_json::from_slice(&without_ignores.stdout).expect("no-ignore snapshot JSON");
    assert_eq!(without_ignores["file_count"], 4);

    let explicit_file = run(&[
        selected.join("inside.rs").to_str().unwrap(),
        "--format",
        "json",
        "--summary-only",
    ]);
    assert!(explicit_file.status.success());
    let explicit_file: Value =
        serde_json::from_slice(&explicit_file.stdout).expect("explicit-file snapshot JSON");
    assert_eq!(explicit_file["file_count"], 1);

    let comparison = run(&[
        selected.to_str().unwrap(),
        "--baseline",
        baseline.trim(),
        "--format",
        "json",
    ]);
    assert!(
        comparison.status.success(),
        "{}",
        String::from_utf8_lossy(&comparison.stderr)
    );
    let comparison: Value = serde_json::from_slice(&comparison.stdout).expect("comparison JSON");
    for metric in [
        "files",
        "bytes",
        "physical",
        "code",
        "comments",
        "docs",
        "blank",
        "lexical_complexity",
        "cyclomatic_authored",
        "cognitive_authored",
        "declared_public",
    ] {
        let snapshot_metric = match metric {
            "files" => &snapshot["file_count"],
            "physical" => &snapshot["total"]["physical"],
            "code" => &snapshot["total"]["code"],
            "comments" => &snapshot["total"]["comments"],
            "docs" => &snapshot["total"]["docs"],
            "blank" => &snapshot["total"]["blank"],
            "declared_public" => &snapshot["buckets"][0]["declared_public"],
            metric => &snapshot[metric],
        };
        assert_eq!(
            comparison["summary"][metric]["before"], *snapshot_metric,
            "baseline metric {metric}"
        );
        assert_eq!(
            comparison["summary"][metric]["after"], *snapshot_metric,
            "working-tree metric {metric}"
        );
    }
    assert_eq!(comparison["after"]["profile"], snapshot["profile"]);
    assert_eq!(comparison["files"], Value::Array(Vec::new()));
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
    assert_eq!(report["schema_version"], 3);
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
    assert_eq!(
        metric_table_row(&summary, "Production"),
        [
            production["files"].as_u64().unwrap(),
            production["physical"].as_u64().unwrap(),
            production["code"].as_u64().unwrap(),
            production["comments"].as_u64().unwrap(),
            production["docs"].as_u64().unwrap(),
            production["blank"].as_u64().unwrap(),
            production["lexical_complexity"].as_u64().unwrap(),
            production["cyclomatic_authored"].as_u64().unwrap(),
            production["cognitive_authored"].as_u64().unwrap(),
            production["declared_public"].as_u64().unwrap(),
        ]
    );
    assert_eq!(
        metric_table_row(&summary, "Total"),
        [
            report["file_count"].as_u64().unwrap(),
            report["total"]["physical"].as_u64().unwrap(),
            report["total"]["code"].as_u64().unwrap(),
            report["total"]["comments"].as_u64().unwrap(),
            report["total"]["docs"].as_u64().unwrap(),
            report["total"]["blank"].as_u64().unwrap(),
            report["lexical_complexity"].as_u64().unwrap(),
            report["cyclomatic_authored"].as_u64().unwrap(),
            report["cognitive_authored"].as_u64().unwrap(),
            report["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|bucket| bucket["declared_public"].as_u64().unwrap())
                .sum(),
        ]
    );

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
fn rot_rejects_compiler_mode_and_fast_json_has_no_compiler_field() {
    let rejected = run(&["--compiler", fixture().to_str().unwrap()]);
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("unexpected argument"), "{stderr}");
    assert!(stderr.contains("--compiler"), "{stderr}");

    let (_, report) = run_json(&[]);
    assert!(report["file_count"].as_u64().unwrap() > 0);
    assert!(report.get("compiler").is_none());
}
