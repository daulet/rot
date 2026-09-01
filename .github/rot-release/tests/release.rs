use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use rot_release::{emit_plan, plan_release, set_version};
use tempfile::TempDir;

struct TestRepo {
    directory: TempDir,
}

impl TestRepo {
    fn target(version: &str, subject: &str) -> Self {
        let repository = Self::empty();
        repository.write_target_layout(version);
        repository.write("src/lib.rs", "pub fn initial() {}\n");
        repository.commit(subject);
        repository
    }

    fn old_layout(version: &str, subject: &str) -> Self {
        let repository = Self::empty();
        repository.write(
            "Cargo.toml",
            &format!(
                "[workspace]\nmembers = [\"crates/rot-compiler-protocol\"]\n\n[package]\nname = \"rot-metrics\"\nversion = \"{version}\"\n\n[dependencies]\nrot-compiler-protocol = {{ path = \"crates/rot-compiler-protocol\", version = \"={version}\" }}\n"
            ),
        );
        repository.write(
            "crates/rot-compiler-protocol/Cargo.toml",
            &format!("[package]\nname = \"rot-compiler-protocol\"\nversion = \"{version}\"\n"),
        );
        repository.write(
            "compiler/rot-rustc-driver/Cargo.toml",
            &format!(
                "[package]\nname = \"rot-rustc-driver\"\nversion = \"{version}\"\n\n[workspace]\n\n[dependencies]\nrot-compiler-protocol = {{ path = \"../../crates/rot-compiler-protocol\", version = \"={version}\" }}\n"
            ),
        );
        repository.write_locks(version);
        repository.write("src/lib.rs", "pub fn initial() {}\n");
        repository.commit(subject);
        repository
    }

    fn empty() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repository = Self { directory };
        repository.git(&["init", "-q", "-b", "main"]);
        repository.git(&["config", "user.name", "Rot Test"]);
        repository.git(&["config", "user.email", "rot-test@example.com"]);
        repository
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn write_target_layout(&self, version: &str) {
        self.write(
            "Cargo.toml",
            &format!(
                "[workspace]\nmembers = [\"crates/rot-compiler-protocol\"]\n\n[workspace.package]\nversion = \"{version}\"\n\n[workspace.dependencies]\nrot-compiler-protocol = {{ path = \"crates/rot-compiler-protocol\", version = \"={version}\" }}\n\n[package]\nname = \"rot-metrics\"\nversion.workspace = true\n\n[dependencies]\nrot-compiler-protocol = {{ workspace = true }}\n"
            ),
        );
        self.write(
            "crates/rot-compiler-protocol/Cargo.toml",
            "[package]\nname = \"rot-compiler-protocol\"\nversion.workspace = true\n",
        );
        self.write(
            "compiler/rot-rustc-driver/Cargo.toml",
            "[package]\nname = \"rot-rustc-driver\"\nversion = \"0.1.0\"\n\n[workspace]\n\n[dependencies]\nrot-compiler-protocol = { path = \"../../crates/rot-compiler-protocol\" }\n",
        );
        self.write_locks(version);
    }

