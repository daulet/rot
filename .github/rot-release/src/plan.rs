use std::path::Path;

use anyhow::{Result, bail, ensure};
use semver::Version;

use crate::git::{Repository, valid_oid};
use crate::version::{Bump, feature_subject, next, version};
use crate::workspace::{current_version, validate_materialized_release};
use crate::{Outputs, outputs};

const MARKER: &str = "rot-v1";
const RELEASE_COMMITTER: &str = "41898282+github-actions[bot]@users.noreply.github.com";
const VERSION_PATHS: [&str; 3] = [
    "Cargo.lock",
    "Cargo.toml",
    "compiler/rot-rustc-driver/Cargo.lock",
];
const NEUTRAL_ROOTS: [&str; 2] = [".github", "docs"];
const NEUTRAL_COMPONENTS: [&str; 3] = ["benches", "examples", "tests"];
const NEUTRAL_FILES: [&str; 8] = [
    ".editorconfig",
    ".gitattributes",
    ".gitignore",
    ".rustfmt.toml",
    "clippy.toml",
    "license-apache",
    "license-mit",
    "rustfmt.toml",
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct GeneratedRelease {
    version: Version,
    source: String,
}

struct ReleaseRange {
    history: Vec<String>,
    previous: Option<(String, GeneratedRelease)>,
    commits: Vec<String>,
}

pub fn plan_release(root: &Path, source_revision: &str, remote_ref: &str) -> Result<Outputs> {
    let repo = Repository::new(root);
    let source = repo.resolve(source_revision)?;
    let remote_tip = repo.resolve(remote_ref)?;
    let remote_history = repo.history(&remote_tip)?;

    let mut newer: Option<(String, GeneratedRelease)> = None;
    for (index, commit) in remote_history.iter().enumerate() {
        let Some(release) = generated_release(&repo.message(commit)?)? else {
            continue;
        };
        validate_generated(&repo, commit, &release)?;
        if commit == &source || release.source == source {
            if let Some((newer_commit, newer_release)) = newer {
                return Ok(outputs([
                    ("state", "superseded"),
                    ("source", &release.source),
                    ("release_sha", commit),
                    ("tag", &format!("v{}", release.version)),
                    ("superseded_by", &newer_commit),
                    ("superseded_by_tag", &format!("v{}", newer_release.version)),
                ]));
            }
            let previous_tag = previous_tag(&repo, &remote_history[index + 1..])?;
            return Ok(outputs([
                ("state", "pending"),
                ("source", &release.source),
                ("release_sha", commit),
                ("previous_tag", &previous_tag),
                ("version", &release.version.to_string()),
                ("tag", &format!("v{}", release.version)),
            ]));
        }
        newer.get_or_insert_with(|| (commit.clone(), release));
    }

    if remote_tip != source {
        return Ok(outputs([("state", "stale"), ("source", &source)]));
    }

    let range = release_range(&repo, &source)?;
    if let Some((commit, release)) = &range.previous {
        validate_generated(&repo, commit, release)?;
    }
    let checked_in = current_version(root)?;
    let (baseline, previous_tag) = match &range.previous {
        Some((_, release)) => {
            ensure!(
                checked_in == release.version,
                "manifest version drifted from the last generated release: manifest={checked_in}, release={}",
                release.version
            );
            (release.version.clone(), format!("v{}", release.version))
        }
        None => (Version::new(0, 0, 0), String::new()),
    };

    if let Some(boundary) = latest_tagged(&repo, &range.history)?
        && let Some(state) = no_release_state(&repo, &boundary, &source)?
    {
        return Ok(no_release(state, &source, &previous_tag));
    }
    let Some((bump, count)) = release_bump(&repo, &range.commits)? else {
        return Ok(no_release("markdown-only", &source, &previous_tag));
    };
    let first = range.previous.is_none();
    let release = next(baseline, bump)?;
    ensure!(
        !first || checked_in == release,
        "the first computed version must match the checked-in bootstrap version: manifest={checked_in}, computed={release}"
    );
    Ok(outputs([
        ("state", "new"),
        ("source", &source),
        ("previous_tag", &previous_tag),
        ("bump", bump.name()),
        ("version", &release.to_string()),
        ("tag", &format!("v{release}")),
        ("commit_count", &count.to_string()),
    ]))
}

fn release_range(repo: &Repository, source: &str) -> Result<ReleaseRange> {
    let history = repo.history(source)?;
    let mut previous = None;
    for commit in &history {
        if let Some(release) = generated_release(&repo.message(commit)?)? {
            previous = Some((commit.clone(), release));
            break;
        }
    }
    let commits = if let Some((commit, _)) = &previous {
        repo.range(commit, source)?
    } else {
        history.iter().rev().cloned().collect()
    };
    Ok(ReleaseRange {
        history,
        previous,
        commits,
    })
}

fn validate_generated(repo: &Repository, commit: &str, release: &GeneratedRelease) -> Result<()> {
    let parent = repo.parent(commit)?;
    ensure!(
        parent == release.source,
        "generated release {commit} records source {}, not parent {parent}",
        release.source
    );
    let range = release_range(repo, &release.source)?;
    if range.previous.is_some()
        && let Some(boundary) = latest_tagged(repo, &range.history)?
        && let Some(state) = no_release_state(repo, &boundary, &release.source)?
    {
        bail!(
            "generated release {commit} has no release-relevant changes since {boundary}: {state}"
        );
    }
    let Some((bump, _)) = release_bump(repo, &range.commits)? else {
        bail!("no unreleased commit subjects found");
    };
    let baseline = range
        .previous
        .map_or_else(|| Version::new(0, 0, 0), |(_, release)| release.version);
    let expected = next(baseline, bump)?;
    ensure!(
        release.version == expected,
        "generated release {commit} has version {}, but commit policy requires {expected}",
        release.version
    );
    let committer = repo.committer(commit)?;
    ensure!(
        committer == RELEASE_COMMITTER,
        "generated release {commit} has unexpected committer {committer:?}"
    );
    validate_materialized_release(repo, commit, &release.version, &VERSION_PATHS)?;
    Ok(())
}

fn generated_release(message: &str) -> Result<Option<GeneratedRelease>> {
    let Some(raw) = message
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("chore(release): v"))
    else {
        return Ok(None);
    };
    if !numeric_version(raw) {
        return Ok(None);
    }
    let release_version = version(raw)?;
    let (mut source, mut marker) = ("", "");
    for line in message.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "Release-Source" => source = value.trim(),
                "Release-Automation" => marker = value.trim(),
                _ => {}
            }
        }
    }
    if marker != MARKER {
        return Ok(None);
    }
    ensure!(
        valid_oid(source),
        "generated release has an invalid source object: {source:?}"
    );
    Ok(Some(GeneratedRelease {
        version: release_version,
        source: source.to_owned(),
    }))
}

