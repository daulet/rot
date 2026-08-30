use std::io::{self, Write};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    cli::{FastCli, OutputFormat},
    diff::{Change, Comparison, Endpoint, FileChange, MetricChanges, RoleChanges},
    model::{
        BucketReport, ComplexityMetrics, Diagnostic, DiagnosticSeverity, FileReport, LineCounts,
        OutputRole, ProfileReport, Report, SelectionReport,
    },
};

#[derive(Serialize)]
struct SnapshotJson<'a> {
    schema_version: u32,
    report_kind: &'static str,
    detail: &'static str,
    root: &'a str,
    selection: &'a SelectionReport,
    profile: &'a ProfileReport,
    file_count: u64,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<&'a [FileReport]>,
    buckets: &'a [BucketReport],
    total: LineCounts,
    #[serde(flatten)]
    metrics: ComplexityMetrics,
    diagnostics: &'a [Diagnostic],
}

pub fn render_snapshot(report: &Report, cli: &FastCli) -> Result<()> {
    match cli.format {
        OutputFormat::Table => render_table(report, cli.files),
        OutputFormat::Json => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            let view = SnapshotJson {
                schema_version: report.schema_version,
                report_kind: "snapshot",
                detail: if cli.summary_only { "summary" } else { "files" },
                root: &report.root,
                selection: &report.selection,
                profile: &report.profile,
                file_count: report.file_count,
                bytes: report.bytes,
                files: (!cli.summary_only).then_some(report.files.as_slice()),
                buckets: &report.buckets,
                total: report.total,
                metrics: report.metrics,
                diagnostics: &report.diagnostics,
            };
            serde_json::to_writer(&mut output, &view).context("cannot serialize JSON report")?;
            writeln!(output).context("cannot write JSON report")?;
            Ok(())
        }
    }
}

#[derive(Serialize)]
struct ComparisonJson<'a> {
    schema_version: u32,
    report_kind: &'static str,
    detail: &'static str,
    root: &'a str,
    selection: &'a SelectionReport,
    before: &'a Endpoint,
    after: &'a Endpoint,
    summary: &'a MetricChanges,
    buckets: &'a [RoleChanges],
    metric_changed_files: crate::diff::ChangedFileCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<&'a [FileChange]>,
}

pub fn render_comparison(comparison: &Comparison, cli: &FastCli) -> Result<()> {
    match cli.format {
        OutputFormat::Table => render_comparison_table(comparison, cli.files),
        OutputFormat::Json => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            let view = ComparisonJson {
                schema_version: 3,
                report_kind: "comparison",
                detail: if cli.summary_only { "summary" } else { "files" },
                root: &comparison.root,
                selection: &comparison.selection,
                before: &comparison.before,
                after: &comparison.after,
                summary: &comparison.summary,
                buckets: &comparison.buckets,
                metric_changed_files: comparison.metric_changed_files,
                files: (!cli.summary_only).then_some(comparison.files.as_slice()),
            };
            serde_json::to_writer(&mut output, &view)
                .context("cannot serialize comparison JSON")?;
            writeln!(output).context("cannot write comparison JSON")?;
            Ok(())
        }
    }
}

pub fn render_diagnostics(report: &Report) {
    for diagnostic in &report.diagnostics {
        let severity = match diagnostic.severity {
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
        };
        match &diagnostic.path {
            Some(path) => eprintln!("rot: {severity}: {path}: {}", diagnostic.message),
            None => eprintln!("rot: {severity}: {}", diagnostic.message),
        }
    }
}

pub fn render_comparison_diagnostics(comparison: &Comparison) {
    render_endpoint_diagnostics("baseline", &comparison.before.diagnostics);
    render_endpoint_diagnostics("working tree", &comparison.after.diagnostics);
}

fn render_endpoint_diagnostics(label: &str, diagnostics: &[crate::model::Diagnostic]) {
    for diagnostic in diagnostics {
        let severity = match diagnostic.severity {
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
        };
        match &diagnostic.path {
            Some(path) => eprintln!("rot: {severity}: {label}: {path}: {}", diagnostic.message),
            None => eprintln!("rot: {severity}: {label}: {}", diagnostic.message),
        }
    }
}

