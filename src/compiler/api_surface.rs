use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use rot_compiler_protocol::{
    Definition, DefinitionKind, ExpansionOrigin, Exposure, Namespace, PublicBinding, SourceSpan,
};
use serde::Serialize;

use crate::{
    model::CompilerSourceSpanReport,
    workspace::{AuditInventory, PackageInfo},
};

use super::{closed_world::GraphInvocation, generated_source_label};

const SCOPE: &str = "selected production library and proc-macro public-name topology";
const LIMITATIONS: [&str; 3] = [
    "function signatures, generics, ABI, and semver compatibility are not compared",
    "unnamed implementation and opaque-type identities are excluded",
    "external consumers, doctests, and inactive required-feature targets are outside the selected compiled graph",
];

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct ApiUnitReport {
    pub package: String,
    pub package_path: String,
    pub target: String,
    pub kind: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ApiDefinitionReport {
    pub unit: ApiUnitReport,
    pub definition_path: String,
    pub kind: String,
    pub expansion_origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<CompilerSourceSpanReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution_callsite: Option<CompilerSourceSpanReport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ApiBindingReport {
    pub unit: ApiUnitReport,
    pub parent_definition_path: String,
    pub name: String,
    pub namespace: String,
    pub resolved_target_path: String,
    pub exposure: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<CompilerSourceSpanReport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ApiSurfaceReport {
    pub scope: String,
    pub limitations: Vec<String>,
    pub units: Vec<ApiUnitReport>,
    pub definitions: Vec<ApiDefinitionReport>,
    pub bindings: Vec<ApiBindingReport>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ApiDiffSummary {
    pub added_definitions: u64,
    pub removed_definitions: u64,
    pub added_bindings: u64,
    pub removed_bindings: u64,
    pub retargeted_bindings: u64,
    pub total_changes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub(crate) enum ApiChangeReport {
    DefinitionAdded {
        definition: ApiDefinitionReport,
    },
    DefinitionRemoved {
        definition: ApiDefinitionReport,
    },
    BindingAdded {
        binding: ApiBindingReport,
    },
    BindingRemoved {
        binding: ApiBindingReport,
    },
    BindingRetargeted {
        unit: ApiUnitReport,
        parent_definition_path: String,
        name: String,
        namespace: String,
        before_target_path: String,
        after_target_path: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ApiDiffReport {
    pub summary: ApiDiffSummary,
    pub changes: Vec<ApiChangeReport>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ApiUnitKey {
    package_path: String,
    target: String,
    kind: String,
}

impl From<&ApiUnitReport> for ApiUnitKey {
    fn from(unit: &ApiUnitReport) -> Self {
        Self {
            package_path: unit.package_path.clone(),
            target: unit.target.clone(),
            kind: unit.kind.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DefinitionKey {
    unit: ApiUnitKey,
    definition_path: String,
    kind: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct BindingSlot {
    unit: ApiUnitKey,
    parent_definition_path: String,
    name: String,
    namespace: String,
}

pub(super) fn aggregate(
    root: &Path,
    inventory: &AuditInventory,
    invocations: &[GraphInvocation<'_>],
) -> Result<ApiSurfaceReport, String> {
    let packages = inventory
        .packages
        .iter()
        .map(|package| (package.id.to_string(), package))
        .collect::<BTreeMap<_, _>>();
    let mut units = BTreeSet::new();
    let mut definitions = BTreeMap::<DefinitionKey, ApiDefinitionReport>::new();
    let mut bindings = BTreeMap::<BindingSlot, ApiBindingReport>::new();

    for input in invocations {
        let Some(unit) = api_unit(root, input, &packages)? else {
            continue;
        };
        units.insert(unit.clone());
        let by_id = input
            .invocation
            .definitions
            .iter()
            .map(|definition| (definition.compiler_id, definition))
            .collect::<BTreeMap<_, _>>();
        let exposed_definition_paths = input
            .invocation
            .bindings
            .iter()
            .filter(|binding| by_id.contains_key(&binding.target))
            .map(|binding| binding.resolved_target_path.as_str())
            .collect::<BTreeSet<_>>();

        for definition in input
            .invocation
            .definitions
            .iter()
            .filter(|definition| api_definition(definition, &exposed_definition_paths))
        {
            let report = definition_report(root, input, unit.clone(), definition);
            let key = DefinitionKey {
                unit: ApiUnitKey::from(&unit),
                definition_path: report.definition_path.clone(),
                kind: report.kind.clone(),
            };
            match definitions.get_mut(&key) {
                Some(existing) if !same_definition(existing, &report) => {
                    return Err(format!(
                        "public API definition {:?} has conflicting compiler representations",
                        report.definition_path
                    ));
                }
                Some(existing) => retain_definition_provenance(existing, report),
                None => {
                    definitions.insert(key, report);
                }
            }
        }

        for binding in &input.invocation.bindings {
            let parent = by_id.get(&binding.parent).ok_or_else(|| {
                format!(
                    "public binding {:?} has no local parent definition",
                    binding.name
                )
            })?;
            let report = binding_report(root, input, unit.clone(), parent, binding);
            let slot = BindingSlot {
                unit: ApiUnitKey::from(&unit),
                parent_definition_path: report.parent_definition_path.clone(),
                name: report.name.clone(),
                namespace: report.namespace.clone(),
            };
            match bindings.get_mut(&slot) {
                Some(existing) if existing.resolved_target_path != report.resolved_target_path => {
                    return Err(format!(
                        "public binding {}::{} resolves to multiple targets in one profile",
                        report.parent_definition_path, report.name
                    ));
                }
                Some(existing) => retain_stable_provenance(existing, report),
                None => {
                    bindings.insert(slot, report);
                }
            }
        }
    }

    Ok(ApiSurfaceReport {
        scope: SCOPE.to_owned(),
        limitations: LIMITATIONS
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        units: units.into_iter().collect(),
        definitions: definitions.into_values().collect(),
        bindings: bindings.into_values().collect(),
    })
}

pub(crate) fn compare(before: &ApiSurfaceReport, after: &ApiSurfaceReport) -> ApiDiffReport {
    let before_definitions = definition_index(before);
    let after_definitions = definition_index(after);
    let before_bindings = binding_index(before);
    let after_bindings = binding_index(after);
    let mut changes = Vec::new();

    for (key, definition) in &before_definitions {
        if !after_definitions.contains_key(key) {
            changes.push(ApiChangeReport::DefinitionRemoved {
                definition: (*definition).clone(),
            });
        }
    }
    for (key, definition) in &after_definitions {
        if !before_definitions.contains_key(key) {
            changes.push(ApiChangeReport::DefinitionAdded {
                definition: (*definition).clone(),
            });
        }
    }
    for (slot, binding) in &before_bindings {
        match after_bindings.get(slot) {
            None => changes.push(ApiChangeReport::BindingRemoved {
                binding: (*binding).clone(),
            }),
            Some(after_binding)
                if binding.resolved_target_path != after_binding.resolved_target_path =>
            {
                changes.push(ApiChangeReport::BindingRetargeted {
                    unit: after_binding.unit.clone(),
                    parent_definition_path: slot.parent_definition_path.clone(),
                    name: slot.name.clone(),
                    namespace: slot.namespace.clone(),
                    before_target_path: binding.resolved_target_path.clone(),
                    after_target_path: after_binding.resolved_target_path.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for (slot, binding) in &after_bindings {
        if !before_bindings.contains_key(slot) {
            changes.push(ApiChangeReport::BindingAdded {
                binding: (*binding).clone(),
            });
        }
    }

    changes.sort_by(|left, right| change_sort_key(left).cmp(&change_sort_key(right)));
    let mut summary = ApiDiffSummary {
        added_definitions: 0,
        removed_definitions: 0,
        added_bindings: 0,
        removed_bindings: 0,
        retargeted_bindings: 0,
        total_changes: changes.len() as u64,
    };
    for change in &changes {
        match change {
            ApiChangeReport::DefinitionAdded { .. } => summary.added_definitions += 1,
            ApiChangeReport::DefinitionRemoved { .. } => summary.removed_definitions += 1,
            ApiChangeReport::BindingAdded { .. } => summary.added_bindings += 1,
            ApiChangeReport::BindingRemoved { .. } => summary.removed_bindings += 1,
            ApiChangeReport::BindingRetargeted { .. } => summary.retargeted_bindings += 1,
        }
    }
    ApiDiffReport { summary, changes }
}

fn api_unit(
    root: &Path,
    input: &GraphInvocation<'_>,
    packages: &BTreeMap<String, &PackageInfo>,
) -> Result<Option<ApiUnitReport>, String> {
    if input.target.role != "production" {
        return Ok(None);
    }
    let proc_macro = input.target.kinds.iter().any(|kind| kind == "proc-macro");
    let library = input.target.kinds.iter().any(|kind| {
        matches!(
            kind.as_str(),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib"
        )
    });
    if !proc_macro && (!library || input.target.compilation_context != "target") {
        return Ok(None);
    }
    let package = packages.get(&input.target.package_id).ok_or_else(|| {
        format!(
            "correlated API target {} has no selected Cargo package",
            input.target.name
        )
    })?;
    let package_path = package
        .root
        .strip_prefix(root)
        .map_err(|_| {
            format!(
                "selected package {} is outside workspace root {}",
                package.root.display(),
                root.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    Ok(Some(ApiUnitReport {
        package: package.name.clone(),
        package_path: if package_path.is_empty() {
            ".".to_owned()
        } else {
            package_path
        },
        target: input.target.name.clone(),
        kind: if proc_macro { "proc_macro" } else { "library" }.to_owned(),
    }))
}

fn api_definition(definition: &Definition, exposed_paths: &BTreeSet<&str>) -> bool {
    definition.externally_reachable
        && !definition.definition_path.starts_with('<')
        && has_exposed_ancestor(&definition.definition_path, exposed_paths)
        && !matches!(
            definition.kind,
            DefinitionKind::Crate
                | DefinitionKind::Import
                | DefinitionKind::ExternCrate
                | DefinitionKind::Implementation
                | DefinitionKind::ForeignModule
                | DefinitionKind::OpaqueType
        )
}

fn has_exposed_ancestor(path: &str, exposed_paths: &BTreeSet<&str>) -> bool {
    let mut candidate = path;
    loop {
        if exposed_paths.contains(candidate) {
            return true;
        }
        let Some((parent, _)) = candidate.rsplit_once("::") else {
            return false;
        };
        candidate = parent;
    }
}

fn definition_report(
    root: &Path,
    input: &GraphInvocation<'_>,
    unit: ApiUnitReport,
    definition: &Definition,
) -> ApiDefinitionReport {
    ApiDefinitionReport {
        unit,
        definition_path: definition.definition_path.clone(),
        kind: definition_kind(definition.kind).to_owned(),
        expansion_origin: expansion_origin(definition.expansion_origin).to_owned(),
        span: definition
            .span
            .as_ref()
            .and_then(|span| source_span(root, input, span)),
        attribution_callsite: definition
            .attribution_callsite
            .as_ref()
            .and_then(|span| source_span(root, input, span)),
    }
}

fn binding_report(
    root: &Path,
    input: &GraphInvocation<'_>,
    unit: ApiUnitReport,
    parent: &Definition,
    binding: &PublicBinding,
) -> ApiBindingReport {
    ApiBindingReport {
        unit,
        parent_definition_path: parent.definition_path.clone(),
        name: binding.name.clone(),
        namespace: namespace(binding.namespace).to_owned(),
        resolved_target_path: binding.resolved_target_path.clone(),
        exposure: exposure(binding.exposure).to_owned(),
        span: binding
            .span
            .as_ref()
            .and_then(|span| source_span(root, input, span)),
    }
}

fn same_definition(left: &ApiDefinitionReport, right: &ApiDefinitionReport) -> bool {
    left.unit == right.unit
        && left.definition_path == right.definition_path
        && left.kind == right.kind
        && left.expansion_origin == right.expansion_origin
}

fn retain_definition_provenance(
    existing: &mut ApiDefinitionReport,
    candidate: ApiDefinitionReport,
) {
    if definition_provenance_key(&candidate) < definition_provenance_key(existing) {
        existing.span = candidate.span;
        existing.attribution_callsite = candidate.attribution_callsite;
    }
}

fn retain_stable_provenance(existing: &mut ApiBindingReport, candidate: ApiBindingReport) {
    if binding_provenance_key(&candidate) < binding_provenance_key(existing) {
        existing.exposure = candidate.exposure;
        existing.span = candidate.span;
    }
}

fn definition_provenance_key(
    definition: &ApiDefinitionReport,
) -> (
    bool,
    &Option<CompilerSourceSpanReport>,
    bool,
    &Option<CompilerSourceSpanReport>,
) {
    (
        definition.span.is_none(),
        &definition.span,
        definition.attribution_callsite.is_none(),
        &definition.attribution_callsite,
    )
}

fn binding_provenance_key(
    binding: &ApiBindingReport,
) -> (bool, &Option<CompilerSourceSpanReport>, &str) {
    (binding.span.is_none(), &binding.span, &binding.exposure)
}

fn definition_index(report: &ApiSurfaceReport) -> BTreeMap<DefinitionKey, &ApiDefinitionReport> {
    report
        .definitions
        .iter()
        .map(|definition| {
            (
                DefinitionKey {
                    unit: ApiUnitKey::from(&definition.unit),
                    definition_path: definition.definition_path.clone(),
                    kind: definition.kind.clone(),
                },
                definition,
            )
        })
        .collect()
}

fn binding_index(report: &ApiSurfaceReport) -> BTreeMap<BindingSlot, &ApiBindingReport> {
    report
        .bindings
        .iter()
        .map(|binding| {
            (
                BindingSlot {
                    unit: ApiUnitKey::from(&binding.unit),
                    parent_definition_path: binding.parent_definition_path.clone(),
                    name: binding.name.clone(),
                    namespace: binding.namespace.clone(),
                },
                binding,
            )
        })
        .collect()
}

fn change_sort_key(change: &ApiChangeReport) -> (u8, &ApiUnitReport, &str, &str, &str) {
    match change {
        ApiChangeReport::DefinitionAdded { definition } => (
            0,
            &definition.unit,
            &definition.definition_path,
            &definition.kind,
            "",
        ),
        ApiChangeReport::DefinitionRemoved { definition } => (
            1,
            &definition.unit,
            &definition.definition_path,
            &definition.kind,
            "",
        ),
        ApiChangeReport::BindingAdded { binding } => (
            2,
            &binding.unit,
            &binding.parent_definition_path,
            &binding.name,
            &binding.namespace,
        ),
        ApiChangeReport::BindingRemoved { binding } => (
            3,
            &binding.unit,
            &binding.parent_definition_path,
            &binding.name,
            &binding.namespace,
        ),
        ApiChangeReport::BindingRetargeted {
            unit,
            parent_definition_path,
            name,
            namespace,
            ..
        } => (4, unit, parent_definition_path, name, namespace),
    }
}

fn source_span(
    root: &Path,
    input: &GraphInvocation<'_>,
    span: &SourceSpan,
) -> Option<CompilerSourceSpanReport> {
    let source = input
        .invocation
        .sources
        .iter()
        .find(|source| source.key == span.file)?;
    if span.start > span.end || span.end > source.byte_len {
        return None;
    }
    let local_path = Path::new(source.local_path.as_deref()?);
    let path = if source.generated {
        generated_source_label(input.owner, local_path, &source.source_hash)
    } else {
        local_path
            .strip_prefix(root)
            .unwrap_or(local_path)
            .to_string_lossy()
            .replace('\\', "/")
    };
    Some(CompilerSourceSpanReport {
        path,
        source_hash: source.source_hash.clone(),
        generated: source.generated,
        start_byte: u64::from(span.start),
        end_byte: u64::from(span.end),
        line: u64::from(span.line),
        column: u64::from(span.column),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use rot_compiler_protocol::{CompilerDefId, FactId, NominalVisibility};

    fn unit() -> ApiUnitReport {
        ApiUnitReport {
            package: "api".to_owned(),
            package_path: ".".to_owned(),
            target: "api".to_owned(),
            kind: "library".to_owned(),
        }
    }

    fn surface(target: &str, exposure: &str) -> ApiSurfaceReport {
        ApiSurfaceReport {
            scope: SCOPE.to_owned(),
            limitations: Vec::new(),
            units: vec![unit()],
            definitions: Vec::new(),
            bindings: vec![ApiBindingReport {
                unit: unit(),
                parent_definition_path: "api".to_owned(),
                name: "Thing".to_owned(),
                namespace: "type".to_owned(),
                resolved_target_path: target.to_owned(),
                exposure: exposure.to_owned(),
                span: None,
            }],
        }
    }

    fn definition(path: &str) -> Definition {
        Definition {
            id: FactId("definition".to_owned()),
            compiler_id: CompilerDefId {
                stable_crate_id: 1,
                local_hash: 1,
            },
            parent: None,
            name: Some("item".to_owned()),
            definition_path: path.to_owned(),
            kind: DefinitionKind::Function,
            visibility_editable: true,
            nominal_visibility: NominalVisibility::Public,
            externally_reachable: true,
            span: None,
            attribution_callsite: None,
            expansion_origin: ExpansionOrigin::Authored,
        }
    }

    #[test]
    fn binding_target_changes_are_retargeted() {
        let diff = compare(
            &surface("dep::Old", "direct"),
            &surface("dep::New", "direct"),
        );
        assert_eq!(diff.summary.retargeted_bindings, 1);
        assert_eq!(diff.summary.total_changes, 1);
        assert!(matches!(
            diff.changes.as_slice(),
            [ApiChangeReport::BindingRetargeted { .. }]
        ));
    }

    #[test]
    fn exposure_only_changes_do_not_change_api_topology() {
        let diff = compare(
            &surface("dep::Thing", "single_reexport"),
            &surface("dep::Thing", "glob_reexport"),
        );
        assert_eq!(diff.summary.total_changes, 0);
        assert!(diff.changes.is_empty());
    }

    #[test]
    fn package_display_name_is_not_cross_revision_unit_identity() {
        let mut before = surface("dep::Thing", "direct");
        before.definitions.push(ApiDefinitionReport {
            unit: unit(),
            definition_path: "api::Thing".to_owned(),
            kind: "struct".to_owned(),
            expansion_origin: "authored".to_owned(),
            span: None,
            attribution_callsite: None,
        });
        let mut after = before.clone();
        after.units[0].package = "renamed-api".to_owned();
        after.definitions[0].unit.package = "renamed-api".to_owned();
        after.bindings[0].unit.package = "renamed-api".to_owned();

        let diff = compare(&before, &after);

        assert_eq!(diff.summary.total_changes, 0);
        assert!(diff.changes.is_empty());
    }

    #[test]
    fn duplicate_fragments_retain_available_source_provenance() {
        let span = CompilerSourceSpanReport {
            path: "src/lib.rs".to_owned(),
            source_hash: format!("sha256={}", "0".repeat(64)),
            generated: false,
            start_byte: 0,
            end_byte: 1,
            line: 1,
            column: 1,
        };
        let mut definition = ApiDefinitionReport {
            unit: unit(),
            definition_path: "api::Thing".to_owned(),
            kind: "struct".to_owned(),
            expansion_origin: "authored".to_owned(),
            span: None,
            attribution_callsite: None,
        };
        let callsite_candidate = ApiDefinitionReport {
            attribution_callsite: Some(span.clone()),
            ..definition.clone()
        };
        retain_definition_provenance(&mut definition, callsite_candidate);
        assert_eq!(definition.attribution_callsite, Some(span.clone()));
        let primary_candidate = ApiDefinitionReport {
            span: Some(span.clone()),
            attribution_callsite: None,
            ..definition.clone()
        };
        retain_definition_provenance(&mut definition, primary_candidate);
        assert_eq!(definition.span, Some(span.clone()));

        let mut binding = surface("dep::Thing", "direct").bindings.remove(0);
        let candidate = ApiBindingReport {
            span: Some(span.clone()),
            ..binding.clone()
        };
        retain_stable_provenance(&mut binding, candidate);
        assert_eq!(binding.span, Some(span));
    }

    #[test]
    fn definitions_require_a_public_name_path_and_exclude_trait_impl_containers() {
        let exposed = BTreeSet::from(["api::Thing"]);
        assert!(api_definition(&definition("api::Thing::field"), &exposed));
        assert!(!api_definition(&definition("api::Thingy"), &exposed));
        assert!(!api_definition(
            &definition("private::PublicButUnnameable"),
            &exposed
        ));
        assert!(!api_definition(
            &definition("<api::Thing as core::clone::Clone>::clone"),
            &exposed
        ));
    }
}
