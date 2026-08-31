use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use rot_compiler_protocol::{
    CompilerDefId, Definition, DefinitionKind, NominalVisibility, ReferenceKind, RootKind,
    SourceSpan,
};

use crate::model::{
    ClosedWorldFindingReport, ClosedWorldReport, ClosedWorldSummaryReport,
    CompilerDefinitionIdReport, CompilerSourceSpanReport, CompilerTargetReport,
    ImpactDefinitionReport, ImpactProvenanceClass, ImpactProvenanceReport, ImpactQueryReport,
    ImpactReferenceReport, ImpactReferenceStepReport, ImpactReport, ImpactSummaryReport,
    ImpactVisibilityDisposition, ImpactWitnessReport, RequiredVisibilityDefinitionReport,
    RequiredVisibilityReport, SemanticStatus,
};

use super::{generated_source_label, sidecar::Invocation};

const SCOPE: &str = "selected-workspace compiled-target closed world";
const EVIDENCE_EXCLUSIONS: [&str; 2] = [
    "doctests",
    "Cargo targets skipped by the active feature profile",
];

fn evidence_exclusions() -> Vec<String> {
    EVIDENCE_EXCLUSIONS
        .iter()
        .map(|exclusion| (*exclusion).to_owned())
        .collect()
}

pub(super) struct GraphInvocation<'a> {
    pub target: &'a CompilerTargetReport,
    pub owner: &'a str,
    pub crate_name: &'a str,
    pub status: SemanticStatus,
    pub invocation: &'a Invocation,
}

