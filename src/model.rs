use std::collections::BTreeMap;

use serde::Serialize;

pub const TARGET_ROLE_COUNT: usize = 5;
pub const OUTPUT_ROLE_COUNT: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Activation {
    #[default]
    Never = 0,
    Maybe = 1,
    Always = 2,
}

impl Activation {
    pub fn and(self, other: Self) -> Self {
        self.min(other)
    }

    pub fn or(self, other: Self) -> Self {
        self.max(other)
    }

    pub fn not(self) -> Self {
        [Self::Always, Self::Maybe, Self::Never][self as usize]
    }
}

impl From<bool> for Activation {
    fn from(active: bool) -> Self {
        [Self::Never, Self::Always][active as usize]
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reachability {
    pub production: Activation,
    pub test: Activation,
}

impl Reachability {
    pub const NEVER: Self = Self::new(Activation::Never, Activation::Never);
    pub const PRODUCTION: Self = Self::new(Activation::Always, Activation::Never);
    pub const TEST: Self = Self::new(Activation::Never, Activation::Always);
    pub const BOTH: Self = Self::new(Activation::Always, Activation::Always);

    const fn new(production: Activation, test: Activation) -> Self {
        Self { production, test }
    }

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
    pub referenced: bool,
}

impl Contexts {
    pub fn merge(&mut self, other: Self) -> bool {
        let before = *self;
        for (current, incoming) in self.roles.iter_mut().zip(other.roles) {
            *current = current.or(incoming);
        }
        self.referenced |= other.referenced;
        *self != before
    }

    pub fn through(self, gate: Reachability) -> Self {
        let mut roles = self.roles;
        for reachability in &mut roles {
            *reachability = reachability.and(gate);
        }
        Self {
            roles,
            referenced: self.referenced,
        }
    }

    pub fn seed(role: TargetRole, reachability: Reachability) -> Self {
        let mut roles = [Reachability::NEVER; TARGET_ROLE_COUNT];
        roles[role as usize] = reachability;
        Self {
            roles,
            referenced: true,
        }
    }

    pub fn production() -> Self {
        Self::seed(TargetRole::Production, Reachability::BOTH)
    }

    pub fn classify(self, local: Reachability) -> OutputRole {
        let production = self.roles[TargetRole::Production as usize].and(local);
        let example = self.roles[TargetRole::Example as usize].and(local);
        let integration_test = self.roles[TargetRole::Test as usize].and(local).test;
        let bench = self.roles[TargetRole::Bench as usize].and(local).test;
        let build = self.roles[TargetRole::Build as usize].and(local).production;
        for (role, activation) in [
            (OutputRole::Production, production.production),
            (OutputRole::Example, example.production),
            (
                OutputRole::Test,
                integration_test.or(production.test).or(example.test),
            ),
            (OutputRole::Bench, bench),
            (OutputRole::Build, build),
        ] {
            match activation {
                Activation::Always => return role,
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

#[rustfmt::skip]
labelled_enum! { pub enum OutputRole {
    Production => "Production", Test => "Tests", Bench => "Benches", Example => "Examples", Build => "Build",
    Conditional => "Conditional", Inactive => "Inactive", Orphan => "Orphan",
} }

impl OutputRole {
    #[rustfmt::skip]
    pub const ALL: [Self; OUTPUT_ROLE_COUNT] = [Self::Production, Self::Test, Self::Bench, Self::Example, Self::Build, Self::Conditional, Self::Inactive, Self::Orphan];

    pub fn bucket(self, buckets: &[BucketReport]) -> Option<&BucketReport> {
        buckets.iter().find(|bucket| bucket.role == self)
    }
}

macro_rules! metric_group {
    ($name:ident { $($field:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
        pub struct $name {
            $(pub $field: u64,)+
        }

        impl $name {
            pub fn add(&mut self, other: Self) {
                $(self.$field += other.$field;)+
            }
        }
    };
}

#[rustfmt::skip]
metric_group!(LineCounts { physical, code, comments, docs, blank });
#[rustfmt::skip]
metric_group!(ComplexityMetrics { lexical_complexity, cyclomatic_authored, cognitive_authored });

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SourceMetrics {
    #[serde(flatten)]
    pub lines: LineCounts,
    #[serde(flatten)]
    pub metrics: ComplexityMetrics,
    pub declared_public: u64,
}

impl SourceMetrics {
    pub fn total(lines: LineCounts, metrics: ComplexityMetrics, buckets: &[BucketReport]) -> Self {
        Self {
            lines,
            metrics,
            declared_public: buckets.iter().map(|bucket| bucket.declared_public).sum(),
        }
    }

    pub fn add(&mut self, other: Self) {
        self.lines.add(other.lines);
        self.metrics.add(other.metrics);
        self.declared_public += other.declared_public;
    }

    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BucketReport {
    pub role: OutputRole,
    pub files: u64,
    #[serde(flatten)]
    pub source: SourceMetrics,
}

deref_field!(BucketReport => SourceMetrics, source);

impl BucketReport {
    pub fn add(&mut self, other: &Self) {
        self.files += other.files;
        self.source.add(other.source);
    }

    pub fn is_empty(&self) -> bool {
        self.files == 0 && self.source.is_empty()
    }
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
    #[serde(flatten)]
    pub metrics: ComplexityMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProfileReport {
    pub target: String,
    pub rustc: String,
    pub cfg_preset: &'static str,
    pub cfg_resolution: &'static str,
    pub feature_mode: &'static str,
    pub feature_resolution: &'static str,
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

#[cfg(feature = "audit")]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticStatus {
    Complete,
    Partial,
    Unavailable,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Serialize)]
pub struct CompilerTargetReport {
    pub package_id: String,
    pub name: String,
    pub kinds: Vec<String>,
    pub crate_types: Vec<String>,
    pub source: String,
    pub role: String,
    pub compilation_context: String,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompilerDefinitionIdReport {
    pub stable_crate_id: String,
    pub local_hash: String,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CompilerSourceSpanReport {
    pub path: String,
    pub source_hash: String,
    pub generated: bool,
    pub start_byte: u64,
    pub end_byte: u64,
    pub line: u64,
    pub column: u64,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RequiredVisibilityDefinitionReport {
    pub package_id: String,
    pub crate_name: String,
    pub representative_invocation: String,
    pub representative_id: CompilerDefinitionIdReport,
    pub definition_path: String,
    pub kind: String,
    pub current_visibility: String,
    pub required_visibility: String,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<CompilerSourceSpanReport>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RequiredVisibilityReport {
    pub scope: String,
    pub evidence_exclusions: Vec<String>,
    pub definitions: Vec<RequiredVisibilityDefinitionReport>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClosedWorldSummaryReport {
    pub definition_nodes: u64,
    pub reference_edges: u64,
    pub production_roots: u64,
    pub nonproduction_roots: u64,
    pub production_live: u64,
    pub nonproduction_live: u64,
    pub public_candidates: u64,
    pub dead_public: u64,
    pub unnecessary_public: u64,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClosedWorldFindingReport {
    pub kind: String,
    pub reason: String,
    pub package_id: String,
    pub crate_name: String,
    pub representative_invocation: String,
    pub representative_id: CompilerDefinitionIdReport,
    pub definition_path: String,
    pub definition_kind: String,
    pub production_live: bool,
    pub nonproduction_live: bool,
    pub test_compiled_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<CompilerSourceSpanReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution_callsite: Option<CompilerSourceSpanReport>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClosedWorldReport {
    pub scope: String,
    pub evidence_exclusions: Vec<String>,
    pub summary: ClosedWorldSummaryReport,
    pub findings: Vec<ClosedWorldFindingReport>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactQueryReport {
    pub package: String,
    pub definition_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactDefinitionReport {
    pub package_id: String,
    pub package_name: String,
    pub crate_name: String,
    pub target_name: String,
    pub definition_path: String,
    pub definition_kind: String,
    pub expansion_origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<CompilerSourceSpanReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution_callsite: Option<CompilerSourceSpanReport>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactProvenanceClass {
    Production,
    Nonproduction,
    BuildTime,
    PublicInterface,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactProvenanceReport {
    pub class: ImpactProvenanceClass,
    pub package_id: String,
    pub target_name: String,
    pub target_role: String,
    pub compilation_context: String,
}

#[cfg(feature = "audit")]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactVisibilityDisposition {
    RequiredPublic,
    NarrowablePublic,
    DeadPublic,
    NotPublicCandidate,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactReferenceReport {
    pub consumer: ImpactDefinitionReport,
    pub dependency: ImpactDefinitionReport,
    pub reference_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub representative_span: Option<CompilerSourceSpanReport>,
    pub provenance: ImpactProvenanceReport,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactReferenceStepReport {
    pub from: ImpactDefinitionReport,
    pub to: ImpactDefinitionReport,
    pub reference_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub representative_span: Option<CompilerSourceSpanReport>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactWitnessReport {
    pub provenance: ImpactProvenanceReport,
    pub root: ImpactDefinitionReport,
    pub root_reason: String,
    pub steps: Vec<ImpactReferenceStepReport>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactSummaryReport {
    pub direct_reference_relationships: u64,
    pub transitive_consumers: u64,
    pub production: bool,
    pub nonproduction: bool,
    pub build_time: bool,
    pub public_interface: bool,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImpactReport {
    pub status: SemanticStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub scope: String,
    pub evidence_exclusions: Vec<String>,
    pub query: ImpactQueryReport,
    pub candidates: Vec<ImpactDefinitionReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<ImpactDefinitionReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility_disposition: Option<ImpactVisibilityDisposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ImpactSummaryReport>,
    pub direct_references: Vec<ImpactReferenceReport>,
    pub witnesses: Vec<ImpactWitnessReport>,
    pub reference_site_note: String,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Serialize)]
pub struct CompilerInvocationReport {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<CompilerTargetReport>,
    pub crate_name: String,
    pub target_triple: String,
    pub compilation_context: String,
    pub test: bool,
    pub features: Vec<String>,
    pub cfg: Vec<String>,
    pub definitions: u64,
    pub public_bindings: u64,
    pub roots: u64,
    pub references: u64,
    pub status: SemanticStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug, Serialize)]
pub struct CompilerReport {
    pub protocol_version: u32,
    pub driver_version: String,
    pub rustc_version: String,
    pub rustc_commit: String,
    pub rustc_commit_date: String,
    pub rustc_host: String,
    pub expected_invocations: u64,
    pub collected_invocations: u64,
    pub correlated_invocations: u64,
    pub invocations: Vec<CompilerInvocationReport>,
    pub status: SemanticStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_visibility: Option<RequiredVisibilityReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_world: Option<ClosedWorldReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_surface: Option<crate::compiler::api_surface::ApiSurfaceReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<ImpactReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub root: String,
    pub selection: SelectionReport,
    pub profile: ProfileReport,
    pub file_count: u64,
    pub bytes: u64,
    #[serde(skip)]
    pub files: Vec<FileReport>,
    pub buckets: Vec<BucketReport>,
    pub total: LineCounts,
    #[serde(flatten)]
    pub metrics: ComplexityMetrics,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionReport {
    pub paths: Vec<SelectedPathReport>,
    pub include_hidden: bool,
    pub respect_ignores: bool,
    pub ignore_boundary: &'static str,
}

impl SelectionReport {
    pub fn new(
        mut paths: Vec<SelectedPathReport>,
        include_hidden: bool,
        respect_ignores: bool,
    ) -> Self {
        paths.sort();
        paths.dedup();
        Self {
            paths,
            include_hidden,
            respect_ignores,
            ignore_boundary: "path",
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SelectedPathReport {
    pub path: String,
    pub kind: SelectedPathKind,
}

#[rustfmt::skip]
labelled_enum! { pub enum SelectedPathKind { File => "file", Directory => "directory" } }
