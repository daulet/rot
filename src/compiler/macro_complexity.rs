use std::collections::{BTreeMap, BTreeSet};

use rot_compiler_protocol::{
    Body, CompilerDefId, Decision, DecisionKind, ExpansionOrigin, MacroKind,
};

use crate::model::{
    CompilerTargetReport, MacroExpansionComplexityReport, MacroExpansionDeltaBreakdownReport,
    MacroExpansionInvocationMetricsReport, MacroExpansionInvocationReport, SemanticStatus,
};

pub(super) struct MacroInvocation<'a> {
    pub key: &'a str,
    pub target: Option<&'a CompilerTargetReport>,
    pub crate_name: &'a str,
    pub status: SemanticStatus,
    pub reason: Option<&'a str>,
    pub bodies: &'a [Body],
    pub decisions: &'a [Decision],
}

pub(super) fn aggregate<'a>(
    collection_trustworthy: bool,
    invocations: impl IntoIterator<Item = MacroInvocation<'a>>,
) -> Option<MacroExpansionComplexityReport> {
    let mut invocations = invocations
        .into_iter()
        .map(|invocation| invocation_report(collection_trustworthy, invocation))
        .collect::<Vec<_>>();
    if invocations.is_empty() {
        return None;
    }
    invocations.sort_by(|left, right| left.key.cmp(&right.key));
    Some(MacroExpansionComplexityReport {
        metric: "macro_expansion_cyclomatic_delta".to_owned(),
        baseline: "source-authored cyclomatic complexity; combine only after exact body and profile correlation"
            .to_owned(),
        invocations,
    })
}

fn invocation_report(
    collection_trustworthy: bool,
    invocation: MacroInvocation<'_>,
) -> MacroExpansionInvocationReport {
    let mut status = invocation.status;
    let mut reasons = invocation
        .reason
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let metrics = if !collection_trustworthy {
        if status == SemanticStatus::Complete {
            status = SemanticStatus::Partial;
        }
        reasons.push("compiler sidecar or generated-source integrity was incomplete".to_owned());
        None
    } else if status != SemanticStatus::Complete {
        None
    } else {
        match measure(invocation.bodies, invocation.decisions) {
            Ok(metrics) => Some(metrics),
            Err(reason) => {
                status = SemanticStatus::Partial;
                reasons.push(reason);
                None
            }
        }
    };
    reasons.sort();
    reasons.dedup();
    MacroExpansionInvocationReport {
        key: invocation.key.to_owned(),
        target: invocation.target.cloned(),
        crate_name: invocation.crate_name.to_owned(),
        status,
        reason: (!reasons.is_empty()).then(|| reasons.join("; ")),
        metrics,
    }
}

fn measure(
    bodies: &[Body],
    decisions: &[Decision],
) -> Result<MacroExpansionInvocationMetricsReport, String> {
    validate(bodies, decisions)?;
    let mut totals = MacroExpansionDeltaBreakdownReport::default();
    let mut by_origin = BTreeMap::new();
    let mut by_macro_kind = BTreeMap::new();
    let mut decisions_by_kind = BTreeMap::new();

    for body in bodies.iter().filter(|body| is_macro(body.expansion_origin)) {
        add_body_base(&mut totals);
        add_body_base(
            by_origin
                .entry(origin_name(body.expansion_origin).to_owned())
                .or_default(),
        );
        add_body_base(
            by_macro_kind
                .entry(macro_kind_name(
                    body.macro_kind.expect("validated macro body kind"),
                ))
                .or_default(),
        );
    }
    for decision in decisions {
        let weight = cyclomatic_weight(decision.kind);
        add_decision(&mut totals, weight);
        add_decision(
            by_origin
                .entry(origin_name(decision.expansion_origin).to_owned())
                .or_default(),
            weight,
        );
        add_decision(
            by_macro_kind
                .entry(macro_kind_name(decision.macro_kind))
                .or_default(),
            weight,
        );
        *decisions_by_kind
            .entry(decision_kind_name(decision.kind).to_owned())
            .or_insert(0) += 1;
    }

    Ok(MacroExpansionInvocationMetricsReport {
        totals,
        by_origin,
        by_macro_kind,
        decisions_by_kind,
    })
}