fn render_table(report: &Report, by_file: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "{:<13} {:>7} {:>10} {:>10} {:>10} {:>9} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "Role",
        "Files",
        "Lines",
        "Code",
        "Comments",
        "Docs",
        "Blank",
        "Lexical",
        "Cyclomatic",
        "Cognitive",
        "Declared pub",
    )?;
    for role in OutputRole::ALL {
        if let Some(bucket) = bucket(report, role) {
            write_bucket(&mut output, role.label(), bucket)?;
        }
    }
    let total_public = report
        .buckets
        .iter()
        .map(|bucket| bucket.declared_public)
        .sum::<u64>();
    writeln!(
        output,
        "{:<13} {:>7} {:>10} {:>10} {:>10} {:>9} {:>10} {:>10} {:>10} {:>10} {:>12}",
        "Total",
        report.file_count,
        report.total.physical,
        report.total.code,
        report.total.comments,
        report.total.docs,
        report.total.blank,
        report.metrics.lexical_complexity,
        report.metrics.cyclomatic_authored,
        report.metrics.cognitive_authored,
        total_public,
    )?;
    writeln!(
        output,
        "Role file counts overlap; Total is the number of distinct Rust files."
    )?;
    if by_file {
        writeln!(output)?;
        writeln!(
            output,
            "{:<48} {:>9} {:>9} {:>9} {:>9} {:>9} {:>11} {:>10} {:>9}",
            "File",
            "Lines",
            "Prod LOC",
            "Test LOC",
            "Other LOC",
            "Lexical",
            "Cyclomatic",
            "Cognitive",
            "Prod pub",
        )?;
        for file in &report.files {
            let production = file_bucket(file, OutputRole::Production);
            let test = file_bucket(file, OutputRole::Test);
            let other = file.total.code
                - production.map_or(0, |bucket| bucket.lines.code)
                - test.map_or(0, |bucket| bucket.lines.code);
            writeln!(
                output,
                "{:<48} {:>9} {:>9} {:>9} {:>9} {:>9} {:>11} {:>10} {:>9}",
                file.path,
                file.total.physical,
                production.map_or(0, |bucket| bucket.lines.code),
                test.map_or(0, |bucket| bucket.lines.code),
                other,
                file.metrics.lexical_complexity,
                file.metrics.cyclomatic_authored,
                file.metrics.cognitive_authored,
                production.map_or(0, |bucket| bucket.declared_public),
            )?;
        }
    }
    Ok(())
}

fn render_comparison_table(comparison: &Comparison, all_files: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let revision = comparison.before.revision.as_deref().unwrap_or("baseline");
    let state = if comparison.after.dirty == Some(true) {
        "dirty"
    } else {
        "clean"
    };
    writeln!(
        output,
        "Comparison: {revision} ({}) -> working tree at {} ({state})",
        short_commit(&comparison.before.commit),
        short_commit(&comparison.after.commit),
    )?;
    writeln!(output, "Root: {}", comparison.root)?;
    writeln!(
        output,
        "Paths: {}",
        human_selection_paths(&comparison.selection)
    )?;
    writeln!(
        output,
        "Discovery: include hidden = {}, respect ignores = {}",
        comparison.selection.include_hidden, comparison.selection.respect_ignores,
    )?;
    writeln!(
        output,
        "Metric-changing files: {} added, {} modified, {} deleted",
        comparison.metric_changed_files.added,
        comparison.metric_changed_files.modified,
        comparison.metric_changed_files.deleted,
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "{:<21} {:>12} {:>12} {:>12} {:>10}",
        "Metric", "Before", "After", "Delta", "%"
    )?;
    for (label, change) in metric_rows(&comparison.summary) {
        writeln!(
            output,
            "{label:<21} {:>12} {:>12} {:>+12} {:>10}",
            change.before,
            change.after,
            change.delta,
            human_percent(change),
        )?;
    }

    writeln!(output)?;
    writeln!(
        output,
        "{:<13} {:>10} {:>10} {:>10} {:>12} {:>12} {:>12}",
        "Role", "Files before", "Files after", "Delta", "Code before", "Code after", "Delta"
    )?;
    for role in &comparison.buckets {
        if role.metrics.files.before == 0
            && role.metrics.files.after == 0
            && role.metrics.code.before == 0
            && role.metrics.code.after == 0
        {
            continue;
        }
        writeln!(
            output,
            "{:<13} {:>10} {:>10} {:>+10} {:>12} {:>12} {:>+12}",
            role_label(&role.role),
            role.metrics.files.before,
            role.metrics.files.after,
            role.metrics.files.delta,
            role.metrics.code.before,
            role.metrics.code.after,
            role.metrics.code.delta,
        )?;
    }
    writeln!(
        output,
        "Role file counts overlap; project Files is the distinct Rust-file count."
    )?;

    let limit = (!all_files).then_some(10);
    let contributors = comparison.contributors(limit);
    writeln!(output)?;
    if all_files {
        writeln!(output, "All metric-changing files:")?;
    } else {
        writeln!(
            output,
            "Largest metric changes (top {}):",
            contributors.len()
        )?;
    }
    if contributors.is_empty() {
        writeln!(output, "No metric changes.")?;
        return Ok(());
    }
    writeln!(output, "{:<44} {:<9} Metric deltas", "File", "Status")?;
    for file in contributors {
        writeln!(
            output,
            "{:<44} {:<9} {}",
            file.path,
            file.status.label(),
            human_file_deltas(file),
        )?;
    }
    writeln!(
        output,
        "Metric diff is not textual Git churn; renames appear as one deletion and one addition."
    )?;
    Ok(())
}

