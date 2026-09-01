use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rot_release::{Version, emit_plan, parse_version, plan_release, set_version};

#[derive(Debug, Parser)]
#[command(
    name = "rot-release",
    about = "Plan and materialize Rot releases without treating tags as version input"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Plan a new release or recover an existing generated release.
    Plan {
        /// Repository root.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Commit that triggered release planning.
        #[arg(long)]
        source: String,

        /// Remote branch or commit containing generated release commits.
        #[arg(long, default_value = "origin/main")]
        remote_ref: String,

        /// Append flat key=value output for GitHub Actions instead of JSON.
        #[arg(long)]
        github_output: Option<PathBuf>,
    },

    /// Synchronize the workspace version authority and lockfiles.
    SetVersion {
        /// New semantic version.
        #[arg(value_parser = parse_version)]
        version: Version,

        /// Repository root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("release: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Plan {
            root,
            source,
            remote_ref,
            github_output,
        } => {
            let root = root.canonicalize().map_err(|error| {
                anyhow::anyhow!("could not resolve {}: {error}", root.display())
            })?;
            let plan = plan_release(&root, &source, &remote_ref)?;
            emit_plan(&plan, github_output.as_deref())
        }
        Command::SetVersion { version, root } => {
            let root = root.canonicalize().map_err(|error| {
                anyhow::anyhow!("could not resolve {}: {error}", root.display())
            })?;
            set_version(&root, version)
        }
    }
}
