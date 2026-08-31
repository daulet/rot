use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    analyze,
    cli::FastCli,
    model::{
        BucketReport, Diagnostic, FileReport, OutputRole, ProfileReport, Report, SelectionReport,
        SourceMetrics,
    },
    paths::portable,
    revision::{Repository, validate_baseline_ref},
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

macro_rules! source_metric_changes {
    ($($field:ident => $($path:ident).+),+ $(,)?) => {
        #[derive(Clone, Debug, Serialize)]
        pub struct SourceMetricChanges {
            $(pub $field: Change,)+
        }

        impl SourceMetricChanges {
            fn between(before: MetricValues, after: MetricValues) -> Self {
                Self {
                    $($field: Change::new(before$(.$path)+, after$(.$path)+),)+
                }
            }

            fn changed(&self) -> bool {
                false $(|| self.$field.changed())+
            }
        }
    };
}

#[rustfmt::skip]
source_metric_changes! {
    physical => source.lines.physical, code => source.lines.code, comments => source.lines.comments,
    docs => source.lines.docs, blank => source.lines.blank, lexical_complexity => source.metrics.lexical_complexity,
    cyclomatic_authored => source.metrics.cyclomatic_authored, cognitive_authored => source.metrics.cognitive_authored, declared_public => source.declared_public,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricChanges {
    pub files: Change,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Change>,
    #[serde(flatten)]
    pub source: SourceMetricChanges,
}

impl MetricChanges {
    fn between(before: MetricValues, after: MetricValues, include_bytes: bool) -> Self {
        Self {
            files: Change::new(before.files, after.files),
            bytes: include_bytes.then(|| Change::new(before.bytes, after.bytes)),
            source: SourceMetricChanges::between(before, after),
        }
    }

    fn changed(&self) -> bool {
        self.files.changed() || self.bytes.is_some_and(Change::changed) || self.source.changed()
    }

    pub fn bytes(&self) -> Change {
        self.bytes.expect("bytes are present outside role metrics")
    }

    pub fn entries(&self) -> [(&'static str, Change); 11] {
        [
            ("Files", self.files),
            ("Bytes", self.bytes()),
            ("Lines", self.physical),
            ("Code", self.code),
            ("Comments", self.comments),
            ("Docs", self.docs),
            ("Blank", self.blank),
            ("Lexical", self.lexical_complexity),
            ("Cyclomatic", self.cyclomatic_authored),
            ("Cognitive", self.cognitive_authored),
            ("Declared pub", self.declared_public),
        ]
    }
}

deref_field!(MetricChanges => SourceMetricChanges, source);

#[derive(Clone, Debug, Serialize)]
pub struct RoleChanges {
    pub role: OutputRole,
    #[serde(flatten)]
    pub metrics: MetricChanges,
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

#[rustfmt::skip]
labelled_enum! { pub enum FileStatus { Added => "added", Deleted => "deleted", Modified => "modified" } }

#[derive(Clone, Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub status: FileStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<MetricValues>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<MetricValues>,
    pub metrics: MetricChanges,
    pub production_code: Change,
    pub test_code: Change,
    pub other_code: Change,
}

impl FileChange {
    fn ranking_key(&self) -> [u128; 2] {
        [
            self.production_code.delta.unsigned_abs()
                + self.test_code.delta.unsigned_abs()
                + self.other_code.delta.unsigned_abs(),
            self.metrics
                .entries()
                .iter()
                .map(|(_, change)| change.delta.unsigned_abs())
                .sum(),
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
    #[serde(skip)]
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

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct MetricValues {
    #[serde(skip)]
    files: u64,
    pub bytes: u64,
    #[serde(flatten)]
    pub source: SourceMetrics,
    pub production_code: u64,
    pub test_code: u64,
    pub other_code: u64,
}

deref_field!(MetricValues => SourceMetrics, source);

impl MetricValues {
    fn report(report: &Report) -> Self {
        Self {
            files: report.file_count,
            bytes: report.bytes,
            source: SourceMetrics::total(report.total, report.metrics, &report.buckets),
            ..Self::default()
        }
    }

    fn bucket(bucket: Option<&BucketReport>) -> Self {
        bucket.map_or_else(Self::default, |bucket| Self {
            files: bucket.files,
            source: bucket.source,
            ..Self::default()
        })
    }

    fn file(file: &FileReport) -> Self {
        let production_code = file_role_code(file, OutputRole::Production);
        let test_code = file_role_code(file, OutputRole::Test);
        Self {
            files: 1,
            bytes: file.bytes,
            source: SourceMetrics::total(file.total, file.metrics, &file.buckets),
            production_code,
            test_code,
            other_code: file
                .total
                .code
                .saturating_sub(production_code)
                .saturating_sub(test_code),
        }
    }
}

pub fn compare(cli: &FastCli, baseline_ref: &str) -> Result<Comparison> {
    validate_baseline_ref(baseline_ref)?;
    let repository = Repository::discover(&cli.paths)?;
    let selection = repository.selection(&cli.paths, cli.hidden, !cli.no_ignore)?;
    let baseline_commit = repository.resolve_commit(baseline_ref)?;
    let current_commit = repository.head_commit()?;
    let dirty = repository.dirty()?;
    let checkout = repository.materialize(&baseline_commit)?;
    let baseline_paths = repository.baseline_paths(&cli.paths, checkout.root())?;
    let mut current = analyze::analyze(cli)?;
    let mut baseline_cli = cli.clone();
    baseline_cli.cargo.paths = baseline_paths;
    baseline_cli.baseline = None;
    let mut baseline = analyze::analyze(&baseline_cli)
        .with_context(|| format!("cannot analyze baseline {baseline_ref:?}"))?;

    normalize_diagnostics(&mut baseline, checkout.root(), repository.root());
    normalize_diagnostics(&mut current, repository.root(), repository.root());
    let baseline_files = indexed_files(&baseline, checkout.root())?;
    let current_files = indexed_files(&current, repository.root())?;
    let mut paths = baseline_files.keys().cloned().collect::<BTreeSet<_>>();
    paths.extend(current_files.keys().cloned());

    let mut files = Vec::new();
    let mut counts = ChangedFileCounts::default();
    for path in paths {
        let before = baseline_files.get(&path).copied();
        let after = current_files.get(&path).copied();
        let before_metrics = before.map_or_else(MetricValues::default, MetricValues::file);
        let after_metrics = after.map_or_else(MetricValues::default, MetricValues::file);
        let metric_changes = MetricChanges::between(before_metrics, after_metrics, true);
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
            role,
            metrics: MetricChanges::between(
                MetricValues::bucket(role.bucket(&baseline.buckets)),
                MetricValues::bucket(role.bucket(&current.buckets)),
                false,
            ),
        })
        .collect();
    let summary = MetricChanges::between(
        MetricValues::report(&baseline),
        MetricValues::report(&current),
        true,
    );

    Ok(Comparison {
        root: repository.root().to_string_lossy().into_owned(),
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
            Ok((portable(relative), file))
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
            diagnostic.path = Some(portable(relative));
        } else if let Ok(relative) = absolute.strip_prefix(logical_root) {
            diagnostic.path = Some(portable(relative));
        }
    }
}

fn file_role_code(file: &FileReport, role: OutputRole) -> u64 {
    role.bucket(&file.buckets)
        .map_or(0, |bucket| bucket.lines.code)
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