fn validate(bodies: &[Body], decisions: &[Decision]) -> Result<(), String> {
    let mut body_ids = BTreeSet::<CompilerDefId>::new();
    for body in bodies {
        if !body_ids.insert(body.compiler_id) {
            return Err(format!(
                "duplicate conceptual body identity {}:{}",
                body.compiler_id.stable_crate_id, body.compiler_id.local_hash
            ));
        }
        let has_macro_identity = body.macro_kind.is_some() || body.macro_definition.is_some();
        if is_macro(body.expansion_origin) != body.macro_kind.is_some()
            || (!is_macro(body.expansion_origin) && has_macro_identity)
        {
            return Err("body expansion origin and macro identity disagree".to_owned());
        }
    }
    for decision in decisions {
        if !body_ids.contains(&decision.body) {
            return Err("macro decision refers to an unknown conceptual body".to_owned());
        }
        if !is_macro(decision.expansion_origin) {
            return Err("expansion decision is not attributed to a macro".to_owned());
        }
    }
    Ok(())
}

fn add_body_base(breakdown: &mut MacroExpansionDeltaBreakdownReport) {
    breakdown.macro_body_bases += 1;
    breakdown.cyclomatic_delta += 1;
}

fn add_decision(breakdown: &mut MacroExpansionDeltaBreakdownReport, weight: u64) {
    breakdown.decisions += 1;
    breakdown.decision_delta += weight;
    breakdown.cyclomatic_delta += weight;
}

fn cyclomatic_weight(kind: DecisionKind) -> u64 {
    match kind {
        DecisionKind::Match => 0,
        DecisionKind::Conditional
        | DecisionKind::Loop
        | DecisionKind::MatchAlternative
        | DecisionKind::Guard
        | DecisionKind::ShortCircuit
        | DecisionKind::Try
        | DecisionKind::LetElse => 1,
    }
}

fn is_macro(origin: ExpansionOrigin) -> bool {
    matches!(
        origin,
        ExpansionOrigin::LocalMacro | ExpansionOrigin::ExternalMacro
    )
}

fn origin_name(origin: ExpansionOrigin) -> &'static str {
    match origin {
        ExpansionOrigin::Authored => "authored",
        ExpansionOrigin::BuiltinDesugaring => "builtin_desugaring",
        ExpansionOrigin::LocalMacro => "local_macro",
        ExpansionOrigin::ExternalMacro => "external_macro",
    }
}

fn macro_kind_name(kind: MacroKind) -> String {
    match kind {
        MacroKind::Bang => "bang",
        MacroKind::Attribute => "attribute",
        MacroKind::Derive => "derive",
    }
    .to_owned()
}

fn decision_kind_name(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::Conditional => "conditional",
        DecisionKind::Loop => "loop",
        DecisionKind::Match => "match",
        DecisionKind::MatchAlternative => "match_alternative",
        DecisionKind::Guard => "guard",
        DecisionKind::ShortCircuit => "short_circuit",
        DecisionKind::Try => "try",
        DecisionKind::LetElse => "let_else",
    }
}

#[cfg(test)]
mod tests {
    use rot_compiler_protocol::{BodyKind, ExpansionOrigin, FactId};

    use super::*;

