use std::collections::BTreeMap;

use serde::Serialize;

pub const TARGET_ROLE_COUNT: usize = 5;
pub const OUTPUT_ROLE_COUNT: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum Activation {
    #[default]
    Never = 0,
    Maybe = 1,
    Always = 2,
}

impl Activation {
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Never, _) | (_, Self::Never) => Self::Never,
            (Self::Always, value) | (value, Self::Always) => value,
            (Self::Maybe, Self::Maybe) => Self::Maybe,
        }
    }

    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Always, _) | (_, Self::Always) => Self::Always,
            (Self::Never, value) | (value, Self::Never) => value,
            (Self::Maybe, Self::Maybe) => Self::Maybe,
        }
    }

    pub fn not(self) -> Self {
        match self {
            Self::Never => Self::Always,
            Self::Maybe => Self::Maybe,
            Self::Always => Self::Never,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reachability {
    pub production: Activation,
    pub test: Activation,
}

impl Reachability {
    pub const NEVER: Self = Self {
        production: Activation::Never,
        test: Activation::Never,
    };

    pub const PRODUCTION: Self = Self {
        production: Activation::Always,
        test: Activation::Never,
    };

    pub const TEST: Self = Self {
        production: Activation::Never,
        test: Activation::Always,
    };

    pub const BOTH: Self = Self {
        production: Activation::Always,
        test: Activation::Always,
    };

    pub fn and(self, other: Self) -> Self {
        Self {
            production: self.production.and(other.production),
            test: self.test.and(other.test),
        }
    }

    pub fn or(self, other: Self) -> Self {
        Self {
            production: self.production.or(other.production),
            test: self.test.or(other.test),
        }
    }

    pub fn index(self) -> usize {
        self.production as usize * 3 + self.test as usize
    }

    pub fn from_index(index: usize) -> Self {
        const VALUES: [Activation; 3] = [Activation::Never, Activation::Maybe, Activation::Always];
        Self {
            production: VALUES[index / 3],
            test: VALUES[index % 3],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum TargetRole {
    Production = 0,
    Test = 1,
    Bench = 2,
    Example = 3,
    Build = 4,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Contexts {
    pub roles: [Reachability; TARGET_ROLE_COUNT],
    pub library_crate: Reachability,
    pub library_api: Reachability,
    pub referenced: bool,
}

impl Contexts {
    pub fn merge(&mut self, other: Self) -> bool {
        let before = *self;
        for (current, incoming) in self.roles.iter_mut().zip(other.roles) {
            *current = current.or(incoming);
        }
        self.library_crate = self.library_crate.or(other.library_crate);
        self.library_api = self.library_api.or(other.library_api);
        self.referenced |= other.referenced;
        *self != before
    }

    pub fn through(self, gate: Reachability, public_module: bool) -> Self {
        let mut roles = self.roles;
        for reachability in &mut roles {
            *reachability = reachability.and(gate);
        }
        Self {
            roles,
            library_crate: self.library_crate.and(gate),
            library_api: if public_module {
                self.library_api.and(gate)
            } else {
                Reachability::NEVER
            },
            referenced: self.referenced,
        }
    }

    pub fn seed(role: TargetRole, reachability: Reachability, library_api: bool) -> Self {
        let mut roles = [Reachability::NEVER; TARGET_ROLE_COUNT];
        roles[role as usize] = reachability;
        Self {
            roles,
            library_crate: if library_api {
                reachability
            } else {
                Reachability::NEVER
            },
            library_api: if library_api {
                reachability
            } else {
                Reachability::NEVER
            },
            referenced: true,
        }
    }

    pub fn classify(self, local: Reachability) -> OutputRole {
        let production = self.roles[TargetRole::Production as usize].and(local);
        match production.production {
            Activation::Always => return OutputRole::Production,
            Activation::Maybe => return OutputRole::Conditional,
            Activation::Never => {}
        }

        let integration_test = self.roles[TargetRole::Test as usize].and(local).test;
        let unit_test = production.test;
        match integration_test.or(unit_test) {
            Activation::Always => return OutputRole::Test,
            Activation::Maybe => return OutputRole::Conditional,
            Activation::Never => {}
        }

        for (target_role, output_role, mode) in [
            (TargetRole::Bench, OutputRole::Bench, true),
            (TargetRole::Example, OutputRole::Example, false),
            (TargetRole::Build, OutputRole::Build, false),
        ] {
            let reachability = self.roles[target_role as usize].and(local);
            let activation = if mode {
                reachability.test
            } else {
                reachability.production
            };
            match activation {
                Activation::Always => return output_role,
                Activation::Maybe => return OutputRole::Conditional,
                Activation::Never => {}
            }
        }

        if self.referenced {
            OutputRole::Inactive
        } else {
            OutputRole::Orphan
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(usize)]
pub enum OutputRole {
    Production = 0,
    Test = 1,
    Bench = 2,
    Example = 3,
    Build = 4,
    Conditional = 5,
    Inactive = 6,
    Orphan = 7,
}

impl OutputRole {
    pub const ALL: [Self; OUTPUT_ROLE_COUNT] = [
        Self::Production,
        Self::Test,
        Self::Bench,
        Self::Example,
        Self::Build,
        Self::Conditional,
        Self::Inactive,
        Self::Orphan,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Production => "Production",
            Self::Test => "Tests",
            Self::Bench => "Benches",
            Self::Example => "Examples",
            Self::Build => "Build",
            Self::Conditional => "Conditional",
            Self::Inactive => "Inactive",
            Self::Orphan => "Orphan",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Test => "test",
            Self::Bench => "bench",
            Self::Example => "example",
            Self::Build => "build",
            Self::Conditional => "conditional",
            Self::Inactive => "inactive",
            Self::Orphan => "orphan",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct LineCounts {
    pub physical: u64,
    pub code: u64,
    pub comments: u64,
    pub docs: u64,
    pub blank: u64,
}

impl LineCounts {
    pub fn add(&mut self, other: Self) {
        self.physical += other.physical;
        self.code += other.code;
        self.comments += other.comments;
        self.docs += other.docs;
        self.blank += other.blank;
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BucketReport {
    pub role: String,
    pub files: u64,
    #[serde(flatten)]
    pub lines: LineCounts,
    pub complexity: u64,
    pub declared_public: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SurfaceReport {
    pub exported_items: u64,
    pub signature_lines: u64,
    pub production_declared_public: u64,
    pub unresolved_public_uses: u64,
    pub unresolved_glob_reexports: u64,
    pub opaque_macro_calls: u64,
    pub unresolved_inherent_public_items: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FileReport {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    pub bytes: u64,
    pub syntax_errors: u64,
    pub buckets: Vec<BucketReport>,
    pub total: LineCounts,
    pub complexity: u64,
    pub surface: SurfaceReport,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileReport {
    pub target: String,
    pub rustc: String,
    pub feature_mode: String,
    pub enabled_features: BTreeMap<String, Vec<String>>,
    pub excluded_features: Vec<String>,
    pub active_cfg: Vec<String>,
    pub forced_cfg: Vec<String>,
    pub forced_unset_cfg: Vec<String>,
    pub additional_test_attributes: Vec<String>,
    pub synthetic: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub root: String,
    pub profile: ProfileReport,
    pub file_count: u64,
    pub bytes: u64,
    pub files: Vec<FileReport>,
    pub buckets: Vec<BucketReport>,
    pub total: LineCounts,
    pub complexity: u64,
    pub surface: SurfaceReport,
    pub diagnostics: Vec<Diagnostic>,
}
