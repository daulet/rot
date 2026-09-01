#!/usr/bin/env python3
"""Plan and materialize Rot releases without treating tags as version input."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence


AUTOMATION_MARKER = "rot-v1"
RELEASE_COMMITTER = "41898282+github-actions[bot]@users.noreply.github.com"
VERSION_PATHS = frozenset(
    {
        "Cargo.lock",
        "Cargo.toml",
        "compiler/rot-rustc-driver/Cargo.lock",
        "compiler/rot-rustc-driver/Cargo.toml",
        "crates/rot-compiler-protocol/Cargo.toml",
    }
)
FEATURE_SUBJECT = re.compile(r"^feat(?:\([^()]+\))?!?:")
RELEASE_SUBJECT = re.compile(r"^chore\(release\): v(?P<version>\d+\.\d+\.\d+)$")
GIT_OID = re.compile(r"^[0-9a-f]{40,64}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
FORMULA_VERSION = re.compile(
    r'^\s*version "(?P<version>\d+\.\d+\.\d+)"\s*$', re.MULTILINE
)
FORMULA_HOMEPAGE = re.compile(r'^\s*homepage "(?P<homepage>[^"]+)"\s*$', re.MULTILINE)


class ReleaseError(RuntimeError):
    """A release invariant was violated."""


@dataclass(frozen=True, order=True)
class Version:
    major: int
    minor: int
    patch: int

    @classmethod
    def parse(cls, raw: str) -> "Version":
        match = re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", raw)
        if match is None:
            raise ReleaseError(f"expected MAJOR.MINOR.PATCH, got {raw!r}")
        return cls(*(int(part) for part in match.groups()))

    def bump(self, kind: str) -> "Version":
        if kind == "minor":
            return Version(self.major, self.minor + 1, 0)
        if kind == "patch":
            return Version(self.major, self.minor, self.patch + 1)
        raise ReleaseError(f"unknown bump kind {kind!r}")

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}.{self.patch}"


@dataclass(frozen=True)
class GeneratedRelease:
    version: Version
    source: str


def bump_kind(subjects: Iterable[str]) -> str:
    subjects = tuple(subject for subject in subjects if subject.strip())
    if not subjects:
        raise ReleaseError("no unreleased commit subjects found")
    return (
        "minor"
        if any(FEATURE_SUBJECT.match(subject) for subject in subjects)
        else "patch"
    )


def generated_release(message: str) -> GeneratedRelease | None:
    lines = message.splitlines()
    if not lines:
        return None
    subject = RELEASE_SUBJECT.fullmatch(lines[0])
    if subject is None:
        return None

    trailers: dict[str, str] = {}
    for line in lines[1:]:
        key, separator, value = line.partition(":")
        if separator:
            trailers[key.strip()] = value.strip()

    if trailers.get("Release-Automation") != AUTOMATION_MARKER:
        return None
    source = trailers.get("Release-Source", "")
    if GIT_OID.fullmatch(source) is None:
        raise ReleaseError(
            f"generated release has an invalid source object: {source!r}"
        )
    return GeneratedRelease(Version.parse(subject.group("version")), source)


def run_git(root: Path, *arguments: str) -> str:
    process = subprocess.run(
        ["git", *arguments],
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if process.returncode != 0:
        detail = process.stderr.strip() or process.stdout.strip()
        raise ReleaseError(f"git {' '.join(arguments)} failed: {detail}")
    return process.stdout.strip()


def resolve_commit(root: Path, revision: str) -> str:
    resolved = run_git(root, "rev-parse", f"{revision}^{{commit}}")
    if GIT_OID.fullmatch(resolved) is None:
        raise ReleaseError(f"git resolved {revision!r} to invalid object {resolved!r}")
    return resolved


def first_parent_commits(root: Path, revision: str) -> list[str]:
    output = run_git(root, "rev-list", "--first-parent", revision)
    return output.splitlines() if output else []


def commit_message(root: Path, commit: str) -> str:
    return run_git(root, "show", "-s", "--format=%B", commit)


def commit_subject(root: Path, commit: str) -> str:
    return run_git(root, "show", "-s", "--format=%s", commit)


def package_version(manifest: Path, expected_name: str) -> Version:
    with manifest.open("rb") as source:
        package = tomllib.load(source).get("package", {})
    if package.get("name") != expected_name:
        raise ReleaseError(
            f"{manifest} package is {package.get('name')!r}, expected {expected_name!r}"
        )
    raw_version = package.get("version")
    if not isinstance(raw_version, str):
        raise ReleaseError(f"{manifest} has no literal package version")
    return Version.parse(raw_version)


def package_version_from_text(
    content: str, expected_name: str, description: str
) -> Version:
    package = tomllib.loads(content).get("package", {})
    if package.get("name") != expected_name:
        raise ReleaseError(
            f"{description} package is {package.get('name')!r}, expected {expected_name!r}"
        )
    raw_version = package.get("version")
    if not isinstance(raw_version, str):
        raise ReleaseError(f"{description} has no literal package version")
    return Version.parse(raw_version)


def file_at_commit(root: Path, commit: str, path: str) -> str:
    return run_git(root, "show", f"{commit}:{path}")


def validate_generated_commit(
    root: Path, commit: str, generated: GeneratedRelease
) -> None:
    parent = resolve_commit(root, f"{commit}^")
    if parent != generated.source:
        raise ReleaseError(
            f"generated release {commit} records source {generated.source}, not parent {parent}"
        )

    source_history = first_parent_commits(root, generated.source)
    previous_commit: str | None = None
    baseline = Version(0, 0, 0)
    for candidate in source_history:
        previous = generated_release(commit_message(root, candidate))
        if previous is not None:
            previous_commit = candidate
            baseline = previous.version
            break
    if previous_commit is None:
        unreleased = list(reversed(source_history))
    else:
        output = run_git(
            root,
            "rev-list",
            "--first-parent",
            "--reverse",
            f"{previous_commit}..{generated.source}",
        )
        unreleased = output.splitlines() if output else []
    subjects = [
        commit_subject(root, candidate)
        for candidate in unreleased
        if generated_release(commit_message(root, candidate)) is None
    ]
    expected_version = baseline.bump(bump_kind(subjects))
    if generated.version != expected_version:
        raise ReleaseError(
            f"generated release {commit} has version {generated.version}, "
            f"but commit policy requires {expected_version}"
        )

    committer = run_git(root, "show", "-s", "--format=%ce", commit)
    if committer != RELEASE_COMMITTER:
        raise ReleaseError(
            f"generated release {commit} has unexpected committer {committer!r}"
        )

    changed_output = run_git(
        root, "diff-tree", "--no-commit-id", "--name-only", "-r", parent, commit
    )
    changed = frozenset(changed_output.splitlines()) if changed_output else frozenset()
    unexpected = changed - VERSION_PATHS
    if unexpected:
        raise ReleaseError(
            f"generated release {commit} changes non-version paths: {sorted(unexpected)}"
        )

    manifests = (
        ("Cargo.toml", "rot-metrics"),
        ("crates/rot-compiler-protocol/Cargo.toml", "rot-compiler-protocol"),
        ("compiler/rot-rustc-driver/Cargo.toml", "rot-rustc-driver"),
    )
    for path, package in manifests:
        version = package_version_from_text(
            file_at_commit(root, commit, path), package, f"{commit}:{path}"
        )
        if version != generated.version:
            raise ReleaseError(
                f"generated release {commit} says {generated.version}, but {path} says {version}"
            )

    for path in ("Cargo.toml", "compiler/rot-rustc-driver/Cargo.toml"):
        dependency = tomllib.loads(file_at_commit(root, commit, path))["dependencies"][
            "rot-compiler-protocol"
        ]
        if dependency.get("version") != f"={generated.version}":
            raise ReleaseError(
                f"generated release {commit} has a non-exact protocol version in {path}"
            )

    locks = (
        ("Cargo.lock", ("rot-metrics", "rot-compiler-protocol")),
        (
            "compiler/rot-rustc-driver/Cargo.lock",
            ("rot-rustc-driver", "rot-compiler-protocol"),
        ),
    )
    for path, expected_packages in locks:
        packages = tomllib.loads(file_at_commit(root, commit, path)).get("package", [])
        versions = {
            package["name"]: package["version"]
            for package in packages
            if package.get("name") in expected_packages
        }
        expected = {package: str(generated.version) for package in expected_packages}
        if versions != expected:
            raise ReleaseError(
                f"generated release {commit} has inconsistent versions in {path}: {versions}"
            )


def current_version(root: Path) -> Version:
    versions = {
        package_version(root / "Cargo.toml", "rot-metrics"),
        package_version(
            root / "crates/rot-compiler-protocol/Cargo.toml",
            "rot-compiler-protocol",
        ),
        package_version(
            root / "compiler/rot-rustc-driver/Cargo.toml", "rot-rustc-driver"
        ),
    }
    if len(versions) != 1:
        rendered = ", ".join(str(version) for version in sorted(versions))
        raise ReleaseError(f"workspace package versions disagree: {rendered}")
    return versions.pop()


def plan_release(root: Path, source_revision: str, remote_ref: str) -> dict[str, str]:
    source = resolve_commit(root, source_revision)
    remote_tip = resolve_commit(root, remote_ref)
    remote_history = first_parent_commits(root, remote_tip)

    newer_release: tuple[str, GeneratedRelease] | None = None
    for index, commit in enumerate(remote_history):
        release = generated_release(commit_message(root, commit))
        if release is not None:
            validate_generated_commit(root, commit, release)
        if release is not None and (commit == source or release.source == source):
            if newer_release is not None:
                newer_commit, newer = newer_release
                return {
                    "state": "superseded",
                    "source": release.source,
                    "release_sha": commit,
                    "tag": f"v{release.version}",
                    "superseded_by": newer_commit,
                    "superseded_by_tag": f"v{newer.version}",
                }
            previous_tag = ""
            for older_commit in remote_history[index + 1 :]:
                older_release = generated_release(commit_message(root, older_commit))
                if older_release is not None:
                    previous_tag = f"v{older_release.version}"
                    break
            return {
                "state": "pending",
                "source": release.source,
                "release_sha": commit,
                "previous_tag": previous_tag,
                "version": str(release.version),
                "tag": f"v{release.version}",
            }
        if release is not None and newer_release is None:
            newer_release = (commit, release)

    if remote_tip != source:
        return {"state": "stale", "source": source}

    source_history = first_parent_commits(root, source)
    previous_commit: str | None = None
    previous_release: GeneratedRelease | None = None
    for commit in source_history:
        release = generated_release(commit_message(root, commit))
        if release is not None:
            validate_generated_commit(root, commit, release)
            previous_commit = commit
            previous_release = release
            break

    manifest_version = current_version(root)
    if previous_release is None:
        baseline = Version(0, 0, 0)
        unreleased = list(reversed(source_history))
    else:
        baseline = previous_release.version
        if manifest_version != baseline:
            raise ReleaseError(
                "manifest version drifted from the last generated release: "
                f"manifest={manifest_version}, release={baseline}"
            )
        assert previous_commit is not None
        range_output = run_git(
            root,
            "rev-list",
            "--first-parent",
            "--reverse",
            f"{previous_commit}..{source}",
        )
        unreleased = range_output.splitlines() if range_output else []

    subjects = []
    for commit in unreleased:
        message = commit_message(root, commit)
        if generated_release(message) is None:
            subjects.append(commit_subject(root, commit))

    kind = bump_kind(subjects)
    version = baseline.bump(kind)
    if previous_release is None and manifest_version != version:
        raise ReleaseError(
            "the first computed version must match the checked-in bootstrap version: "
            f"manifest={manifest_version}, computed={version}"
        )

    return {
        "state": "new",
        "source": source,
        "previous_tag": "" if previous_release is None else f"v{baseline}",
        "bump": kind,
        "version": str(version),
        "tag": f"v{version}",
        "commit_count": str(len(subjects)),
    }


def atomic_write(path: Path, content: str) -> None:
    mode = path.stat().st_mode if path.exists() else 0o644
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, delete=False
    ) as temporary:
        temporary.write(content)
        temporary_path = Path(temporary.name)
    os.chmod(temporary_path, mode)
    temporary_path.replace(path)


def replace_package_version(
    path: Path, expected_name: str, old: Version, new: Version
) -> None:
    if package_version(path, expected_name) != old:
        raise ReleaseError(f"{path} is not at expected version {old}")
    content = path.read_text(encoding="utf-8")
    pattern = re.compile(
        rf"(\[package\][\s\S]*?^name = \"{re.escape(expected_name)}\"$"
        rf"[\s\S]*?^version = \"){re.escape(str(old))}(\"$)",
        re.MULTILINE,
    )
    updated, count = pattern.subn(rf"\g<1>{new}\2", content, count=1)
    if count != 1:
        raise ReleaseError(f"could not update package version in {path}")
    atomic_write(path, updated)


def replace_dependency_version(path: Path, old: Version, new: Version) -> None:
    content = path.read_text(encoding="utf-8")
    pattern = re.compile(
        r'^(rot-compiler-protocol\s*=\s*\{[^\n]*\bversion\s*=\s*")='
        + re.escape(str(old))
        + r'("[^\n]*\})$',
        re.MULTILINE,
    )
    updated, count = pattern.subn(rf"\g<1>={new}\2", content, count=1)
    if count != 1:
        raise ReleaseError(f"could not update exact protocol dependency in {path}")
    atomic_write(path, updated)


def replace_lock_version(path: Path, package: str, old: Version, new: Version) -> None:
    content = path.read_text(encoding="utf-8")
    pattern = re.compile(
        rf'(^\[\[package\]\]\nname = "{re.escape(package)}"\nversion = ")'
        + re.escape(str(old))
        + r'("$)',
        re.MULTILINE,
    )
    updated, count = pattern.subn(rf"\g<1>{new}\2", content, count=1)
    if count != 1:
        raise ReleaseError(f"could not update {package} in {path}")
    atomic_write(path, updated)


def set_version(root: Path, new: Version) -> None:
    old = current_version(root)
    if old == new:
        return

    root_manifest = root / "Cargo.toml"
    protocol_manifest = root / "crates/rot-compiler-protocol/Cargo.toml"
    driver_manifest = root / "compiler/rot-rustc-driver/Cargo.toml"
    replace_package_version(root_manifest, "rot-metrics", old, new)
    replace_package_version(protocol_manifest, "rot-compiler-protocol", old, new)
    replace_package_version(driver_manifest, "rot-rustc-driver", old, new)
    replace_dependency_version(root_manifest, old, new)
    replace_dependency_version(driver_manifest, old, new)

    replace_lock_version(root / "Cargo.lock", "rot-metrics", old, new)
    replace_lock_version(root / "Cargo.lock", "rot-compiler-protocol", old, new)
    driver_lock = root / "compiler/rot-rustc-driver/Cargo.lock"
    replace_lock_version(driver_lock, "rot-rustc-driver", old, new)
    replace_lock_version(driver_lock, "rot-compiler-protocol", old, new)


def render_homebrew(
    repository: str, version: Version, intel_sha: str, arm_sha: str
) -> str:
    if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None:
        raise ReleaseError(f"invalid GitHub repository {repository!r}")
    for name, digest in (("Intel", intel_sha), ("Apple Silicon", arm_sha)):
        if SHA256.fullmatch(digest) is None:
            raise ReleaseError(f"invalid {name} SHA-256 digest {digest!r}")

    base = f"https://github.com/{repository}/releases/download/v{version}"
    return f'''class Rot < Formula
  desc "Fast, configuration-aware Rust source metrics"
  homepage "https://github.com/{repository}"
  version "{version}"
  license "Apache-2.0"
  depends_on :macos

  on_macos do
    if Hardware::CPU.intel?
      url "{base}/rot-x86_64-apple-darwin.tar.gz"
      sha256 "{intel_sha}"
    else
      url "{base}/rot-aarch64-apple-darwin.tar.gz"
      sha256 "{arm_sha}"
    end
  end

  def install
    bin.install "rot"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/rot --version")
  end
end
'''


def write_homebrew_formula(
    output: Path,
    repository: str,
    version: Version,
    intel_sha: str,
    arm_sha: str,
) -> bool:
    if output.exists():
        existing = output.read_text(encoding="utf-8")
        version_match = FORMULA_VERSION.search(existing)
        homepage_match = FORMULA_HOMEPAGE.search(existing)
        if version_match is None or homepage_match is None:
            raise ReleaseError(
                f"existing formula is not a recognizable Rot formula: {output}"
            )
        existing_version = Version.parse(version_match.group("version"))
        expected_homepage = f"https://github.com/{repository}"
        if existing_version > version:
            if homepage_match.group("homepage") != expected_homepage:
                raise ReleaseError(
                    f"newer formula at {output} belongs to another project"
                )
            return False

    content = render_homebrew(repository, version, intel_sha, arm_sha)
    output.parent.mkdir(parents=True, exist_ok=True)
    atomic_write(output, content)
    return True


def write_archive(
    root: Path, binary: Path, output: Path, source_date_epoch: int
) -> None:
    if source_date_epoch < 0:
        raise ReleaseError("source date epoch cannot be negative")
    if not binary.is_file():
        raise ReleaseError(f"release binary does not exist: {binary}")

    members = (
        (binary, "rot", 0o755),
        (root / "README.md", "README.md", 0o644),
        (root / "LICENSE-APACHE", "LICENSE-APACHE", 0o644),
        (root / "docs/releases.md", "docs/releases.md", 0o644),
        (
            root / "docs/rustc-backed-analysis.md",
            "docs/rustc-backed-analysis.md",
            0o644,
        ),
    )
    missing = [str(path) for path, _, _ in members if not path.is_file()]
    if missing:
        raise ReleaseError(f"archive inputs do not exist: {missing}")

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=output.parent, delete=False) as raw:
        temporary = Path(raw.name)
        with gzip.GzipFile(
            filename="", mode="wb", fileobj=raw, mtime=source_date_epoch
        ) as compressed:
            with tarfile.open(
                fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT
            ) as archive:
                for source, archive_name, mode in members:
                    info = archive.gettarinfo(str(source), arcname=archive_name)
                    info.uid = 0
                    info.gid = 0
                    info.uname = "root"
                    info.gname = "root"
                    info.mode = mode
                    info.mtime = source_date_epoch
                    with source.open("rb") as contents:
                        archive.addfile(info, contents)
    os.chmod(temporary, 0o644)
    temporary.replace(output)


def expected_release_assets(version: Version) -> frozenset[str]:
    return frozenset(
        {
            "rot-aarch64-apple-darwin.tar.gz",
            "rot-aarch64-unknown-linux-musl.tar.gz",
            "rot-x86_64-apple-darwin.tar.gz",
            "rot-x86_64-unknown-linux-musl.tar.gz",
            f"rot_{version}_amd64.deb",
            f"rot_{version}_arm64.deb",
        }
    )


def verify_release_assets(directory: Path, version: Version) -> None:
    if not directory.is_dir():
        raise ReleaseError(f"release asset directory does not exist: {directory}")
    expected_artifacts = expected_release_assets(version)
    expected_files = expected_artifacts | {"SHA256SUMS"}
    actual_files = frozenset(
        entry.name for entry in directory.iterdir() if entry.is_file()
    )
    if actual_files != expected_files:
        raise ReleaseError(
            "release asset names disagree: "
            f"expected={sorted(expected_files)}, actual={sorted(actual_files)}"
        )

    checksum_pattern = re.compile(r"([0-9a-f]{64})  ([^/]+)")
    checksums: dict[str, str] = {}
    for line in (directory / "SHA256SUMS").read_text(encoding="utf-8").splitlines():
        match = checksum_pattern.fullmatch(line)
        if match is None or match.group(2) in checksums:
            raise ReleaseError(f"invalid SHA256SUMS line: {line!r}")
        digest, name = match.groups()
        checksums[name] = digest
    if frozenset(checksums) != expected_artifacts:
        raise ReleaseError(
            "SHA256SUMS entries disagree: "
            f"expected={sorted(expected_artifacts)}, actual={sorted(checksums)}"
        )
    for name, expected_digest in checksums.items():
        actual_digest = hashlib.sha256((directory / name).read_bytes()).hexdigest()
        if actual_digest != expected_digest:
            raise ReleaseError(f"release asset checksum mismatch: {name}")


def verify_identical_release_assets(
    canonical: Path, candidate: Path, version: Version
) -> None:
    verify_release_assets(canonical, version)
    verify_release_assets(candidate, version)
    for name in expected_release_assets(version) | {"SHA256SUMS"}:
        if (canonical / name).read_bytes() != (candidate / name).read_bytes():
            raise ReleaseError(f"release asset differs from canonical build: {name}")


def emit_plan(plan: dict[str, str], github_output: Path | None) -> None:
    if github_output is None:
        print(json.dumps(plan, sort_keys=True))
        return
    with github_output.open("a", encoding="utf-8") as destination:
        for key, value in plan.items():
            if "\n" in value or "\r" in value:
                raise ReleaseError(f"output {key!r} contains a newline")
            destination.write(f"{key}={value}\n")


def parse_arguments(arguments: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    plan = subparsers.add_parser("plan", help="plan or recover a release")
    plan.add_argument("--root", type=Path, default=Path.cwd())
    plan.add_argument("--source", required=True)
    plan.add_argument("--remote-ref", default="origin/main")
    plan.add_argument("--github-output", type=Path)

    set_version_parser = subparsers.add_parser(
        "set-version", help="synchronize publishable package and lock versions"
    )
    set_version_parser.add_argument("version", type=Version.parse)
    set_version_parser.add_argument("--root", type=Path, default=Path.cwd())

    homebrew = subparsers.add_parser("homebrew", help="render Formula/rot.rb")
    homebrew.add_argument("--repository", required=True)
    homebrew.add_argument("--version", required=True, type=Version.parse)
    homebrew.add_argument("--intel-sha", required=True)
    homebrew.add_argument("--arm-sha", required=True)
    homebrew.add_argument("--output", required=True, type=Path)

    archive = subparsers.add_parser(
        "archive", help="build a deterministic native release archive"
    )
    archive.add_argument("--root", type=Path, default=Path.cwd())
    archive.add_argument("--binary", required=True, type=Path)
    archive.add_argument("--output", required=True, type=Path)
    archive.add_argument("--source-date-epoch", required=True, type=int)

    verify_assets = subparsers.add_parser(
        "verify-assets", help="verify the exact native release asset set"
    )
    verify_assets.add_argument("--directory", required=True, type=Path)
    verify_assets.add_argument("--version", required=True, type=Version.parse)

    verify_identical = subparsers.add_parser(
        "verify-identical-assets",
        help="compare a release asset set with the canonical workflow build",
    )
    verify_identical.add_argument("--canonical", required=True, type=Path)
    verify_identical.add_argument("--candidate", required=True, type=Path)
    verify_identical.add_argument("--version", required=True, type=Version.parse)
    return parser.parse_args(arguments)


def main(arguments: Sequence[str] = sys.argv[1:]) -> int:
    try:
        options = parse_arguments(arguments)
        if options.command == "plan":
            plan = plan_release(
                options.root.resolve(), options.source, options.remote_ref
            )
            emit_plan(plan, options.github_output)
        elif options.command == "set-version":
            set_version(options.root.resolve(), options.version)
        elif options.command == "homebrew":
            write_homebrew_formula(
                options.output,
                options.repository,
                options.version,
                options.intel_sha,
                options.arm_sha,
            )
        elif options.command == "archive":
            write_archive(
                options.root.resolve(),
                options.binary.resolve(),
                options.output.resolve(),
                options.source_date_epoch,
            )
        elif options.command == "verify-assets":
            verify_release_assets(options.directory.resolve(), options.version)
        elif options.command == "verify-identical-assets":
            verify_identical_release_assets(
                options.canonical.resolve(),
                options.candidate.resolve(),
                options.version,
            )
        else:
            raise AssertionError(f"unhandled command {options.command}")
    except ReleaseError as error:
        print(f"release: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
