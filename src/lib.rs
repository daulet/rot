mod analyze;
#[cfg(feature = "audit")]
mod audit;
mod cfg;
mod cli;
#[cfg(feature = "audit")]
mod compiler;
mod diff;
mod model;
mod report;
mod source;
mod workspace;

use std::process::ExitCode;

use clap::Parser;

pub fn run() -> ExitCode {
    run_fast(cli::FastCli::parse())
}

#[cfg(feature = "audit")]
pub fn run_audit() -> ExitCode {
    audit::run(cli::AuditCli::parse())
}

fn run_fast(cli: cli::FastCli) -> ExitCode {
    if let Err(message) = validate_fast_output(&cli) {
        eprintln!("rot: error: {message}\n\nFor more information, try '--help'.");
        return ExitCode::from(2);
    }
    if let Some(baseline) = cli.baseline.as_deref() {
        return match diff::compare(&cli, baseline) {
            Ok(comparison) => {
                if let Err(error) = report::render_comparison(&comparison, &cli) {
                    if report::is_broken_pipe(&error) {
                        return ExitCode::SUCCESS;
                    }
                    eprintln!("rot: error: {error:#}");
                    return ExitCode::FAILURE;
                }
                report::render_comparison_diagnostics(&comparison);
                if cli.strict && comparison.has_diagnostics() {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(error) => {
                eprintln!("rot: error: {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    match analyze::analyze(&cli) {
        Ok(report) => {
            if let Err(error) = report::render_snapshot(&report, &cli) {
                if report::is_broken_pipe(&error) {
                    return ExitCode::SUCCESS;
                }
                eprintln!("rot: error: {error:#}");
                return ExitCode::FAILURE;
            }
            report::render_diagnostics(&report);
            if cli.strict && !report.diagnostics.is_empty() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("rot: error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn validate_fast_output(cli: &cli::FastCli) -> Result<(), &'static str> {
    match (cli.format, cli.files, cli.summary_only) {
        (cli::OutputFormat::Json, true, _) => Err(
            "--files is only valid with table output; JSON includes files unless --summary-only is used",
        ),
        (cli::OutputFormat::Table, _, true) => {
            Err("--summary-only is only valid with --format json")
        }
        _ => Ok(()),
    }
}
