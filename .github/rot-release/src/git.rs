use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use semver::Version;

pub(crate) struct Repository(PathBuf);

impl Repository {
    pub(crate) fn new(root: &Path) -> Self {
        Self(root.to_owned())
    }

    fn bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.0)
            .output()
            .context("could not run git")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            bail!("git {} failed: {detail}", args.join(" "));
        }
        Ok(output.stdout)
    }

    pub(crate) fn text(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8(self.bytes(args)?)
            .with_context(|| format!("git {} returned non-UTF-8 output", args.join(" ")))?
            .trim()
            .to_owned())
    }

    pub(crate) fn resolve(&self, revision: &str) -> Result<String> {
        let oid = self.text(&["rev-parse", "--verify", &format!("{revision}^{{commit}}")])?;
        ensure!(
            valid_oid(&oid),
            "git resolved {revision:?} to invalid object {oid:?}"
        );
        Ok(oid)
    }

    pub(crate) fn history(&self, revision: &str) -> Result<Vec<String>> {
        Ok(lines(&self.text(&[
            "rev-list",
            "--first-parent",
            revision,
        ])?))
    }

    pub(crate) fn range(&self, before: &str, after: &str) -> Result<Vec<String>> {
        Ok(lines(&self.text(&[
            "rev-list",
            "--first-parent",
            "--reverse",
            &format!("{before}..{after}"),
        ])?))
    }

    pub(crate) fn message(&self, commit: &str) -> Result<String> {
        self.text(&["show", "-s", "--format=%B", commit])
    }

    pub(crate) fn committer(&self, commit: &str) -> Result<String> {
        self.text(&["show", "-s", "--format=%ce", commit])
    }

    pub(crate) fn paths(&self, before: &str, after: &str) -> Result<Vec<String>> {
        self.nul_paths(&[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            before,
            after,
            "--",
        ])
    }

    pub(crate) fn commit_paths(&self, commit: &str) -> Result<Vec<String>> {
        let ancestry = self.text(&["rev-list", "--parents", "-n", "1", commit])?;
        let parents: Vec<_> = ancestry.split_whitespace().collect();
        ensure!(
            parents.first().copied() == Some(commit),
            "could not resolve ancestry for {commit}"
        );
        if parents.len() > 1 {
            self.paths(parents[1], commit)
        } else {
            self.nul_paths(&[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-only",
                "--no-renames",
                "-r",
                "-z",
                commit,
                "--",
            ])
        }
    }

    pub(crate) fn parent(&self, commit: &str) -> Result<String> {
        self.resolve(&format!("{commit}^"))
    }

    pub(crate) fn tag_target(&self, version: &Version) -> Result<Option<String>> {
        let tag = format!("v{version}");
        if self.text(&["tag", "--list", &tag])?.is_empty() {
            return Ok(None);
        }
        let reference = format!("refs/tags/{tag}");
        ensure!(
            self.text(&["cat-file", "-t", &reference])? == "tag",
            "release tag {tag} is not annotated"
        );
        self.resolve(&reference).map(Some)
    }

    pub(crate) fn file(&self, commit: &str, path: &str) -> Result<String> {
        self.text(&["show", &format!("{commit}:{path}")])
    }

    fn nul_paths(&self, args: &[&str]) -> Result<Vec<String>> {
        self.bytes(args)?
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.to_vec()).context("git returned a non-UTF-8 path"))
            .collect()
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

pub(crate) fn valid_oid(value: &str) -> bool {
    (40..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
