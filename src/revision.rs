use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[cfg(any(feature = "audit", test))]
use std::{
    fs::File,
    io::{self, Write},
    path::Component,
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;

use crate::{
    model::{SelectedPathKind, SelectedPathReport, SelectionReport},
    paths::{containing_directory, portable},
};

pub(crate) fn validate_baseline_ref(revision: &str) -> Result<()> {
    if revision.contains("..") {
        bail!(
            "--baseline accepts one commit, not a revision range; compare the live working tree with a ref such as HEAD~1"
        );
    }
    Ok(())
}

pub(crate) struct Repository {
    root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(any(feature = "audit", test))]
pub(crate) struct WorkingState {
    commit: String,
    porcelain: Vec<u8>,
    fingerprint: String,
}

#[cfg(any(feature = "audit", test))]
impl WorkingState {
    pub(crate) fn commit(&self) -> &str {
        &self.commit
    }

    pub(crate) fn dirty(&self) -> bool {
        !self.porcelain.is_empty()
    }
}

impl Repository {
    pub(crate) fn discover(paths: &[PathBuf]) -> Result<Self> {
        let first = paths
            .first()
            .context("at least one input path is required")?;
        let first = resolve_path(first, "input path")?;
        let anchor = containing_directory(&first);
        let root = git_text(anchor, ["rev-parse", "--show-toplevel"])
            .context("--baseline requires every input to be inside one Git repository")?;
        let root = resolve_path(Path::new(root.trim()), "Git root")?;
        for path in paths {
            let path = resolve_path(path, "input path")?;
            if !path.starts_with(&root) {
                bail!(
                    "input {} is outside Git repository {}; run one comparison per repository",
                    path.display(),
                    root.display()
                );
            }
            let path_root = git_text(
                containing_directory(&path),
                ["rev-parse", "--show-toplevel"],
            )
            .with_context(|| format!("input {} is not in a Git repository", path.display()))?;
            let path_root = resolve_path(Path::new(path_root.trim()), "Git root")?;
            if path_root != root {
                bail!(
                    "inputs span Git repositories {} and {}; run one comparison per repository",
                    root.display(),
                    path_root.display()
                );
            }
        }
        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn selection(
        &self,
        paths: &[PathBuf],
        include_hidden: bool,
        respect_ignores: bool,
    ) -> Result<SelectionReport> {
        Ok(SelectionReport::new(
            paths
                .iter()
                .map(|path| {
                    let (relative, resolved) = self.relative_input(path)?;
                    Ok(SelectedPathReport {
                        path: if relative.as_os_str().is_empty() {
                            ".".to_owned()
                        } else {
                            portable(&relative)
                        },
                        kind: if resolved.is_dir() {
                            SelectedPathKind::Directory
                        } else {
                            SelectedPathKind::File
                        },
                    })
                })
                .collect::<Result<_>>()?,
            include_hidden,
            respect_ignores,
        ))
    }

    pub(crate) fn resolve_commit(&self, revision: &str) -> Result<String> {
        validate_baseline_ref(revision)?;
        if revision.trim().is_empty() {
            bail!("--baseline requires a non-empty Git ref");
        }
        let expression = format!("{revision}^{{commit}}");
        let stdout = checked_git(
            git_output(
                &self.root,
                ["rev-parse", "--verify", "--end-of-options", &expression],
            )?,
            &format!("Git ref {revision:?} does not resolve to a commit"),
        )?;
        Ok(String::from_utf8(stdout)
            .context("Git emitted a non-UTF-8 commit ID")?
            .trim()
            .to_owned())
    }

    pub(crate) fn head_commit(&self) -> Result<String> {
        self.resolve_commit("HEAD")
    }

    pub(crate) fn dirty(&self) -> Result<bool> {
        Ok(!self.status_porcelain("normal")?.is_empty())
    }

    #[cfg(any(feature = "audit", test))]
    pub(crate) fn working_state(&self) -> Result<WorkingState> {
        let commit = self.head_commit()?;
        let stdout = self.status_porcelain("all")?;
        Ok(WorkingState {
            commit,
            fingerprint: working_tree_fingerprint(&self.root, &stdout)?,
            porcelain: stdout,
        })
    }

    fn status_porcelain(&self, untracked_files: &str) -> Result<Vec<u8>> {
        let untracked = format!("--untracked-files={untracked_files}");
        checked_git(
            git_command(&self.root)
                .args(["status", "--porcelain=v1", "-z", &untracked, "--"])
                .output()
                .context("cannot execute git")?,
            "cannot inspect working-tree status",
        )
    }

    pub(crate) fn materialize(&self, commit: &str) -> Result<Checkout> {
        let parent = self.root.parent().with_context(|| {
            format!(
                "cannot materialize baseline beside Git repository {} because it has no parent directory",
                self.root.display()
            )
        })?;
        // Cargo discovers configuration from the invocation directory upward,
        // and path dependencies commonly use `..`. A sibling checkout keeps
        // both endpoint trees at the same depth with the same outer ancestors.
        let temporary = tempfile::Builder::new()
            .prefix(".rot-baseline-")
            .tempdir_in(parent)
            .with_context(|| {
                format!(
                    "cannot create a baseline checkout beside Git repository {} in {}; sibling placement is required to preserve Cargo configuration and relative path dependencies",
                    self.root.display(),
                    parent.display()
                )
            })?;
        let root = fs::canonicalize(temporary.path()).with_context(|| {
            format!(
                "cannot resolve baseline checkout beside Git repository {}",
                self.root.display()
            )
        })?;
        let index_directory = tempfile::Builder::new()
            .prefix("rot-baseline-index-")
            .tempdir()
            .context("cannot create temporary baseline Git index directory")?;
        let index = index_directory.path().join("index");
        checked_git(
            git_with_index(&self.root, &index, ["read-tree", "--reset", commit])?,
            &format!("cannot read baseline commit {commit}"),
        )?;
        let mut prefix = root.as_os_str().to_os_string();
        prefix.push(std::path::MAIN_SEPARATOR.to_string());
        checked_git(
            git_with_index(
                &self.root,
                &index,
                [
                    "checkout-index".into(),
                    "--all".into(),
                    "--force".into(),
                    "--prefix".into(),
                    prefix,
                ],
            )?,
            &format!("cannot materialize baseline commit {commit}"),
        )?;
        self.create_ignore_context(&root)?;
        Ok(Checkout {
            _temporary: temporary,
            root,
        })
    }

    pub(crate) fn baseline_paths(
        &self,
        paths: &[PathBuf],
        baseline_root: &Path,
    ) -> Result<Vec<PathBuf>> {
        paths
            .iter()
            .map(|path| {
                let (relative, current) = self.relative_input(path)?;
                let baseline = baseline_root.join(&relative);
                if !baseline.exists() {
                    bail!(
                        "selected path {} does not exist in baseline; compare a containing directory to include additions",
                        portable(&relative)
                    );
                }
                let resolved = resolve_path(&baseline, "baseline path")?;
                if !resolved.starts_with(baseline_root) {
                    bail!(
                        "baseline path {} resolves outside the materialized commit",
                        portable(&relative)
                    );
                }
                if current.is_dir() != resolved.is_dir() {
                    let kinds = ["file", "directory"];
                    let current_kind = kinds[usize::from(current.is_dir())];
                    let baseline_kind = kinds[usize::from(resolved.is_dir())];
                    bail!(
                        "selected path {} is a {current_kind} in the working tree but a {baseline_kind} in the baseline; compare a stable containing directory instead",
                        portable(&relative),
                    );
                }
                Ok(resolved)
            })
            .collect()
    }

    fn relative_input(&self, path: &Path) -> Result<(PathBuf, PathBuf)> {
        let lexical = lexical_absolute(path)?;
        let resolved = resolve_path(path, "input path")?;
        let canonical_relative = resolved.strip_prefix(&self.root).with_context(|| {
            format!(
                "input {} resolves outside Git repository {}",
                lexical.display(),
                self.root.display(),
            )
        })?;

        // Preserve the repository-relative spelling of positional paths, even
        // when an input or repository ancestor is a symlink (macOS /tmp is a
        // common example). Choose the outermost matching ancestor so a child
        // symlink back to the repository does not erase its own path segment.
        let lexical_root = lexical
            .ancestors()
            .filter(|ancestor| {
                fs::canonicalize(ancestor).is_ok_and(|canonical| canonical == self.root)
            })
            .last();
        let relative = lexical_root.map_or_else(
            || canonical_relative.to_path_buf(),
            |lexical_root| {
                lexical
                    .strip_prefix(lexical_root)
                    .expect("ancestor is a lexical prefix")
                    .to_path_buf()
            },
        );
        Ok((relative, resolved))
    }

    fn create_ignore_context(&self, baseline_root: &Path) -> Result<()> {
        // `ignore` deliberately requires a repository marker before applying
        // Git ignore rules. A materialized tree has no Git metadata, so give
        // it an inert marker and the same repository-local exclude file. This
        // keeps discovery symmetric without linking the temporary tree to the
        // live repository or registering a worktree.
        let marker = baseline_root.join(".git");
        match fs::symlink_metadata(&marker) {
            Ok(_) => bail!(
                "baseline commit materialized a reserved .git path at {}; refusing to create ignore metadata",
                marker.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect baseline path {}", marker.display()));
            }
        }
        let info = marker.join("info");
        fs::create_dir_all(&info).context("cannot create baseline Git ignore context")?;
        let exclude = git_text(&self.root, ["rev-parse", "--git-path", "info/exclude"])?;
        let exclude = PathBuf::from(exclude.trim());
        let exclude = if exclude.is_absolute() {
            exclude
        } else {
            self.root.join(exclude)
        };
        if exclude.is_file() {
            fs::copy(&exclude, info.join("exclude")).with_context(|| {
                format!("cannot copy repository exclude file {}", exclude.display())
            })?;
        }
        Ok(())
    }
}

pub(crate) struct Checkout {
    _temporary: TempDir,
    root: PathBuf,
}

impl Checkout {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

fn checked_git(output: Output, action: &str) -> Result<Vec<u8>> {
    if !output.status.success() {
        bail!(
            "{action}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn git_text<const N: usize>(directory: &Path, arguments: [&str; N]) -> Result<String> {
    String::from_utf8(checked_git(
        git_output(directory, arguments)?,
        "git failed",
    )?)
    .context("Git emitted non-UTF-8 output")
}

fn git_output<const N: usize>(directory: &Path, arguments: [&str; N]) -> Result<Output> {
    git_command(directory)
        .args(arguments)
        .output()
        .context("cannot execute git")
}

fn git_with_index(
    directory: &Path,
    index: &Path,
    arguments: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Result<Output> {
    git_command(directory)
        .env("GIT_INDEX_FILE", index)
        .args(arguments)
        .output()
        .context("cannot execute git")
}

fn git_command(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(directory);
    // Hooks and `git rebase --exec` export repository-local variables that
    // override `current_dir`. Revision selection must always follow PATH.
    for variable in "GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_NAMESPACE GIT_PREFIX".split_whitespace() {
        command.env_remove(variable);
    }
    command
}

#[cfg(any(feature = "audit", test))]
fn working_tree_fingerprint(root: &Path, porcelain: &[u8]) -> Result<String> {
    let tracked = checked_git(
        git_output(
            root,
            [
                "diff",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
            ],
        )?,
        "cannot fingerprint tracked working-tree changes",
    )?;
    let untracked = checked_git(
        git_output(
            root,
            ["ls-files", "--others", "--exclude-standard", "-z", "--"],
        )?,
        "cannot list untracked files for working-tree fingerprint",
    )?;

    let mut child = git_command(root)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot start Git working-tree fingerprint")?;
    let write_result = (|| -> Result<()> {
        let mut input = child
            .stdin
            .take()
            .context("Git working-tree fingerprint has no input pipe")?;
        write_fingerprint_frame(&mut input, porcelain)?;
        write_fingerprint_frame(&mut input, &tracked)?;
        write_fingerprint_frame(&mut input, &untracked)?;
        for raw_path in untracked
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            fingerprint_untracked_file(root, raw_path, &mut input)?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let output = child
        .wait_with_output()
        .context("cannot finish Git working-tree fingerprint")?;
    let fingerprint = checked_git(output, "cannot hash working-tree state")?;
    Ok(String::from_utf8(fingerprint)
        .context("Git emitted a non-UTF-8 working-tree fingerprint")?
        .trim()
        .to_owned())
}

#[cfg(any(feature = "audit", test))]
fn write_fingerprint_frame(output: &mut impl Write, bytes: &[u8]) -> Result<()> {
    output
        .write_all(&(bytes.len() as u64).to_le_bytes())
        .context("cannot frame working-tree fingerprint")?;
    output
        .write_all(bytes)
        .context("cannot write working-tree fingerprint")
}

#[cfg(any(feature = "audit", test))]
fn fingerprint_untracked_file(root: &Path, raw_path: &[u8], output: &mut impl Write) -> Result<()> {
    let relative = path_from_git_bytes(raw_path)?;
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("Git reported an unsafe untracked path");
    }
    let path = root.join(&relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect untracked file {}", path.display()))?;
    write_fingerprint_frame(output, raw_path)?;
    write_fingerprint_frame(output, &file_mode(&metadata).to_le_bytes())?;
    if metadata.file_type().is_symlink() {
        output.write_all(b"l")?;
        let target = fs::read_link(&path)
            .with_context(|| format!("cannot read untracked symlink {}", path.display()))?;
        write_fingerprint_frame(output, &os_bytes(target.as_os_str()))?;
    } else if metadata.is_file() {
        output.write_all(b"f")?;
        output.write_all(&metadata.len().to_le_bytes())?;
        let mut file = File::open(&path)
            .with_context(|| format!("cannot read untracked file {}", path.display()))?;
        let copied = io::copy(&mut file, output)
            .with_context(|| format!("cannot hash untracked file {}", path.display()))?;
        if copied != metadata.len() {
            bail!(
                "untracked file {} changed while its content was fingerprinted",
                path.display()
            );
        }
    } else {
        output.write_all(b"o")?;
    }
    Ok(())
}

#[cfg(all(any(feature = "audit", test), unix))]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

#[cfg(all(any(feature = "audit", test), not(unix)))]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::str::from_utf8(bytes).context("Git emitted a non-UTF-8 untracked path")?,
    ))
}

#[cfg(all(any(feature = "audit", test), unix))]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes().to_vec()
}

#[cfg(all(any(feature = "audit", test), not(unix)))]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(all(any(feature = "audit", test), unix))]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;

    metadata.mode()
}

#[cfg(all(any(feature = "audit", test), not(unix)))]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

fn resolve_path(path: &Path, kind: &str) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("cannot resolve {kind} {}", path.display()))
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    use std::path::Component;

    let absolute = std::path::absolute(path)
        .with_context(|| format!("cannot make input path {} absolute", path.display()))?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .output()
            .expect("execute git");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("Git output is UTF-8")
    }

    #[test]
    fn baseline_ref_validation_rejects_only_revision_ranges() {
        assert!(validate_baseline_ref("HEAD~1").is_ok());
        assert!(
            validate_baseline_ref("HEAD~1..HEAD")
                .unwrap_err()
                .to_string()
                .contains("not a revision range")
        );
    }

    #[cfg(unix)]
    #[test]
    fn baseline_checkout_without_a_sibling_parent_is_actionable() {
        let repository = Repository {
            root: PathBuf::from("/"),
        };
        let error = repository
            .materialize("unused")
            .err()
            .expect("root path has no sibling parent");

        assert!(error.to_string().contains("has no parent directory"));
    }

    #[cfg(unix)]
    #[test]
    fn ignore_context_refuses_a_reserved_git_symlink() {
        use std::os::unix::fs::symlink;

        let baseline = tempfile::tempdir().expect("create baseline root");
        let outside = tempfile::tempdir().expect("create outside root");
        symlink(outside.path(), baseline.path().join(".git")).expect("create reserved symlink");
        let repository = Repository {
            root: PathBuf::from("/unused"),
        };

        let error = repository
            .create_ignore_context(baseline.path())
            .unwrap_err();
        assert!(error.to_string().contains("reserved .git path"));
        assert!(!outside.path().join("info").exists());
    }

    #[test]
    fn baseline_checkout_is_a_sibling_of_the_repository() {
        let parent = tempfile::tempdir().expect("create fixture parent");
        let repository_path = parent.path().join("repository");
        let shared_path = parent.path().join("shared");
        fs::create_dir(&repository_path).expect("create repository");
        fs::create_dir(&shared_path).expect("create sibling dependency");
        fs::write(repository_path.join("source.rs"), "pub fn source() {}\n").expect("write source");
        git(&repository_path, &["init", "-q"]);
        git(&repository_path, &["add", "."]);
        git(
            &repository_path,
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

        let repository = Repository::discover(std::slice::from_ref(&repository_path)).unwrap();
        let commit = repository.head_commit().unwrap();
        let checkout = repository.materialize(&commit).unwrap();
        let checkout_path = checkout.root().to_path_buf();

        assert_eq!(checkout.root().parent(), repository.root().parent());
        assert_eq!(
            fs::canonicalize(checkout.root().join("../shared")).unwrap(),
            fs::canonicalize(repository.root().join("../shared")).unwrap()
        );
        assert_eq!(
            fs::read_to_string(checkout.root().join("source.rs")).unwrap(),
            "pub fn source() {}\n"
        );
        drop(checkout);
        assert!(!checkout_path.exists());
    }

    #[test]
    fn working_state_tracks_head_and_exact_porcelain() {
        let repository_path = tempfile::tempdir().expect("create fixture repository");
        fs::write(
            repository_path.path().join("source.rs"),
            "pub fn source() {}\n",
        )
        .expect("write source");
        git(repository_path.path(), &["init", "-q"]);
        git(repository_path.path(), &["add", "."]);
        git(
            repository_path.path(),
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
        let repository =
            Repository::discover(&[repository_path.path().to_path_buf()]).expect("discover repo");
        let clean = repository.working_state().expect("capture clean state");
        assert_eq!(clean.commit(), repository.head_commit().unwrap());
        assert!(!clean.dirty());

        fs::write(
            repository_path.path().join("untracked.rs"),
            "fn untracked() {}\n",
        )
        .expect("write untracked source");
        let dirty = repository.working_state().expect("capture dirty state");
        assert_eq!(dirty.commit(), clean.commit());
        assert!(dirty.dirty());
        assert_ne!(dirty, clean);

        fs::write(repository_path.path().join("other.rs"), "fn other() {}\n")
            .expect("write another source");
        let changed = repository
            .working_state()
            .expect("capture changed dirty state");
        assert!(changed.dirty());
        assert_ne!(changed, dirty);
    }

    #[test]
    fn working_state_fingerprint_changes_when_porcelain_shape_does_not() {
        let repository_path = tempfile::tempdir().expect("create fixture repository");
        let tracked = repository_path.path().join("source.rs");
        fs::write(&tracked, "pub fn source() {}\n").expect("write source");
        git(repository_path.path(), &["init", "-q"]);
        git(repository_path.path(), &["add", "."]);
        git(
            repository_path.path(),
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
        let repository =
            Repository::discover(&[repository_path.path().to_path_buf()]).expect("discover repo");

        fs::write(&tracked, "pub fn first() {}\n").expect("write first tracked change");
        let first = repository.working_state().expect("capture first state");
        fs::write(&tracked, "pub fn other() {}\n").expect("write second tracked change");
        let second = repository.working_state().expect("capture second state");
        assert_eq!(first.porcelain, second.porcelain);
        assert_ne!(first.fingerprint, second.fingerprint);

        let untracked = repository_path.path().join("new.rs");
        fs::write(&untracked, "pub fn first() {}\n").expect("write first untracked content");
        let first = repository
            .working_state()
            .expect("capture first untracked state");
        fs::write(&untracked, "pub fn other() {}\n").expect("write second untracked content");
        let second = repository
            .working_state()
            .expect("capture second untracked state");
        assert_eq!(first.porcelain, second.porcelain);
        assert_ne!(first.fingerprint, second.fingerprint);
    }
}
