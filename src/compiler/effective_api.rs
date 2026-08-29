use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};

use rot_compiler_protocol::{
    CompilerDefId, Definition, DefinitionKind, EffectiveVisibilityLevel, ExpansionOrigin, Exposure,
    Namespace, NominalVisibility, PublicBinding, SourceFile, SourceSpan,
};

use crate::{
    model::{
        CompilerDefinitionIdReport, CompilerSourceSpanReport, CompilerTargetReport,
        EffectiveApiBindingReport, EffectiveApiDefinitionReport, EffectiveApiReport,
        EffectiveApiSummaryReport, ProductAvailabilityReport, SemanticStatus,
    },
    workspace::Inventory,
};

use super::{generated_source_label, profile::CompilerProfile};

pub(super) struct ApiInvocation<'a> {
    pub target: Option<&'a CompilerTargetReport>,
    pub owner: &'a str,
    pub crate_name: &'a str,
    pub status: SemanticStatus,
    pub sources: &'a [SourceFile],
    pub definitions: &'a [Definition],
    pub public_bindings: &'a [PublicBinding],
}

pub(super) struct Aggregation {
    pub product: ProductAvailabilityReport,
    pub report: Option<EffectiveApiReport>,
}

pub(super) fn aggregate<'a>(
    inventory: &Inventory,
    compiler_profile: &CompilerProfile,
    collection_trustworthy: bool,
    invocations: impl IntoIterator<Item = ApiInvocation<'a>>,
) -> Aggregation {
    let expected = match expected_targets(inventory, compiler_profile) {
        Ok(expected) => expected,
        Err(reason) => return incomplete(SemanticStatus::Unavailable, reason),
    };
    aggregate_expected(
        &inventory.root,
        expected,
        collection_trustworthy,
        invocations,
    )
}

fn aggregate_expected<'a>(
    root: &Path,
    expected: BTreeSet<ApiTargetKey>,
    collection_trustworthy: bool,
    invocations: impl IntoIterator<Item = ApiInvocation<'a>>,
) -> Aggregation {
    if expected.is_empty() {
        return incomplete(
            SemanticStatus::Unavailable,
            "no selected production library or proc-macro target".to_owned(),
        );
    }
    let mut observed = BTreeMap::<ApiTargetKey, Vec<ApiInvocation<'a>>>::new();
    let mut unexpected = Vec::new();
    let mut any_semantic_evidence = false;
    for invocation in invocations {
        let Some(target) = invocation.target else {
            continue;
        };
        if !eligible(target) {
            continue;
        }
        any_semantic_evidence |= invocation.status != SemanticStatus::Unavailable
            || !invocation.definitions.is_empty()
            || !invocation.public_bindings.is_empty();
        let key = ApiTargetKey::from_report(target);
        if expected.contains(&key) {
            observed.entry(key).or_default().push(invocation);
        } else {
            unexpected.push(key);
        }
    }

    let mut reasons = Vec::new();
    if !collection_trustworthy {
        reasons.push("compiler sidecar or build-profile validation was incomplete".to_owned());
    }
    for key in &unexpected {
        reasons.push(format!(
            "unexpected production library compiler fragment: {}",
            key.render()
        ));
    }
    for key in &expected {
        match observed.get(key).map(Vec::as_slice).unwrap_or_default() {
            [] => reasons.push(format!(
                "missing production library compiler fragment: {}",
                key.render()
            )),
            [invocation] if invocation.status != SemanticStatus::Complete => {
                reasons.push(format!(
                    "effective API is {:?} for {}",
                    invocation.status,
                    key.render()
                ));
            }
            [_] => {}
            fragments => reasons.push(format!(
                "{} production library compiler fragments matched {}",
                fragments.len(),
                key.render()
            )),
        }
    }
    if !reasons.is_empty() {
        reasons.sort();
        reasons.dedup();
        let status = if any_semantic_evidence {
            SemanticStatus::Partial
        } else {
            SemanticStatus::Unavailable
        };
        return incomplete(status, reasons.join("; "));
    }

    let mut definitions = BTreeMap::new();
    let mut bindings = BTreeMap::new();
    for key in &expected {
        let invocation = &observed[key][0];
        collect_invocation(
            root,
            &key.package_id,
            invocation,
            &mut definitions,
            &mut bindings,
        );
    }
    let definitions = definitions.into_values().collect::<Vec<_>>();
    let public_bindings = bindings.into_values().collect::<Vec<_>>();
    let summary = summarize(expected.len(), &definitions, &public_bindings);
    Aggregation {
        product: ProductAvailabilityReport {
            product: "effective_api".to_owned(),
            status: SemanticStatus::Complete,
            reason: None,
        },
        report: Some(EffectiveApiReport {
            summary,
            definitions,
            public_bindings,
        }),
    }
}

