use std::io::{self, Write};

use anyhow::{Context, Result};
use serde::Serialize;
use tabwriter::TabWriter;

use crate::{
    cli::{FastCli, OutputFormat},
    diff::{Change, Comparison, FileChange},
    model::{BucketReport, DiagnosticSeverity, OutputRole, Report, SelectionReport, SourceMetrics},
};

/// Version of the fast `rot` JSON documents (snapshot and comparison).
pub const SCHEMA_VERSION: u32 = 3;

#[derive(Serialize)]
struct JsonReport<'a, Report, File> {
    schema_version: u32,
    report_kind: &'static str,
    detail: &'static str,
    #[serde(flatten)]
    report: &'a Report,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<&'a [File]>,
}

pub fn render_snapshot(output: &mut impl Write, report: &Report, cli: &FastCli) -> Result<()> {
    match cli.format {
        OutputFormat::Table => render_table(output, report, cli.files),
        OutputFormat::Json => write_json(
            output,
            &JsonReport {
                schema_version: SCHEMA_VERSION,
                report_kind: "snapshot",
                detail: if cli.summary_only { "summary" } else { "files" },
                report,
                files: (!cli.summary_only).then_some(report.files.as_slice()),
            },
            "JSON report",
        ),
    }
}

pub fn render_comparison(
    output: &mut impl Write,
    comparison: &Comparison,
    cli: &FastCli,
) -> Result<()> {
    match cli.format {
        OutputFormat::Table => render_comparison_table(output, comparison, cli.files),
        OutputFormat::Json => write_json(
            output,
            &JsonReport {
                schema_version: SCHEMA_VERSION,
                report_kind: "comparison",
                detail: if cli.summary_only { "summary" } else { "files" },
                report: comparison,
                files: (!cli.summary_only).then_some(comparison.files.as_slice()),
            },
            "comparison JSON",
        ),
    }
}

pub fn write_json(
    output: &mut impl Write,
    value: &impl Serialize,
    description: &str,
) -> Result<()> {
    serde_json::to_writer(&mut *output, value)
        .with_context(|| format!("cannot serialize {description}"))?;
    writeln!(output).with_context(|| format!("cannot write {description}"))
}

pub fn render_diagnostics(report: &Report) {
    render_diagnostic_list("", &report.diagnostics);
}

pub fn render_comparison_diagnostics(comparison: &Comparison) {
    render_diagnostic_list("baseline: ", &comparison.before.diagnostics);
    render_diagnostic_list("working tree: ", &comparison.after.diagnostics);
}

fn render_diagnostic_list(prefix: &str, diagnostics: &[crate::model::Diagnostic]) {
    for diagnostic in diagnostics {
        let severity = match diagnostic.severity {
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
        };
        match &diagnostic.path {
            Some(path) => eprintln!("rot: {severity}: {prefix}{path}: {}", diagnostic.message),
            None => eprintln!("rot: {severity}: {prefix}{}", diagnostic.message),
        }
    }
}

fn render_table(output: &mut impl Write, report: &Report, by_file: bool) -> Result<()> {
    let mut output = TabWriter::new(output).padding(2);
    writeln!(
        output,
        "Role\tFiles\tLines\tCode\tComments\tDocs\tBlank\tLexical\tCyclomatic\tCognitive\tDeclared pub"
    )?;
    for role in OutputRole::ALL {
        if let Some(bucket) = role.bucket(&report.buckets) {
            write_bucket(&mut output, role.label(), bucket)?;
        }
    }
    let total = BucketReport {
        files: report.file_count,
        source: SourceMetrics::total(report.total, report.metrics, &report.buckets),
        ..BucketReport::default()
    };
    write_bucket(&mut output, "Total", &total)?;
    writeln!(
        output,
        "Role file counts overlap; Total is the number of distinct Rust files."
    )?;
    if by_file {
        writeln!(output)?;
        writeln!(
            output,
            "File\tLines\tProd LOC\tTest LOC\tOther LOC\tLexical\tCyclomatic\tCognitive\tProd pub"
        )?;
        for file in &report.files {
            let production = OutputRole::Production.bucket(&file.buckets);
            let production_code = production.map_or(0, |bucket| bucket.source.lines.code);
            let test_code = OutputRole::Test
                .bucket(&file.buckets)
                .map_or(0, |bucket| bucket.source.lines.code);
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                file.path,
                file.total.physical,
                production_code,
                test_code,
                file.total.code - production_code - test_code,
                file.metrics.lexical_complexity,
                file.metrics.cyclomatic_authored,
                file.metrics.cognitive_authored,
                production.map_or(0, |bucket| bucket.source.declared_public),
            )?;
        }
    }
    output.flush()?;
    Ok(())
}

