use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tempfile::TempDir;

use crate::{
    analyze,
    cli::FastCli,
    model::{
        BucketReport, Diagnostic, FileReport, OutputRole, ProfileReport, Report, SelectedPathKind,
        SelectedPathReport, SelectionReport,
    },
};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Change {
    pub before: u64,
    pub after: u64,
    pub delta: i128,
    pub percent_change: Option<f64>,
}

impl Change {
    fn new(before: u64, after: u64) -> Self {
        let delta = i128::from(after) - i128::from(before);
        let percent_change = if before == 0 {
            (after == 0).then_some(0.0)
        } else {
            let percent = delta as f64 * 100.0 / before as f64;
            Some((percent * 100.0).round() / 100.0)
        };
        Self {
            before,
            after,
            delta,
            percent_change,
        }
    }

    fn changed(self) -> bool {
        self.delta != 0
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricChanges {
    pub files: Change,
    pub bytes: Change,
    pub physical: Change,
    pub code: Change,
    pub comments: Change,
    pub docs: Change,
    pub blank: Change,
    pub lexical_complexity: Change,
    pub cyclomatic_authored: Change,
    pub cognitive_authored: Change,
    pub declared_public: Change,
}

impl MetricChanges {
    fn between(before: Metrics, after: Metrics) -> Self {
        Self {
            files: Change::new(before.files, after.files),
            bytes: Change::new(before.bytes, after.bytes),
            physical: Change::new(before.physical, after.physical),
            code: Change::new(before.code, after.code),
            comments: Change::new(before.comments, after.comments),
            docs: Change::new(before.docs, after.docs),
            blank: Change::new(before.blank, after.blank),
            lexical_complexity: Change::new(before.lexical_complexity, after.lexical_complexity),
            cyclomatic_authored: Change::new(before.cyclomatic_authored, after.cyclomatic_authored),
            cognitive_authored: Change::new(before.cognitive_authored, after.cognitive_authored),
            declared_public: Change::new(before.declared_public, after.declared_public),
        }
    }

    fn changed(&self) -> bool {
        [
            self.files,
            self.bytes,
            self.physical,
            self.code,
            self.comments,
            self.docs,
            self.blank,
            self.lexical_complexity,
            self.cyclomatic_authored,
            self.cognitive_authored,
            self.declared_public,
        ]
        .into_iter()
        .any(Change::changed)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RoleChanges {
    pub role: String,
    #[serde(flatten)]
    pub metrics: RoleMetricChanges,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoleMetricChanges {
    pub files: Change,
    pub physical: Change,
    pub code: Change,
    pub comments: Change,
    pub docs: Change,
    pub blank: Change,
    pub lexical_complexity: Change,
    pub cyclomatic_authored: Change,
    pub cognitive_authored: Change,
    pub declared_public: Change,
}

impl RoleMetricChanges {
    fn between(before: Metrics, after: Metrics) -> Self {
        Self {
            files: Change::new(before.files, after.files),
            physical: Change::new(before.physical, after.physical),
            code: Change::new(before.code, after.code),
            comments: Change::new(before.comments, after.comments),
            docs: Change::new(before.docs, after.docs),
            blank: Change::new(before.blank, after.blank),
            lexical_complexity: Change::new(before.lexical_complexity, after.lexical_complexity),
            cyclomatic_authored: Change::new(before.cyclomatic_authored, after.cyclomatic_authored),
            cognitive_authored: Change::new(before.cognitive_authored, after.cognitive_authored),
            declared_public: Change::new(before.declared_public, after.declared_public),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Endpoint {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    pub profile: ProfileReport,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
}

impl FileStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct FileMetrics {
    pub bytes: u64,
    pub physical: u64,
    pub code: u64,
    pub comments: u64,
    pub docs: u64,
    pub blank: u64,
    pub lexical_complexity: u64,
    pub cyclomatic_authored: u64,
    pub cognitive_authored: u64,
    pub declared_public: u64,
    pub production_code: u64,
    pub test_code: u64,
    pub other_code: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub status: FileStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<FileMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<FileMetrics>,
    pub metrics: MetricChanges,
    pub production_code: Change,
    pub test_code: Change,
    pub other_code: Change,
}

impl FileChange {
    fn ranking_key(&self) -> [u128; 12] {
        // Prefer role-aware authored-code churn, then fall back through the
        // remaining semantic, line, and byte metrics in report order.
        [
            self.production_code.delta.unsigned_abs()
                + self.test_code.delta.unsigned_abs()
                + self.other_code.delta.unsigned_abs(),
            self.metrics.code.delta.unsigned_abs(),
            self.metrics.cyclomatic_authored.delta.unsigned_abs(),
            self.metrics.cognitive_authored.delta.unsigned_abs(),
            self.metrics.declared_public.delta.unsigned_abs(),
            self.metrics.lexical_complexity.delta.unsigned_abs(),
            self.metrics.physical.delta.unsigned_abs(),
            self.metrics.comments.delta.unsigned_abs(),
            self.metrics.docs.delta.unsigned_abs(),
            self.metrics.blank.delta.unsigned_abs(),
            self.metrics.bytes.delta.unsigned_abs(),
            self.metrics.files.delta.unsigned_abs(),
        ]
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ChangedFileCounts {
    pub added: u64,
    pub deleted: u64,
    pub modified: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Comparison {
    pub root: String,
    pub selection: SelectionReport,
    pub before: Endpoint,
    pub after: Endpoint,
    pub summary: MetricChanges,
    pub buckets: Vec<RoleChanges>,
    pub metric_changed_files: ChangedFileCounts,
    pub files: Vec<FileChange>,
}

impl Comparison {
    pub fn has_diagnostics(&self) -> bool {
        !self.before.diagnostics.is_empty() || !self.after.diagnostics.is_empty()
    }

    pub fn contributors(&self, limit: Option<usize>) -> Vec<&FileChange> {
        let mut files = self.files.iter().collect::<Vec<_>>();
        files.sort_by(|left, right| {
            right
                .ranking_key()
                .cmp(&left.ranking_key())
                .then_with(|| left.path.cmp(&right.path))
        });
        files.truncate(limit.unwrap_or(files.len()));
        files
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Metrics {
    files: u64,
    bytes: u64,
    physical: u64,
    code: u64,
    comments: u64,
    docs: u64,
    blank: u64,
    lexical_complexity: u64,
    cyclomatic_authored: u64,
    cognitive_authored: u64,
    declared_public: u64,
}

impl Metrics {
    fn report(report: &Report) -> Self {
        Self {
            files: report.file_count,
            bytes: report.bytes,
            physical: report.total.physical,
            code: report.total.code,
            comments: report.total.comments,
            docs: report.total.docs,
            blank: report.total.blank,
            lexical_complexity: report.metrics.lexical_complexity,
            cyclomatic_authored: report.metrics.cyclomatic_authored,
            cognitive_authored: report.metrics.cognitive_authored,
            declared_public: report
                .buckets
                .iter()
                .map(|bucket| bucket.declared_public)
                .sum(),
        }
    }

    fn bucket(bucket: Option<&BucketReport>) -> Self {
        bucket.map_or_else(Self::default, |bucket| Self {
            files: bucket.files,
            physical: bucket.lines.physical,
            code: bucket.lines.code,
            comments: bucket.lines.comments,
            docs: bucket.lines.docs,
            blank: bucket.lines.blank,
            lexical_complexity: bucket.metrics.lexical_complexity,
            cyclomatic_authored: bucket.metrics.cyclomatic_authored,
            cognitive_authored: bucket.metrics.cognitive_authored,
            declared_public: bucket.declared_public,
            ..Self::default()
        })
    }
}

pub fn compare(cli: &FastCli, baseline_ref: &str) -> Result<Comparison> {
    if baseline_ref.contains("..") {
        bail!(
            "--baseline accepts one commit, not a revision range; compare the live working tree with a ref such as HEAD~1"
        );
    }
    let repository = Repository::discover(&cli.paths)?;
    let selection = repository.selection(&cli.paths, cli.hidden, !cli.no_ignore)?;
    let baseline_commit = repository.resolve_commit(baseline_ref)?;
    let current_commit = repository.resolve_commit("HEAD")?;
    let dirty = repository.dirty()?;
    let checkout = repository.materialize(&baseline_commit)?;
    let baseline_paths = repository.baseline_paths(&cli.paths, checkout.root())?;
    let current = analyze::analyze(cli)?;
    let mut baseline_cli = cli.clone();
    baseline_cli.cargo.paths = baseline_paths;
    baseline_cli.baseline = None;
    let baseline = analyze::analyze(&baseline_cli)
        .with_context(|| format!("cannot analyze baseline {baseline_ref:?}"))?;

    build_comparison(
        &repository.root,
        checkout.root(),
        baseline_ref,
        baseline_commit,
        current_commit,
        dirty,
        selection,
        baseline,
        current,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_comparison(
    repository_root: &Path,
    baseline_root: &Path,
    baseline_ref: &str,
    baseline_commit: String,
    current_commit: String,
    dirty: bool,
    selection: SelectionReport,
    mut baseline: Report,
    mut current: Report,
) -> Result<Comparison> {
    normalize_diagnostics(&mut baseline, baseline_root, repository_root);
    normalize_diagnostics(&mut current, repository_root, repository_root);
    let baseline_files = indexed_files(&baseline, baseline_root)?;
    let current_files = indexed_files(&current, repository_root)?;
    let mut paths = baseline_files.keys().cloned().collect::<BTreeSet<_>>();
    paths.extend(current_files.keys().cloned());

    let mut files = Vec::new();
    let mut counts = ChangedFileCounts::default();
    for path in paths {
        let before = baseline_files.get(&path).copied();
        let after = current_files.get(&path).copied();
        let before_metrics = before.map_or_else(FileMetrics::default, file_metrics);
        let after_metrics = after.map_or_else(FileMetrics::default, file_metrics);
        let metric_changes = MetricChanges::between(
            file_metric_totals(before_metrics, before.is_some()),
            file_metric_totals(after_metrics, after.is_some()),
        );
        let production_code = Change::new(
            before_metrics.production_code,
            after_metrics.production_code,
        );
        let test_code = Change::new(before_metrics.test_code, after_metrics.test_code);
        let other_code = Change::new(before_metrics.other_code, after_metrics.other_code);
        if !metric_changes.changed()
            && !production_code.changed()
            && !test_code.changed()
            && !other_code.changed()
        {
            continue;
        }
        let status = match (before, after) {
            (None, Some(_)) => {
                counts.added += 1;
                FileStatus::Added
            }
            (Some(_), None) => {
                counts.deleted += 1;
                FileStatus::Deleted
            }
            (Some(_), Some(_)) => {
                counts.modified += 1;
                FileStatus::Modified
            }
            (None, None) => unreachable!("path originated from one file index"),
        };
        files.push(FileChange {
            path,
            status,
            before: before.map(|_| before_metrics),
            after: after.map(|_| after_metrics),
            metrics: metric_changes,
            production_code,
            test_code,
            other_code,
        });
    }

    let buckets = OutputRole::ALL
        .into_iter()
        .map(|role| RoleChanges {
            role: role.key().to_owned(),
            metrics: RoleMetricChanges::between(
                Metrics::bucket(find_bucket(&baseline, role)),
                Metrics::bucket(find_bucket(&current, role)),
            ),
        })
        .collect();
    let summary = MetricChanges::between(Metrics::report(&baseline), Metrics::report(&current));

    Ok(Comparison {
        root: repository_root.to_string_lossy().into_owned(),
        selection,
        before: Endpoint {
            kind: "git",
            revision: Some(baseline_ref.to_owned()),
            commit: baseline_commit,
            dirty: None,
            profile: baseline.profile,
            diagnostics: baseline.diagnostics,
        },
        after: Endpoint {
            kind: "working_tree",
            revision: None,
            commit: current_commit,
            dirty: Some(dirty),
            profile: current.profile,
            diagnostics: current.diagnostics,
        },
        summary,
        buckets,
        metric_changed_files: counts,
        files,
    })
}

fn indexed_files<'a>(
    report: &'a Report,
    physical_root: &Path,
) -> Result<BTreeMap<String, &'a FileReport>> {
    report
        .files
        .iter()
        .map(|file| {
            let absolute = Path::new(&report.root).join(&file.path);
            let relative = absolute.strip_prefix(physical_root).with_context(|| {
                format!(
                    "reported source {} is outside comparison root {}",
                    absolute.display(),
                    physical_root.display()
                )
            })?;
            Ok((portable_path(relative), file))
        })
        .collect()
}

fn normalize_diagnostics(report: &mut Report, physical_root: &Path, logical_root: &Path) {
    let physical_root_text = physical_root.to_string_lossy();
    let logical_root_text = logical_root.to_string_lossy();
    for diagnostic in &mut report.diagnostics {
        diagnostic.message = diagnostic
            .message
            .replace(physical_root_text.as_ref(), logical_root_text.as_ref());
        let Some(path) = diagnostic.path.as_deref() else {
            continue;
        };
        let path = Path::new(path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(&report.root).join(path)
        };
        if let Ok(relative) = absolute.strip_prefix(physical_root) {
            diagnostic.path = Some(portable_path(relative));
        } else if let Ok(relative) = absolute.strip_prefix(logical_root) {
            diagnostic.path = Some(portable_path(relative));
        }
    }
}

fn find_bucket(report: &Report, role: OutputRole) -> Option<&BucketReport> {
    report
        .buckets
        .iter()
        .find(|bucket| bucket.role == role.key())
}

fn file_metrics(file: &FileReport) -> FileMetrics {
    let production_code = file_role_code(file, OutputRole::Production);
    let test_code = file_role_code(file, OutputRole::Test);
    FileMetrics {
        bytes: file.bytes,
        physical: file.total.physical,
        code: file.total.code,
        comments: file.total.comments,
        docs: file.total.docs,
        blank: file.total.blank,
        lexical_complexity: file.metrics.lexical_complexity,
        cyclomatic_authored: file.metrics.cyclomatic_authored,
        cognitive_authored: file.metrics.cognitive_authored,
        declared_public: file
            .buckets
            .iter()
            .map(|bucket| bucket.declared_public)
            .sum(),
        production_code,
        test_code,
        other_code: file
            .total
            .code
            .saturating_sub(production_code)
            .saturating_sub(test_code),
    }
}

fn file_metric_totals(file: FileMetrics, present: bool) -> Metrics {
    Metrics {
        files: u64::from(present),
        bytes: file.bytes,
        physical: file.physical,
        code: file.code,
        comments: file.comments,
        docs: file.docs,
        blank: file.blank,
        lexical_complexity: file.lexical_complexity,
        cyclomatic_authored: file.cyclomatic_authored,
        cognitive_authored: file.cognitive_authored,
        declared_public: file.declared_public,
    }
}

fn file_role_code(file: &FileReport, role: OutputRole) -> u64 {
    file.buckets
        .iter()
        .find(|bucket| bucket.role == role.key())
        .map_or(0, |bucket| bucket.lines.code)
}

struct Repository {
    root: PathBuf,
}

impl Repository {
    fn discover(paths: &[PathBuf]) -> Result<Self> {
        let first = paths
            .first()
            .context("at least one input path is required")?;
        let first = fs::canonicalize(first)
            .with_context(|| format!("cannot resolve input path {}", first.display()))?;
        let anchor = path_anchor(&first);
        let root = git_text(anchor, ["rev-parse", "--show-toplevel"])
            .context("--baseline requires every input to be inside one Git repository")?;
        let root = fs::canonicalize(root.trim())
            .with_context(|| format!("cannot resolve Git root {}", root.trim()))?;
        for path in paths {
            let path = fs::canonicalize(path)
                .with_context(|| format!("cannot resolve input path {}", path.display()))?;
            if !path.starts_with(&root) {
                bail!(
                    "input {} is outside Git repository {}; run one comparison per repository",
                    path.display(),
                    root.display()
                );
            }
            let path_root = git_text(path_anchor(&path), ["rev-parse", "--show-toplevel"])
                .with_context(|| format!("input {} is not in a Git repository", path.display()))?;
            let path_root = fs::canonicalize(path_root.trim())
                .with_context(|| format!("cannot resolve Git root {}", path_root.trim()))?;
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

    fn selection(
        &self,
        paths: &[PathBuf],
        include_hidden: bool,
        respect_ignores: bool,
    ) -> Result<SelectionReport> {
        let mut selected = paths
            .iter()
            .map(|path| {
                let (relative, resolved) = self.relative_input(path)?;
                Ok(SelectedPathReport {
                    path: if relative.as_os_str().is_empty() {
                        ".".to_owned()
                    } else {
                        portable_path(&relative)
                    },
                    kind: if resolved.is_dir() {
                        SelectedPathKind::Directory
                    } else {
                        SelectedPathKind::File
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        selected.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.kind.cmp(&right.kind))
        });
        selected.dedup();
        Ok(SelectionReport {
            paths: selected,
            include_hidden,
            respect_ignores,
            ignore_boundary: "path",
        })
    }

    fn relative_input(&self, path: &Path) -> Result<(PathBuf, PathBuf)> {
        let lexical = lexical_absolute(path)?;
        let resolved = fs::canonicalize(path)
            .with_context(|| format!("cannot resolve input path {}", path.display()))?;
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

    fn resolve_commit(&self, revision: &str) -> Result<String> {
        if revision.trim().is_empty() {
            bail!("--baseline requires a non-empty Git ref");
        }
        let expression = format!("{revision}^{{commit}}");
        let output = git_output(
            &self.root,
            ["rev-parse", "--verify", "--end-of-options", &expression],
        )?;
        if !output.status.success() {
            bail!(
                "Git ref {revision:?} does not resolve to a commit: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8(output.stdout)
            .context("Git emitted a non-UTF-8 commit ID")?
            .trim()
            .to_owned())
    }

    fn dirty(&self) -> Result<bool> {
        let output = git_output(
            &self.root,
            ["status", "--porcelain=v1", "--untracked-files=normal", "--"],
        )?;
        if !output.status.success() {
            bail!(
                "cannot inspect working-tree status: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(!output.stdout.is_empty())
    }

    fn materialize(&self, commit: &str) -> Result<Checkout> {
        let temporary = tempfile::Builder::new()
            .prefix("rot-baseline-")
            .tempdir()
            .context("cannot create baseline directory")?;
        let root = temporary.path().join("tree");
        fs::create_dir(&root).context("cannot create baseline checkout root")?;
        let root = fs::canonicalize(&root).context("cannot resolve baseline checkout root")?;
        let index = temporary.path().join("index");
        let read_tree = git_with_index(&self.root, &index, ["read-tree", "--reset", commit])?;
        if !read_tree.status.success() {
            bail!(
                "cannot read baseline commit {commit}: {}",
                String::from_utf8_lossy(&read_tree.stderr).trim()
            );
        }
        let mut prefix = root.as_os_str().to_os_string();
        prefix.push(std::path::MAIN_SEPARATOR.to_string());
        let checkout = git_with_index_os(
            &self.root,
            &index,
            [
                "checkout-index".into(),
                "--all".into(),
                "--force".into(),
                "--prefix".into(),
                prefix,
            ],
        )?;
        if !checkout.status.success() {
            bail!(
                "cannot materialize baseline commit {commit}: {}",
                String::from_utf8_lossy(&checkout.stderr).trim()
            );
        }
        self.create_ignore_context(&root)?;
        Ok(Checkout {
            _temporary: temporary,
            root,
        })
    }

    fn create_ignore_context(&self, baseline_root: &Path) -> Result<()> {
        // `ignore` deliberately requires a repository marker before applying
        // Git ignore rules. A materialized tree has no Git metadata, so give
        // it an inert marker and the same repository-local exclude file. This
        // keeps discovery symmetric without linking the temporary tree to the
        // live repository or registering a worktree.
        let info = baseline_root.join(".git/info");
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

    fn baseline_paths(&self, paths: &[PathBuf], baseline_root: &Path) -> Result<Vec<PathBuf>> {
        paths
            .iter()
            .map(|path| {
                let (relative, current) = self.relative_input(path)?;
                let baseline = baseline_root.join(&relative);
                if !baseline.exists() {
                    bail!(
                        "selected path {} does not exist in baseline; compare a containing directory to include additions",
                        portable_path(&relative)
                    );
                }
                let resolved = fs::canonicalize(&baseline).with_context(|| {
                    format!("cannot resolve baseline path {}", baseline.display())
                })?;
                if !resolved.starts_with(baseline_root) {
                    bail!(
                        "baseline path {} resolves outside the materialized commit",
                        portable_path(&relative)
                    );
                }
                if current.is_dir() != resolved.is_dir() {
                    let current_kind = if current.is_dir() { "directory" } else { "file" };
                    let baseline_kind = if resolved.is_dir() {
                        "directory"
                    } else {
                        "file"
                    };
                    bail!(
                        "selected path {} is a {current_kind} in the working tree but a {baseline_kind} in the baseline; compare a stable containing directory instead",
                        portable_path(&relative),
                    );
                }
                Ok(resolved)
            })
            .collect()
    }
}

struct Checkout {
    _temporary: TempDir,
    root: PathBuf,
}

impl Checkout {
    fn root(&self) -> &Path {
        &self.root
    }
}

fn git_text<const N: usize>(directory: &Path, arguments: [&str; N]) -> Result<String> {
    let output = git_output(directory, arguments)?;
    if !output.status.success() {
        return Err(anyhow!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).context("Git emitted non-UTF-8 output")
}

fn git_output<const N: usize>(directory: &Path, arguments: [&str; N]) -> Result<Output> {
    git_command(directory)
        .args(arguments)
        .output()
        .context("cannot execute git")
}

fn git_with_index<const N: usize>(
    directory: &Path,
    index: &Path,
    arguments: [&str; N],
) -> Result<Output> {
    git_command(directory)
        .env("GIT_INDEX_FILE", index)
        .args(arguments)
        .output()
        .context("cannot execute git")
}

fn git_with_index_os<const N: usize>(
    directory: &Path,
    index: &Path,
    arguments: [std::ffi::OsString; N],
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
    // override `current_dir`. Baseline selection must always follow PATH.
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(variable);
    }
    command
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn path_anchor(path: &Path) -> &Path {
    if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    }
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
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::Change;

    #[test]
    fn percentage_change_handles_zero_without_non_finite_json() {
        assert_eq!(Change::new(0, 0).percent_change, Some(0.0));
        assert_eq!(Change::new(0, 4).percent_change, None);
        assert_eq!(Change::new(4, 0).percent_change, Some(-100.0));
        assert_eq!(Change::new(3, 4).percent_change, Some(33.33));
    }
}
