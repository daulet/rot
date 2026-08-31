use std::path::PathBuf;

#[cfg(feature = "audit")]
use std::str::FromStr;

use clap::{Args, Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

#[derive(Clone, Debug, Args)]
pub struct CargoSelection {
    /// Rust file or directory scope (repeatable)
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
}

deref_field!(FastCli => CargoSelection, cargo);

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Parser)]
#[command(
    name = "rot-audit",
    version,
    about = "Compiler-proven Rust API and dependency audit",
    long_about = "Compiler-proven visibility, public-API topology, and dependency-impact evidence for deliberate refactoring.\n\
                  This is a real Cargo/rustc build, not a source-metrics mode. PATH selects complete\n\
                  Cargo packages; it does not restrict analysis to declarations in one source file.",
    after_help = "EXAMPLES:\n  \
                  rot-audit . --locked --offline --driver PATH\n  \
                  rot-audit . --baseline HEAD~1 --locked --offline --driver PATH\n  \
                  rot-audit . --explain 'rot-compiler-protocol:PublicBinding' --driver PATH\n  \
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
                  trusted. --baseline performs two isolated real builds. API comparison covers public\n\
                  name/topology changes, not signature or semver compatibility. --explain accepts an\n\
                  exact Cargo package and rustc definition path. JSON is written to stdout and\n\
                  diagnostics to stderr."
)]
pub struct AuditCli {
    #[command(flatten)]
    pub cargo: CargoSelection,

    /// Emit actionable text or versioned JSON
    #[arg(long, value_enum, default_value_t, help_heading = "OUTPUT")]
    pub format: OutputFormat,

    /// Compare public API topology at one Git commit with the live working tree
    #[arg(
        long,
        value_name = "REF",
        conflicts_with = "explain",
        help_heading = "DEEP ANALYSIS"
    )]
    pub baseline: Option<String>,

    /// Explain consumers of one exact PACKAGE:DEFINITION_PATH
    #[arg(
        long,
        value_name = "PACKAGE:DEFINITION_PATH",
        conflicts_with = "baseline",
        help_heading = "DEEP ANALYSIS"
    )]
    pub explain: Option<ExplainSelector>,

    /// Disambiguate --explain with an exact PATH:LINE:COLUMN start location
    #[arg(
        long,
        value_name = "PATH:LINE:COLUMN",
        requires = "explain",
        help_heading = "DEEP ANALYSIS"
    )]
    pub explain_at: Option<ExplainLocation>,

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplainSelector {
    pub package: String,
    pub definition_path: String,
}

#[cfg(feature = "audit")]
impl FromStr for ExplainSelector {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (package, definition_path) = value.split_once(':').ok_or_else(|| {
            "expected PACKAGE:DEFINITION_PATH, for example rot:rot::compiler::collect".to_owned()
        })?;
        if package.trim().is_empty() || definition_path.trim().is_empty() {
            return Err("package and definition path must both be non-empty".to_owned());
        }
        if package != package.trim() || definition_path != definition_path.trim() {
            return Err(
                "package and definition path must not have surrounding whitespace".to_owned(),
            );
        }
        Ok(Self {
            package: package.to_owned(),
            definition_path: definition_path.to_owned(),
        })
    }
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplainLocation {
    pub path: String,
    pub line: u64,
    pub column: u64,
}

#[cfg(feature = "audit")]
impl FromStr for ExplainLocation {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (path_and_line, column) = value
            .rsplit_once(':')
            .ok_or_else(|| "expected PATH:LINE:COLUMN".to_owned())?;
        let (path, line) = path_and_line
            .rsplit_once(':')
            .ok_or_else(|| "expected PATH:LINE:COLUMN".to_owned())?;
        let line = line
            .parse::<u64>()
            .map_err(|_| "LINE must be a positive integer".to_owned())?;
        let column = column
            .parse::<u64>()
            .map_err(|_| "COLUMN must be a positive integer".to_owned())?;
        if path.is_empty() || line == 0 || column == 0 {
            return Err("PATH must be non-empty and LINE/COLUMN must be positive".to_owned());
        }
        Ok(Self {
            path: path.replace('\\', "/"),
            line,
            column,
        })
    }
}

#[cfg(feature = "audit")]
deref_field!(AuditCli => CargoSelection, cargo);