    fn write_locks(&self, version: &str) {
        self.write(
            "Cargo.lock",
            &format!(
                "version = 4\n\n[[package]]\nname = \"rot-compiler-protocol\"\nversion = \"{version}\"\n\n[[package]]\nname = \"rot-metrics\"\nversion = \"{version}\"\n"
            ),
        );
        self.write(
            "compiler/rot-rustc-driver/Cargo.lock",
            &format!(
                "version = 4\n\n[[package]]\nname = \"rot-compiler-protocol\"\nversion = \"{version}\"\n\n[[package]]\nname = \"rot-rustc-driver\"\nversion = \"0.1.0\"\n"
            ),
        );
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn change(&self, path: &str, content: &str, message: &str) -> String {
        self.write(path, content);
        self.commit(message)
    }

    fn release(&self, version: &str, source: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "user.name=github-actions[bot]",
            "-c",
            "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!(
                "chore(release): v{version}\n\nRelease-Source: {source}\nRelease-Automation: rot-v1"
            ),
        ]);
        self.head()
    }

    fn remove(&self, relative: &str) {
        fs::remove_file(self.path().join(relative)).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.head()
    }

    fn generated_commit(&self, version: &str, tag: bool) -> (String, String) {
        let source = self.head();
        set_version(self.path(), version.parse().unwrap()).unwrap();
        let release = self.release(version, &source);
        if tag {
            self.annotated_tag(&format!("v{version}"), &release);
        }
        (source, release)
    }

    fn annotated_tag(&self, tag: &str, target: &str) {
        self.git(&["tag", "-a", tag, target, "-m", tag]);
    }

    fn lightweight_tag(&self, tag: &str, target: &str) {
        self.git(&["tag", tag, target]);
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    fn plan(&self, source: &str) -> rot_release::Result<BTreeMap<String, String>> {
        plan_release(self.path(), source, "HEAD")
    }

    fn plan_error(&self, source: &str) -> String {
        self.plan(source).unwrap_err().to_string()
    }

    fn read(&self, path: &str) -> Vec<u8> {
        fs::read(self.path().join(path)).unwrap()
    }

    fn snapshot(&self, paths: &[&str]) -> Vec<Vec<u8>> {
        paths.iter().map(|path| self.read(path)).collect()
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

fn value<'a>(plan: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    plan.get(key).unwrap().as_str()
}

macro_rules! fields {
    ($plan:expr; $($key:literal => $expected:expr),+ $(,)?) => {{
        let plan = $plan;
        $(assert_eq!(value(&plan, $key), $expected);)+
    }};
}

fn tagged_baseline() -> TestRepo {
    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    repository.generated_commit("0.1.0", true);
    repository
}

fn assert_plan_error(repository: &TestRepo, source: &str, fragments: &[&str]) {
    let error = repository.plan_error(source);
    for fragment in fragments {
        assert!(error.contains(fragment), "{error}");
    }
}

#[test]
fn first_feature_bootstraps_minor_release() {
    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    fields!(repository.plan("HEAD").unwrap();
        "state" => "new", "bump" => "minor", "version" => "0.1.0",
        "previous_tag" => "", "commit_count" => "1");
}

#[test]
fn patch_follows_tagged_generated_release() {
    let repository = tagged_baseline();
    repository.change(
        "src/lib.rs",
        "pub fn fixed() {}\n",
        "fix: correct the parser",
    );
    fields!(repository.plan("HEAD").unwrap();
        "state" => "new", "bump" => "patch", "version" => "0.1.1",
        "previous_tag" => "v0.1.0");
}

#[test]
fn markdown_only_merge_is_a_noop_from_tagged_boundary() {
    let repository = tagged_baseline();
    repository.git(&["checkout", "-q", "-b", "docs"]);
    repository.change("README.md", "install rot\n", "docs: add installation guide");
    repository.git(&["checkout", "-q", "main"]);
    repository.git(&[
        "merge",
        "-q",
        "--no-ff",
        "docs",
        "-m",
        "docs: merge installation guide",
    ]);
    fields!(repository.plan("HEAD").unwrap();
        "state" => "markdown-only", "commit_count" => "0");
}

#[test]
fn release_neutral_trees_are_noops() {
    for (path, content, extra, subject, state) in [
        (
            "README.MD",
            "words\n",
            None,
            "docs: explain output",
            "markdown-only",
        ),
        (
            ".github/scripts/release.rs",
            "const FIXED: bool = true;\n",
            Some(("docs/releases.txt", "policy\n")),
            "feat(release): improve automation",
            "release-neutral",
        ),
        (
            "src/TESTS/case.rs",
            "#[test] fn case() {}\n",
            None,
            "feat: add a regression test",
            "release-neutral",
        ),
    ] {
        let repository = tagged_baseline();
        repository.write(path, content);
        if let Some((path, content)) = extra {
            repository.write(path, content);
        }
        repository.commit(subject);
        fields!(repository.plan("HEAD").unwrap(); "state" => state, "commit_count" => "0");
    }
}

#[test]
fn neutral_features_do_not_promote_a_code_fix() {
    for (path, content, subject) in [
        (
            "README.md",
            "future feature\n",
            "feat(docs): explain future work",
        ),
        (
            "tests/new.rs",
            "#[test] fn new() {}\n",
            "feat: add coverage",
        ),
    ] {
        let repository = tagged_baseline();
        repository.change(path, content, subject);
        repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
        fields!(repository.plan("HEAD").unwrap(); "bump" => "patch", "commit_count" => "1");
    }
}

#[test]
fn mixed_markdown_and_product_feature_is_minor() {
    let repository = tagged_baseline();
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    repository.write("README.md", "new docs\n");
    repository.write("src/lib.rs", "pub fn feature() {}\n");
    repository.commit("feat(cli): expose detail");
    fields!(repository.plan("HEAD").unwrap();
        "state" => "new", "bump" => "minor", "version" => "0.2.0",
        "commit_count" => "2");
}

#[test]
fn net_tree_gate_sees_reverts() {
    for (docs, state) in [(false, "unchanged"), (true, "markdown-only")] {
        let repository = tagged_baseline();
        repository.change(
            "src/new.rs",
            "pub fn temporary() {}\n",
            "feat: temporary code",
        );
        repository.remove("src/new.rs");
        if docs {
            repository.write("README.md", "still useful\n");
        }
        repository.commit("revert: temporary code");
        assert_eq!(value(&repository.plan("HEAD").unwrap(), "state"), state);
    }
}

#[test]
fn first_parent_merge_filters_neutral_merge_diff() {
    let repository = tagged_baseline();
    let baseline = repository.head();
    repository.git(&["checkout", "-q", "-b", "docs", &baseline]);
    repository.change("docs/guide.txt", "guide\n", "feat: side-branch docs");
    repository.git(&["checkout", "-q", "main"]);
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: mainline code");
    repository.git(&["merge", "-q", "--no-ff", "docs", "-m", "merge docs"]);
    fields!(repository.plan("HEAD").unwrap(); "bump" => "patch", "commit_count" => "1");
}

#[test]
fn generated_release_requires_net_product_change() {
    for reverted_product in [false, true] {
        let repository = tagged_baseline();
        if reverted_product {
            repository.change(
                "src/new.rs",
                "pub fn temporary() {}\n",
                "fix: temporary change",
            );
            repository.remove("src/new.rs");
        }
        let source = repository.change("README.md", "install rot\n", "docs: leave docs only");
        set_version(repository.path(), "0.1.1".parse().unwrap()).unwrap();
        repository.release("0.1.1", &source);
        assert_plan_error(
            &repository,
            &source,
            &["no release-relevant changes", "markdown-only"],
        );
    }
}

#[test]
fn pending_release_is_recovered() {
    let repository = tagged_baseline();
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    let (source, release) = repository.generated_commit("0.1.1", false);
    fields!(repository.plan(&source).unwrap();
        "state" => "pending", "source" => source, "release_sha" => release,
        "version" => "0.1.1", "previous_tag" => "v0.1.0");
    fields!(repository.plan(&release).unwrap();
        "state" => "pending", "source" => source, "release_sha" => release);
}

#[test]
fn newer_generated_release_supersedes_older_source() {
    let repository = tagged_baseline();
    let old_source = repository.git(&["rev-parse", "v0.1.0^"]);
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    let (_, newer_release) = repository.generated_commit("0.1.1", false);
    fields!(repository.plan(&old_source).unwrap();
        "state" => "superseded", "superseded_by" => newer_release,
        "superseded_by_tag" => "v0.1.1");
}

#[test]
fn non_tip_source_without_generated_release_is_stale() {
    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    let source = repository.head();
    repository.change("src/lib.rs", "pub fn next() {}\n", "fix: later commit");
    fields!(repository.plan(&source).unwrap(); "state" => "stale", "source" => source);
}

#[test]
fn untagged_generated_release_burns_its_version() {
    let repository = tagged_baseline();
    repository.change(
        "src/lib.rs",
        "pub fn feature() {}\n",
        "feat: product feature",
    );
    repository.generated_commit("0.2.0", false);
    repository.change("src/next.rs", "pub fn next() {}\n", "fix: follow-up");
    fields!(repository.plan("HEAD").unwrap();
        "previous_tag" => "v0.2.0", "version" => "0.2.1", "bump" => "patch");
}

#[test]
fn boundary_tags_are_validated() {
    let repository = tagged_baseline();
    repository.annotated_tag("v9.9.9", "HEAD");
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    assert_eq!(value(&repository.plan("HEAD").unwrap(), "version"), "0.1.1");

    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    let (_, release) = repository.generated_commit("0.1.0", false);
    repository.lightweight_tag("v0.1.0", &release);
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    assert_plan_error(&repository, "HEAD", &["is not annotated"]);

    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    let source = repository.head();
    repository.generated_commit("0.1.0", false);
    repository.annotated_tag("v0.1.0", &source);
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    assert_plan_error(&repository, "HEAD", &["points to"]);
}

#[test]
fn malformed_generated_releases_fail() {
    let repository = tagged_baseline();
    repository.commit("chore(release): v0.1.1\n\nRelease-Source: nope\nRelease-Automation: rot-v1");
    assert_plan_error(&repository, "HEAD", &["invalid source object"]);

    let repository = tagged_baseline();
    let wrong_source = repository.git(&["rev-parse", "HEAD^"]);
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    set_version(repository.path(), "0.1.1".parse().unwrap()).unwrap();
    repository.release("0.1.1", &wrong_source);
    assert_plan_error(&repository, "HEAD", &["not parent"]);

    let repository = tagged_baseline();
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    let source = repository.head();
    set_version(repository.path(), "0.1.2".parse().unwrap()).unwrap();
    repository.release("0.1.2", &source);
    assert_plan_error(&repository, "HEAD", &["requires 0.1.1"]);

    let repository = tagged_baseline();
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    let source = repository.head();
    set_version(repository.path(), "0.1.1".parse().unwrap()).unwrap();
    repository.write("README.md", "smuggled\n");
    repository.release("0.1.1", &source);
    assert_plan_error(&repository, &source, &["wrong paths", "README.md"]);

    let repository = tagged_baseline();
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    let source = repository.head();
    set_version(repository.path(), "0.1.1".parse().unwrap()).unwrap();
    let manifest_path = repository.path().join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap()
        .replace("version = \"=0.1.1\"", "version = \"=9.9.9\"");
    fs::write(manifest_path, manifest).unwrap();
    repository.release("0.1.1", &source);
    assert_plan_error(&repository, &source, &["dependency disagree"]);

    let repository = tagged_baseline();
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    let source = repository.head();
    set_version(repository.path(), "0.1.1".parse().unwrap()).unwrap();
    repository.write("README.md", "smuggled into prior release\n");
    let release = repository.release("0.1.1", &source);
    repository.annotated_tag("v0.1.1", &release);
    repository.change("src/lib.rs", "pub fn next() {}\n", "fix: follow-up");
    assert_plan_error(&repository, "HEAD", &["wrong paths", "README.md"]);

    let repository = tagged_baseline();
    repository.change("src/lib.rs", "pub fn fixed() {}\n", "fix: production bug");
    let source = repository.head();
    set_version(repository.path(), "0.1.1".parse().unwrap()).unwrap();
    repository.commit(&format!(
        "chore(release): v0.1.1\n\nRelease-Source: {source}\nRelease-Automation: rot-v1"
    ));
    assert_plan_error(&repository, "HEAD", &["unexpected committer"]);
}

#[test]
fn historical_old_layout_is_accepted() {
    let repository = TestRepo::old_layout("0.1.0", "feat: initial implementation");
    let source = repository.head();
    let release = repository.release("0.1.0", &source);
    repository.annotated_tag("v0.1.0", &release);
    assert_eq!(
        value(&repository.plan(&source).unwrap(), "state"),
        "pending"
    );
    repository.write_target_layout("0.1.0");
    repository.change(
        "src/lib.rs",
        "pub fn fixed() {}\n",
        "fix: centralize versions and fix code",
    );
    fields!(repository.plan("HEAD").unwrap(); "state" => "new", "version" => "0.1.1");
}

#[test]
fn set_version_updates_only_the_three_authority_files() {
    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    let static_paths = [
        "crates/rot-compiler-protocol/Cargo.toml",
        "compiler/rot-rustc-driver/Cargo.toml",
    ];
    let before = repository.snapshot(&static_paths);
    set_version(repository.path(), "0.2.0".parse().unwrap()).unwrap();
    let manifest = fs::read_to_string(repository.path().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("version = \"0.2.0\""));
    assert!(manifest.contains("version = \"=0.2.0\""));
    let root_lock = fs::read_to_string(repository.path().join("Cargo.lock")).unwrap();
    assert_eq!(root_lock.matches("version = \"0.2.0\"").count(), 2);
    let driver_lock =
        String::from_utf8(repository.read("compiler/rot-rustc-driver/Cargo.lock")).unwrap();
    assert_eq!(driver_lock.matches("version = \"0.2.0\"").count(), 1);
    assert_eq!(repository.snapshot(&static_paths), before);
}

#[test]
fn set_version_failure_does_not_write_any_file() {
    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    let paths = [
        "Cargo.toml",
        "Cargo.lock",
        "compiler/rot-rustc-driver/Cargo.lock",
    ];
    repository.write(
        "compiler/rot-rustc-driver/Cargo.lock",
        "this is not TOML = [",
    );
    let before = repository.snapshot(&paths);
    assert!(set_version(repository.path(), "0.1.1".parse().unwrap()).is_err());
    assert_eq!(repository.snapshot(&paths), before);
}

#[test]
fn set_version_noop_still_validates_all_authorities() {
    let repository = TestRepo::target("0.1.0", "feat: initial implementation");
    repository.write("compiler/rot-rustc-driver/Cargo.lock", "version = 4\n");
    let error = set_version(repository.path(), "0.1.0".parse().unwrap())
        .unwrap_err()
        .to_string();
    assert!(error.contains("package array"), "{error}");
}

#[test]
fn set_version_validates_the_static_manifest_layout_before_writing() {
    for (path, bad, expected) in [
        (
            "crates/rot-compiler-protocol/Cargo.toml",
            "[package]\nname = \"rot-compiler-protocol\"\nversion = \"0.1.0\"\n",
            "workspace",
        ),
        (
            "compiler/rot-rustc-driver/Cargo.toml",
            "[package]\nname = \"rot-rustc-driver\"\nversion = \"0.1.0\"\n\n[workspace]\n\n[dependencies]\nrot-compiler-protocol = { path = \"../../crates/rot-compiler-protocol\", version = \"=0.1.0\" }\n",
            "path-only",
        ),
    ] {
        let repository = TestRepo::target("0.1.0", "feat: initial implementation");
        let authorities = [
            "Cargo.toml",
            "Cargo.lock",
            "compiler/rot-rustc-driver/Cargo.lock",
        ];
        repository.write(path, bad);
        let before = repository.snapshot(&authorities);
        let error = set_version(repository.path(), "0.1.1".parse().unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
        assert_eq!(repository.snapshot(&authorities), before);
    }
}

#[test]
fn github_output_is_flat_and_appended() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("output");
    fs::write(&path, "existing=yes\n").unwrap();
    let plan = BTreeMap::from([
        ("state".to_owned(), "new".to_owned()),
        ("version".to_owned(), "0.2.0".to_owned()),
    ]);
    emit_plan(&plan, Some(&path)).unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "existing=yes\nstate=new\nversion=0.2.0\n"
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("output");
    let plan = BTreeMap::from([("state".to_owned(), "new\nversion=evil".to_owned())]);
    let error = emit_plan(&plan, Some(&path)).unwrap_err().to_string();
    assert!(error.contains("contains a newline"), "{error}");
}

#[test]
fn cli_domain_errors_have_release_prefix_and_status_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_rot-release"))
        .args([
            "plan",
            "--root",
            "/definitely/not/a/repository",
            "--source",
            "HEAD",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("release: ")
    );
}
