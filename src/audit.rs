use std::{io, io::Write, process::ExitCode};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{
    cli::{AuditCli, OutputFormat},
    compiler,
    model::{CompilerReport, Diagnostic, DiagnosticSeverity, SemanticStatus},
    workspace,
};

#[derive(Serialize)]
struct AuditOutput<'a> {
    schema_version: u32,
    root: &'a str,
    profile: AuditProfile<'a>,
    #[serde(flatten)]
    evidence: &'a CompilerReport,
    diagnostics: &'a [Diagnostic],
}

#[derive(Serialize)]
struct AuditProfile<'a> {
    toolchain: &'a str,
    target: &'a str,
    feature_mode: &'static str,
    requested_features: &'a [String],
    all_features: bool,
    no_default_features: bool,
    forced_cfg: &'a [String],
}

pub fn run(cli: AuditCli) -> ExitCode {
    match execute(&cli) {
        Ok(complete) => {
            if complete {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            if !is_broken_pipe(&error) {
                eprintln!("rot-audit: error: {error:#}");
            }
            if is_broken_pipe(&error) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn execute(cli: &AuditCli) -> Result<bool> {
    let inventory = workspace::audit_inventory(cli)?;
    let root = inventory.root.to_string_lossy().into_owned();
    let mut diagnostics = inventory.diagnostics.clone();
    let outcome = compiler::collect(cli, &inventory);
    diagnostics.extend(outcome.diagnostics);
    diagnostics
        .sort_by(|left, right| (&left.path, &left.message).cmp(&(&right.path, &right.message)));
    diagnostics.dedup_by(|left, right| left.path == right.path && left.message == right.message);
    let status = outcome.report.status;
    let reason = outcome.report.reason.as_deref();
    let output = AuditOutput {
        schema_version: 2,
        root: &root,
        profile: AuditProfile {
            toolchain: &cli.toolchain,
            target: inventory
                .audit_target
                .as_deref()
                .unwrap_or("unknown-target"),
            feature_mode: feature_mode(cli),
            requested_features: &cli.features,
            all_features: cli.all_features,
            no_default_features: cli.no_default_features,
            forced_cfg: &cli.cfg,
        },
        evidence: &outcome.report,
        diagnostics: &diagnostics,
    };

    match cli.format {
        OutputFormat::Json => {
            let stdout = io::stdout();
            let mut writer = stdout.lock();
            serde_json::to_writer(&mut writer, &output).context("cannot serialize audit JSON")?;
            writeln!(writer).context("cannot write audit JSON")?;
        }
        OutputFormat::Table => render_table(&outcome.report, status, reason)?,
    }
    render_diagnostics(&diagnostics);
    Ok(status == SemanticStatus::Complete)
}

fn feature_mode(cli: &AuditCli) -> &'static str {
    if cli.all_features {
        "all"
    } else if cli.no_default_features {
        if cli.features.is_empty() {
            "none"
        } else {
            "selected_without_defaults"
        }
    } else if cli.features.is_empty() {
        "default"
    } else {
        "default_plus_selected"
    }
}

fn render_table(
    report: &CompilerReport,
    status: SemanticStatus,
    reason: Option<&str>,
) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "Visibility audit: {} ({}/{} Cargo invocations correlated)",
        status_name(status),
        report.correlated_invocations,
        report.expected_invocations,
    )?;
    writeln!(
        output,
        "Compiler: {} ({} {} for {})",
        report.rustc_version, report.rustc_commit, report.rustc_commit_date, report.rustc_host,
    )?;
    if let Some(reason) = reason {
        writeln!(output, "Reason: {reason}")?;
    }
    if status != SemanticStatus::Complete {
        return Ok(());
    }
    if let Some(closed_world) = &report.closed_world {
        writeln!(output, "Scope: {}", closed_world.scope)?;
        writeln!(
            output,
            "Evidence excludes: {}",
            closed_world.evidence_exclusions.join(", ")
        )?;
    }
    let required = report
        .required_visibility
        .as_ref()
        .map_or(0, |required| required.definitions.len());
    let summary = report.closed_world.as_ref().map(|report| &report.summary);
    let can_narrow = summary.map_or(0, |summary| summary.unnecessary_public);
    let dead = summary.map_or(0, |summary| summary.dead_public);
    writeln!(output, "Required public: {required}")?;
    writeln!(output, "Can narrow unrestricted pub: {can_narrow}")?;
    writeln!(output, "Dead public: {dead}")?;
    if let Some(closed_world) = &report.closed_world {
        if !closed_world.findings.is_empty() {
            writeln!(output)?;
            writeln!(output, "Findings:")?;
        }
        for finding in &closed_world.findings {
            let location = finding
                .span
                .as_ref()
                .or(finding.attribution_callsite.as_ref())
                .map_or_else(
                    || "<unknown>".to_owned(),
                    |span| format!("{}:{}:{}", span.path, span.line, span.column),
                );
            let liveness = match (finding.production_live, finding.nonproduction_live) {
                (true, true) => "production+nonproduction",
                (true, false) => "production",
                (false, true) => "nonproduction",
                (false, false) => "unreachable",
            };
            writeln!(
                output,
                "{location}  {}  {}  [{liveness}]  {}",
                finding.definition_path, finding.kind, finding.reason,
            )?;
        }
    }
    Ok(())
}

fn status_name(status: SemanticStatus) -> &'static str {
    match status {
        SemanticStatus::Complete => "complete",
        SemanticStatus::Partial => "partial",
        SemanticStatus::Unavailable => "unavailable",
    }
}

fn render_diagnostics(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        let severity = match diagnostic.severity {
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
        };
        match &diagnostic.path {
            Some(path) => eprintln!("rot-audit: {severity}: {path}: {}", diagnostic.message),
            None => eprintln!("rot-audit: {severity}: {}", diagnostic.message),
        }
    }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
}
