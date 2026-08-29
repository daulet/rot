use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "rot",
    version,
    about = "Fast, configuration-aware Rust source metrics",
    after_help = "By default rot analyzes Cargo's default feature set. Use --all-features \
                  --exclude-feature NAME to measure an all-except-NAME synthetic profile."
)]
pub struct Cli {
    /// Files or directories to analyze
    #[arg(value_name = "PATH", default_value = ".")]
    pub paths: Vec<PathBuf>,

    /// Emit a summary table or versioned JSON
    #[arg(long, value_enum, default_value_t)]
    pub format: OutputFormat,

    /// Include a per-file table below the summary
    #[arg(long)]
    pub files: bool,

    /// Number of analysis workers (defaults to available parallelism)
    #[arg(short = 'j', long, value_name = "N")]
    pub threads: Option<usize>,

    /// Enable features (repeat or pass comma-separated values; PACKAGE/FEATURE is accepted)
    #[arg(long, value_delimiter = ',', conflicts_with = "all_features")]
    pub features: Vec<String>,

    /// Enable every declared feature before applying exclusions
    #[arg(long, conflicts_with = "features")]
    pub all_features: bool,

    /// Do not enable each package's default feature set
    #[arg(long)]
    pub no_default_features: bool,

    /// Force a feature predicate false after ordinary feature resolution
    #[arg(long, value_delimiter = ',')]
    pub exclude_feature: Vec<String>,

    /// Analyze built-in cfg predicates for this target triple
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,

    /// Force a custom cfg predicate true (NAME or KEY=VALUE)
    #[arg(long, value_name = "PREDICATE")]
    pub cfg: Vec<String>,

    /// Force a custom cfg predicate false (NAME or KEY=VALUE)
    #[arg(long, value_name = "PREDICATE")]
    pub unset_cfg: Vec<String>,

    /// Recognize an additional test attribute path
    #[arg(long, value_name = "PATH")]
    pub test_attribute: Vec<String>,

    /// Run pinned Cargo/rustc semantics; executes project build scripts and proc macros
    #[arg(long)]
    pub compiler: bool,

    /// Path to the pinned rot rustc wrapper
    #[arg(long, value_name = "PATH", requires = "compiler")]
    pub compiler_driver: Option<PathBuf>,

    /// Require Cargo.lock to remain unchanged in compiler mode
    #[arg(long, requires = "compiler")]
    pub locked: bool,

    /// Forbid network access in compiler mode
    #[arg(long, requires = "compiler")]
    pub offline: bool,

    /// Parent for a temporary isolated compiler target/build directory
    #[arg(long, value_name = "DIR", requires = "compiler")]
    pub compiler_target_dir: Option<PathBuf>,

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