pub(super) fn eligible(target: &CompilerTargetReport) -> bool {
    target.role == "production"
        && library_kinds(&target.kinds)
        && (target.compilation_context == "target" || proc_macro(&target.kinds))
}

fn proc_macro(kinds: &[String]) -> bool {
    kinds.iter().any(|kind| kind == "proc-macro")
}

fn library_kinds(kinds: &[String]) -> bool {
    kinds.iter().any(|kind| {
        matches!(
            kind.as_str(),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
        )
    })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ApiTargetKey {
    package_id: String,
    name: String,
    source: PathBuf,
    kinds: Vec<String>,
    crate_types: Vec<String>,
    compilation_context: String,
}

impl ApiTargetKey {
    fn from_report(target: &CompilerTargetReport) -> Self {
        Self {
            package_id: target.package_id.clone(),
            name: target.name.clone(),
            source: canonical(Path::new(&target.source)),
            kinds: sorted(target.kinds.clone()),
            crate_types: sorted(target.crate_types.clone()),
            compilation_context: target.compilation_context.clone(),
        }
    }

    fn render(&self) -> String {
        format!(
            "{}:{} at {}",
            self.package_id,
            self.name,
            self.source.display()
        )
    }
}

fn expected_targets(
    inventory: &Inventory,
    compiler_profile: &CompilerProfile,
) -> Result<BTreeSet<ApiTargetKey>, String> {
    let selected = inventory.selected_package_ids();
    let mut expected = BTreeSet::new();
    for package in inventory
        .packages
        .iter()
        .filter(|package| selected.contains(&package.id.to_string()))
    {
        let Some(enabled_features) = compiler_profile.resolved_features(&package.id) else {
            return Err(format!(
                "pinned Cargo profile has no resolved features for selected package {}",
                package.name
            ));
        };
        for target in package.targets.iter().filter(|target| {
            library_kinds(&target.kinds)
                && target
                    .required_features
                    .iter()
                    .all(|feature| enabled_features.contains(feature))
        }) {
            expected.insert(ApiTargetKey {
                package_id: package.id.to_string(),
                name: target.name.clone(),
                source: canonical(&target.source),
                kinds: sorted(target.kinds.clone()),
                crate_types: sorted(target.crate_types.clone()),
                compilation_context: if proc_macro(&target.kinds) {
                    "host"
                } else {
                    "target"
                }
                .to_owned(),
            });
        }
    }
    Ok(expected)
}

type DefinitionKey = (String, String, u64, u64);
type BindingKey = (
    String,
    String,
    u64,
    u64,
    String,
    String,
    String,
    u64,
    u64,
    Option<(u64, u64)>,
);

fn collect_invocation(
    root: &Path,
    package_id: &str,
    invocation: &ApiInvocation<'_>,
    definitions: &mut BTreeMap<DefinitionKey, EffectiveApiDefinitionReport>,
    bindings: &mut BTreeMap<BindingKey, EffectiveApiBindingReport>,
) {
    let sources = invocation
        .sources
        .iter()
        .map(|source| (source.key.0.as_str(), source))
        .collect::<HashMap<_, _>>();
    let definition_paths = invocation
        .definitions
        .iter()
        .map(|definition| (definition.compiler_id, definition.definition_path.as_str()))
        .collect::<HashMap<_, _>>();

    for definition in invocation
        .definitions
        .iter()
        .filter(|definition| definition.effective_public_at.is_some())
    {
        let (nominal_visibility, restricted_to) = match &definition.nominal_visibility {
            NominalVisibility::Public => ("public", None),
            NominalVisibility::Restricted(boundary) => {
                ("restricted", Some(definition_id(*boundary)))
            }
        };
        let key = (
            package_id.to_owned(),
            invocation.crate_name.to_owned(),
            definition.compiler_id.stable_crate_id,
            definition.compiler_id.local_hash,
        );
        definitions.insert(
            key,
            EffectiveApiDefinitionReport {
                package_id: package_id.to_owned(),
                crate_name: invocation.crate_name.to_owned(),
                id: definition_id(definition.compiler_id),
                parent: definition.parent.map(definition_id),
                name: definition.name.clone(),
                definition_path: definition.definition_path.clone(),
                kind: definition_kind(definition.kind).to_owned(),
                nominal_visibility: nominal_visibility.to_owned(),
                restricted_to,
                effective_public_at: effective_visibility(
                    definition
                        .effective_public_at
                        .expect("filtered to effective definitions"),
                )
                .to_owned(),
                expansion_origin: expansion_origin(definition.expansion_origin).to_owned(),
                span: definition
                    .span
                    .as_ref()
                    .and_then(|span| source_span(root, invocation.owner, &sources, span)),
                attribution_callsite: definition
                    .attribution_callsite
                    .as_ref()
                    .and_then(|span| source_span(root, invocation.owner, &sources, span)),
            },
        );
    }

    for binding in invocation.public_bindings {
        let namespace = namespace(binding.namespace);
        let exposure = exposure(binding.exposure);
        let key = (
            package_id.to_owned(),
            invocation.crate_name.to_owned(),
            binding.parent.stable_crate_id,
            binding.parent.local_hash,
            binding.name.clone(),
            namespace.to_owned(),
            exposure.to_owned(),
            binding.target.stable_crate_id,
            binding.target.local_hash,
            binding
                .exposing_import
                .map(|id| (id.stable_crate_id, id.local_hash)),
        );
        bindings.insert(
            key,
            EffectiveApiBindingReport {
                package_id: package_id.to_owned(),
                crate_name: invocation.crate_name.to_owned(),
                parent: definition_id(binding.parent),
                target: definition_id(binding.target),
                name: binding.name.clone(),
                namespace: namespace.to_owned(),
                exposure: exposure.to_owned(),
                exposing_import: binding.exposing_import.map(definition_id),
                parent_definition_path: definition_paths
                    .get(&binding.parent)
                    .map(|path| (*path).to_owned()),
                target_definition_path: definition_paths
                    .get(&binding.target)
                    .map(|path| (*path).to_owned()),
                span: binding
                    .span
                    .as_ref()
                    .and_then(|span| source_span(root, invocation.owner, &sources, span)),
            },
        );
    }
}

fn source_span(
    root: &Path,
    owner: &str,
    sources: &HashMap<&str, &SourceFile>,
    span: &SourceSpan,
) -> Option<CompilerSourceSpanReport> {
    let source = sources.get(span.file.0.as_str())?;
    if span.start > span.end || span.end > source.byte_len {
        return None;
    }
    let local_path = Path::new(source.local_path.as_deref()?);
    let path = if source.generated {
        generated_source_label(owner, local_path, &source.source_hash)
    } else {
        display_path(root, local_path)
    };
    Some(CompilerSourceSpanReport {
        path,
        source_hash: source.source_hash.clone(),
        generated: source.generated,
        start_byte: u64::from(span.start),
        end_byte: u64::from(span.end),
    })
}

fn summarize(
    invocation_count: usize,
    definitions: &[EffectiveApiDefinitionReport],
    bindings: &[EffectiveApiBindingReport],
) -> EffectiveApiSummaryReport {
    let mut definitions_by_kind = BTreeMap::new();
    for definition in definitions {
        *definitions_by_kind
            .entry(definition.kind.clone())
            .or_insert(0) += 1;
    }
    let mut bindings_by_namespace = BTreeMap::new();
    let mut bindings_by_exposure = BTreeMap::new();
    for binding in bindings {
        *bindings_by_namespace
            .entry(binding.namespace.clone())
            .or_insert(0) += 1;
        *bindings_by_exposure
            .entry(binding.exposure.clone())
            .or_insert(0) += 1;
    }
    EffectiveApiSummaryReport {
        production_library_invocations: invocation_count as u64,
        effective_definitions: definitions.len() as u64,
        public_bindings: bindings.len() as u64,
        definitions_by_kind,
        bindings_by_namespace,
        bindings_by_exposure,
    }
}

fn incomplete(status: SemanticStatus, reason: String) -> Aggregation {
    Aggregation {
        product: ProductAvailabilityReport {
            product: "effective_api".to_owned(),
            status,
            reason: Some(reason),
        },
        report: None,
    }
}

fn definition_id(id: CompilerDefId) -> CompilerDefinitionIdReport {
    CompilerDefinitionIdReport {
        stable_crate_id: format!("{:016x}", id.stable_crate_id),
        local_hash: format!("{:016x}", id.local_hash),
    }
}

fn definition_kind(kind: DefinitionKind) -> &'static str {
    match kind {
        DefinitionKind::Crate => "crate",
        DefinitionKind::Module => "module",
        DefinitionKind::Struct => "struct",
        DefinitionKind::Union => "union",
        DefinitionKind::Enum => "enum",
        DefinitionKind::Variant => "variant",
        DefinitionKind::Trait => "trait",
        DefinitionKind::TypeAlias => "type_alias",
        DefinitionKind::ForeignType => "foreign_type",
        DefinitionKind::TraitAlias => "trait_alias",
        DefinitionKind::AssociatedType => "associated_type",
        DefinitionKind::Function => "function",
        DefinitionKind::Constant => "constant",
        DefinitionKind::Static => "static",
        DefinitionKind::Constructor => "constructor",
        DefinitionKind::AssociatedFunction => "associated_function",
        DefinitionKind::AssociatedConstant => "associated_constant",
        DefinitionKind::Macro => "macro",
        DefinitionKind::ExternCrate => "extern_crate",
        DefinitionKind::Import => "import",
        DefinitionKind::ForeignModule => "foreign_module",
        DefinitionKind::OpaqueType => "opaque_type",
        DefinitionKind::Field => "field",
        DefinitionKind::Implementation => "implementation",
    }
}

