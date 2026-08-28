use std::io::{self, Write};

use anyhow::{Context, Result};

use crate::{
    cli::{Cli, OutputFormat},
    model::{BucketReport, DiagnosticSeverity, OutputRole, Report},
};

pub fn render(report: &Report, cli: &Cli) -> Result<()> {
    match cli.format {
        OutputFormat::Table => render_table(report, cli.files),
        OutputFormat::Json => {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            serde_json::to_writer(&mut output, report).context("cannot serialize JSON report")?;
            writeln!(output).context("cannot write JSON report")?;
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

fn render_table(report: &Report, by_file: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "{:<13} {:>7} {:>10} {:>10} {:>10} {:>9} {:>10} {:>11} {:>10}",
        "Role", "Files", "Lines", "Code", "Comments", "Docs", "Blank", "Complexity", "Public"
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
        "{:<13} {:>7} {:>10} {:>10} {:>10} {:>9} {:>10} {:>11} {:>10}",
        "Total",
        report.file_count,
        report.total.physical,
        report.total.code,
        report.total.comments,
        report.total.docs,
        report.total.blank,
        report.complexity,
        total_public,
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "Exported surface: {} items, {} signature lines; unresolved: public uses {} (glob {}), macro calls {}, inherent public items {}",
        report.surface.exported_items,
        report.surface.signature_lines,
        report.surface.unresolved_public_uses,
        report.surface.unresolved_glob_reexports,
        report.surface.opaque_macro_calls,
        report.surface.unresolved_inherent_public_items,
    )?;

    if by_file {
        writeln!(output)?;
        writeln!(
            output,
            "{:<48} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
            "File", "Lines", "Prod LOC", "Test LOC", "Other LOC", "Cx", "Prod pub", "Surface"
        )?;
        for file in &report.files {
            let production = file_bucket(file, OutputRole::Production);
            let test = file_bucket(file, OutputRole::Test);
            let other = file.total.code
                - production.map_or(0, |bucket| bucket.lines.code)
                - test.map_or(0, |bucket| bucket.lines.code);
            writeln!(
                output,
                "{:<48} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
                truncate_path(&file.path, 48),
                file.total.physical,
                production.map_or(0, |bucket| bucket.lines.code),
                test.map_or(0, |bucket| bucket.lines.code),
                other,
                file.complexity,
                file.surface.production_declared_public,
                file.surface.signature_lines,
            )?;
        }
    }
    Ok(())
}

fn write_bucket(output: &mut impl Write, label: &str, bucket: &BucketReport) -> io::Result<()> {
    writeln!(
        output,
        "{label:<13} {:>7} {:>10} {:>10} {:>10} {:>9} {:>10} {:>11} {:>10}",
        bucket.files,
        bucket.lines.physical,
        bucket.lines.code,
        bucket.lines.comments,
        bucket.lines.docs,
        bucket.lines.blank,
        bucket.complexity,
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

fn truncate_path(path: &str, width: usize) -> String {
    let characters = path.chars().collect::<Vec<_>>();
    if characters.len() <= width {
        return path.to_owned();
    }
    let keep = width.saturating_sub(1);
    format!(
        "…{}",
        characters[characters.len() - keep..]
            .iter()
            .collect::<String>()
    )
}
