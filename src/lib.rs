mod analyze;
#[cfg(feature = "audit")]
mod audit;
mod cfg;
mod cli;
#[cfg(feature = "audit")]
mod compiler;
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
    match analyze::analyze(&cli) {
        Ok(report) => {
            if let Err(error) = report::render(&report, &cli) {
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