fn render_comparison_table(
    output: &mut impl Write,
    comparison: &Comparison,
    all_files: bool,
) -> Result<()> {
    let mut output = TabWriter::new(output).padding(2);
    let revision = comparison.before.revision.as_deref().unwrap_or("baseline");
    let state = ["clean", "dirty"][usize::from(comparison.after.dirty == Some(true))];
    writeln!(
        output,
        "Comparison: {revision} ({}) -> working tree at {} ({state})\nRoot: {}\nPaths: {}\nDiscovery: include hidden = {}, respect ignores = {}\nMetric-changing files: {} added, {} modified, {} deleted",
        short_commit(&comparison.before.commit),
        short_commit(&comparison.after.commit),
        comparison.root,
        human_selection_paths(&comparison.selection),
        comparison.selection.include_hidden,
        comparison.selection.respect_ignores,
        comparison.metric_changed_files.added,
        comparison.metric_changed_files.modified,
        comparison.metric_changed_files.deleted,
    )?;
    writeln!(output)?;
    writeln!(output, "Metric\tBefore\tAfter\tDelta\t%")?;
    for (label, change) in comparison.summary.entries() {
        writeln!(
            output,
            "{label}\t{}\t{}\t{:+}\t{}",
            change.before,
            change.after,
            change.delta,
            human_percent(change),
        )?;
    }

    writeln!(output)?;
    writeln!(
        output,
        "Role\tFiles before\tFiles after\tDelta\tCode before\tCode after\tDelta"
    )?;
    for role in &comparison.buckets {
        let files = role.metrics.files;
        let code = role.metrics.source.code;
        if [files, code]
            .iter()
            .all(|change| change.before == 0 && change.after == 0)
        {
            continue;
        }
        writeln!(
            output,
            "{}\t{}\t{}\t{:+}\t{}\t{}\t{:+}",
            role.role.label(),
            files.before,
            files.after,
            files.delta,
            code.before,
            code.after,
            code.delta,
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
        output.flush()?;
        return Ok(());
    }
    writeln!(output, "File\tStatus\tMetric deltas")?;
    for file in contributors {
        writeln!(
            output,
            "{}\t{}\t{}",
            file.path,
            file.status.label(),
            human_file_deltas(file),
        )?;
    }
    writeln!(
        output,
        "Metric diff is not textual Git churn; renames appear as one deletion and one addition."
    )?;
    output.flush()?;
    Ok(())
}

fn human_file_deltas(file: &FileChange) -> String {
    let mut deltas = Vec::with_capacity(14);
    for (label, change) in file.metrics.entries().into_iter().chain([
        ("Prod", file.production_code),
        ("Test", file.test_code),
        ("Other", file.other_code),
    ]) {
        if change.delta != 0 {
            deltas.push(format!("{} {:+}", label.to_ascii_lowercase(), change.delta));
        }
    }
    debug_assert!(!deltas.is_empty(), "comparison retained an unchanged file");
    deltas.join(", ")
}

fn human_percent(change: Change) -> String {
    match change.percent_change {
        Some(percent) => format!("{percent:+.2}%"),
        None if change.after > 0 => "new".to_owned(),
        None => "—".to_owned(),
    }
}

pub(crate) fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn human_selection_paths(selection: &SelectionReport) -> String {
    selection
        .paths
        .iter()
        .map(|path| format!("{} ({})", path.path, path.kind.label()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A closed reader surfaces as an `io::Error` from the writer or, while
/// streaming JSON, as the I/O kind wrapped inside a `serde_json::Error`.
pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let kind = cause
            .downcast_ref::<io::Error>()
            .map(io::Error::kind)
            .or_else(|| {
                cause
                    .downcast_ref::<serde_json::Error>()
                    .and_then(serde_json::Error::io_error_kind)
            });
        kind == Some(io::ErrorKind::BrokenPipe)
    })
}

fn write_bucket(output: &mut impl Write, label: &str, bucket: &BucketReport) -> io::Result<()> {
    let lines = bucket.source.lines;
    let metrics = bucket.source.metrics;
    writeln!(
        output,
        "{label}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        bucket.files,
        lines.physical,
        lines.code,
        lines.comments,
        lines.docs,
        lines.blank,
        metrics.lexical_complexity,
        metrics.cyclomatic_authored,
        metrics.cognitive_authored,
        bucket.source.declared_public,
    )
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{SCHEMA_VERSION, is_broken_pipe, write_json};

    struct ClosedPipe;

    impl io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn json_written_to_a_closed_pipe_is_reported_as_a_broken_pipe() {
        let document = serde_json::json!({ "schema_version": SCHEMA_VERSION });
        let error = write_json(&mut ClosedPipe, &document, "JSON report").unwrap_err();
        assert!(is_broken_pipe(&error), "{error:#}");
    }
}