    #[test]
    fn macro_body_bases_and_weighted_decisions_remain_separate() {
        let bodies = vec![
            body(1, ExpansionOrigin::Authored, None),
            body(2, ExpansionOrigin::LocalMacro, Some(MacroKind::Bang)),
            body(3, ExpansionOrigin::ExternalMacro, Some(MacroKind::Derive)),
        ];
        let decisions = vec![
            decision(
                1,
                1,
                DecisionKind::Conditional,
                ExpansionOrigin::LocalMacro,
                MacroKind::Bang,
            ),
            decision(
                2,
                2,
                DecisionKind::Match,
                ExpansionOrigin::LocalMacro,
                MacroKind::Bang,
            ),
            decision(
                3,
                3,
                DecisionKind::ShortCircuit,
                ExpansionOrigin::ExternalMacro,
                MacroKind::Derive,
            ),
        ];

        let metrics = measure(&bodies, &decisions).unwrap();

        assert_eq!(metrics.totals.macro_body_bases, 2);
        assert_eq!(metrics.totals.decisions, 3);
        assert_eq!(metrics.totals.decision_delta, 2);
        assert_eq!(metrics.totals.cyclomatic_delta, 4);
        assert_eq!(metrics.by_origin["local_macro"].cyclomatic_delta, 2);
        assert_eq!(metrics.by_origin["external_macro"].cyclomatic_delta, 2);
        assert_eq!(metrics.decisions_by_kind["match"], 1);
    }

    #[test]
    fn incomplete_or_untrusted_facts_never_serialize_a_semantic_zero() {
        let partial = aggregate(
            true,
            [MacroInvocation {
                key: "partial",
                target: None,
                crate_name: "crate",
                status: SemanticStatus::Partial,
                reason: Some("truncated"),
                bodies: &[],
                decisions: &[],
            }],
        )
        .unwrap();
        assert!(partial.invocations[0].metrics.is_none());

        let untrusted = aggregate(
            false,
            [MacroInvocation {
                key: "complete",
                target: None,
                crate_name: "crate",
                status: SemanticStatus::Complete,
                reason: None,
                bodies: &[],
                decisions: &[],
            }],
        )
        .unwrap();
        assert_eq!(untrusted.invocations[0].status, SemanticStatus::Partial);
        assert!(untrusted.invocations[0].metrics.is_none());
    }

    #[test]
    fn invalid_macro_provenance_downgrades_the_invocation() {
        let bodies = [body(1, ExpansionOrigin::Authored, None)];
        let decisions = [decision(
            1,
            1,
            DecisionKind::Conditional,
            ExpansionOrigin::Authored,
            MacroKind::Bang,
        )];
        let report = aggregate(
            true,
            [MacroInvocation {
                key: "invalid",
                target: None,
                crate_name: "crate",
                status: SemanticStatus::Complete,
                reason: None,
                bodies: &bodies,
                decisions: &decisions,
            }],
        )
        .unwrap();

        assert_eq!(report.invocations[0].status, SemanticStatus::Partial);
        assert!(report.invocations[0].metrics.is_none());
    }

    fn compiler_id(local_hash: u64) -> CompilerDefId {
        CompilerDefId {
            stable_crate_id: 1,
            local_hash,
        }
    }

    fn body(local_hash: u64, origin: ExpansionOrigin, macro_kind: Option<MacroKind>) -> Body {
        Body {
            id: FactId(format!("body-{local_hash}")),
            compiler_id: compiler_id(local_hash),
            definition_path: format!("crate::body_{local_hash}"),
            kind: BodyKind::Function,
            span: None,
            attribution_callsite: None,
            expansion_origin: origin,
            macro_kind,
            macro_definition: macro_kind.map(|_| compiler_id(99)),
        }
    }

    fn decision(
        ordinal: u32,
        body: u64,
        kind: DecisionKind,
        origin: ExpansionOrigin,
        macro_kind: MacroKind,
    ) -> Decision {
        Decision {
            id: FactId(format!("decision-{ordinal}")),
            body: compiler_id(body),
            kind,
            generated_span: None,
            attribution_callsite: None,
            expansion_origin: origin,
            macro_kind,
            macro_definition: Some(compiler_id(99)),
            ordinal,
            nesting: 0,
        }
    }
}