fn effective_visibility(level: EffectiveVisibilityLevel) -> &'static str {
    match level {
        EffectiveVisibilityLevel::Direct => "direct",
        EffectiveVisibilityLevel::Reexported => "reexported",
        EffectiveVisibilityLevel::Reachable => "reachable",
        EffectiveVisibilityLevel::ReachableThroughImplTrait => "reachable_through_impl_trait",
    }
}

fn expansion_origin(origin: ExpansionOrigin) -> &'static str {
    match origin {
        ExpansionOrigin::Authored => "authored",
        ExpansionOrigin::BuiltinDesugaring => "builtin_desugaring",
        ExpansionOrigin::LocalMacro => "local_macro",
        ExpansionOrigin::ExternalMacro => "external_macro",
    }
}

fn namespace(namespace: Namespace) -> &'static str {
    match namespace {
        Namespace::Type => "type",
        Namespace::Value => "value",
        Namespace::Macro => "macro",
    }
}

fn exposure(exposure: Exposure) -> &'static str {
    match exposure {
        Exposure::Direct => "direct",
        Exposure::SingleReexport => "single_reexport",
        Exposure::GlobReexport => "glob_reexport",
        Exposure::ExternCrate => "extern_crate",
        Exposure::MacroUse => "macro_use",
        Exposure::MacroExport => "macro_export",
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use rot_compiler_protocol::{SourceFileKey, SourceSpan};

    use super::*;

    const ROOT: &str = "/workspace";

    #[test]
    fn complete_api_keeps_definitions_and_finite_bindings_separate() {
        let target = target("production", &["lib"]);
        let source = source("source", "/workspace/src/lib.rs", false, 128);
        let mut definitions = vec![
            definition(3, "crate::Later", DefinitionKind::Struct, true),
            definition(2, "crate::hidden", DefinitionKind::Function, false),
            definition(1, "crate::First", DefinitionKind::Function, true),
        ];
        definitions[2].span = Some(span("source", 4, 12));
        let bindings = vec![PublicBinding {
            id: rot_compiler_protocol::FactId("binding".to_owned()),
            parent: compiler_id(0),
            target: compiler_id(1),
            name: "First".to_owned(),
            namespace: Namespace::Value,
            exposure: Exposure::GlobReexport,
            exposing_import: Some(compiler_id(9)),
            span: Some(span("source", 1, 3)),
        }];
        let expected = BTreeSet::from([ApiTargetKey::from_report(&target)]);

        let aggregation = aggregate_expected(
            Path::new(ROOT),
            expected,
            true,
            [ApiInvocation {
                target: Some(&target),
                owner: "package",
                crate_name: "crate",
                status: SemanticStatus::Complete,
                sources: &[source],
                definitions: &definitions,
                public_bindings: &bindings,
            }],
        );

        assert_eq!(aggregation.product.status, SemanticStatus::Complete);
        let report = aggregation.report.expect("complete API report");
        assert_eq!(report.summary.production_library_invocations, 1);
        assert_eq!(report.summary.effective_definitions, 2);
        assert_eq!(report.summary.public_bindings, 1);
        assert_eq!(report.summary.definitions_by_kind["function"], 1);
        assert_eq!(report.summary.definitions_by_kind["struct"], 1);
        assert_eq!(report.summary.bindings_by_namespace["value"], 1);
        assert_eq!(report.summary.bindings_by_exposure["glob_reexport"], 1);
        assert_eq!(report.definitions[0].definition_path, "crate::First");
        assert_eq!(report.definitions[1].definition_path, "crate::Later");
        assert_eq!(report.definitions[0].id.local_hash, "0000000000000001");
        assert_eq!(
            report.definitions[0].span.as_ref().unwrap().path,
            "src/lib.rs"
        );
        assert_eq!(report.public_bindings[0].name, "First");
        assert_eq!(
            report.public_bindings[0].target_definition_path.as_deref(),
            Some("crate::First")
        );
    }

    #[test]
    fn unrelated_target_roles_and_executables_do_not_inflate_or_suppress_api() {
        let library = target("production", &["lib"]);
        let unit_test = target("unit_test", &["lib"]);
        let executable = target("production", &["bin"]);
        let library_definitions = [definition(
            1,
            "crate::LibraryApi",
            DefinitionKind::Function,
            true,
        )];
        let unit_definitions = [definition(
            2,
            "crate::TestOnly",
            DefinitionKind::Function,
            true,
        )];
        let executable_definitions = [definition(3, "crate::Main", DefinitionKind::Function, true)];
        let expected = BTreeSet::from([ApiTargetKey::from_report(&library)]);

        let aggregation = aggregate_expected(
            Path::new(ROOT),
            expected,
            true,
            [
                ApiInvocation {
                    target: Some(&unit_test),
                    owner: "package",
                    crate_name: "crate",
                    status: SemanticStatus::Unavailable,
                    sources: &[],
                    definitions: &unit_definitions,
                    public_bindings: &[],
                },
                ApiInvocation {
                    target: Some(&executable),
                    owner: "package",
                    crate_name: "main",
                    status: SemanticStatus::Partial,
                    sources: &[],
                    definitions: &executable_definitions,
                    public_bindings: &[],
                },
                ApiInvocation {
                    target: Some(&library),
                    owner: "package",
                    crate_name: "crate",
                    status: SemanticStatus::Complete,
                    sources: &[],
                    definitions: &library_definitions,
                    public_bindings: &[],
                },
            ],
        );

        let report = aggregation.report.expect("complete library API");
        assert_eq!(report.summary.effective_definitions, 1);
        assert_eq!(report.definitions[0].definition_path, "crate::LibraryApi");
        assert!(eligible(&target("production", &["proc-macro"])));
        assert!(!eligible(&unit_test));
        assert!(!eligible(&executable));
    }

    #[test]
    fn incomplete_relevant_fragment_never_serializes_semantic_zero() {
        let target = target("production", &["lib"]);
        let expected = BTreeSet::from([ApiTargetKey::from_report(&target)]);
        let aggregation = aggregate_expected(
            Path::new(ROOT),
            expected,
            true,
            [ApiInvocation {
                target: Some(&target),
                owner: "package",
                crate_name: "crate",
                status: SemanticStatus::Partial,
                sources: &[],
                definitions: &[],
                public_bindings: &[],
            }],
        );

        assert_eq!(aggregation.product.status, SemanticStatus::Partial);
        assert!(aggregation.report.is_none());
        assert!(
            aggregation
                .product
                .reason
                .as_deref()
                .unwrap()
                .contains("effective API is Partial")
        );
    }

    #[test]
    fn malformed_sidecar_transport_suppresses_an_otherwise_complete_api() {
        let target = target("production", &["lib"]);
        let definitions = [definition(1, "crate::Api", DefinitionKind::Function, true)];
        let aggregation = aggregate_expected(
            Path::new(ROOT),
            BTreeSet::from([ApiTargetKey::from_report(&target)]),
            false,
            [ApiInvocation {
                target: Some(&target),
                owner: "package",
                crate_name: "crate",
                status: SemanticStatus::Complete,
                sources: &[],
                definitions: &definitions,
                public_bindings: &[],
            }],
        );

        assert_eq!(aggregation.product.status, SemanticStatus::Partial);
        assert!(aggregation.report.is_none());
    }

    #[test]
    fn only_valid_physical_spans_are_attributed_and_generated_paths_are_stable() {
        let target = target("production", &["lib"]);
        let authored = source("authored", "/workspace/src/lib.rs", false, 8);
        let generated = source(
            "generated",
            "/private/tmp/rot-run-123/out/bindings.rs",
            true,
            16,
        );
        let mut definitions = vec![
            definition(1, "crate::BadSpan", DefinitionKind::Function, true),
            definition(2, "crate::Generated", DefinitionKind::Struct, true),
        ];
        definitions[0].span = Some(span("authored", 7, 9));
        definitions[1].span = Some(span("generated", 2, 10));
        let expected = BTreeSet::from([ApiTargetKey::from_report(&target)]);

        let aggregation = aggregate_expected(
            Path::new(ROOT),
            expected,
            true,
            [ApiInvocation {
                target: Some(&target),
                owner: "package",
                crate_name: "crate",
                status: SemanticStatus::Complete,
                sources: &[authored, generated],
                definitions: &definitions,
                public_bindings: &[],
            }],
        );

        let definitions = aggregation.report.unwrap().definitions;
        assert!(definitions[0].span.is_none());
        let generated_span = definitions[1].span.as_ref().unwrap();
        assert!(generated_span.path.starts_with("<generated>/package/"));
        assert!(generated_span.path.ends_with("/bindings.rs"));
        assert!(!generated_span.path.contains("rot-run-123"));
    }

    #[test]
    fn no_selected_library_never_serializes_semantic_zero() {
        let aggregation =
            aggregate_expected(Path::new(ROOT), BTreeSet::new(), false, std::iter::empty());

        assert_eq!(aggregation.product.status, SemanticStatus::Unavailable);
        assert!(aggregation.report.is_none());
        assert_eq!(
            aggregation.product.reason.as_deref(),
            Some("no selected production library or proc-macro target")
        );
    }

    fn compiler_id(local_hash: u64) -> CompilerDefId {
        CompilerDefId {
            stable_crate_id: 0xfeed,
            local_hash,
        }
    }

    fn definition(
        local_hash: u64,
        path: &str,
        kind: DefinitionKind,
        effective: bool,
    ) -> Definition {
        Definition {
            id: rot_compiler_protocol::FactId(format!("definition-{local_hash}")),
            compiler_id: compiler_id(local_hash),
            parent: Some(compiler_id(0)),
            name: path.rsplit("::").next().map(ToOwned::to_owned),
            definition_path: path.to_owned(),
            kind,
            visibility_editable: true,
            nominal_visibility: NominalVisibility::Public,
            effective_public_at: effective.then_some(EffectiveVisibilityLevel::Direct),
            span: None,
            attribution_callsite: None,
            expansion_origin: ExpansionOrigin::Authored,
        }
    }

    fn source(key: &str, path: &str, generated: bool, byte_len: u32) -> SourceFile {
        SourceFile {
            key: SourceFileKey(key.to_owned()),
            local_path: Some(path.to_owned()),
            remapped_path: path.to_owned(),
            source_hash_algorithm: "sha256".to_owned(),
            source_hash: "sha256=0123456789abcdef".to_owned(),
            byte_len,
            generated,
        }
    }

    fn span(source: &str, start: u32, end: u32) -> SourceSpan {
        SourceSpan {
            file: SourceFileKey(source.to_owned()),
            start,
            end,
        }
    }

    fn target(role: &str, kinds: &[&str]) -> CompilerTargetReport {
        CompilerTargetReport {
            package_id: "package 0.1.0 (path+file:///workspace)".to_owned(),
            name: "crate".to_owned(),
            kinds: kinds.iter().map(|kind| (*kind).to_owned()).collect(),
            crate_types: kinds.iter().map(|kind| (*kind).to_owned()).collect(),
            source: "/workspace/src/lib.rs".to_owned(),
            role: role.to_owned(),
            compilation_context: "target".to_owned(),
        }
    }
}