fn human_file_deltas(file: &FileChange) -> String {
    let mut deltas = Vec::with_capacity(14);
    for (label, change) in [
        ("files", file.metrics.files),
        ("bytes", file.metrics.bytes),
        ("lines", file.metrics.physical),
        ("code", file.metrics.code),
        ("prod", file.production_code),
        ("test", file.test_code),
        ("other", file.other_code),
        ("comments", file.metrics.comments),
        ("docs", file.metrics.docs),
        ("blank", file.metrics.blank),
        ("lexical", file.metrics.lexical_complexity),
        ("cyclomatic", file.metrics.cyclomatic_authored),
        ("cognitive", file.metrics.cognitive_authored),
        ("pub", file.metrics.declared_public),
    ] {
        if change.delta != 0 {
            deltas.push(format!("{label} {:+}", change.delta));
        }
    }
    debug_assert!(!deltas.is_empty(), "comparison retained an unchanged file");
    deltas.join(", ")
}

fn metric_rows(metrics: &MetricChanges) -> [(&'static str, Change); 11] {
    [
        ("Files", metrics.files),
        ("Bytes", metrics.bytes),
        ("Lines", metrics.physical),
        ("Code", metrics.code),
        ("Comments", metrics.comments),
        ("Docs", metrics.docs),
        ("Blank", metrics.blank),
        ("Lexical", metrics.lexical_complexity),
        ("Cyclomatic", metrics.cyclomatic_authored),
        ("Cognitive", metrics.cognitive_authored),
        ("Declared pub", metrics.declared_public),
    ]
}

fn human_percent(change: Change) -> String {
    match change.percent_change {
        Some(percent) => format!("{percent:+.2}%"),
        None if change.after > 0 => "new".to_owned(),
        None => "—".to_owned(),
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn role_label(role: &str) -> &str {
    for candidate in OutputRole::ALL {
        if candidate.key() == role {
            return candidate.label();
        }
    }
    role
}

fn human_selection_paths(selection: &SelectionReport) -> String {
    selection
        .paths
        .iter()
        .map(|path| format!("{} ({})", path.path, path.kind.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
}

fn write_bucket(output: &mut impl Write, label: &str, bucket: &BucketReport) -> io::Result<()> {
    writeln!(
        output,
        "{label:<13} {:>7} {:>10} {:>10} {:>10} {:>9} {:>10} {:>10} {:>10} {:>10} {:>12}",
        bucket.files,
        bucket.lines.physical,
        bucket.lines.code,
        bucket.lines.comments,
        bucket.lines.docs,
        bucket.lines.blank,
        bucket.metrics.lexical_complexity,
        bucket.metrics.cyclomatic_authored,
        bucket.metrics.cognitive_authored,
        bucket.declared_public,
    )
}

fn bucket(report: &Report, role: OutputRole) -> Option<&BucketReport> {
    report
        .buckets
        .iter()
        .find(|bucket| bucket.role == role.key())
}

fn file_bucket(file: &crate::model::FileReport, role: OutputRole) -> Option<&BucketReport> {
    file.buckets.iter().find(|bucket| bucket.role == role.key())
}
