mod analyze;
mod cfg;
mod cli;
mod model;
mod report;
mod source;
mod workspace;

use std::{io, process::ExitCode};

use clap::Parser;

use crate::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match analyze::analyze(&cli) {
        Ok(report) => {
            if let Err(error) = report::render(&report, &cli) {
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
                {
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