pub(super) struct Aggregation {
    pub status: SemanticStatus,
    pub reason: Option<String>,
    pub required_visibility: Option<RequiredVisibilityReport>,
    pub closed_world: Option<ClosedWorldReport>,
    pub impact: Option<ImpactReport>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ImpactQuery {
    pub package: String,
    pub definition_path: String,
    pub location: Option<ImpactLocationQuery>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ImpactLocationQuery {
    pub path: String,
    pub line: u64,
    pub column: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum GraphClass {
    Production,
    Nonproduction,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LogicalTarget {
    package_id: String,
    name: String,
    source: PathBuf,
    kinds: Vec<String>,
    crate_types: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SpanIdentity {
    path: String,
    source_hash: String,
    generated: bool,
    start: u32,
    end: u32,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct VisibilityKey {
    kind: DefinitionKind,
    span: SpanIdentity,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum NodeKey {
    Definition {
        package_id: String,
        target: Box<LogicalTarget>,
        definition_path: String,
        kind: DefinitionKind,
        span: SpanIdentity,
    },
    InvocationLocal {
        invocation: usize,
        compiler_id: CompilerDefId,
    },
}

#[derive(Default)]
struct Node {
    definitions: Vec<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GraphState {
    invocation: usize,
    node: usize,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReferenceEdge {
    from: GraphState,
    to: GraphState,
    kind: ReferenceKind,
    invocation: usize,
    reference: usize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RootEvidence {
    kind: RootKind,
    reason: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PhysicalDeclarationKey {
    Spanned {
        package_id: String,
        definition_path: String,
        span: SpanIdentity,
    },
    Logical {
        package_id: String,
        target: Box<LogicalTarget>,
        definition_path: String,
    },
}

struct Graph {
    nodes: Vec<Node>,
    runtime_edges: BTreeSet<(GraphState, GraphState)>,
    interface_edges: BTreeSet<(GraphState, GraphState, ReferenceKind)>,
    all_edges: BTreeSet<(usize, usize, ReferenceKind)>,
    reference_edges: BTreeSet<ReferenceEdge>,
    roots: BTreeMap<GraphState, BTreeSet<RootEvidence>>,
    production_roots: BTreeSet<GraphState>,
    nonproduction_roots: BTreeSet<GraphState>,
    suppressed_candidates: BTreeSet<usize>,
    required_state_reasons: BTreeMap<GraphState, BTreeSet<String>>,
    required_reasons: BTreeMap<usize, BTreeSet<String>>,
    errors: Vec<String>,
}

#[cfg(test)]
pub(super) fn aggregate(
    root: &Path,
    raw_status: SemanticStatus,
    invocations: &[GraphInvocation<'_>],
) -> Aggregation {
    aggregate_with_query(root, raw_status, invocations, None)
}

pub(super) fn aggregate_with_query(
    root: &Path,
    raw_status: SemanticStatus,
    invocations: &[GraphInvocation<'_>],
    query: Option<&ImpactQuery>,
) -> Aggregation {
    if raw_status != SemanticStatus::Complete {
        return incomplete_with_query(
            raw_status,
            "closed-world products require complete reference facts for every selected Cargo unit",
            query,
        );
    }
    let relevant = invocations.iter().collect::<Vec<_>>();
    if relevant.is_empty() {
        return incomplete_with_query(
            SemanticStatus::Unavailable,
            "no selected compiler fragments",
            query,
        );
    }
    if relevant
        .iter()
        .any(|invocation| invocation.status != SemanticStatus::Complete)
    {
        return incomplete_with_query(
            SemanticStatus::Partial,
            "at least one selected reference fragment is incomplete",
            query,
        );
    }

    let mut graph = build_graph(&relevant);
    propagate_required(&mut graph);
    if !graph.errors.is_empty() {
        graph.errors.sort();
        graph.errors.dedup();
        return incomplete_with_query(
            SemanticStatus::Partial,
            &format!(
                "reference graph could not be closed: {}",
                graph.errors.join("; ")
            ),
            query,
        );
    }

    let required_visibility = Some(required_visibility_report(
        root,
        &relevant,
        &graph.nodes,
        &graph.required_reasons,
    ));
    let production_states = reachable(&graph.production_roots, &graph.runtime_edges);
    let nonproduction_states = reachable(&graph.nonproduction_roots, &graph.runtime_edges);
    let production = reached_nodes(&production_states);
    let nonproduction = reached_nodes(&nonproduction_states);
    let mut candidates = candidates(&relevant, &graph.nodes);
    let safe_candidates = safe_visibility_candidates(
        &relevant,
        &graph.nodes,
        &candidates,
        &graph.required_reasons,
        &graph.suppressed_candidates,
    );
    candidates.retain(|node, _| safe_candidates.contains(node));
    let impact = query.map(|query| {
        impact_report(
            root,
            &relevant,
            &graph,
            query,
            &production,
            &nonproduction,
            &candidates,
        )
    });
    let mut findings = Vec::new();
    for (node, (invocation_index, definition_index)) in &candidates {
        if graph.required_reasons.contains_key(node) {
            continue;
        }
        let production_live = production.contains(node);
        let nonproduction_live = nonproduction.contains(node);
        let test_compiled_only = !graph.nodes[*node]
            .definitions
            .iter()
            .any(|(invocation, _)| relevant[*invocation].target.role == "production");
        let kind = if production_live || nonproduction_live {
            "unnecessary_public"
        } else {
            "dead_public"
        };
        findings.push(finding_report(
            root,
            relevant[*invocation_index],
            &relevant[*invocation_index].invocation.definitions[*definition_index],
            kind,
            production_live,
            nonproduction_live,
            test_compiled_only,
        ));
    }
    findings.sort_by(|left, right| {
        (
            &left.kind,
            &left.package_id,
            &left.definition_path,
            &left.representative_id.stable_crate_id,
            &left.representative_id.local_hash,
        )
            .cmp(&(
                &right.kind,
                &right.package_id,
                &right.definition_path,
                &right.representative_id.stable_crate_id,
                &right.representative_id.local_hash,
            ))
    });
    let dead_public = findings
        .iter()
        .filter(|finding| finding.kind == "dead_public")
        .count() as u64;
    let unnecessary_public = findings.len() as u64 - dead_public;
    let definition_nodes = graph
        .nodes
        .iter()
        .filter(|node| !node.definitions.is_empty())
        .count() as u64;
    let report = ClosedWorldReport {
        scope: SCOPE.to_owned(),
        evidence_exclusions: evidence_exclusions(),
        summary: ClosedWorldSummaryReport {
            definition_nodes,
            reference_edges: graph.all_edges.len() as u64,
            production_roots: graph.production_roots.len() as u64,
            nonproduction_roots: graph.nonproduction_roots.len() as u64,
            production_live: production.len() as u64,
            nonproduction_live: nonproduction.len() as u64,
            public_candidates: candidates.len() as u64,
            dead_public,
            unnecessary_public,
        },
        findings,
    };

    Aggregation {
        status: SemanticStatus::Complete,
        reason: None,
        required_visibility,
        closed_world: Some(report),
        impact,
    }
}

fn build_graph(invocations: &[&GraphInvocation<'_>]) -> Graph {
    let mut nodes = Vec::<Node>::new();
    let mut node_by_key = BTreeMap::<NodeKey, usize>::new();
    let mut raw_nodes = BTreeMap::<(usize, CompilerDefId), usize>::new();
    for (invocation_index, input) in invocations.iter().enumerate() {
        for (definition_index, definition) in input.invocation.definitions.iter().enumerate() {
            let key = definition_key(invocation_index, input, definition);
            let node = intern_node(&mut nodes, &mut node_by_key, key);
            nodes[node]
                .definitions
                .push((invocation_index, definition_index));
            raw_nodes.insert((invocation_index, definition.compiler_id), node);
        }
    }

    let mut global_states = BTreeMap::<CompilerDefId, BTreeSet<GraphState>>::new();
    for ((invocation, compiler_id), node) in &raw_nodes {
        global_states
            .entry(*compiler_id)
            .or_default()
            .insert(GraphState {
                invocation: *invocation,
                node: *node,
            });
    }
    let selected_crates = global_states
        .keys()
        .map(|compiler_id| compiler_id.stable_crate_id)
        .collect::<BTreeSet<_>>();

    let mut graph = Graph {
        nodes,
        runtime_edges: BTreeSet::new(),
        interface_edges: BTreeSet::new(),
        all_edges: BTreeSet::new(),
        reference_edges: BTreeSet::new(),
        roots: BTreeMap::new(),
        production_roots: BTreeSet::new(),
        nonproduction_roots: BTreeSet::new(),
        suppressed_candidates: BTreeSet::new(),
        required_state_reasons: BTreeMap::new(),
        required_reasons: BTreeMap::new(),
        errors: Vec::new(),
    };

    for (invocation_index, input) in invocations.iter().enumerate() {
        let class = graph_class(input.target);
        for root in &input.invocation.roots {
            let Some(node) = raw_nodes.get(&(invocation_index, root.definition)).copied() else {
                graph.errors.push(format!(
                    "{} root {} has no local graph node",
                    input.invocation.started.merge_key.0, root.id.0
                ));
                continue;
            };
            let state = GraphState {
                invocation: invocation_index,
                node,
            };
            graph.roots.entry(state).or_default().insert(RootEvidence {
                kind: root.kind,
                reason: root.reason.clone(),
            });
            match root.kind {
                RootKind::EntryPoint | RootKind::Conservative => match class {
                    Some(GraphClass::Production) => {
                        graph.production_roots.insert(state);
                    }
                    Some(GraphClass::Nonproduction) => {
                        graph.nonproduction_roots.insert(state);
                    }
                    None => {}
                },
                RootKind::RequiredPublic => {
                    graph
                        .required_state_reasons
                        .entry(state)
                        .or_default()
                        .insert(root.reason.clone());
                }
            }
            if root.kind == RootKind::Conservative
                && root.reason == "dead_code is explicitly allowed for this definition"
            {
                graph.suppressed_candidates.insert(node);
            }
        }

        for (reference_index, reference) in input.invocation.references.iter().enumerate() {
            let Some(from_node) = raw_nodes.get(&(invocation_index, reference.from)).copied()
            else {
                graph.errors.push(format!(
                    "{} reference {} has no source graph node",
                    input.invocation.started.merge_key.0, reference.id.0
                ));
                continue;
            };
            let from = GraphState {
                invocation: invocation_index,
                node: from_node,
            };
            let targets =
                match resolve_targets(invocation_index, reference.to, &raw_nodes, &global_states) {
                    Ok(targets) if !targets.is_empty() => targets,
                    Ok(_) => {
                        if selected_crates.contains(&reference.to.stable_crate_id) {
                            graph.errors.push(format!(
                                "{} reference {} has a missing selected target",
                                input.invocation.started.merge_key.0, reference.id.0
                            ));
                        }
                        continue;
                    }
                    Err(()) => {
                        graph.errors.push(format!(
                            "{} reference {} has an ambiguous or missing selected target",
                            input.invocation.started.merge_key.0, reference.id.0
                        ));
                        continue;
                    }
                };
            for to in targets {
                graph.reference_edges.insert(ReferenceEdge {
                    from,
                    to,
                    kind: reference.kind,
                    invocation: invocation_index,
                    reference: reference_index,
                });
                graph.all_edges.insert((from.node, to.node, reference.kind));
                if reference.kind != ReferenceKind::VisibilityRequirement {
                    graph.runtime_edges.insert((from, to));
                }
                if is_interface(reference.kind) {
                    graph.interface_edges.insert((from, to, reference.kind));
                }
                if reference.from.stable_crate_id != reference.to.stable_crate_id {
                    graph
                        .required_state_reasons
                        .entry(to)
                        .or_default()
                        .insert("referenced from another selected crate".to_owned());
                    if class == Some(GraphClass::Production) {
                        graph.production_roots.insert(to);
                    }
                }
            }
        }
    }
    add_visibility_equivalence_edges(invocations, &raw_nodes, &mut graph.interface_edges);
    graph
}

fn add_visibility_equivalence_edges(
    invocations: &[&GraphInvocation<'_>],
    raw_nodes: &BTreeMap<(usize, CompilerDefId), usize>,
    edges: &mut BTreeSet<(GraphState, GraphState, ReferenceKind)>,
) {
    let mut groups = BTreeMap::<VisibilityKey, BTreeSet<GraphState>>::new();
    for (invocation_index, input) in invocations.iter().enumerate() {
        for definition in &input.invocation.definitions {
            let Some(key) = definition_visibility_key(input, definition) else {
                continue;
            };
            let Some(node) = raw_nodes
                .get(&(invocation_index, definition.compiler_id))
                .copied()
            else {
                continue;
            };
            groups.entry(key).or_default().insert(GraphState {
                invocation: invocation_index,
                node,
            });
        }
    }
    for states in groups.values() {
        for &from in states {
            for &to in states {
                if from != to {
                    edges.insert((from, to, ReferenceKind::Interface));
                }
            }
        }
    }
}

fn propagate_required(graph: &mut Graph) {
    let mut adjacency = BTreeMap::<GraphState, BTreeSet<(GraphState, ReferenceKind)>>::new();
    for &(from, to, kind) in &graph.interface_edges {
        adjacency.entry(from).or_default().insert((to, kind));
    }
    let mut pending = VecDeque::new();
    let mut seen = BTreeSet::new();
    for (state, reasons) in &graph.required_state_reasons {
        let follow_visibility_parent = reasons.iter().any(|reason| {
            reason != "a public reexport requires this local target to remain public"
        });
        let item = (*state, follow_visibility_parent);
        seen.insert(item);
        pending.push_back(item);
    }
    while let Some((state, follow_visibility_parent)) = pending.pop_front() {
        let Some(targets) = adjacency.get(&state) else {
            continue;
        };
        for &(target, kind) in targets {
            if kind == ReferenceKind::VisibilityParent && !follow_visibility_parent {
                continue;
            }
            graph
                .required_state_reasons
                .entry(target)
                .or_default()
                .insert("required by a public interface or visibility edge".to_owned());
            let next = (target, follow_visibility_parent);
            if seen.insert(next) {
                pending.push_back(next);
            }
        }
    }
    for (state, reasons) in &graph.required_state_reasons {
        graph
            .required_reasons
            .entry(state.node)
            .or_default()
            .extend(reasons.iter().cloned());
    }
}

fn definition_key(
    invocation_index: usize,
    input: &GraphInvocation<'_>,
    definition: &Definition,
) -> NodeKey {
    definition
        .span
        .as_ref()
        .and_then(|span| span_identity(input.invocation, span))
        .map_or(
            NodeKey::InvocationLocal {
                invocation: invocation_index,
                compiler_id: definition.compiler_id,
            },
            |span| NodeKey::Definition {
                package_id: input.target.package_id.clone(),
                target: Box::new(logical_target(input.target)),
                definition_path: definition.definition_path.clone(),
                kind: definition.kind,
                span,
            },
        )
}

fn span_identity(invocation: &Invocation, span: &SourceSpan) -> Option<SpanIdentity> {
    let source = invocation
        .sources
        .iter()
        .find(|source| source.key == span.file)?;
    if span.start > span.end || span.end > source.byte_len {
        return None;
    }
    let path = source
        .local_path
        .as_deref()
        .unwrap_or(&source.remapped_path);
    Some(SpanIdentity {
        path: if source.generated {
            source.remapped_path.clone()
        } else {
            canonical(Path::new(path)).to_string_lossy().into_owned()
        },
        source_hash: source.source_hash.clone(),
        generated: source.generated,
        start: span.start,
        end: span.end,
    })
}

fn logical_target(target: &CompilerTargetReport) -> LogicalTarget {
    let mut kinds = target.kinds.clone();
    kinds.sort();
    kinds.dedup();
    let mut crate_types = target.crate_types.clone();
    crate_types.sort();
    crate_types.dedup();
    LogicalTarget {
        package_id: target.package_id.clone(),
        name: target.name.clone(),
        source: canonical(Path::new(&target.source)),
        kinds,
        crate_types,
    }
}

fn intern_node(
    nodes: &mut Vec<Node>,
    by_key: &mut BTreeMap<NodeKey, usize>,
    key: NodeKey,
) -> usize {
    *by_key.entry(key).or_insert_with(|| {
        let id = nodes.len();
        nodes.push(Node::default());
        id
    })
}

fn resolve_targets(
    invocation: usize,
    compiler_id: CompilerDefId,
    raw_nodes: &BTreeMap<(usize, CompilerDefId), usize>,
    global_states: &BTreeMap<CompilerDefId, BTreeSet<GraphState>>,
) -> Result<Vec<GraphState>, ()> {
    if let Some(node) = raw_nodes.get(&(invocation, compiler_id)).copied() {
        return Ok(vec![GraphState { invocation, node }]);
    }
    let Some(states) = global_states.get(&compiler_id) else {
        return Ok(Vec::new());
    };
    if states
        .iter()
        .map(|state| state.node)
        .collect::<BTreeSet<_>>()
        .len()
        != 1
    {
        return Err(());
    }
    Ok(states.iter().copied().collect())
}

fn graph_class(target: &CompilerTargetReport) -> Option<GraphClass> {
    if target.compilation_context == "host" && !target.kinds.iter().any(|kind| kind == "proc-macro")
    {
        return None;
    }
    match target.role.as_str() {
        "production" => Some(GraphClass::Production),
        "unit_test" | "test" | "bench" | "example" => Some(GraphClass::Nonproduction),
        _ => None,
    }
}

fn is_interface(kind: ReferenceKind) -> bool {
    matches!(
        kind,
        ReferenceKind::Interface
            | ReferenceKind::Reexport
            | ReferenceKind::VisibilityParent
            | ReferenceKind::VisibilityRequirement
    )
}

fn runtime_adjacency(
    edges: &BTreeSet<(GraphState, GraphState)>,
) -> BTreeMap<GraphState, BTreeSet<GraphState>> {
    let mut adjacency = BTreeMap::<GraphState, BTreeSet<GraphState>>::new();
    for &(from, to) in edges {
        adjacency.entry(from).or_default().insert(to);
    }
    adjacency
}

fn reachable(
    roots: &BTreeSet<GraphState>,
    edges: &BTreeSet<(GraphState, GraphState)>,
) -> BTreeSet<GraphState> {
    let adjacency = runtime_adjacency(edges);
    let mut reached = roots.clone();
    let mut pending = roots.iter().copied().collect::<VecDeque<_>>();
    while let Some(node) = pending.pop_front() {
        for target in adjacency.get(&node).into_iter().flatten() {
            if reached.insert(*target) {
                pending.push_back(*target);
            }
        }
    }
    reached
}

fn reached_nodes(states: &BTreeSet<GraphState>) -> BTreeSet<usize> {
    states.iter().map(|state| state.node).collect()
}

fn candidates(
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
) -> BTreeMap<usize, (usize, usize)> {
    let mut candidates = BTreeMap::new();
    for (node_id, node) in nodes.iter().enumerate() {
        let representative = node
            .definitions
            .iter()
            .copied()
            .filter(|(invocation_index, definition_index)| {
                let input = invocations[*invocation_index];
                let definition = &input.invocation.definitions[*definition_index];
                eligible_candidate(input, definition)
                    && !implicit_trait_member(*invocation_index, definition, input.invocation)
            })
            .min_by_key(|(invocation_index, _)| {
                (
                    invocations[*invocation_index].target.role != "production",
                    *invocation_index,
                )
            });
        if let Some(representative) = representative {
            candidates.insert(node_id, representative);
        }
    }
    candidates
}

fn eligible_candidate(input: &GraphInvocation<'_>, definition: &Definition) -> bool {
    candidate_target(input.target)
        && definition.externally_reachable
        && definition.visibility_editable
        && editable_authored_source(input.invocation, definition)
        && matches!(definition.nominal_visibility, NominalVisibility::Public)
        && !matches!(
            definition.kind,
            DefinitionKind::Crate
                | DefinitionKind::Variant
                | DefinitionKind::Constructor
                | DefinitionKind::Implementation
                | DefinitionKind::Import
                | DefinitionKind::Macro
                | DefinitionKind::ExternCrate
                | DefinitionKind::ForeignModule
                | DefinitionKind::OpaqueType
        )
}

fn editable_authored_source(invocation: &Invocation, definition: &Definition) -> bool {
    definition.expansion_origin == rot_compiler_protocol::ExpansionOrigin::Authored
        && definition.span.as_ref().is_some_and(|span| {
            invocation
                .sources
                .iter()
                .find(|source| source.key == span.file)
                .is_some_and(|source| !source.generated)
        })
}

fn candidate_target(target: &CompilerTargetReport) -> bool {
    let library = target.kinds.iter().any(|kind| {
        matches!(
            kind.as_str(),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
        )
    });
    library
        && match target.role.as_str() {
            "production" => {
                target.compilation_context == "target"
                    || target.kinds.iter().any(|kind| kind == "proc-macro")
            }
            "unit_test" => target.compilation_context == "target",
            _ => false,
        }
}

fn safe_visibility_candidates(
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
    candidates: &BTreeMap<usize, (usize, usize)>,
    required: &BTreeMap<usize, BTreeSet<String>>,
    suppressed: &BTreeSet<usize>,
) -> BTreeSet<usize> {
    let mut groups = BTreeMap::<VisibilityKey, BTreeSet<usize>>::new();
    for (node_id, node) in nodes.iter().enumerate() {
        for &(invocation_index, definition_index) in &node.definitions {
            let input = invocations[invocation_index];
            let definition = &input.invocation.definitions[definition_index];
            let Some(key) = definition_visibility_key(input, definition) else {
                continue;
            };
            groups.entry(key).or_default().insert(node_id);
        }
    }

    let mut safe = BTreeSet::new();
    for nodes in groups.values() {
        let group_is_safe = nodes.iter().all(|node| {
            candidates.contains_key(node)
                && !required.contains_key(node)
                && !suppressed.contains(node)
        });
        if group_is_safe {
            safe.extend(nodes.iter().copied());
        }
    }
    safe
}

fn definition_visibility_key(
    input: &GraphInvocation<'_>,
    definition: &Definition,
) -> Option<VisibilityKey> {
    if !definition.visibility_editable {
        return None;
    }
    let span = definition
        .span
        .as_ref()
        .and_then(|span| span_identity(input.invocation, span))?;
    Some(VisibilityKey {
        kind: definition.kind,
        span,
    })
}

fn library_target(target: &CompilerTargetReport) -> bool {
    target.role == "production"
        && target.kinds.iter().any(|kind| {
            matches!(
                kind.as_str(),
                "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
            )
        })
}

fn implicit_trait_member(
    _invocation_index: usize,
    definition: &Definition,
    invocation: &Invocation,
) -> bool {
    if !matches!(
        definition.kind,
        DefinitionKind::AssociatedType
            | DefinitionKind::AssociatedFunction
            | DefinitionKind::AssociatedConstant
    ) {
        return false;
    }
    definition.parent.is_some_and(|parent| {
        invocation.definitions.iter().any(|candidate| {
            candidate.compiler_id == parent && candidate.kind == DefinitionKind::Trait
        })
    })
}

fn required_visibility_report(
    root: &Path,
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
    required: &BTreeMap<usize, BTreeSet<String>>,
) -> RequiredVisibilityReport {
    let mut definitions = required
        .iter()
        .filter_map(|(node, reasons)| {
            let (invocation_index, definition_index) =
                representative(nodes.get(*node)?, invocations)?;
            let input = invocations[invocation_index];
            let definition = &input.invocation.definitions[definition_index];
            if !library_target(input.target)
                || !definition.externally_reachable
                || !definition.visibility_editable
                || !editable_authored_source(input.invocation, definition)
                || !matches!(definition.nominal_visibility, NominalVisibility::Public)
                || matches!(
                    definition.kind,
                    DefinitionKind::Crate
                        | DefinitionKind::Variant
                        | DefinitionKind::Constructor
                        | DefinitionKind::Implementation
                )
                || implicit_trait_member(invocation_index, definition, input.invocation)
            {
                return None;
            }
            Some(RequiredVisibilityDefinitionReport {
                package_id: input.target.package_id.clone(),
                crate_name: input.crate_name.to_owned(),
                representative_invocation: input.invocation.started.merge_key.0.clone(),
                representative_id: definition_id(definition.compiler_id),
                definition_path: definition.definition_path.clone(),
                kind: definition_kind(definition.kind).to_owned(),
                current_visibility: "public".to_owned(),
                required_visibility: "public".to_owned(),
                reasons: reasons.iter().cloned().collect(),
                span: definition
                    .span
                    .as_ref()
                    .and_then(|span| source_span(root, input, span)),
            })
        })
        .collect::<Vec<_>>();
    definitions.sort_by(|left, right| {
        (
            &left.package_id,
            &left.definition_path,
            &left.representative_id.stable_crate_id,
            &left.representative_id.local_hash,
        )
            .cmp(&(
                &right.package_id,
                &right.definition_path,
                &right.representative_id.stable_crate_id,
                &right.representative_id.local_hash,
            ))
    });
    RequiredVisibilityReport {
        scope: SCOPE.to_owned(),
        evidence_exclusions: evidence_exclusions(),
        definitions,
    }
}

fn representative(node: &Node, invocations: &[&GraphInvocation<'_>]) -> Option<(usize, usize)> {
    node.definitions
        .iter()
        .copied()
        .find(|(invocation, _)| invocations[*invocation].target.role == "production")
        .or_else(|| node.definitions.first().copied())
}

fn finding_report(
    root: &Path,
    input: &GraphInvocation<'_>,
    definition: &Definition,
    kind: &str,
    production_live: bool,
    nonproduction_live: bool,
    test_compiled_only: bool,
) -> ClosedWorldFindingReport {
    ClosedWorldFindingReport {
        kind: kind.to_owned(),
        reason: if kind == "dead_public" {
            "unreachable from every compiled production and nonproduction root"
        } else {
            "reachable, but no selected cross-crate use requires unrestricted public visibility"
        }
        .to_owned(),
        package_id: input.target.package_id.clone(),
        crate_name: input.crate_name.to_owned(),
        representative_invocation: input.invocation.started.merge_key.0.clone(),
        representative_id: definition_id(definition.compiler_id),
        definition_path: definition.definition_path.clone(),
        definition_kind: definition_kind(definition.kind).to_owned(),
        production_live,
        nonproduction_live,
        test_compiled_only,
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

fn impact_report(
    root: &Path,
    invocations: &[&GraphInvocation<'_>],
    graph: &Graph,
    query: &ImpactQuery,
    production: &BTreeSet<usize>,
    nonproduction: &BTreeSet<usize>,
    candidates: &BTreeMap<usize, (usize, usize)>,
) -> ImpactReport {
    let matches = matching_physical_declarations(root, invocations, &graph.nodes, query);
    let candidate_reports = matches
        .values()
        .filter_map(|nodes| representative_for_nodes(nodes, invocations, &graph.nodes, Some(query)))
        .map(|(invocation, definition)| {
            impact_definition_report(root, invocations[invocation], definition)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return unavailable_impact(
            query,
            "no compiled definition exactly matched the requested package, definition path, and optional source location",
            candidate_reports,
        );
    }
    if matches.len() != 1 {
        return unavailable_impact(
            query,
            "the query matched multiple physical declarations; add an exact source path, line, and column",
            candidate_reports,
        );
    }

    let selected_nodes = matches
        .into_values()
        .next()
        .expect("one physical declaration match");
    let selected_states = states_for_nodes(&selected_nodes, &graph.nodes);
    let (representative_invocation, representative_definition) =
        representative_for_nodes(&selected_nodes, invocations, &graph.nodes, Some(query))
            .expect("matched physical declarations have a representative definition");
    let selected = impact_definition_report(
        root,
        invocations[representative_invocation],
        representative_definition,
    );

    let mut direct_references = graph
        .reference_edges
        .iter()
        .filter(|edge| selected_states.contains(&edge.to))
        .filter_map(|edge| impact_reference_report(root, invocations, &graph.nodes, *edge))
        .collect::<Vec<_>>();
    direct_references.sort_by_key(impact_reference_key);
    direct_references.dedup();

    let (reached, next_edge) = reverse_reachable(&selected_states, &graph.reference_edges);
    let transitive_consumers = reached
        .iter()
        .filter(|state| !selected_states.contains(state))
        .filter_map(|state| physical_key_for_state(*state, invocations, &graph.nodes))
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let witnesses = impact_witnesses(
        root,
        invocations,
        graph,
        &selected_states,
        &reached,
        &next_edge,
    );

    let mut provenance = BTreeSet::new();
    for state in reached
        .iter()
        .filter(|state| !selected_states.contains(state) || graph.roots.contains_key(state))
    {
        if let Some(class) = compiled_provenance(invocations[state.invocation].target) {
            provenance.insert(class);
        }
        if graph.roots.get(state).is_some_and(|roots| {
            roots
                .iter()
                .any(|root| root.kind == RootKind::RequiredPublic)
        }) {
            provenance.insert(ImpactProvenanceClass::PublicInterface);
        }
    }
    for witness in &witnesses {
        provenance.insert(witness.provenance.class);
    }

    let visibility_nodes = physical_visibility_nodes(&selected_nodes, invocations, &graph.nodes);
    let visibility_disposition = if visibility_nodes
        .iter()
        .any(|node| graph.required_reasons.contains_key(node))
    {
        ImpactVisibilityDisposition::RequiredPublic
    } else if visibility_nodes
        .iter()
        .any(|node| candidates.contains_key(node))
    {
        if visibility_nodes
            .iter()
            .any(|node| production.contains(node) || nonproduction.contains(node))
        {
            ImpactVisibilityDisposition::NarrowablePublic
        } else {
            ImpactVisibilityDisposition::DeadPublic
        }
    } else {
        ImpactVisibilityDisposition::NotPublicCandidate
    };

    ImpactReport {
        status: SemanticStatus::Complete,
        reason: None,
        scope: SCOPE.to_owned(),
        evidence_exclusions: evidence_exclusions(),
        query: impact_query_report(query),
        candidates: Vec::new(),
        selected: Some(selected),
        visibility_disposition: Some(visibility_disposition),
        summary: Some(ImpactSummaryReport {
            direct_reference_relationships: direct_references.len() as u64,
            transitive_consumers,
            production: provenance.contains(&ImpactProvenanceClass::Production),
            nonproduction: provenance.contains(&ImpactProvenanceClass::Nonproduction),
            build_time: provenance.contains(&ImpactProvenanceClass::BuildTime),
            public_interface: provenance.contains(&ImpactProvenanceClass::PublicInterface),
        }),
        direct_references,
        witnesses,
        reference_site_note:
            "Reference spans are representative reference sites, not exhaustive call sites."
                .to_owned(),
    }
}

fn matching_physical_declarations(
    root: &Path,
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
    query: &ImpactQuery,
) -> BTreeMap<PhysicalDeclarationKey, BTreeSet<usize>> {
    let mut matched_keys = BTreeSet::new();
    for (node, definition) in nodes.iter().enumerate().flat_map(|(node, entry)| {
        entry
            .definitions
            .iter()
            .copied()
            .map(move |definition| (node, definition))
    }) {
        let (invocation, definition) = definition;
        let input = invocations[invocation];
        let definition = &input.invocation.definitions[definition];
        if input.owner != query.package || definition.definition_path != query.definition_path {
            continue;
        }
        if query.location.as_ref().is_some_and(|location| {
            definition
                .span
                .as_ref()
                .or(definition.attribution_callsite.as_ref())
                .and_then(|span| source_span(root, input, span))
                .is_none_or(|span| {
                    normalize_report_path(&span.path) != normalize_report_path(&location.path)
                        || span.line != location.line
                        || span.column != location.column
                })
        }) {
            continue;
        }
        if let Some(key) = physical_declaration_key(input, definition) {
            let _ = node;
            matched_keys.insert(key);
        }
    }

    let mut matches = matched_keys
        .iter()
        .cloned()
        .map(|key| (key, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    if matches.is_empty() {
        return matches;
    }
    for (node, entry) in nodes.iter().enumerate() {
        for &(invocation, definition) in &entry.definitions {
            let input = invocations[invocation];
            let definition = &input.invocation.definitions[definition];
            let Some(key) = physical_declaration_key(input, definition) else {
                continue;
            };
            if let Some(group) = matches.get_mut(&key) {
                group.insert(node);
            }
        }
    }
    matches
}

fn physical_declaration_key(
    input: &GraphInvocation<'_>,
    definition: &Definition,
) -> Option<PhysicalDeclarationKey> {
    if let Some(span) = definition
        .span
        .as_ref()
        .or(definition.attribution_callsite.as_ref())
        .and_then(|span| span_identity(input.invocation, span))
    {
        Some(PhysicalDeclarationKey::Spanned {
            package_id: input.target.package_id.clone(),
            definition_path: definition.definition_path.clone(),
            span,
        })
    } else {
        Some(PhysicalDeclarationKey::Logical {
            package_id: input.target.package_id.clone(),
            target: Box::new(logical_target(input.target)),
            definition_path: definition.definition_path.clone(),
        })
    }
}

fn physical_visibility_nodes(
    selected: &BTreeSet<usize>,
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
) -> BTreeSet<usize> {
    let visibility_keys = selected
        .iter()
        .flat_map(|node| nodes[*node].definitions.iter())
        .filter_map(|(invocation, definition)| {
            definition_visibility_key(
                invocations[*invocation],
                &invocations[*invocation].invocation.definitions[*definition],
            )
        })
        .collect::<BTreeSet<_>>();
    if visibility_keys.is_empty() {
        return selected.clone();
    }
    nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            node.definitions.iter().any(|(invocation, definition)| {
                definition_visibility_key(
                    invocations[*invocation],
                    &invocations[*invocation].invocation.definitions[*definition],
                )
                .is_some_and(|key| visibility_keys.contains(&key))
            })
        })
        .map(|(node, _)| node)
        .collect()
}

fn states_for_nodes(nodes: &BTreeSet<usize>, graph_nodes: &[Node]) -> BTreeSet<GraphState> {
    nodes
        .iter()
        .flat_map(|node| {
            graph_nodes[*node]
                .definitions
                .iter()
                .map(move |(invocation, _)| GraphState {
                    invocation: *invocation,
                    node: *node,
                })
        })
        .collect()
}

fn reverse_reachable(
    selected: &BTreeSet<GraphState>,
    edges: &BTreeSet<ReferenceEdge>,
) -> (BTreeSet<GraphState>, BTreeMap<GraphState, ReferenceEdge>) {
    let mut incoming = BTreeMap::<GraphState, BTreeSet<ReferenceEdge>>::new();
    for edge in edges {
        incoming.entry(edge.to).or_default().insert(*edge);
    }
    let mut reached = selected.clone();
    let mut next_edge = BTreeMap::new();
    let mut pending = selected.iter().copied().collect::<VecDeque<_>>();
    while let Some(target) = pending.pop_front() {
        for edge in incoming.get(&target).into_iter().flatten() {
            if reached.insert(edge.from) {
                next_edge.insert(edge.from, *edge);
                pending.push_back(edge.from);
            }
        }
    }
    (reached, next_edge)
}

fn impact_witnesses(
    root: &Path,
    invocations: &[&GraphInvocation<'_>],
    graph: &Graph,
    selected: &BTreeSet<GraphState>,
    reached: &BTreeSet<GraphState>,
    next_edge: &BTreeMap<GraphState, ReferenceEdge>,
) -> Vec<ImpactWitnessReport> {
    let mut by_class = BTreeMap::<ImpactProvenanceClass, ImpactWitnessReport>::new();
    for (state, roots) in &graph.roots {
        if !reached.contains(state) {
            continue;
        }
        for evidence in roots {
            let class = if evidence.kind == RootKind::RequiredPublic {
                ImpactProvenanceClass::PublicInterface
            } else if let Some(class) = compiled_provenance(invocations[state.invocation].target) {
                class
            } else {
                continue;
            };
            let Some(root_definition) = definition_for_state(*state, invocations, &graph.nodes)
            else {
                continue;
            };
            let mut steps = Vec::new();
            let mut current = *state;
            while !selected.contains(&current) {
                let Some(edge) = next_edge.get(&current).copied() else {
                    steps.clear();
                    break;
                };
                let Some(step) = impact_reference_step(root, invocations, &graph.nodes, edge)
                else {
                    steps.clear();
                    break;
                };
                steps.push(step);
                current = edge.to;
            }
            if !selected.contains(&current) {
                continue;
            }
            let witness = ImpactWitnessReport {
                provenance: impact_provenance(invocations[state.invocation], class),
                root: impact_definition_report(
                    root,
                    invocations[state.invocation],
                    root_definition,
                ),
                root_reason: evidence.reason.clone(),
                steps,
            };
            match by_class.entry(class) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(witness);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if impact_witness_key(&witness) < impact_witness_key(entry.get()) {
                        entry.insert(witness);
                    }
                }
            }
        }
    }
    by_class.into_values().collect()
}

fn impact_reference_report(
    root: &Path,
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
    edge: ReferenceEdge,
) -> Option<ImpactReferenceReport> {
    let input = invocations[edge.invocation];
    let reference = input.invocation.references.get(edge.reference)?;
    let consumer = input
        .invocation
        .definitions
        .iter()
        .find(|definition| definition.compiler_id == reference.from)
        .or_else(|| definition_for_state(edge.from, invocations, nodes))?;
    let dependency_input = invocations[edge.to.invocation];
    let dependency = dependency_input
        .invocation
        .definitions
        .iter()
        .find(|definition| definition.compiler_id == reference.to)
        .or_else(|| definition_for_state(edge.to, invocations, nodes))?;
    let class = compiled_provenance(input.target)?;
    Some(ImpactReferenceReport {
        consumer: impact_definition_report(root, input, consumer),
        dependency: impact_definition_report(root, dependency_input, dependency),
        reference_kind: reference_kind(reference.kind).to_owned(),
        representative_span: reference
            .span
            .as_ref()
            .and_then(|span| source_span(root, input, span)),
        provenance: impact_provenance(input, class),
    })
}

fn impact_reference_step(
    root: &Path,
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
    edge: ReferenceEdge,
) -> Option<ImpactReferenceStepReport> {
    let input = invocations[edge.invocation];
    let reference = input.invocation.references.get(edge.reference)?;
    let from = input
        .invocation
        .definitions
        .iter()
        .find(|definition| definition.compiler_id == reference.from)
        .or_else(|| definition_for_state(edge.from, invocations, nodes))?;
    let to_input = invocations[edge.to.invocation];
    let to = to_input
        .invocation
        .definitions
        .iter()
        .find(|definition| definition.compiler_id == reference.to)
        .or_else(|| definition_for_state(edge.to, invocations, nodes))?;
    Some(ImpactReferenceStepReport {
        from: impact_definition_report(root, input, from),
        to: impact_definition_report(root, to_input, to),
        reference_kind: reference_kind(edge.kind).to_owned(),
        representative_span: reference
            .span
            .as_ref()
            .and_then(|span| source_span(root, input, span)),
    })
}

fn definition_for_state<'a>(
    state: GraphState,
    invocations: &[&'a GraphInvocation<'_>],
    nodes: &[Node],
) -> Option<&'a Definition> {
    nodes[state.node]
        .definitions
        .iter()
        .filter(|(invocation, _)| *invocation == state.invocation)
        .map(|(_, definition)| &invocations[state.invocation].invocation.definitions[*definition])
        .min_by_key(|definition| {
            (
                &definition.definition_path,
                definition.kind,
                definition.compiler_id,
            )
        })
}

fn representative_for_nodes<'a>(
    selected: &BTreeSet<usize>,
    invocations: &[&'a GraphInvocation<'_>],
    nodes: &[Node],
    query: Option<&ImpactQuery>,
) -> Option<(usize, &'a Definition)> {
    selected
        .iter()
        .flat_map(|node| nodes[*node].definitions.iter().copied())
        .map(|(invocation, definition)| {
            (
                invocation,
                &invocations[invocation].invocation.definitions[definition],
            )
        })
        .min_by_key(|(invocation, definition)| {
            let input = invocations[*invocation];
            (
                input.target.role != "production",
                query.is_some_and(|query| definition.definition_path != query.definition_path),
                input.target.package_id.as_str(),
                input.target.name.as_str(),
                definition.definition_path.as_str(),
                definition.kind,
                definition.compiler_id,
            )
        })
}

fn physical_key_for_state(
    state: GraphState,
    invocations: &[&GraphInvocation<'_>],
    nodes: &[Node],
) -> Option<PhysicalDeclarationKey> {
    definition_for_state(state, invocations, nodes)
        .and_then(|definition| physical_declaration_key(invocations[state.invocation], definition))
}

fn impact_definition_report(
    root: &Path,
    input: &GraphInvocation<'_>,
    definition: &Definition,
) -> ImpactDefinitionReport {
    ImpactDefinitionReport {
        package_id: input.target.package_id.clone(),
        package_name: input.owner.to_owned(),
        crate_name: input.crate_name.to_owned(),
        target_name: input.target.name.clone(),
        definition_path: definition.definition_path.clone(),
        definition_kind: definition_kind(definition.kind).to_owned(),
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

fn impact_provenance(
    input: &GraphInvocation<'_>,
    class: ImpactProvenanceClass,
) -> ImpactProvenanceReport {
    ImpactProvenanceReport {
        class,
        package_id: input.target.package_id.clone(),
        target_name: input.target.name.clone(),
        target_role: input.target.role.clone(),
        compilation_context: input.target.compilation_context.clone(),
    }
}

fn compiled_provenance(target: &CompilerTargetReport) -> Option<ImpactProvenanceClass> {
    if target.compilation_context == "host" {
        return Some(ImpactProvenanceClass::BuildTime);
    }
    match target.role.as_str() {
        "production" => Some(ImpactProvenanceClass::Production),
        "unit_test" | "test" | "bench" | "example" => Some(ImpactProvenanceClass::Nonproduction),
        "build" => Some(ImpactProvenanceClass::BuildTime),
        _ => None,
    }
}

fn impact_query_report(query: &ImpactQuery) -> ImpactQueryReport {
    ImpactQueryReport {
        package: query.package.clone(),
        definition_path: query.definition_path.clone(),
        path: query
            .location
            .as_ref()
            .map(|location| location.path.clone()),
        line: query.location.as_ref().map(|location| location.line),
        column: query.location.as_ref().map(|location| location.column),
    }
}

fn unavailable_impact(
    query: &ImpactQuery,
    reason: &str,
    mut candidates: Vec<ImpactDefinitionReport>,
) -> ImpactReport {
    candidates.sort_by_key(impact_definition_key);
    candidates.dedup();
    ImpactReport {
        status: SemanticStatus::Unavailable,
        reason: Some(reason.to_owned()),
        scope: SCOPE.to_owned(),
        evidence_exclusions: evidence_exclusions(),
        query: impact_query_report(query),
        candidates,
        selected: None,
        visibility_disposition: None,
        summary: None,
        direct_references: Vec::new(),
        witnesses: Vec::new(),
        reference_site_note:
            "Reference spans are representative reference sites, not exhaustive call sites."
                .to_owned(),
    }
}

fn normalize_report_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn impact_definition_key(definition: &ImpactDefinitionReport) -> String {
    let span = definition.span.as_ref().map_or_else(String::new, |span| {
        format!(
            "{}\0{:020}\0{:020}\0{:020}",
            span.path, span.line, span.column, span.start_byte
        )
    });
    format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{span}",
        definition.package_id,
        definition.package_name,
        definition.target_name,
        definition.crate_name,
        definition.definition_path,
        definition.definition_kind,
    )
}

fn impact_reference_key(reference: &ImpactReferenceReport) -> String {
    let span = reference
        .representative_span
        .as_ref()
        .map_or_else(String::new, |span| {
            format!("{}\0{:020}\0{:020}", span.path, span.line, span.column)
        });
    format!(
        "{:?}\0{}\0{}\0{}\0{}\0{}\0{}\0{span}",
        reference.provenance.class,
        reference.provenance.package_id,
        reference.provenance.target_name,
        reference.provenance.target_role,
        impact_definition_key(&reference.consumer),
        reference.reference_kind,
        impact_definition_key(&reference.dependency),
    )
}

fn impact_witness_key(witness: &ImpactWitnessReport) -> (usize, String) {
    let steps = witness
        .steps
        .iter()
        .map(|step| {
            format!(
                "{}\0{}\0{}",
                impact_definition_key(&step.from),
                step.reference_kind,
                impact_definition_key(&step.to)
            )
        })
        .collect::<Vec<_>>()
        .join("\0");
    (
        witness.steps.len(),
        format!(
            "{}\0{}\0{}",
            impact_definition_key(&witness.root),
            witness.root_reason,
            steps
        ),
    )
}

fn reference_kind(kind: ReferenceKind) -> &'static str {
    match kind {
        ReferenceKind::Body => "body",
        ReferenceKind::Interface => "interface",
        ReferenceKind::Reexport => "reexport",
        ReferenceKind::VisibilityParent => "visibility_parent",
        ReferenceKind::VisibilityRequirement => "visibility_requirement",
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

fn expansion_origin(origin: rot_compiler_protocol::ExpansionOrigin) -> &'static str {
    match origin {
        rot_compiler_protocol::ExpansionOrigin::Authored => "authored",
        rot_compiler_protocol::ExpansionOrigin::BuiltinDesugaring => "builtin_desugaring",
        rot_compiler_protocol::ExpansionOrigin::LocalMacro => "local_macro",
        rot_compiler_protocol::ExpansionOrigin::ExternalMacro => "external_macro",
    }
}

fn incomplete(status: SemanticStatus, reason: &str) -> Aggregation {
    Aggregation {
        status,
        reason: Some(reason.to_owned()),
        required_visibility: None,
        closed_world: None,
        impact: None,
    }
}

fn incomplete_with_query(
    status: SemanticStatus,
    reason: &str,
    query: Option<&ImpactQuery>,
) -> Aggregation {
    let mut aggregation = incomplete(status, reason);
    aggregation.impact = query.map(|query| unavailable_impact(query, reason, Vec::new()));
    aggregation
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rot_compiler_protocol::{
        ArtifactIdentity, CompilationContext, CompilerIdentity, ExpansionOrigin, FactId,
        InvocationFinished, InvocationId, InvocationMergeKey, InvocationStarted, Reference, Root,
        SourceFile, SourceFileKey,
    };

    const ROOT: &str = "/workspace";

    #[test]
    fn normal_and_test_fragments_do_not_stitch_impossible_paths() {
        let source = source("/workspace/member/src/lib.rs");
        let mut macro_emitted =
            definition(10, 9, "member::macro_emitted", DefinitionKind::Function, 16);
        macro_emitted.expansion_origin = ExpansionOrigin::LocalMacro;
        let normal_definitions = vec![
            definition(10, 1, "member::common", DefinitionKind::Function, 0),
            definition(
                10,
                2,
                "member::production_only",
                DefinitionKind::Function,
                2,
            ),
            definition(10, 3, "member::entry", DefinitionKind::Function, 4),
            definition(10, 5, "member::reexport", DefinitionKind::Import, 8),
            definition(10, 6, "member::exported_macro", DefinitionKind::Macro, 10),
            definition(10, 7, "member::opaque", DefinitionKind::OpaqueType, 12),
            definition(10, 8, "member::opted_out", DefinitionKind::Function, 14),
            macro_emitted,
        ];
        let test_definitions = vec![
            definition(11, 1, "member::common", DefinitionKind::Function, 0),
            definition(11, 4, "member::test_only", DefinitionKind::Function, 6),
        ];
        let normal = invocation(
            "normal",
            vec![source.clone()],
            normal_definitions,
            vec![
                root(10, 3, RootKind::Conservative),
                Root {
                    reason: "dead_code is explicitly allowed for this definition".to_owned(),
                    ..root(10, 8, RootKind::Conservative)
                },
            ],
            vec![reference(10, 1, 10, 2, ReferenceKind::Body)],
        );
        let test = invocation(
            "test",
            vec![source],
            test_definitions,
            vec![root(11, 1, RootKind::EntryPoint)],
            vec![reference(11, 1, 11, 4, ReferenceKind::Body)],
        );
        let normal_target = target("member", "production", "target", &["lib"]);
        let test_target = target("member", "unit_test", "target", &["lib"]);
        let inputs = [
            graph_input(&normal_target, &normal),
            graph_input(&test_target, &test),
        ];

        let aggregation = aggregate(Path::new(ROOT), SemanticStatus::Complete, &inputs);
        let report = aggregation.closed_world.expect("complete liveness report");
        let production_only = report
            .findings
            .iter()
            .find(|finding| finding.definition_path == "member::production_only")
            .expect("production-only finding");
        assert_eq!(production_only.kind, "dead_public");
        assert!(!production_only.nonproduction_live);
        let test_only = report
            .findings
            .iter()
            .find(|finding| finding.definition_path == "member::test_only")
            .expect("test-compiled finding");
        assert!(test_only.test_compiled_only);
        assert!(!test_only.production_live);
        assert!(test_only.nonproduction_live);
        assert!(report.findings.iter().all(|finding| {
            !matches!(
                finding.definition_path.as_str(),
                "member::reexport"
                    | "member::exported_macro"
                    | "member::opaque"
                    | "member::opted_out"
                    | "member::macro_emitted"
            )
        }));
    }

    #[test]
    fn impact_keeps_production_and_nonproduction_chains_separate() {
        let library = invocation(
            "library",
            vec![source("/workspace/helper/src/lib.rs")],
            vec![definition(
                20,
                1,
                "helper::selected",
                DefinitionKind::Function,
                0,
            )],
            Vec::new(),
            Vec::new(),
        );
        let production = invocation(
            "production",
            vec![source("/workspace/app/src/main.rs")],
            vec![definition(30, 1, "app::main", DefinitionKind::Function, 0)],
            vec![root(30, 1, RootKind::EntryPoint)],
            vec![reference(30, 1, 20, 1, ReferenceKind::Body)],
        );
        let test = invocation(
            "integration-test",
            vec![source("/workspace/app/tests/check.rs")],
            vec![definition(
                40,
                1,
                "check::test",
                DefinitionKind::Function,
                0,
            )],
            vec![root(40, 1, RootKind::EntryPoint)],
            vec![reference(40, 1, 20, 1, ReferenceKind::Body)],
        );
        let library_target = target("helper", "production", "target", &["lib"]);
        let production_target = target("app", "production", "target", &["bin"]);
        let test_target = target("app", "test", "target", &["test"]);
        let inputs = [
            graph_input(&library_target, &library),
            graph_input(&production_target, &production),
            graph_input(&test_target, &test),
        ];

        let query = query("helper::selected");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        )
        .impact
        .expect("impact report");

        assert_eq!(impact.status, SemanticStatus::Complete);
        let summary = impact.summary.as_ref().expect("complete impact summary");
        assert_eq!(summary.direct_reference_relationships, 2);
        assert_eq!(summary.transitive_consumers, 2);
        assert!(summary.production);
        assert!(summary.nonproduction);
        assert!(!summary.build_time);
        let witnesses = impact
            .witnesses
            .iter()
            .map(|witness| {
                (
                    witness.provenance.class,
                    witness.root.definition_path.as_str(),
                    witness.steps.len(),
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            witnesses,
            BTreeSet::from([
                (ImpactProvenanceClass::Production, "app::main", 1),
                (ImpactProvenanceClass::Nonproduction, "check::test", 1,),
            ])
        );
    }

    #[test]
    fn host_build_consumers_require_selected_library_visibility() {
        let library = invocation(
            "library",
            vec![source("/workspace/helper/src/lib.rs")],
            vec![definition(
                20,
                1,
                "helper::api",
                DefinitionKind::Function,
                0,
            )],
            Vec::new(),
            Vec::new(),
        );
        let build = invocation(
            "build",
            vec![source("/workspace/consumer/build.rs")],
            vec![definition(
                30,
                1,
                "build_script::main",
                DefinitionKind::Function,
                0,
            )],
            vec![root(30, 1, RootKind::EntryPoint)],
            vec![reference(30, 1, 20, 1, ReferenceKind::Body)],
        );
        let library_target = target("helper", "production", "host", &["lib"]);
        let build_target = target("consumer", "build", "host", &["custom-build"]);
        let inputs = [
            graph_input(&library_target, &library),
            graph_input(&build_target, &build),
        ];

        let aggregation = aggregate(Path::new(ROOT), SemanticStatus::Complete, &inputs);
        assert_eq!(aggregation.status, SemanticStatus::Complete);
        assert!(aggregation.closed_world.is_some());
        let required = aggregation
            .required_visibility
            .expect("complete visibility report");
        assert_eq!(required.definitions.len(), 1);
        assert_eq!(required.definitions[0].definition_path, "helper::api");
        assert_eq!(required.definitions[0].required_visibility, "public");

        let query = query("helper::api");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        )
        .impact
        .expect("impact report");
        assert_eq!(impact.status, SemanticStatus::Complete);
        assert_eq!(
            impact.visibility_disposition,
            Some(ImpactVisibilityDisposition::RequiredPublic)
        );
        let summary = impact.summary.expect("complete impact summary");
        assert!(summary.build_time);
        assert!(!summary.production);
        assert_eq!(summary.direct_reference_relationships, 1);
        assert_eq!(impact.witnesses.len(), 1);
        assert_eq!(
            impact.witnesses[0].provenance.class,
            ImpactProvenanceClass::BuildTime
        );
    }

    #[test]
    fn host_proc_macro_consumers_are_build_time_impact() {
        let library = invocation(
            "library",
            vec![source("/workspace/helper/src/lib.rs")],
            vec![definition(
                21,
                1,
                "helper::api",
                DefinitionKind::Function,
                0,
            )],
            Vec::new(),
            Vec::new(),
        );
        let proc_macro = invocation(
            "proc-macro",
            vec![source("/workspace/derive/src/lib.rs")],
            vec![definition(
                31,
                1,
                "derive::expand",
                DefinitionKind::Macro,
                0,
            )],
            vec![root(31, 1, RootKind::EntryPoint)],
            vec![reference(31, 1, 21, 1, ReferenceKind::Body)],
        );
        let library_target = target("helper", "production", "host", &["lib"]);
        let macro_target = target("derive", "production", "host", &["proc-macro"]);
        let inputs = [
            graph_input(&library_target, &library),
            graph_input(&macro_target, &proc_macro),
        ];

        let query = query("helper::api");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        )
        .impact
        .expect("impact report");

        assert_eq!(impact.status, SemanticStatus::Complete);
        let summary = impact.summary.expect("complete impact summary");
        assert!(summary.build_time);
        assert!(!summary.production);
        assert_eq!(impact.witnesses.len(), 1);
        assert_eq!(
            impact.witnesses[0].provenance.class,
            ImpactProvenanceClass::BuildTime
        );
    }

    #[test]
    fn public_trait_propagates_visibility_to_signature_types() {
        let library = invocation(
            "library",
            vec![source("/workspace/api/src/lib.rs")],
            vec![
                definition(40, 1, "api::Contract", DefinitionKind::Trait, 0),
                definition(40, 2, "api::Payload", DefinitionKind::Struct, 2),
            ],
            vec![root(40, 1, RootKind::RequiredPublic)],
            vec![reference(
                40,
                1,
                40,
                2,
                ReferenceKind::VisibilityRequirement,
            )],
        );
        let target = target("api", "production", "target", &["lib"]);
        let inputs = [graph_input(&target, &library)];

        let aggregation = aggregate(Path::new(ROOT), SemanticStatus::Complete, &inputs);
        let paths = aggregation
            .required_visibility
            .unwrap()
            .definitions
            .into_iter()
            .map(|definition| definition.definition_path)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            paths,
            BTreeSet::from(["api::Contract".to_owned(), "api::Payload".to_owned()])
        );

        let query = query("api::Payload");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        )
        .impact
        .expect("impact report");
        assert_eq!(impact.status, SemanticStatus::Complete);
        assert!(impact.summary.as_ref().unwrap().public_interface);
        let witness = impact
            .witnesses
            .iter()
            .find(|witness| witness.provenance.class == ImpactProvenanceClass::PublicInterface)
            .expect("public-interface witness");
        assert_eq!(witness.root.definition_path, "api::Contract");
        assert_eq!(witness.steps.len(), 1);
        assert_eq!(witness.steps[0].reference_kind, "visibility_requirement");
    }

    #[test]
    fn one_required_occurrence_suppresses_the_same_physical_visibility_token() {
        let shared = source("/workspace/shared/item.rs");
        let first = invocation(
            "first",
            vec![shared.clone()],
            vec![
                definition(50, 1, "first::shared", DefinitionKind::Function, 0),
                definition(50, 2, "first::entry", DefinitionKind::Function, 2),
                definition(50, 3, "first::payload", DefinitionKind::Struct, 4),
            ],
            vec![root(50, 2, RootKind::Conservative)],
            vec![reference(
                50,
                1,
                50,
                3,
                ReferenceKind::VisibilityRequirement,
            )],
        );
        let mut included = definition(60, 1, "second::shared", DefinitionKind::Function, 0);
        included.expansion_origin = ExpansionOrigin::ExternalMacro;
        let second = invocation(
            "second",
            vec![shared],
            vec![included],
            vec![root(60, 1, RootKind::RequiredPublic)],
            Vec::new(),
        );
        let first_target = target("first", "production", "target", &["lib"]);
        let second_target = target("second", "production", "target", &["lib"]);
        let inputs = [
            graph_input(&first_target, &first),
            graph_input(&second_target, &second),
        ];

        let aggregation = aggregate(Path::new(ROOT), SemanticStatus::Complete, &inputs);
        let required = aggregation.required_visibility.unwrap();
        assert!(
            required
                .definitions
                .iter()
                .any(|definition| definition.definition_path == "first::payload")
        );
        let report = aggregation.closed_world.unwrap();
        assert!(report.findings.iter().all(|finding| {
            !matches!(
                finding.definition_path.as_str(),
                "first::shared" | "second::shared"
            )
        }));

        let query = query("first::shared");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        )
        .impact
        .expect("impact report");
        assert_eq!(impact.status, SemanticStatus::Complete);
        assert_eq!(
            impact.visibility_disposition,
            Some(ImpactVisibilityDisposition::RequiredPublic)
        );
        assert_eq!(impact.selected.as_ref().unwrap().package_id, "first");
        assert!(!impact.summary.as_ref().unwrap().public_interface);
        assert!(impact.direct_references.is_empty());
        assert!(impact.witnesses.is_empty());
    }

    #[test]
    fn reexported_target_does_not_require_its_original_module_path() {
        let library = invocation(
            "library",
            vec![source("/workspace/api/src/lib.rs")],
            vec![
                definition(70, 1, "api::original", DefinitionKind::Module, 0),
                definition(70, 2, "api::original::item", DefinitionKind::Function, 2),
            ],
            vec![Root {
                reason: "a public reexport requires this local target to remain public".to_owned(),
                ..root(70, 2, RootKind::RequiredPublic)
            }],
            vec![reference(70, 2, 70, 1, ReferenceKind::VisibilityParent)],
        );
        let target = target("api", "production", "target", &["lib"]);
        let inputs = [graph_input(&target, &library)];

        let paths = aggregate(Path::new(ROOT), SemanticStatus::Complete, &inputs)
            .required_visibility
            .unwrap()
            .definitions
            .into_iter()
            .map(|definition| definition.definition_path)
            .collect::<BTreeSet<_>>();
        assert!(paths.contains("api::original::item"));
        assert!(!paths.contains("api::original"));
    }

    #[test]
    fn impact_reports_dead_public_with_zero_consumers() {
        let library = invocation(
            "library",
            vec![source("/workspace/api/src/lib.rs")],
            vec![definition(80, 1, "api::dead", DefinitionKind::Function, 0)],
            Vec::new(),
            Vec::new(),
        );
        let target = target("api", "production", "target", &["lib"]);
        let inputs = [graph_input(&target, &library)];

        let query = query("api::dead");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        )
        .impact
        .expect("impact report");

        assert_eq!(impact.status, SemanticStatus::Complete);
        assert_eq!(
            impact.visibility_disposition,
            Some(ImpactVisibilityDisposition::DeadPublic)
        );
        let summary = impact.summary.expect("complete impact summary");
        assert_eq!(summary.direct_reference_relationships, 0);
        assert_eq!(summary.transitive_consumers, 0);
        assert!(!summary.production);
        assert!(!summary.nonproduction);
        assert!(impact.direct_references.is_empty());
        assert!(impact.witnesses.is_empty());
    }

    #[test]
    fn ambiguous_impact_query_fails_closed_and_exact_location_selects() {
        let library = invocation(
            "library",
            vec![source("/workspace/api/src/lib.rs")],
            vec![
                definition(90, 1, "api::duplicate", DefinitionKind::Function, 0),
                definition(90, 2, "api::duplicate", DefinitionKind::Function, 2),
            ],
            Vec::new(),
            Vec::new(),
        );
        let target = target("api", "production", "target", &["lib"]);
        let inputs = [graph_input(&target, &library)];

        let query = query("api::duplicate");
        let aggregation = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        );
        assert_eq!(aggregation.status, SemanticStatus::Complete);
        assert!(aggregation.closed_world.is_some());
        let impact = aggregation.impact.expect("impact report");
        assert_eq!(impact.status, SemanticStatus::Unavailable);
        assert!(impact.summary.is_none());
        assert_eq!(impact.candidates.len(), 2);
        assert!(impact.direct_references.is_empty());

        let exact = ImpactQuery {
            location: Some(ImpactLocationQuery {
                path: "api/src/lib.rs".to_owned(),
                line: 1,
                column: 3,
            }),
            ..query
        };
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&exact),
        )
        .impact
        .expect("impact report");
        assert_eq!(impact.status, SemanticStatus::Complete);
        assert_eq!(impact.selected.unwrap().span.unwrap().column, 3);
    }

    #[test]
    fn exact_impact_location_accepts_the_reported_macro_callsite() {
        let mut first_generated = definition(91, 1, "api::generated", DefinitionKind::Function, 0);
        first_generated.attribution_callsite = first_generated.span.take();
        first_generated.expansion_origin = ExpansionOrigin::ExternalMacro;
        let first = invocation(
            "first",
            vec![source("/workspace/api/src/lib.rs")],
            vec![
                first_generated,
                definition(91, 2, "api::first_consumer", DefinitionKind::Function, 4),
            ],
            vec![root(91, 2, RootKind::EntryPoint)],
            vec![reference(91, 2, 91, 1, ReferenceKind::Body)],
        );
        let mut second_generated = definition(92, 1, "api::generated", DefinitionKind::Function, 2);
        second_generated.attribution_callsite = second_generated.span.take();
        second_generated.expansion_origin = ExpansionOrigin::ExternalMacro;
        let second = invocation(
            "second",
            vec![source("/workspace/api/src/lib.rs")],
            vec![
                second_generated,
                definition(92, 2, "api::second_consumer", DefinitionKind::Function, 6),
            ],
            vec![root(92, 2, RootKind::EntryPoint)],
            vec![reference(92, 2, 92, 1, ReferenceKind::Body)],
        );
        let first_target = target("api", "production", "target", &["lib"]);
        let mut second_target = first_target.clone();
        second_target.name = "api-other".to_owned();
        second_target.source = "/workspace/api/src/other.rs".to_owned();
        let inputs = [
            graph_input(&first_target, &first),
            graph_input(&second_target, &second),
        ];
        let ambiguous = query("api::generated");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&ambiguous),
        )
        .impact
        .expect("impact report");
        assert_eq!(impact.status, SemanticStatus::Unavailable);
        assert_eq!(impact.candidates.len(), 2);

        let exact = ImpactQuery {
            location: Some(ImpactLocationQuery {
                path: "api/src/lib.rs".to_owned(),
                line: 1,
                column: 1,
            }),
            ..ambiguous
        };

        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&exact),
        )
        .impact
        .expect("impact report");

        assert_eq!(impact.status, SemanticStatus::Complete);
        let selected = impact.selected.expect("selected definition");
        assert!(selected.span.is_none());
        assert_eq!(selected.attribution_callsite.unwrap().column, 1);
        assert_eq!(impact.summary.unwrap().direct_reference_relationships, 1);
        assert_eq!(impact.direct_references.len(), 1);
        assert_eq!(
            impact.direct_references[0].consumer.definition_path,
            "api::first_consumer"
        );
    }

    #[test]
    fn cyclic_consumers_have_one_shortest_deterministic_witness() {
        let library = invocation(
            "library",
            vec![source("/workspace/api/src/lib.rs")],
            vec![
                definition(100, 1, "api::root", DefinitionKind::Function, 0),
                definition(100, 2, "api::selected", DefinitionKind::Function, 2),
            ],
            vec![root(100, 1, RootKind::EntryPoint)],
            vec![
                reference(100, 1, 100, 2, ReferenceKind::Body),
                reference(100, 2, 100, 1, ReferenceKind::Body),
            ],
        );
        let target = target("api", "production", "target", &["bin"]);
        let inputs = [graph_input(&target, &library)];

        let query = query("api::selected");
        let impact = aggregate_with_query(
            Path::new(ROOT),
            SemanticStatus::Complete,
            &inputs,
            Some(&query),
        )
        .impact
        .expect("impact report");

        assert_eq!(impact.status, SemanticStatus::Complete);
        assert_eq!(impact.summary.as_ref().unwrap().transitive_consumers, 1);
        assert_eq!(impact.witnesses.len(), 1);
        assert_eq!(impact.witnesses[0].root.definition_path, "api::root");
        assert_eq!(impact.witnesses[0].steps.len(), 1);
        assert_eq!(
            impact.witnesses[0].steps[0].to.definition_path,
            "api::selected"
        );
    }

    #[test]
    fn generated_files_are_never_reported_as_edit_targets() {
        let mut generated = source("/workspace/target/generated.rs");
        generated.generated = true;
        let library = invocation(
            "library",
            vec![generated],
            vec![
                definition(80, 1, "generated_required", DefinitionKind::Function, 0),
                definition(80, 2, "generated_dead", DefinitionKind::Function, 2),
            ],
            vec![root(80, 1, RootKind::RequiredPublic)],
            Vec::new(),
        );
        let target = target("api", "production", "target", &["lib"]);
        let inputs = [graph_input(&target, &library)];

        let aggregation = aggregate(Path::new(ROOT), SemanticStatus::Complete, &inputs);
        assert!(
            aggregation
                .required_visibility
                .unwrap()
                .definitions
                .is_empty()
        );
        assert!(aggregation.closed_world.unwrap().findings.is_empty());
    }

    fn graph_input<'a>(
        target: &'a CompilerTargetReport,
        invocation: &'a Invocation,
    ) -> GraphInvocation<'a> {
        GraphInvocation {
            target,
            owner: "owner",
            crate_name: &invocation.started.crate_name,
            status: SemanticStatus::Complete,
            invocation,
        }
    }

    fn query(definition_path: &str) -> ImpactQuery {
        ImpactQuery {
            package: "owner".to_owned(),
            definition_path: definition_path.to_owned(),
            location: None,
        }
    }

    fn target(package: &str, role: &str, context: &str, kinds: &[&str]) -> CompilerTargetReport {
        CompilerTargetReport {
            package_id: package.to_owned(),
            name: package.to_owned(),
            kinds: kinds.iter().map(|kind| (*kind).to_owned()).collect(),
            crate_types: kinds.iter().map(|kind| (*kind).to_owned()).collect(),
            source: format!("/workspace/{package}/src/lib.rs"),
            role: role.to_owned(),
            compilation_context: context.to_owned(),
        }
    }

    fn invocation(
        name: &str,
        sources: Vec<SourceFile>,
        definitions: Vec<Definition>,
        roots: Vec<Root>,
        references: Vec<Reference>,
    ) -> Invocation {
        let compiler = CompilerIdentity {
            release: "nightly".to_owned(),
            commit_hash: "commit".to_owned(),
            commit_date: "date".to_owned(),
            host: "host".to_owned(),
        };
        Invocation {
            id: InvocationId(name.to_owned()),
            started: InvocationStarted {
                merge_key: InvocationMergeKey(name.to_owned()),
                compiler,
                process_id: 1,
                rustc_path: "rustc".to_owned(),
                working_directory: ROOT.to_owned(),
                manifest_dir: Some(ROOT.to_owned()),
                build_script_out_dir: None,
                package_name: Some(name.to_owned()),
                primary_package: true,
                test_mode: false,
                target_triple: "target".to_owned(),
                compilation_context: CompilationContext::Target,
                crate_name: name.to_owned(),
                input: Some(format!("{name}.rs")),
                artifact: ArtifactIdentity {
                    out_dir: Some("out".to_owned()),
                    crate_name: name.to_owned(),
                    crate_types: vec!["lib".to_owned()],
                    extra_filename: Some("-hash".to_owned()),
                    metadata: Some("hash".to_owned()),
                    emit: vec!["metadata".to_owned()],
                },
            },
            profile: None,
            sources,
            products: Vec::new(),
            diagnostics: Vec::new(),
            definitions,
            bindings: Vec::new(),
            roots,
            references,
            finished: InvocationFinished {
                rustc_success: true,
                analysis_reached: true,
            },
        }
    }

    fn definition(
        stable_crate_id: u64,
        local_hash: u64,
        path: &str,
        kind: DefinitionKind,
        start: u32,
    ) -> Definition {
        Definition {
            id: FactId(format!("definition-{stable_crate_id}-{local_hash}")),
            compiler_id: CompilerDefId {
                stable_crate_id,
                local_hash,
            },
            parent: None,
            name: path.rsplit("::").next().map(ToOwned::to_owned),
            definition_path: path.to_owned(),
            kind,
            visibility_editable: true,
            nominal_visibility: NominalVisibility::Public,
            externally_reachable: true,
            span: Some(SourceSpan {
                file: SourceFileKey("source".to_owned()),
                start,
                end: start + 1,
                line: 1,
                column: start + 1,
            }),
            attribution_callsite: None,
            expansion_origin: ExpansionOrigin::Authored,
        }
    }

    fn source(path: &str) -> SourceFile {
        SourceFile {
            key: SourceFileKey("source".to_owned()),
            local_path: Some(path.to_owned()),
            remapped_path: path.to_owned(),
            source_hash_algorithm: "sha256".to_owned(),
            source_hash: format!("sha256={}", "0".repeat(64)),
            byte_len: 64,
            generated: false,
        }
    }

    fn root(stable_crate_id: u64, local_hash: u64, kind: RootKind) -> Root {
        Root {
            id: FactId(format!("root-{stable_crate_id}-{local_hash}")),
            definition: CompilerDefId {
                stable_crate_id,
                local_hash,
            },
            kind,
            reason: "test root".to_owned(),
        }
    }

    fn reference(
        from_crate: u64,
        from_hash: u64,
        to_crate: u64,
        to_hash: u64,
        kind: ReferenceKind,
    ) -> Reference {
        Reference {
            id: FactId(format!(
                "reference-{from_crate}-{from_hash}-{to_crate}-{to_hash}"
            )),
            from: CompilerDefId {
                stable_crate_id: from_crate,
                local_hash: from_hash,
            },
            to: CompilerDefId {
                stable_crate_id: to_crate,
                local_hash: to_hash,
            },
            kind,
            span: None,
        }
    }
}