fn numeric_version(raw: &str) -> bool {
    let mut parts = raw.split('.');
    (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    }) && parts.next().is_none()
}

fn previous_tag(repo: &Repository, commits: &[String]) -> Result<String> {
    for commit in commits {
        if let Some(release) = generated_release(&repo.message(commit)?)? {
            return Ok(format!("v{}", release.version));
        }
    }
    Ok(String::new())
}

fn latest_tagged(repo: &Repository, commits: &[String]) -> Result<Option<String>> {
    for commit in commits {
        let Some(release) = generated_release(&repo.message(commit)?)? else {
            continue;
        };
        let Some(target) = repo.tag_target(&release.version)? else {
            continue;
        };
        ensure!(
            target == *commit,
            "release tag v{} points to {target}, not {commit}",
            release.version
        );
        return Ok(Some(commit.clone()));
    }
    Ok(None)
}

fn release_bump(repo: &Repository, commits: &[String]) -> Result<Option<(Bump, usize)>> {
    let (mut count, mut bump) = (0, Bump::Patch);
    for commit in commits {
        let message = repo.message(commit)?;
        if generated_release(&message)?.is_some() || neutral_commit(repo, commit)? {
            continue;
        }
        count += 1;
        if feature_subject(message.lines().next().unwrap_or_default()) {
            bump = Bump::Minor;
        }
    }
    Ok((count != 0).then_some((bump, count)))
}

fn neutral_commit(repo: &Repository, commit: &str) -> Result<bool> {
    let paths = repo.commit_paths(commit)?;
    Ok(!paths.is_empty() && paths.iter().all(|path| neutral_path(path)))
}

fn no_release_state(
    repo: &Repository,
    baseline: &str,
    source: &str,
) -> Result<Option<&'static str>> {
    let paths = repo.paths(baseline, source)?;
    Ok(if paths.is_empty() {
        Some("unchanged")
    } else if paths.iter().all(|path| markdown(path)) {
        Some("markdown-only")
    } else if paths.iter().all(|path| neutral_path(path)) {
        Some("release-neutral")
    } else {
        None
    })
}

fn markdown(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[bytes.len() - 3] == b'.'
        && bytes[bytes.len() - 2].eq_ignore_ascii_case(&b'm')
        && bytes[bytes.len() - 1].eq_ignore_ascii_case(&b'd')
}

fn neutral_path(path: &str) -> bool {
    let matches = |value: &str, choices: &[&str]| {
        choices
            .iter()
            .any(|choice| value.eq_ignore_ascii_case(choice))
    };
    markdown(path)
        || path
            .split('/')
            .next()
            .is_some_and(|root| matches(root, &NEUTRAL_ROOTS))
        || path
            .split('/')
            .any(|part| matches(part, &NEUTRAL_COMPONENTS))
        || path
            .rsplit('/')
            .next()
            .is_some_and(|file| matches(file, &NEUTRAL_FILES))
}

fn no_release(state: &str, source: &str, previous_tag: &str) -> Outputs {
    outputs([
        ("state", state),
        ("source", source),
        ("previous_tag", previous_tag),
        ("commit_count", "0"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_paths_ignore_ascii_case() {
        for path in [
            ".GITHUB/workflows/x.yml",
            "Docs/x.txt",
            "src/TESTS/x.rs",
            "Examples/x.rs",
            "README.MD",
            "LICENSE-APACHE",
        ] {
            assert!(neutral_path(path), "{path}");
        }
        assert!(!neutral_path("src/lib.rs"));
    }

    #[test]
    fn marker_and_last_trailer_are_semantic() -> Result<()> {
        let first = "a".repeat(40);
        let last = "b".repeat(40);
        let message = format!(
            "chore(release): v1.2.3\nRelease-Source: {first}\nRelease-Source: {last}\nRelease-Automation: rot-v1"
        );
        assert_eq!(generated_release(&message)?.unwrap().source, last);
        assert!(generated_release("chore(release): v1.2.3")?.is_none());
        Ok(())
    }
}
