from __future__ import annotations

import hashlib
import importlib.util
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "release.py"
SPEC = importlib.util.spec_from_file_location("rot_release", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
release = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release
SPEC.loader.exec_module(release)


class VersionPolicyTests(unittest.TestCase):
    def test_feature_subjects_bump_minor(self) -> None:
        for subject in ("feat: add", "feat(cli): add", "feat!: replace"):
            with self.subTest(subject=subject):
                self.assertEqual(release.bump_kind([subject]), "minor")

    def test_everything_else_bumps_patch(self) -> None:
        self.assertEqual(
            release.bump_kind(["fix: repair", "refactor: simplify", "docs: explain"]),
            "patch",
        )
        self.assertEqual(release.bump_kind(["Feat: not conventional"]), "patch")

    def test_any_feature_wins_across_a_batch(self) -> None:
        self.assertEqual(
            release.bump_kind(["fix: one", "feat(parser): two", "chore: three"]),
            "minor",
        )

    def test_version_bumps_are_exact(self) -> None:
        version = release.Version.parse("3.4.5")
        self.assertEqual(str(version.bump("minor")), "3.5.0")
        self.assertEqual(str(version.bump("patch")), "3.4.6")

    def test_generated_release_requires_provenance_trailers(self) -> None:
        source = "a" * 40
        message = (
            "chore(release): v1.2.3\n\n"
            f"Release-Source: {source}\n"
            f"Release-Automation: {release.AUTOMATION_MARKER}\n"
        )
        generated = release.generated_release(message)
        self.assertIsNotNone(generated)
        assert generated is not None
        self.assertEqual(str(generated.version), "1.2.3")
        self.assertEqual(generated.source, source)
        self.assertIsNone(release.generated_release("chore(release): v1.2.3"))


class RepositoryFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "crates/rot-compiler-protocol").mkdir(parents=True)
        (self.root / "compiler/rot-rustc-driver").mkdir(parents=True)
        self._write_version_files("0.1.0")
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Release Test")
        self.git("config", "user.email", release.RELEASE_COMMITTER)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def git(self, *arguments: str) -> str:
        process = subprocess.run(
            ["git", *arguments],
            cwd=self.root,
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        )
        return process.stdout.strip()

    def commit(self, subject: str, *body: str) -> str:
        self.git("add", ".")
        command = ["commit", "--allow-empty", "-m", subject]
        for paragraph in body:
            command.extend(("-m", paragraph))
        self.git(*command)
        return self.git("rev-parse", "HEAD")

    def tag_release(self, commit: str, version: str) -> None:
        self.git("tag", "-a", f"v{version}", commit, "-m", f"Rot v{version}")

    def _write_version_files(self, version: str) -> None:
        (self.root / "Cargo.toml").write_text(
            f'''[package]
name = "rot-metrics"
version = "{version}"

[dependencies]
rot-compiler-protocol = {{ path = "crates/rot-compiler-protocol", version = "={version}", optional = true }}
''',
            encoding="utf-8",
        )
        (self.root / "crates/rot-compiler-protocol/Cargo.toml").write_text(
            f'''[package]
name = "rot-compiler-protocol"
version = "{version}"
''',
            encoding="utf-8",
        )
        (self.root / "compiler/rot-rustc-driver/Cargo.toml").write_text(
            f'''[package]
name = "rot-rustc-driver"
version = "{version}"

[dependencies]
rot-compiler-protocol = {{ path = "../../crates/rot-compiler-protocol", version = "={version}" }}
''',
            encoding="utf-8",
        )
        (self.root / "Cargo.lock").write_text(
            self._lock(version, ("rot-metrics", "rot-compiler-protocol")),
            encoding="utf-8",
        )
        (self.root / "compiler/rot-rustc-driver/Cargo.lock").write_text(
            self._lock(version, ("rot-rustc-driver", "rot-compiler-protocol")),
            encoding="utf-8",
        )

    @staticmethod
    def _lock(version: str, packages: tuple[str, ...]) -> str:
        return "".join(
            f'[[package]]\nname = "{package}"\nversion = "{version}"\n\n'
            for package in packages
        )


