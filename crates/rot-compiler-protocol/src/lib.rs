use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 5;
pub const DRIVER_VERSION: u32 = 5;
pub const MAX_SIDECAR_BYTES: u64 = 64 * 1024 * 1024;

pub const RUN_ID_ENV: &str = "ROT_COMPILER_RUN_ID";
pub const SIDECAR_DIR_ENV: &str = "ROT_COMPILER_SIDECAR_DIR";
pub const SELECTED_MANIFEST_DIRS_ENV: &str = "ROT_COMPILER_SELECTED_MANIFEST_DIRS";
pub const TARGET_DIR_ENV: &str = "ROT_COMPILER_TARGET_DIR";
pub const BUILD_DIR_ENV: &str = "ROT_COMPILER_BUILD_DIR";
pub const HANDSHAKE_ARG: &str = "--rot-handshake";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RunId(pub String);

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct InvocationId(pub String);

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct InvocationMergeKey(pub String);

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct FactId(pub String);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CompilerDefId {
    pub stable_crate_id: u64,
    pub local_hash: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SourceFileKey(pub String);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompilerIdentity {
    pub release: String,
    pub commit_hash: String,
    pub commit_date: String,
    pub host: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Handshake {
    pub protocol_version: u32,
    pub driver_version: u32,
    pub linked_rustc_version: String,
    pub rustc: CompilerIdentity,
    pub max_sidecar_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Record {
    pub protocol_version: u32,
    pub run_id: RunId,
    pub invocation_id: InvocationId,
    pub sequence: u64,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
// Records are decoded and handled one at a time. Boxing the larger wire payloads
// would add an allocation to every invocation without changing the JSON format.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    InvocationStarted(InvocationStarted),
    Profile(Profile),
    SourceFile(SourceFile),
    Definition(Definition),
    PublicBinding(PublicBinding),
    Root(Root),
    Reference(Reference),
    ProductStatus(ProductStatus),
    Diagnostic(Diagnostic),
    InvocationFinished(InvocationFinished),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InvocationStarted {
    pub merge_key: InvocationMergeKey,
    pub compiler: CompilerIdentity,
    pub process_id: u32,
    pub rustc_path: String,
    pub working_directory: String,
    pub manifest_dir: Option<String>,
    #[serde(default)]
    pub build_script_out_dir: Option<String>,
    pub package_name: Option<String>,
    pub primary_package: bool,
    pub test_mode: bool,
    pub target_triple: String,
    pub compilation_context: CompilationContext,
    pub crate_name: String,
    pub input: Option<String>,
    pub artifact: ArtifactIdentity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilationContext {
    Host,
    Target,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactIdentity {
    pub out_dir: Option<String>,
    pub crate_name: String,
    pub crate_types: Vec<String>,
    pub extra_filename: Option<String>,
    pub metadata: Option<String>,
    pub emit: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Profile {
    pub host_triple: String,
    pub target_triple: String,
    pub test_mode: bool,
    pub cfg: Vec<CfgValue>,
    pub features: Vec<String>,
    pub codegen: CodegenProfile,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CfgValue {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CodegenProfile {
    pub optimization: OptimizationLevel,
    pub panic: PanicStrategy,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub codegen_units: usize,
    pub target_cpu: String,
    pub target_features: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizationLevel {
    None,
    Less,
    More,
    Aggressive,
    Size,
    SizeMin,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanicStrategy {
    Unwind,
    Abort,
    ImmediateAbort,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceFile {
    pub key: SourceFileKey,
    pub local_path: Option<String>,
    pub remapped_path: String,
    pub source_hash_algorithm: String,
    pub source_hash: String,
    pub byte_len: u32,
    pub generated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SourceSpan {
    pub file: SourceFileKey,
    pub start: u32,
    pub end: u32,
    /// One-based source line containing `start`.
    pub line: u32,
    /// One-based Unicode scalar column containing `start`.
    pub column: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Definition {
    pub id: FactId,
    pub compiler_id: CompilerDefId,
    pub parent: Option<CompilerDefId>,
    pub name: Option<String>,
    pub definition_path: String,
    pub kind: DefinitionKind,
    pub visibility_editable: bool,
    pub nominal_visibility: NominalVisibility,
    pub externally_reachable: bool,
    pub span: Option<SourceSpan>,
    pub attribution_callsite: Option<SourceSpan>,
    pub expansion_origin: ExpansionOrigin,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublicBinding {
    pub id: FactId,
    pub parent: CompilerDefId,
    pub target: CompilerDefId,
    pub name: String,
    pub namespace: Namespace,
    pub exposure: Exposure,
    pub exposing_import: Option<CompilerDefId>,
    pub span: Option<SourceSpan>,
    pub resolved_target_path: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Namespace {
    Type,
    Value,
    Macro,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    Direct,
    SingleReexport,
    GlobReexport,
    ExternCrate,
    MacroUse,
    MacroExport,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionKind {
    Crate,
    Module,
    Struct,
    Union,
    Enum,
    Variant,
    Trait,
    TypeAlias,
    ForeignType,
    TraitAlias,
    AssociatedType,
    Function,
    Constant,
    Static,
    Constructor,
    AssociatedFunction,
    AssociatedConstant,
    Macro,
    ExternCrate,
    Import,
    ForeignModule,
    OpaqueType,
    Field,
    Implementation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NominalVisibility {
    Public,
    Restricted(CompilerDefId),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpansionOrigin {
    Authored,
    BuiltinDesugaring,
    LocalMacro,
    ExternalMacro,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Root {
    pub id: FactId,
    pub definition: CompilerDefId,
    pub kind: RootKind,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    EntryPoint,
    Conservative,
    RequiredPublic,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Reference {
    pub id: FactId,
    pub from: CompilerDefId,
    pub to: CompilerDefId,
    pub kind: ReferenceKind,
    pub span: Option<SourceSpan>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Body,
    Interface,
    Reexport,
    VisibilityParent,
    VisibilityRequirement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProductStatus {
    pub product: Product,
    pub availability: Availability,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    SemanticGraph,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub phase: DiagnosticPhase,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub span: Option<SourceSpan>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticPhase {
    Invocation,
    Analysis,
    Sidecar,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InvocationFinished {
    pub rustc_success: bool,
    pub analysis_reached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiler_id(stable_crate_id: u64, local_hash: u64) -> CompilerDefId {
        CompilerDefId {
            stable_crate_id,
            local_hash,
        }
    }

    #[test]
    fn record_is_self_versioned_and_round_trips() {
        let record = Record {
            protocol_version: PROTOCOL_VERSION,
            run_id: RunId("run".to_owned()),
            invocation_id: InvocationId("invocation".to_owned()),
            sequence: 4,
            event: Event::ProductStatus(ProductStatus {
                product: Product::SemanticGraph,
                availability: Availability::Complete,
                message: None,
            }),
        };

        let encoded = serde_json::to_string(&record).unwrap();
        assert!(encoded.contains(&format!("\"protocol_version\":{PROTOCOL_VERSION}")));
        assert!(encoded.contains("\"event\":\"product_status\""));
        assert!(encoded.contains("\"product\":\"semantic_graph\""));
        assert_eq!(serde_json::from_str::<Record>(&encoded).unwrap(), record);
    }

    #[test]
    fn public_binding_is_one_finite_resolved_namespace_edge() {
        let binding = PublicBinding {
            id: FactId("binding-0".to_owned()),
            parent: compiler_id(1, 2),
            target: compiler_id(3, 4),
            name: "Alias".to_owned(),
            namespace: Namespace::Type,
            exposure: Exposure::GlobReexport,
            exposing_import: Some(compiler_id(1, 5)),
            span: None,
            resolved_target_path: "dependency::Original".to_owned(),
        };

        let encoded = serde_json::to_string(&binding).unwrap();
        assert!(!encoded.contains("segments"));
        assert!(encoded.contains("\"resolved_target_path\":\"dependency::Original\""));
        assert_eq!(
            serde_json::from_str::<PublicBinding>(&encoded).unwrap(),
            binding
        );
    }

    #[test]
    fn compiler_ids_preserve_crate_and_local_identity() {
        let local = compiler_id(7, 11);
        let other_crate = compiler_id(8, 11);

        assert_ne!(local, other_crate);
        assert_eq!(
            serde_json::to_string(&local).unwrap(),
            r#"{"stable_crate_id":7,"local_hash":11}"#
        );
        assert_eq!(
            serde_json::to_string(&CompilationContext::Target).unwrap(),
            r#""target""#
        );
    }

    #[test]
    fn invocation_accepts_an_absent_build_script_out_dir() {
        let started = InvocationStarted {
            merge_key: InvocationMergeKey("merge".to_owned()),
            compiler: CompilerIdentity {
                release: "nightly".to_owned(),
                commit_hash: "commit".to_owned(),
                commit_date: "date".to_owned(),
                host: "host".to_owned(),
            },
            process_id: 7,
            rustc_path: "/toolchain/rustc".to_owned(),
            working_directory: "/workspace".to_owned(),
            manifest_dir: Some("/workspace/crate".to_owned()),
            build_script_out_dir: Some("/workspace/target/out".to_owned()),
            package_name: Some("crate".to_owned()),
            primary_package: true,
            test_mode: false,
            target_triple: "target".to_owned(),
            compilation_context: CompilationContext::Target,
            crate_name: "crate".to_owned(),
            input: Some("/workspace/crate/src/lib.rs".to_owned()),
            artifact: ArtifactIdentity {
                out_dir: Some("/workspace/target/deps".to_owned()),
                crate_name: "crate".to_owned(),
                crate_types: vec!["lib".to_owned()],
                extra_filename: None,
                metadata: None,
                emit: vec!["metadata".to_owned()],
            },
        };
        let mut encoded = serde_json::to_value(started).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .remove("build_script_out_dir");

        let decoded = serde_json::from_value::<InvocationStarted>(encoded).unwrap();

        assert_eq!(decoded.build_script_out_dir, None);
    }

    #[test]
    fn source_identity_names_its_hash_algorithm() {
        let source = SourceFile {
            key: SourceFileKey("source".to_owned()),
            local_path: Some("src/lib.rs".to_owned()),
            remapped_path: "src/lib.rs".to_owned(),
            source_hash_algorithm: "md5".to_owned(),
            source_hash: "md5=0123456789abcdef0123456789abcdef".to_owned(),
            byte_len: 42,
            generated: false,
        };

        let encoded = serde_json::to_string(&source).unwrap();
        assert!(encoded.contains(r#""source_hash_algorithm":"md5""#));
        assert_eq!(
            serde_json::from_str::<SourceFile>(&encoded).unwrap(),
            source
        );
    }

    #[test]
    fn source_spans_include_one_based_locations() {
        let span = SourceSpan {
            file: SourceFileKey("source".to_owned()),
            start: 41,
            end: 47,
            line: 3,
            column: 5,
        };

        let encoded = serde_json::to_string(&span).unwrap();
        assert!(encoded.contains(r#""line":3,"column":5"#));
        assert_eq!(serde_json::from_str::<SourceSpan>(&encoded).unwrap(), span);
    }
}
