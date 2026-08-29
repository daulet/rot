use std::{ops::Deref, path::PathBuf};

use clap::{Args, Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

#[derive(Debug, Args)]
pub struct CargoSelection {
    /// Files or directories to analyze
    #[arg(value_name = "PATH", default_value = ".")]
    pub paths: Vec<PathBuf>,

    /// Enable features (repeat or pass comma-separated values; PACKAGE/FEATURE is accepted)
    #[arg(long, value_delimiter = ',', conflicts_with = "all_features")]
    pub features: Vec<String>,

    /// Enable every declared feature
    #[arg(long, conflicts_with = "features")]
    pub all_features: bool,

    /// Do not enable each package's default feature set
    #[arg(long)]
    pub no_default_features: bool,

    /// Analyze for this target triple
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Force a custom cfg predicate true (NAME or KEY=VALUE)
    #[arg(long, value_name = "PREDICATE")]
    pub cfg: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "rot",
    version,
    about = "Fast, configuration-aware Rust source metrics",
    after_help = "By default rot analyzes Cargo's default feature set. Use --all-features \
                  --exclude-feature NAME to measure an all-except-NAME synthetic profile."
)]
pub struct FastCli {
    #[command(flatten)]
    pub cargo: CargoSelection,

    /// Emit a summary table or versioned JSON
    #[arg(long, value_enum, default_value_t)]
    pub format: OutputFormat,

    /// Include a per-file table below the summary
    #[arg(long)]
    pub files: bool,

    /// Number of analysis workers (defaults to available parallelism)
    #[arg(short = 'j', long, value_name = "N")]
    pub threads: Option<usize>,

    /// Force a feature predicate false after ordinary feature resolution
    #[arg(long, value_delimiter = ',')]
    pub exclude_feature: Vec<String>,

    /// Force a custom cfg predicate false (NAME or KEY=VALUE)
    #[arg(long, value_name = "PREDICATE")]
    pub unset_cfg: Vec<String>,

    /// Recognize an additional test attribute path
    #[arg(long, value_name = "PATH")]
    pub test_attribute: Vec<String>,

    /// Include hidden files and directories (except .git)
    #[arg(long)]
    pub hidden: bool,

    /// Ignore .gitignore and related ignore files
    #[arg(long)]
    pub no_ignore: bool,

    /// Exit unsuccessfully when diagnostics remain
    #[arg(long)]
    pub strict: bool,
}

impl Deref for FastCli {
    type Target = CargoSelection;

    fn deref(&self) -> &Self::Target {
        &self.cargo
    }
}

#[cfg(feature = "audit")]
#[derive(Debug, Parser)]
#[command(
    name = "rot-audit",
    version,
    about = "Compiler-proven Rust visibility audit",
    after_help = "The selected Cargo targets are the audit's closed world. Build scripts and \
                  procedural macros execute under the pinned toolchain."
)]
pub struct AuditCli {
    #[command(flatten)]
    pub cargo: CargoSelection,

    /// Emit actionable text or versioned JSON
    #[arg(long, value_enum, default_value_t)]
    pub format: OutputFormat,

    /// Path to the pinned rot rustc wrapper
    #[arg(long, value_name = "PATH")]
    pub driver: Option<PathBuf>,

    /// Require Cargo.lock to remain unchanged
    #[arg(long)]
    pub locked: bool,

    /// Forbid network access
    #[arg(long)]
    pub offline: bool,

    /// Parent for the temporary isolated Cargo target and build directories
    #[arg(long, value_name = "DIR")]
    pub scratch_dir: Option<PathBuf>,
}

#[cfg(feature = "audit")]
impl Deref for AuditCli {
    type Target = CargoSelection;

    fn deref(&self) -> &Self::Target {
        &self.cargo
    }
}