class ReleasePlanTests(RepositoryFixture):
    def test_first_feature_history_bootstraps_v0_1_0(self) -> None:
        source = self.commit("feat: initial product")
        plan = release.plan_release(self.root, source, "HEAD")
        self.assertEqual(plan["state"], "new")
        self.assertEqual(plan["version"], "0.1.0")
        self.assertEqual(plan["bump"], "minor")
        self.assertEqual(plan["previous_tag"], "")

    def test_patch_after_generated_release(self) -> None:
        source = self.commit("feat: initial product")
        self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        (self.root / "release.py").write_text("fixed = True\n", encoding="utf-8")
        next_source = self.commit("fix: correct total")
        plan = release.plan_release(self.root, next_source, "HEAD")
        self.assertEqual(plan["version"], "0.1.1")
        self.assertEqual(plan["previous_tag"], "v0.1.0")

    def test_markdown_only_commits_are_a_no_op(self) -> None:
        source = self.commit("feat: initial product")
        self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        (self.root / "README.md").write_text("install rot\n", encoding="utf-8")
        (self.root / "GUIDE.MD").write_text("use rot\n", encoding="utf-8")
        docs_source = self.commit("docs: add installation guide")

        plan = release.plan_release(self.root, docs_source, "HEAD")

        self.assertEqual(
            plan,
            {
                "state": "markdown-only",
                "source": docs_source,
                "previous_tag": "v0.1.0",
                "commit_count": "0",
            },
        )

    def test_markdown_only_merge_is_a_no_op_from_release_boundary(self) -> None:
        source = self.commit("feat: initial product")
        self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.git("checkout", "-b", "docs")
        (self.root / "README.md").write_text("install rot\n", encoding="utf-8")
        self.commit("docs: add installation guide")
        self.git("checkout", "main")
        self.git("merge", "--no-ff", "docs", "-m", "docs: merge installation guide")
        merge_source = self.git("rev-parse", "HEAD")

        plan = release.plan_release(self.root, merge_source, "HEAD")

        self.assertEqual(plan["state"], "markdown-only")
        self.assertEqual(plan["commit_count"], "0")

    def test_release_automation_and_docs_are_release_neutral(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.tag_release(released, "0.1.0")
        (self.root / ".github/scripts").mkdir(parents=True)
        (self.root / ".github/scripts/release.py").write_text(
            "fixed = True\n", encoding="utf-8"
        )
        (self.root / "docs").mkdir()
        (self.root / "docs/releases.md").write_text(
            "release policy\n", encoding="utf-8"
        )
        final_source = self.commit("feat(release): improve automation")

        plan = release.plan_release(self.root, final_source, "HEAD")

        self.assertEqual(
            plan,
            {
                "state": "release-neutral",
                "source": final_source,
                "previous_tag": "v0.1.0",
                "commit_count": "0",
            },
        )

    def test_release_neutral_feature_does_not_promote_a_code_fix(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.tag_release(released, "0.1.0")
        (self.root / ".github/scripts").mkdir(parents=True)
        (self.root / ".github/scripts/release.py").write_text(
            "fixed = True\n", encoding="utf-8"
        )
        self.commit("feat(release): improve automation")
        (self.root / "src").mkdir()
        (self.root / "src/main.rs").write_text("fn main() {}\n", encoding="utf-8")
        final_source = self.commit("fix: correct output")

        plan = release.plan_release(self.root, final_source, "HEAD")

        self.assertEqual(plan["state"], "new")
        self.assertEqual(plan["version"], "0.1.1")
        self.assertEqual(plan["bump"], "patch")
        self.assertEqual(plan["commit_count"], "1")

    def test_test_only_commit_is_release_neutral(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.tag_release(released, "0.1.0")
        (self.root / "tests").mkdir()
        (self.root / "tests/cli.rs").write_text(
            "#[test]\nfn cli() {}\n", encoding="utf-8"
        )
        final_source = self.commit("test: cover CLI")

        plan = release.plan_release(self.root, final_source, "HEAD")

        self.assertEqual(plan["state"], "release-neutral")
        self.assertEqual(plan["commit_count"], "0")

    def test_release_gate_uses_net_diff_from_previous_release(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.tag_release(released, "0.1.0")
        (self.root / "README.md").write_text("install rot\n", encoding="utf-8")
        (self.root / "feature.py").write_text("enabled = True\n", encoding="utf-8")
        self.commit("feat: stage documented feature")
        (self.root / "feature.py").unlink()
        final_source = self.commit("revert: drop staged feature")

        plan = release.plan_release(self.root, final_source, "HEAD")

        self.assertEqual(plan["state"], "markdown-only")
        self.assertEqual(plan["commit_count"], "0")

    def test_reverted_code_is_unchanged_since_previous_release(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.tag_release(released, "0.1.0")
        (self.root / "feature.py").write_text("enabled = True\n", encoding="utf-8")
        self.commit("feat: stage feature")
        (self.root / "feature.py").unlink()
        final_source = self.commit("revert: drop staged feature")

        plan = release.plan_release(self.root, final_source, "HEAD")

        self.assertEqual(
            plan,
            {
                "state": "unchanged",
                "source": final_source,
                "previous_tag": "v0.1.0",
                "commit_count": "0",
            },
        )

    def test_generated_release_requires_a_net_release_change(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.tag_release(released, "0.1.0")
        (self.root / "README.md").write_text("install rot\n", encoding="utf-8")
        docs_source = self.commit("docs: add installation guide")
        self._write_version_files("0.1.1")
        generated = self.commit(
            "chore(release): v0.1.1",
            f"Release-Source: {docs_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )

        with self.assertRaisesRegex(
            release.ReleaseError, "no release-relevant changes"
        ):
            release.plan_release(self.root, generated, "HEAD")

    def test_markdown_feature_does_not_raise_a_code_fix_to_minor(self) -> None:
        source = self.commit("feat: initial product")
        self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        (self.root / "README.md").write_text("new feature docs\n", encoding="utf-8")
        self.commit("feat(docs): explain future work")
        (self.root / "release.py").write_text("fixed = True\n", encoding="utf-8")
        fix_source = self.commit("fix: repair release")

        plan = release.plan_release(self.root, fix_source, "HEAD")

        self.assertEqual(plan["version"], "0.1.1")
        self.assertEqual(plan["bump"], "patch")
        self.assertEqual(plan["commit_count"], "1")

        self._write_version_files("0.1.1")
        release_commit = self.commit(
            "chore(release): v0.1.1",
            f"Release-Source: {fix_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        recovered = release.plan_release(self.root, fix_source, "HEAD")
        self.assertEqual(recovered["state"], "pending")
        self.assertEqual(recovered["release_sha"], release_commit)

    def test_mixed_markdown_and_code_commit_is_release_relevant(self) -> None:
        source = self.commit("feat: initial product")
        self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        (self.root / "README.md").write_text("new feature docs\n", encoding="utf-8")
        (self.root / "feature.py").write_text("enabled = True\n", encoding="utf-8")
        mixed_source = self.commit("feat: add documented feature")

        plan = release.plan_release(self.root, mixed_source, "HEAD")

        self.assertEqual(plan["version"], "0.2.0")
        self.assertEqual(plan["bump"], "minor")
        self.assertEqual(plan["commit_count"], "1")

    def test_existing_release_is_recovered_by_source(self) -> None:
        source = self.commit("feat: initial product")
        release_commit = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        plan = release.plan_release(self.root, source, "HEAD")
        self.assertEqual(plan["state"], "pending")
        self.assertEqual(plan["release_sha"], release_commit)
        self.assertEqual(plan["previous_tag"], "")

    def test_manual_tag_does_not_change_planning(self) -> None:
        source = self.commit("feat: initial product")
        self.git("tag", "v99.99.99")
        plan = release.plan_release(self.root, source, "HEAD")
        self.assertEqual(plan["version"], "0.1.0")

    def test_release_boundary_tag_must_be_annotated(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.git("tag", "v0.1.0", released)
        docs_source = self.commit("docs: explain installation")

        with self.assertRaisesRegex(release.ReleaseError, "not annotated"):
            release.plan_release(self.root, docs_source, "HEAD")

    def test_release_boundary_tag_must_target_its_generated_commit(self) -> None:
        source = self.commit("feat: initial product")
        released = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        docs_source = self.commit("docs: explain installation")
        self.tag_release(docs_source, "0.1.0")

        with self.assertRaisesRegex(
            release.ReleaseError, f"not {released}"
        ):
            release.plan_release(self.root, docs_source, "HEAD")

    def test_recovered_release_keeps_generated_notes_baseline(self) -> None:
        first_source = self.commit("feat: initial product")
        self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {first_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        (self.root / "release.py").write_text("fixed = True\n", encoding="utf-8")
        second_source = self.commit("fix: correct total")
        self._write_version_files("0.1.1")
        self.git("add", ".")
        second_release = self.commit(
            "chore(release): v0.1.1",
            f"Release-Source: {second_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        plan = release.plan_release(self.root, second_release, "HEAD")
        self.assertEqual(plan["previous_tag"], "v0.1.0")

    def test_newer_generated_release_supersedes_an_older_retry(self) -> None:
        first_source = self.commit("feat: initial product")
        first_release = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {first_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        (self.root / "release.py").write_text("fixed = True\n", encoding="utf-8")
        second_source = self.commit("fix: repair packaging")
        self._write_version_files("0.1.1")
        second_release = self.commit(
            "chore(release): v0.1.1",
            f"Release-Source: {second_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )

        plan = release.plan_release(self.root, first_release, "HEAD")
        self.assertEqual(plan["state"], "superseded")
        self.assertEqual(plan["tag"], "v0.1.0")
        self.assertEqual(plan["superseded_by"], second_release)
        self.assertEqual(plan["superseded_by_tag"], "v0.1.1")

    def test_untagged_release_does_not_move_the_diff_boundary(self) -> None:
        first_source = self.commit("feat: initial product")
        first_release = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {first_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        self.tag_release(first_release, "0.1.0")
        (self.root / "release.py").write_text("fixed = True\n", encoding="utf-8")
        second_source = self.commit("fix: repair packaging")
        self._write_version_files("0.1.1")
        self.commit(
            "chore(release): v0.1.1",
            f"Release-Source: {second_source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        retry_source = self.commit("fix: retry publication")

        plan = release.plan_release(self.root, retry_source, "HEAD")

        self.assertEqual(plan["state"], "new")
        self.assertEqual(plan["version"], "0.1.2")
        self.assertEqual(plan["previous_tag"], "v0.1.1")
        self.assertEqual(plan["commit_count"], "1")

    def test_old_unreleased_source_is_stale(self) -> None:
        old_source = self.commit("fix: old")
        self.commit("fix: current")
        plan = release.plan_release(self.root, old_source, "HEAD")
        self.assertEqual(plan, {"state": "stale", "source": old_source})

    def test_release_trailer_must_name_its_parent(self) -> None:
        self.commit("feat: initial product")
        generated = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {'b' * 40}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        with self.assertRaisesRegex(release.ReleaseError, "not parent"):
            release.plan_release(self.root, generated, "HEAD")

    def test_release_commit_may_only_change_version_authorities(self) -> None:
        source = self.commit("feat: initial product")
        (self.root / "README.md").write_text("not a version file\n", encoding="utf-8")
        generated = self.commit(
            "chore(release): v0.1.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        with self.assertRaisesRegex(release.ReleaseError, "non-version paths"):
            release.plan_release(self.root, generated, "HEAD")

    def test_release_version_must_match_semantic_history(self) -> None:
        source = self.commit("feat: initial product")
        self._write_version_files("0.9.0")
        generated = self.commit(
            "chore(release): v0.9.0",
            f"Release-Source: {source}",
            f"Release-Automation: {release.AUTOMATION_MARKER}",
        )
        with self.assertRaisesRegex(release.ReleaseError, "policy requires 0.1.0"):
            release.plan_release(self.root, generated, "HEAD")


class MaterializationTests(RepositoryFixture):
    def test_set_version_updates_every_authority(self) -> None:
        release.set_version(self.root, release.Version.parse("0.2.0"))
        for path in (
            "Cargo.toml",
            "Cargo.lock",
            "crates/rot-compiler-protocol/Cargo.toml",
            "compiler/rot-rustc-driver/Cargo.toml",
            "compiler/rot-rustc-driver/Cargo.lock",
        ):
            content = (self.root / path).read_text(encoding="utf-8")
            self.assertIn('version = "0.2.0"', content, path)
            self.assertNotIn('version = "0.1.0"', content, path)
        self.assertIn('version = "=0.2.0"', (self.root / "Cargo.toml").read_text())

    def test_homebrew_formula_uses_exact_archives_and_hashes(self) -> None:
        intel = "1" * 64
        arm = "2" * 64
        formula = release.render_homebrew(
            "daulet/rot", release.Version.parse("1.2.3"), intel, arm
        )
        self.assertIn("rot-x86_64-apple-darwin.tar.gz", formula)
        self.assertIn("rot-aarch64-apple-darwin.tar.gz", formula)
        self.assertIn(f'sha256 "{intel}"', formula)
        self.assertIn(f'sha256 "{arm}"', formula)
        self.assertIn("depends_on :macos", formula)
        self.assertIn('bin.install "rot"', formula)
        self.assertNotRegex(formula, r"(?m)^\s*version ")
        self.assertEqual(
            release.homebrew_formula_release(formula),
            ("daulet/rot", release.Version.parse("1.2.3")),
        )

    def test_homebrew_formula_rejects_mixed_archive_versions(self) -> None:
        formula = self.root / "rot.rb"
        content = release.render_homebrew(
            "daulet/rot", release.Version.parse("1.2.3"), "1" * 64, "2" * 64
        ).replace(
            "v1.2.3/rot-aarch64-apple-darwin.tar.gz",
            "v1.2.4/rot-aarch64-apple-darwin.tar.gz",
        )
        formula.write_text(content, encoding="utf-8")
        with self.assertRaisesRegex(release.ReleaseError, "one GitHub release"):
            release.write_homebrew_formula(
                formula,
                "daulet/rot",
                release.Version.parse("1.2.3"),
                "1" * 64,
                "2" * 64,
            )

    def test_older_recovery_cannot_downgrade_homebrew(self) -> None:
        formula = self.root / "rot.rb"
        formula.write_text(
            release.render_homebrew(
                "daulet/rot", release.Version.parse("1.3.0"), "1" * 64, "2" * 64
            ),
            encoding="utf-8",
        )
        before = formula.read_text(encoding="utf-8")
        changed = release.write_homebrew_formula(
            formula,
            "daulet/rot",
            release.Version.parse("1.2.9"),
            "3" * 64,
            "4" * 64,
        )
        self.assertFalse(changed)
        self.assertEqual(formula.read_text(encoding="utf-8"), before)

    def test_archive_is_reproducible_and_has_normalized_metadata(self) -> None:
        for path in (
            "README.md",
            "LICENSE-APACHE",
            "docs/releases.md",
            "docs/rustc-backed-analysis.md",
        ):
            destination = self.root / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(f"contents of {path}\n", encoding="utf-8")
        binary = self.root / "built-rot"
        binary.write_bytes(b"native binary\n")
        binary.chmod(0o755)
        first = self.root / "first.tar.gz"
        second = self.root / "second.tar.gz"

        release.write_archive(self.root, binary, first, 1_700_000_000)
        os.utime(binary, (1_800_000_000, 1_800_000_000))
        release.write_archive(self.root, binary, second, 1_700_000_000)

        self.assertEqual(first.read_bytes(), second.read_bytes())
        with tarfile.open(first, "r:gz") as archive:
            self.assertEqual(
                archive.getnames(),
                [
                    "rot",
                    "README.md",
                    "LICENSE-APACHE",
                    "docs/releases.md",
                    "docs/rustc-backed-analysis.md",
                ],
            )
            self.assertEqual(archive.getmember("rot").mode, 0o755)
            self.assertEqual(archive.getmember("README.md").mode, 0o644)
            self.assertTrue(
                all(member.mtime == 1_700_000_000 for member in archive.getmembers())
            )

    def test_release_asset_manifest_rejects_missing_or_modified_bytes(self) -> None:
        version = release.Version.parse("1.2.3")
        directory = self.root / "dist"
        directory.mkdir()
        checksums = []
        for name in sorted(release.expected_release_assets(version)):
            contents = f"contents of {name}\n".encode()
            (directory / name).write_bytes(contents)
            checksums.append(f"{hashlib.sha256(contents).hexdigest()}  {name}\n")
        (directory / "SHA256SUMS").write_text("".join(checksums), encoding="utf-8")

        release.verify_release_assets(directory, version)
        (directory / "rot_1.2.3_amd64.deb").write_bytes(b"tampered\n")
        with self.assertRaisesRegex(release.ReleaseError, "checksum mismatch"):
            release.verify_release_assets(directory, version)

    def test_release_assets_must_match_canonical_bytes(self) -> None:
        version = release.Version.parse("1.2.3")
        canonical = self.root / "canonical"
        candidate = self.root / "candidate"
        canonical.mkdir()
        candidate.mkdir()
        checksums = []
        for name in sorted(release.expected_release_assets(version)):
            contents = f"contents of {name}\n".encode()
            (canonical / name).write_bytes(contents)
            (candidate / name).write_bytes(contents)
            checksums.append(f"{hashlib.sha256(contents).hexdigest()}  {name}\n")
        manifest = "".join(checksums)
        (canonical / "SHA256SUMS").write_text(manifest, encoding="utf-8")
        (candidate / "SHA256SUMS").write_text(manifest, encoding="utf-8")

        release.verify_identical_release_assets(canonical, candidate, version)
        (candidate / "SHA256SUMS").write_text(
            "".join(reversed(checksums)), encoding="utf-8"
        )
        with self.assertRaisesRegex(release.ReleaseError, "differs from canonical"):
            release.verify_identical_release_assets(canonical, candidate, version)


if __name__ == "__main__":
    unittest.main()
