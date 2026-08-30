use std::path::PathBuf;

use clap::{Args, Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

#[derive(Clone, Debug, Args)]
pub struct CargoSelection {
    /// Rust file or directory to analyze (repeatable; this selects input files)
    #[arg(value_name = "PATH", default_value = ".")]
    pub paths: Vec<PathBuf>,

    /// Enable PATH-root features or QUALIFIER/FEATURE for a reachable package/direct dependency
    #[arg(
        long,
        value_delimiter = ',',
        conflicts_with = "all_features",
        help_heading = "FEATURE SELECTION"
    )]
    pub features: Vec<String>,

    /// Enable every declared feature
    #[arg(long, conflicts_with = "features", help_heading = "FEATURE SELECTION")]
    pub all_features: bool,

    /// Do not enable the selected PATH-root packages' default feature sets
    #[arg(long, help_heading = "FEATURE SELECTION")]
    pub no_default_features: bool,

    /// Analyze for this target triple
    #[arg(long, value_name = "TRIPLE", help_heading = "CONFIGURATION")]
    pub target: Option<String>,

    /// Force a custom cfg predicate true (NAME or KEY=VALUE)
    #[arg(long, value_name = "PREDICATE", help_heading = "CONFIGURATION")]
    pub cfg: Vec<String>,
}

impl CargoSelection {
    pub fn feature_mode(&self, exclusions: bool) -> &'static str {
        match (
            self.all_features,
            self.no_default_features,
            self.features.is_empty(),
            exclusions,
        ) {
            (true, _, _, false) => "all",
            (true, _, _, true) => "all_except",
            (false, true, true, _) => "none",
            (false, true, false, _) => "selected_without_defaults",
            (false, false, true, _) => "default",
            (false, false, false, _) => "default_plus_selected",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "rot",
    version,
    about = "Fast, configuration-aware Rust source metrics",
    long_about = "Fast, configuration-aware Rust source metrics for humans and coding agents.\n\
                  Counts Rust only; Markdown and other files are ignored.",
    after_help = include_str!("fast-help.txt")
)]
#[derive(Clone)]
pub struct FastCli {
    #[command(flatten)]
    pub cargo: CargoSelection,

    /// Emit a human table or deterministic, versioned JSON
    #[arg(long, value_enum, default_value_t, help_heading = "OUTPUT")]
    pub format: OutputFormat,

    /// Show every per-file row in human table output; does not select input files
    #[arg(long, conflicts_with = "summary_only", help_heading = "OUTPUT")]
    pub files: bool,

    /// Omit per-file records from JSON without changing aggregate values
    #[arg(long, conflicts_with = "files", help_heading = "OUTPUT")]
    pub summary_only: bool,

    /// Compare one Git commit with live Rust discovered under PATH-local ignore rules
    #[arg(long, value_name = "REF", help_heading = "COMPARISON")]
    pub baseline: Option<String>,

    /// Number of analysis workers (defaults to available parallelism)
    #[arg(short = 'j', long, value_name = "N", help_heading = "AUTOMATION")]
    pub threads: Option<usize>,

    /// Force a PATH-root or qualified active feature false; never activates dependencies
    #[arg(long, value_delimiter = ',', help_heading = "FEATURE SELECTION")]
    pub exclude_feature: Vec<String>,

    /// Force a custom cfg predicate false (NAME or KEY=VALUE)
    #[arg(long, value_name = "PREDICATE", help_heading = "CONFIGURATION")]
    pub unset_cfg: Vec<String>,

    /// Use the release preset for rustc's built-in debug_assertions cfg only
    #[arg(short = 'r', long, help_heading = "CONFIGURATION")]
    pub release: bool,

    /// Recognize an additional test attribute path
    #[arg(long, value_name = "PATH", help_heading = "CONFIGURATION")]
    pub test_attribute: Vec<String>,

    /// Include hidden files and directories (except .git)
    #[arg(long, help_heading = "DISCOVERY")]
    pub hidden: bool,

    /// Ignore .gitignore and related ignore files
    #[arg(long, help_heading = "DISCOVERY")]
    pub no_ignore: bool,

    /// Exit unsuccessfully when analysis diagnostics remain (profile controls are not diagnostics)
    #[arg(long, help_heading = "AUTOMATION")]
    pub strict: bool,
}

deref_field!(FastCli => CargoSelection, cargo);

#[cfg(feature = "audit")]
#[derive(Debug, Parser)]
#[command(
    name = "rot-audit",
    version,
    about = "Compiler-proven Rust visibility audit",
    long_about = "Compiler-proven visibility findings for deliberate refactoring.\n\
                  This is a real Cargo/rustc build, not a source-metrics mode.",
    after_help = "EXAMPLES:\n  \
                  rot-audit . --locked --offline --driver PATH\n  \
                  rot-audit . --all-features --format json --driver PATH\n  \
                  rot-audit . --toolchain 1.98.0 --driver PATH --locked --offline\n\n\
                  --driver is required; Rot does not guess a path or read an environment fallback.\n\
                  --toolchain accepts only compiler identities in compiler/supported-rustc.toml.\n\
                  The driver must be built by that exact rustc release, commit, and host.\n\
                  The selected compiled Cargo targets are the audit's closed world; doctests and\n\
                  targets disabled by required-features are explicitly excluded. Missing compiler\n\
                  evidence fails closed and is never reported as zero findings. Build scripts and\n\
                  procedural macros execute under the selected exact toolchain. Rot removes ambient\n\
                  RUSTC_BOOTSTRAP before that build; explicit Cargo user/project configuration remains\n\
                  trusted. JSON is written to stdout and diagnostics to stderr."
)]
pub struct AuditCli {
    #[command(flatten)]
    pub cargo: CargoSelection,

    /// Emit actionable text or versioned JSON
    #[arg(long, value_enum, default_value_t, help_heading = "OUTPUT")]
    pub format: OutputFormat,

    /// Path to a rot driver built with the exact selected toolchain
    #[arg(long, value_name = "PATH", help_heading = "COMPILER")]
    pub driver: PathBuf,

    /// Verified exact rustup toolchain used by Cargo and the matching driver
    #[arg(
        long,
        value_name = "TOOLCHAIN",
        default_value = "nightly-2026-08-27",
        help_heading = "COMPILER"
    )]
    pub toolchain: String,

    /// Require Cargo.lock to remain unchanged
    #[arg(long, help_heading = "SAFETY")]
    pub locked: bool,

    /// Forbid network access
    #[arg(long, help_heading = "SAFETY")]
    pub offline: bool,

    /// Parent for the temporary isolated Cargo target and build directories
    #[arg(long, value_name = "DIR", help_heading = "COMPILER")]
    pub scratch_dir: Option<PathBuf>,
}

#[cfg(feature = "audit")]
deref_field!(AuditCli => CargoSelection, cargo);
