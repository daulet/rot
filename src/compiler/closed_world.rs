use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use rot_compiler_protocol::{
    BodyKind, CompilerDefId, Definition, DefinitionKind, NominalVisibility, ReferenceKind,
    RootKind, SourceSpan,
};

use crate::model::{
    ClosedWorldFindingReport, ClosedWorldReport, ClosedWorldSummaryReport,
    CompilerDefinitionIdReport, CompilerSourceSpanReport, CompilerTargetReport,
    ProductAvailabilityReport, RequiredVisibilityDefinitionReport, RequiredVisibilityReport,
    SemanticStatus,
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
    pub liveness_product: ProductAvailabilityReport,
    pub required_visibility_product: ProductAvailabilityReport,
    pub required_visibility: Option<RequiredVisibilityReport>,
    pub closed_world: Option<ClosedWorldReport>,
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
        target: LogicalTarget,
        definition_path: String,
        kind: DefinitionKind,
        span: SpanIdentity,
    },
    Body {
        package_id: String,
        target: LogicalTarget,
        definition_path: String,
        kind: BodyKind,
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

struct Graph {
    nodes: Vec<Node>,
    runtime_edges: BTreeSet<(GraphState, GraphState)>,
    interface_edges: BTreeSet<(GraphState, GraphState, ReferenceKind)>,
    all_edges: BTreeSet<(usize, usize, ReferenceKind)>,
    production_roots: BTreeSet<GraphState>,
    nonproduction_roots: BTreeSet<GraphState>,
    suppressed_candidates: BTreeSet<usize>,
    required_state_reasons: BTreeMap<GraphState, BTreeSet<String>>,
    required_reasons: BTreeMap<usize, BTreeSet<String>>,
    errors: Vec<String>,
}

pub(super) fn aggregate(
    root: &Path,
    raw_status: SemanticStatus,
    invocations: &[GraphInvocation<'_>],
) -> Aggregation {
    if raw_status != SemanticStatus::Complete {
        return incomplete(
            raw_status,
            "closed-world products require complete reference facts for every selected Cargo unit",
        );
    }
    let relevant = invocations.iter().collect::<Vec<_>>();
    if relevant.is_empty() {
        return incomplete(
            SemanticStatus::Unavailable,
            "no selected compiler fragments",
        );
    }
    if relevant
        .iter()
        .any(|invocation| invocation.status != SemanticStatus::Complete)
    {
        return incomplete(
            SemanticStatus::Partial,
            "at least one selected reference fragment is incomplete",
        );
    }

    let mut graph = build_graph(&relevant);
    propagate_required(&mut graph);
    if !graph.errors.is_empty() {
        graph.errors.sort();
        graph.errors.dedup();
        return incomplete(
            SemanticStatus::Partial,
            &format!(
                "reference graph could not be closed: {}",
                graph.errors.join("; ")
            ),
        );
    }

    let required_visibility = Some(required_visibility_report(
        root,
        &relevant,
        &graph.nodes,
        &graph.required_reasons,
    ));
    let required_visibility_product = complete_product("required_visibility");

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
        liveness_product: complete_product("closed_world_liveness"),
        required_visibility_product,
        required_visibility,
        closed_world: Some(report),
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
        for body in &input.invocation.bodies {
            if raw_nodes.contains_key(&(invocation_index, body.compiler_id)) {
                continue;
            }
            let key = body_key(invocation_index, input, body);
            let node = intern_node(&mut nodes, &mut node_by_key, key);
            raw_nodes.insert((invocation_index, body.compiler_id), node);
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

        for reference in &input.invocation.references {
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
                target: logical_target(input.target),
                definition_path: definition.definition_path.clone(),
                kind: definition.kind,
                span,
            },
        )
}

fn body_key(
    invocation_index: usize,
    input: &GraphInvocation<'_>,
    body: &rot_compiler_protocol::Body,
) -> NodeKey {
    body.span
        .as_ref()
        .and_then(|span| span_identity(input.invocation, span))
        .map_or(
            NodeKey::InvocationLocal {
                invocation: invocation_index,
                compiler_id: body.compiler_id,
            },
            |span| NodeKey::Body {
                package_id: input.target.package_id.clone(),
                target: logical_target(input.target),
                definition_path: body.definition_path.clone(),
                kind: body.kind,
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
                eligible_candidate(input.target, definition)
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

fn eligible_candidate(target: &CompilerTargetReport, definition: &Definition) -> bool {
    candidate_target(target)
        && definition.effective_public_at.is_some()
        && definition.visibility_editable
        && definition.expansion_origin == rot_compiler_protocol::ExpansionOrigin::Authored
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
        && !(definition.kind == DefinitionKind::Field
            && definition.expansion_origin != rot_compiler_protocol::ExpansionOrigin::Authored)
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
                || definition.effective_public_at.is_none()
                || !definition.visibility_editable
                || definition.expansion_origin != rot_compiler_protocol::ExpansionOrigin::Authored
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

fn complete_product(product: &str) -> ProductAvailabilityReport {
    ProductAvailabilityReport {
        product: product.to_owned(),
        status: SemanticStatus::Complete,
        reason: None,
    }
}

fn incomplete(status: SemanticStatus, reason: &str) -> Aggregation {
    Aggregation {
        liveness_product: ProductAvailabilityReport {
            product: "closed_world_liveness".to_owned(),
            status,
            reason: Some(reason.to_owned()),
        },
        required_visibility_product: ProductAvailabilityReport {
            product: "required_visibility".to_owned(),
            status,
            reason: Some(reason.to_owned()),
        },
        required_visibility: None,
        closed_world: None,
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rot_compiler_protocol::{
        ArtifactIdentity, CompilationContext, CompilerIdentity, EffectiveVisibilityLevel,
        ExpansionOrigin, FactId, InvocationFinished, InvocationId, InvocationMergeKey,
        InvocationStarted, Reference, Root, SourceFile, SourceFileKey,
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
    fn host_build_consumers_require_selected_library_visibility() {
        let library = invocation(
            "library",
            Vec::new(),
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
            Vec::new(),
            vec![definition(
                30,
                1,
                "build_script::main",
                DefinitionKind::Function,
                0,
            )],
            Vec::new(),
            vec![reference(30, 1, 20, 1, ReferenceKind::Body)],
        );
        let library_target = target("helper", "production", "host", &["lib"]);
        let build_target = target("consumer", "build", "host", &["custom-build"]);
        let inputs = [
            graph_input(&library_target, &library),
            graph_input(&build_target, &build),
        ];

        let aggregation = aggregate(Path::new(ROOT), SemanticStatus::Complete, &inputs);
        assert_eq!(
            aggregation.liveness_product.status,
            SemanticStatus::Complete
        );
        assert!(aggregation.closed_world.is_some());
        let required = aggregation
            .required_visibility
            .expect("complete visibility report");
        assert_eq!(required.definitions.len(), 1);
        assert_eq!(required.definitions[0].definition_path, "helper::api");
        assert_eq!(required.definitions[0].required_visibility, "public");
    }

    #[test]
    fn public_trait_propagates_visibility_to_signature_types() {
        let library = invocation(
            "library",
            Vec::new(),
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
    }

    #[test]
    fn reexported_target_does_not_require_its_original_module_path() {
        let library = invocation(
            "library",
            Vec::new(),
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
            bodies: Vec::new(),
            products: Vec::new(),
            diagnostics: Vec::new(),
            definitions,
            public_bindings: Vec::new(),
            roots,
            references,
            decisions: Vec::new(),
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
            effective_public_at: Some(EffectiveVisibilityLevel::Direct),
            span: Some(SourceSpan {
                file: SourceFileKey("source".to_owned()),
                start,
                end: start + 1,
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
