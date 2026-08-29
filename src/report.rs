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
    if let Some(compiler) = &report.compiler {
        let status = |name: &str| {
            compiler
                .products
                .iter()
                .find(|product| product.product == name)
                .map_or(crate::model::SemanticStatus::Unavailable, |product| {
                    product.status
                })
        };
        writeln!(
            output,
            "\nCompiler: {}/{} invocations correlated; HIR {:?}; API {:?}; required visibility {:?}; closed-world {:?}; macro delta {:?}",
            compiler.correlated_invocations,
            compiler.expected_invocations,
            status("hir_bodies"),
            status("effective_api"),
            status("required_visibility"),
            status("closed_world_liveness"),
            status("macro_expansion_cyclomatic_delta"),
        )?;
        if let Some(api) = &compiler.effective_api {
            writeln!(
                output,
                "Effective API: {} definitions, {} public bindings across {} production library invocations",
                api.summary.effective_definitions,
                api.summary.public_bindings,
                api.summary.production_library_invocations,
            )?;
        }
        if let Some(required) = &compiler.required_visibility {
            writeln!(
                output,
                "Required visibility: {} public definitions [{}; excludes {}]",
                required.definitions.len(),
                required.scope,
                required.evidence_exclusions.join(", "),
            )?;
        }
        if let Some(closed_world) = &compiler.closed_world {
            writeln!(
                output,
                "Closed world: {} dead public, {} unnecessarily public [{}; excludes {}]",
                closed_world.summary.dead_public,
                closed_world.summary.unnecessary_public,
                closed_world.scope,
                closed_world.evidence_exclusions.join(", "),
            )?;
        }
        if let Some(expansion) = &compiler.macro_expansion_complexity {
            let (complete, macro_body_bases, decision_delta, cyclomatic_delta) = expansion
                .invocations
                .iter()
                .filter_map(|invocation| invocation.metrics.as_ref())
                .fold((0_u64, 0_u64, 0_u64, 0_u64), |totals, metrics| {
                    (
                        totals.0 + 1,
                        totals.1 + metrics.totals.macro_body_bases,
                        totals.2 + metrics.totals.decision_delta,
                        totals.3 + metrics.totals.cyclomatic_delta,
                    )
                });
            if complete > 0 {
                writeln!(
                    output,
                    "Macro expansion delta: invocation-local sum +{cyclomatic_delta} cyclomatic ({macro_body_bases} body bases + {decision_delta} decision weight; {}/{} invocations complete)",
                    complete,
                    expansion.invocations.len(),
                )?;
            }
        }
    }
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
                truncate_path(&file.path, 48),
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
