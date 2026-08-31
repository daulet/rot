#[rustfmt::skip]
macro_rules! labelled_enum {
    ($vis:vis enum $name:ident { $first:ident => $first_label:literal $(, $variant:ident => $label:literal)* $(,)? }) => {
        #[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
        #[serde(rename_all = "snake_case")]
        #[repr(usize)]
        $vis enum $name { #[default] $first, $($variant,)* }
        impl $name { pub fn label(self) -> &'static str { match self {
            Self::$first => $first_label, $(Self::$variant => $label,)*
        } } }
    };
}

#[rustfmt::skip]
macro_rules! deref_field {
    ($type:ty => $target:ty, $field:ident) => {
        impl std::ops::Deref for $type {
            type Target = $target;
            fn deref(&self) -> &Self::Target { &self.$field }
        }
        impl std::ops::DerefMut for $type {
            fn deref_mut(&mut self) -> &mut Self::Target { &mut self.$field }
        }
    };
}

mod analyze;
#[cfg(feature = "audit")]
mod audit;
mod cfg;
mod cli;
#[cfg(feature = "audit")]
mod compiler;
mod diff;
mod model;
mod paths;
mod report;
mod revision;
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
    let result = match cli.baseline.as_deref() {
        Some(baseline) => diff::compare(&cli, baseline).and_then(|comparison| {
            report::render_comparison(&comparison, &cli)?;
            report::render_comparison_diagnostics(&comparison);
            Ok(comparison.has_diagnostics())
        }),
        None => analyze::analyze(&cli).and_then(|snapshot| {
            report::render_snapshot(&snapshot, &cli)?;
            report::render_diagnostics(&snapshot);
            Ok(!snapshot.diagnostics.is_empty())
        }),
    };
    match result {
        Ok(true) => ExitCode::FAILURE,
        Ok(false) => ExitCode::SUCCESS,
        Err(error) if report::is_broken_pipe(&error) => ExitCode::SUCCESS,
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
