use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

use rot_compiler_protocol::{
    Availability, CompilationContext, DecisionKind, Definition, DefinitionKind,
    EffectiveVisibilityLevel, Event, ExpansionOrigin, Exposure, MacroKind, Namespace, Product,
    RUN_ID_ENV, Record, ReferenceKind, RootKind, SELECTED_MANIFEST_DIRS_ENV, SIDECAR_DIR_ENV,
};

static NEXT_TEMPORARY_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[test]
fn macro_decisions_are_normalized_and_nested_bodies_are_independent() {
    let records = collect_fixture("macro_expansion");
    let bodies = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Body(body) => Some(body),
            _ => None,
        })
        .collect::<Vec<_>>();
    let body_paths = bodies
        .iter()
        .map(|body| (body.compiler_id, body.definition_path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let macro_body_paths = bodies
        .iter()
        .filter(|body| is_macro_origin(body.expansion_origin))
        .map(|body| body.definition_path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        macro_body_paths,
        [
            "generated",
            "async_generated",
            "generated::{closure#0}",
            "<Derived as std::cmp::PartialEq>::eq",
        ]
    );
    assert!(bodies.iter().any(|body| {
        body.definition_path == "authored"
            && body.expansion_origin == ExpansionOrigin::Authored
            && body.macro_kind.is_none()
            && body.macro_definition.is_none()
    }));
    assert!(
        !bodies
            .iter()
            .any(|body| body.definition_path == "async_generated::{closure#0}")
    );

    let decisions = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Decision(decision) => Some(decision),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(decisions.len(), 19);
    assert!(decisions.iter().all(|decision| {
        is_macro_origin(decision.expansion_origin) && decision.attribution_callsite.is_some()
    }));

    let mut decisions_by_body = BTreeMap::<&str, Vec<_>>::new();
    for decision in &decisions {
        decisions_by_body
            .entry(body_paths[&decision.body])
            .or_default()
            .push(*decision);
    }
    let generated = &decisions_by_body["generated"];
    assert_eq!(generated.len(), 15);
    assert_eq!(
        kind_counts(generated),
        [
            (DecisionKind::Conditional, 4),
            (DecisionKind::Loop, 3),
            (DecisionKind::Match, 1),
            (DecisionKind::MatchAlternative, 2),
            (DecisionKind::Guard, 1),
            (DecisionKind::ShortCircuit, 2),
            (DecisionKind::Try, 1),
            (DecisionKind::LetElse, 1),
        ]
    );
    assert_eq!(
        generated
            .iter()
            .filter(|decision| decision.kind == DecisionKind::Conditional && decision.nesting == 1)
            .count(),
        2
    );
    assert_eq!(decisions_by_body["generated::{closure#0}"].len(), 1);
    assert_eq!(decisions_by_body["authored"].len(), 1);
    assert_eq!(
        decisions_by_body["async_generated"]
            .iter()
            .map(|decision| decision.kind)
            .collect::<Vec<_>>(),
        [DecisionKind::Conditional]
    );
    assert_eq!(
        decisions_by_body["<Derived as std::cmp::PartialEq>::eq"]
            .iter()
            .map(|decision| (decision.kind, decision.macro_kind))
            .collect::<Vec<_>>(),
        [(DecisionKind::ShortCircuit, MacroKind::Derive)]
    );
    assert_contiguous_group_ordinals(&decisions);
    assert_product(
        &records,
        Product::ExpansionDecisions,
        Availability::Complete,
    );
}

#[test]
fn effective_api_events_are_finite_complete_and_source_honest() {
    let records = collect_fixture("effective_api");
    let definitions = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => Some(definition),
            _ => None,
        })
        .collect::<Vec<_>>();
    let definitions_by_id = definitions
        .iter()
        .map(|definition| (definition.compiler_id, *definition))
        .collect::<BTreeMap<_, _>>();
    let bindings = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::PublicBinding(binding) => Some(binding),
            _ => None,
        })
        .collect::<Vec<_>>();
    let binding_facts = bindings
        .iter()
        .map(|binding| {
            let parent = definitions_by_id[&binding.parent].definition_path.as_str();
            let target = definitions_by_id
                .get(&binding.target)
                .map(|definition| (definition.definition_path.as_str(), definition.kind));
            (
                parent,
                binding.name.as_str(),
                binding.namespace,
                binding.exposure,
                target,
            )
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(bindings.len(), 24, "public bindings must remain finite");
    assert_eq!(
        binding_facts.len(),
        bindings.len(),
        "duplicate binding event"
    );
    assert_eq!(
        binding_facts,
        BTreeSet::from([
            (
                "",
                "Choice",
                Namespace::Type,
                Exposure::SingleReexport,
                Some(("hidden::Choice", DefinitionKind::Enum)),
            ),
            (
                "",
                "Contract",
                Namespace::Type,
                Exposure::SingleReexport,
                Some(("hidden::Contract", DefinitionKind::Trait)),
            ),
            (
                "",
                "Renamed",
                Namespace::Type,
                Exposure::SingleReexport,
                Some(("hidden::Named", DefinitionKind::Struct)),
            ),
            (
                "",
                "Globbed",
                Namespace::Type,
                Exposure::GlobReexport,
                Some(("hidden::globbed::Globbed", DefinitionKind::Struct)),
            ),
            (
                "",
                "Globbed",
                Namespace::Value,
                Exposure::GlobReexport,
                Some(("hidden::globbed::Globbed", DefinitionKind::Constructor)),
            ),
            (
                "",
                "globbed_function",
                Namespace::Value,
                Exposure::GlobReexport,
                Some((
                    "hidden::globbed::globbed_function",
                    DefinitionKind::Function,
                )),
            ),
            (
                "",
                "ExternalDebug",
                Namespace::Type,
                Exposure::SingleReexport,
                None,
            ),
            (
                "",
                "ExternalDebug",
                Namespace::Macro,
                Exposure::SingleReexport,
                None,
            ),
            (
                "",
                "cycle_a",
                Namespace::Type,
                Exposure::Direct,
                Some(("cycle_a", DefinitionKind::Module)),
            ),
            (
                "",
                "cycle_b",
                Namespace::Type,
                Exposure::Direct,
                Some(("cycle_b", DefinitionKind::Module)),
            ),
            (
                "",
                "Generated",
                Namespace::Type,
                Exposure::Direct,
                Some(("Generated", DefinitionKind::Struct)),
            ),
            (
                "",
                "Generated",
                Namespace::Value,
                Exposure::Direct,
                Some(("Generated", DefinitionKind::Constructor)),
            ),
            (
                "",
                "exported_macro",
                Namespace::Macro,
                Exposure::MacroExport,
                Some(("exported_macro", DefinitionKind::Macro)),
            ),
            (
                "",
                "CONSTANT",
                Namespace::Value,
                Exposure::Direct,
                Some(("CONSTANT", DefinitionKind::Constant)),
            ),
            (
                "",
                "STATIC",
                Namespace::Value,
                Exposure::Direct,
                Some(("STATIC", DefinitionKind::Static)),
            ),
            (
                "",
                "body_shapes",
                Namespace::Value,
                Exposure::Direct,
                Some(("body_shapes", DefinitionKind::Function)),
            ),
            (
                "cycle_a",
                "A",
                Namespace::Type,
                Exposure::Direct,
                Some(("cycle_a::A", DefinitionKind::Struct)),
            ),
            (
                "cycle_a",
                "A",
                Namespace::Value,
                Exposure::Direct,
                Some(("cycle_a::A", DefinitionKind::Constructor)),
            ),
            (
                "cycle_a",
                "B",
                Namespace::Type,
                Exposure::SingleReexport,
                Some(("cycle_b::B", DefinitionKind::Struct)),
            ),
            (
                "cycle_a",
                "B",
                Namespace::Value,
                Exposure::SingleReexport,
                Some(("cycle_b::B", DefinitionKind::Constructor)),
            ),
            (
                "cycle_b",
                "A",
                Namespace::Type,
                Exposure::SingleReexport,
                Some(("cycle_a::A", DefinitionKind::Struct)),
            ),
            (
                "cycle_b",
                "A",
                Namespace::Value,
                Exposure::SingleReexport,
                Some(("cycle_a::A", DefinitionKind::Constructor)),
            ),
            (
                "cycle_b",
                "B",
                Namespace::Type,
                Exposure::Direct,
                Some(("cycle_b::B", DefinitionKind::Struct)),
            ),
            (
                "cycle_b",
                "B",
                Namespace::Value,
                Exposure::Direct,
                Some(("cycle_b::B", DefinitionKind::Constructor)),
            ),
        ])
    );

    assert_eq!(
        definition_kinds(&definitions, "hidden::Choice::Unit"),
        BTreeSet::from([DefinitionKind::Variant, DefinitionKind::Constructor])
    );
    assert_eq!(
        definition_kinds(&definitions, "hidden::Choice::Tuple"),
        BTreeSet::from([DefinitionKind::Variant, DefinitionKind::Constructor])
    );
    assert_eq!(
        definition_kinds(&definitions, "hidden::Choice::Record"),
        BTreeSet::from([DefinitionKind::Variant])
    );
    for field in [
        "hidden::Named::visible",
        "hidden::Choice::Tuple::0",
        "hidden::Choice::Record::visible",
        "hidden::Choice::Record::hidden",
    ] {
        assert_eq!(
            find_definition(&definitions, field, DefinitionKind::Field).effective_public_at,
            Some(EffectiveVisibilityLevel::Reexported),
            "public ADT field {field} must be part of the effective API"
        );
    }
    assert_eq!(
        find_definition(
            &definitions,
            "hidden::Named::private",
            DefinitionKind::Field,
        )
        .effective_public_at,
        None
    );

    for (path, kind) in [
        ("hidden::Contract::Item", DefinitionKind::AssociatedType),
        (
            "hidden::Contract::VALUE",
            DefinitionKind::AssociatedConstant,
        ),
        ("hidden::Contract::call", DefinitionKind::AssociatedFunction),
        (
            "<hidden::Named as hidden::Contract>::Item",
            DefinitionKind::AssociatedType,
        ),
        (
            "<hidden::Named as hidden::Contract>::VALUE",
            DefinitionKind::AssociatedConstant,
        ),
        (
            "<hidden::Named as hidden::Contract>::call",
            DefinitionKind::AssociatedFunction,
        ),
    ] {
        let definition = find_definition(&definitions, path, kind);
        assert_eq!(
            definition.effective_public_at,
            Some(EffectiveVisibilityLevel::Reexported),
            "trait-associated API item {path} must inherit reexport exposure"
        );
        assert!(
            !definition.visibility_editable,
            "trait-associated API item {path} has no independently editable visibility"
        );
    }
    let exposed = find_definition(
        &definitions,
        "hidden::Named::exposed",
        DefinitionKind::AssociatedFunction,
    );
    assert_eq!(
        exposed.effective_public_at,
        Some(EffectiveVisibilityLevel::Reexported)
    );
    assert!(exposed.visibility_editable);
    let private = find_definition(
        &definitions,
        "hidden::Named::private",
        DefinitionKind::AssociatedFunction,
    );
    assert_eq!(private.effective_public_at, None);
    assert!(private.visibility_editable);
    let unreachable_public_method = find_definition(
        &definitions,
        "hidden::PrivateReceiver::nominally_public",
        DefinitionKind::AssociatedFunction,
    );
    assert_eq!(unreachable_public_method.effective_public_at, None);
    assert!(unreachable_public_method.visibility_editable);

    let generated_definitions = definitions
        .iter()
        .copied()
        .filter(|definition| definition.expansion_origin == ExpansionOrigin::LocalMacro)
        .collect::<Vec<_>>();
    assert_eq!(
        generated_definitions
            .iter()
            .map(|definition| (definition.definition_path.as_str(), definition.kind))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ("Generated", DefinitionKind::Struct),
            ("Generated", DefinitionKind::Constructor),
            ("Generated", DefinitionKind::Implementation),
            ("Generated::generated", DefinitionKind::AssociatedFunction,),
        ])
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/effective_api.rs");
    let source = fs::read(fixture).unwrap();
    for definition in generated_definitions {
        assert!(
            definition.span.is_none(),
            "expanded definition {} must not claim a trustworthy authored span",
            definition.definition_path
        );
        let callsite = definition
            .attribution_callsite
            .as_ref()
            .expect("expanded definition attribution callsite");
        assert_eq!(source_bytes(&source, callsite), b"emit_api!()");
    }
    let authored = find_definition(&definitions, "hidden::Named", DefinitionKind::Struct);
    assert_eq!(authored.expansion_origin, ExpansionOrigin::Authored);
    assert!(authored.attribution_callsite.is_none());
    assert_eq!(
        source_bytes(
            &source,
            authored.span.as_ref().expect("authored definition span")
        ),
        b"pub struct Named"
    );
    assert_product(&records, Product::EffectiveApi, Availability::Complete);
}

#[test]
fn references_cover_typed_bodies_interfaces_visibility_and_roots() {
    let records = collect_fixture("references");
    let definitions = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => Some(definition),
            _ => None,
        })
        .collect::<Vec<_>>();
    let paths = definitions
        .iter()
        .map(|definition| (definition.compiler_id, definition.definition_path.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert!(
        definitions
            .iter()
            .find(|definition| definition.definition_path == "public_api::Receiver::method")
            .expect("inherent method definition")
            .visibility_editable
    );
    assert!(
        !definitions
            .iter()
            .find(|definition| definition.definition_path == "public_api::Contract::produce")
            .expect("trait method definition")
            .visibility_editable
    );
    assert!(definitions.iter().any(|definition| {
        definition.definition_path.contains("Implementation")
            && definition.definition_path.ends_with("::produce")
            && !definition.visibility_editable
    }));
    assert!(
        definitions
            .iter()
            .find(|definition| definition.definition_path == "public_api::Payload::value")
            .expect("struct field definition")
            .visibility_editable
    );
    assert!(
        !definitions
            .iter()
            .find(|definition| definition.definition_path == "public_api::Choice::Record::value")
            .expect("enum variant field definition")
            .visibility_editable
    );
    let definition_ids = definitions
        .iter()
        .map(|definition| definition.compiler_id)
        .collect::<BTreeSet<_>>();
    let body_ids = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Body(body) => Some(body.compiler_id),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let local_crates = definition_ids
        .iter()
        .map(|definition| definition.stable_crate_id)
        .collect::<BTreeSet<_>>();
    let references = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Reference(reference) => Some(reference),
            _ => None,
        })
        .collect::<Vec<_>>();

    for reference in &references {
        assert!(definition_ids.contains(&reference.from) || body_ids.contains(&reference.from));
        if local_crates.contains(&reference.to.stable_crate_id) {
            assert!(definition_ids.contains(&reference.to));
        }
    }

    assert_eq!(
        references
            .iter()
            .map(|reference| reference.kind)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ReferenceKind::Body,
            ReferenceKind::Interface,
            ReferenceKind::Reexport,
            ReferenceKind::VisibilityParent,
            ReferenceKind::VisibilityRequirement,
        ])
    );
    assert_reference(
        &references,
        &paths,
        "caller",
        "free_function",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "caller",
        "public_api::Receiver::method",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "caller",
        "public_api::Payload::value",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "caller",
        "public_api::Payload",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "caller",
        "public_api::Choice::Record",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "caller",
        "public_api::Choice::Record::value",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "caller",
        "public_api::Contract::produce",
        ReferenceKind::Body,
    );
    for field in [
        "field_precision::Named::used",
        "field_precision::Named::wildcard",
        "field_precision::Named::rest",
        "field_precision::Tuple::0",
        "field_precision::Tuple::1",
        "field_precision::Spread::0",
        "field_precision::Spread::1",
        "field_precision::Spread::2",
    ] {
        assert_reference(
            &references,
            &paths,
            "field_precision::construct",
            field,
            ReferenceKind::Body,
        );
    }
    for field in [
        "field_precision::Named::used",
        "field_precision::Named::wildcard",
        "field_precision::Tuple::0",
        "field_precision::Tuple::1",
        "field_precision::Spread::0",
        "field_precision::Spread::1",
        "field_precision::Spread::2",
    ] {
        assert_reference(
            &references,
            &paths,
            "field_precision::destructure",
            field,
            ReferenceKind::Body,
        );
    }
    for field in ["field_precision::Named::rest"] {
        assert!(!references.iter().any(|reference| {
            reference.kind == ReferenceKind::Body
                && paths.get(&reference.from) == Some(&"field_precision::destructure")
                && paths.get(&reference.to) == Some(&field)
        }));
    }
    assert_reference(
        &references,
        &paths,
        "public_api::interface",
        "public_api::Payload",
        ReferenceKind::Interface,
    );
    assert_reference(
        &references,
        &paths,
        "public_api::interface",
        "interface_body_only",
        ReferenceKind::Body,
    );
    assert!(!references.iter().any(|reference| {
        reference.kind == ReferenceKind::Interface
            && paths.get(&reference.from) == Some(&"public_api::interface")
            && paths.get(&reference.to) == Some(&"interface_body_only")
    }));
    assert_reference(
        &references,
        &paths,
        "public_api::Receiver",
        "public_api",
        ReferenceKind::VisibilityParent,
    );
    assert!(references.iter().any(|reference| {
        reference.kind == ReferenceKind::Reexport
            && paths[&reference.to] == "hidden::reexported"
            && definitions.iter().any(|definition| {
                definition.compiler_id == reference.from
                    && definition.kind == DefinitionKind::Import
            })
    }));
    assert!(references.iter().any(|reference| {
        reference.kind == ReferenceKind::VisibilityRequirement
            && paths[&reference.from].ends_with("::Item")
            && paths[&reference.to] == "public_api::Required"
    }));
    assert_reference(
        &references,
        &paths,
        "public_api::Contract",
        "public_api::Payload",
        ReferenceKind::VisibilityRequirement,
    );
    assert_reference(
        &references,
        &paths,
        "public_api::Contract",
        "public_api::Required",
        ReferenceKind::VisibilityRequirement,
    );

    let roots = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Root(root) => Some(root),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(roots.iter().any(|root| {
        root.kind == RootKind::Conservative && paths[&root.definition].ends_with("::produce")
    }));
    assert!(roots.iter().any(|root| {
        root.kind == RootKind::Conservative && paths[&root.definition] == "exported_symbol"
    }));
    assert!(roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic && paths[&root.definition] == "hidden::reexported"
    }));
    assert!(roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic
            && paths[&root.definition] == "hidden::nested_reexported"
    }));
    assert!(!roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic && paths[&root.definition] == "glob_hidden::globbed"
    }));
    assert!(!roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic && paths[&root.definition] == "glob_hidden"
    }));
    assert!(roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic && paths[&root.definition] == "facade"
    }));
    assert!(roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic && paths[&root.definition] == "public_api::Required"
    }));
    assert!(!roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic
            && paths[&root.definition] == "public_api::unrelated_public"
    }));
    assert!(!roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic
            && paths[&root.definition] == "private_hidden::not_exported"
    }));

    let nested_definitions = definitions
        .iter()
        .filter(|definition| {
            definition.definition_path.ends_with("closure_nested")
                || definition.definition_path.ends_with("const_nested")
        })
        .collect::<Vec<_>>();
    assert_eq!(nested_definitions.len(), 2);
    for definition in nested_definitions {
        assert_eq!(paths[&definition.parent.unwrap()], "nested_items");
    }
    assert_reference(
        &references,
        &paths,
        "nested_items",
        "nested_items::{closure#0}::closure_nested",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "nested_items",
        "nested_items::{constant#0}::const_nested",
        ReferenceKind::Body,
    );
    assert_reference(
        &references,
        &paths,
        "nested_items",
        "async_block_only",
        ReferenceKind::Body,
    );
    assert_product(&records, Product::References, Availability::Complete);
}

#[test]
fn pathless_self_constructor_records_tuple_field_use() {
    let records = collect_fixture("references");
    let paths = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => {
                Some((definition.compiler_id, definition.definition_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let references = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Reference(reference) => Some(reference),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_reference(
        &references,
        &paths,
        "<field_precision::SelfConstructed as field_precision::Rebuild>::rebuild",
        "field_precision::SelfConstructed::0",
        ReferenceKind::Body,
    );
}

#[test]
fn type_system_anon_const_records_signature_reference() {
    let records = collect_fixture("references");
    let paths = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => {
                Some((definition.compiler_id, definition.definition_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let references = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Reference(reference) => Some(reference),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_reference(
        &references,
        &paths,
        "type_system::array_signature",
        "type_system::SIGNATURE_WIDTH",
        ReferenceKind::Body,
    );
}

#[test]
fn inherent_associated_type_is_conservatively_rooted_with_rhs_reference() {
    let records = collect_fixture("references");
    let paths = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => {
                Some((definition.compiler_id, definition.definition_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let references = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Reference(reference) => Some(reference),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_reference(
        &references,
        &paths,
        "inherent_types::S::Assoc",
        "inherent_types::Value",
        ReferenceKind::Interface,
    );
    assert!(records.iter().any(|record| {
        matches!(
            &record.event,
            Event::Root(root)
                if root.kind == RootKind::RequiredPublic
                    && paths.get(&root.definition) == Some(&"inherent_types::S::Assoc")
        )
    }));
}

#[test]
fn foreign_item_records_enclosing_module_visibility() {
    let records = collect_fixture("references");
    let paths = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => {
                Some((definition.compiler_id, definition.definition_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let references = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Reference(reference) => Some(reference),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_reference(
        &references,
        &paths,
        "foreign_api::imported",
        "foreign_api",
        ReferenceKind::VisibilityParent,
    );
}

#[test]
fn global_asm_symbol_target_is_a_conservative_root() {
    let records = collect_fixture("references");
    let paths = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => {
                Some((definition.compiler_id, definition.definition_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();

    assert!(records.iter().any(|record| {
        matches!(
            &record.event,
            Event::Root(root)
                if root.kind == RootKind::Conservative
                    && paths.get(&root.definition) == Some(&"global_asm_target")
        )
    }));
}

#[test]
fn namespaced_decl_macro_roots_its_namespace_without_rooting_macro_export_home() {
    let records = collect_fixture("references");
    let paths = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => {
                Some((definition.compiler_id, definition.definition_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let roots = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Root(root) => Some(root),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic
            && paths.get(&root.definition) == Some(&"namespaced_macros")
    }));
    assert!(!roots.iter().any(|root| {
        root.kind == RootKind::RequiredPublic
            && paths.get(&root.definition) == Some(&"legacy_macro_home")
    }));
}

#[test]
fn nested_public_extern_crate_roots_its_namespace() {
    let records = collect_fixture("references");
    let paths = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => {
                Some((definition.compiler_id, definition.definition_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();

    assert!(records.iter().any(|record| {
        matches!(
            &record.event,
            Event::Root(root)
                if root.kind == RootKind::RequiredPublic
                    && paths.get(&root.definition) == Some(&"extern_facade")
        )
    }));
}

#[test]
fn closure_only_calls_are_reachable_from_the_entry_point() {
    let records = collect_source_with_crate_type(
        "closure_reachability",
        b"fn helper() {}\nfn main() {\n    let call = || helper();\n    call();\n}\n",
        "bin",
    );
    let definitions = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => Some(definition),
            _ => None,
        })
        .collect::<Vec<_>>();
    let paths = definitions
        .iter()
        .map(|definition| (definition.compiler_id, definition.definition_path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let references = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Reference(reference) => Some(reference),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_reference(&references, &paths, "main", "helper", ReferenceKind::Body);
    assert!(records.iter().any(|record| matches!(
        &record.event,
        Event::Root(root)
            if root.kind == RootKind::EntryPoint && paths[&root.definition] == "main"
    )));
}

#[test]
fn test_harness_entry_reaches_selected_tests_without_rooting_unrelated_code() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir.join("tests/fixtures/test_harness.rs");
    let (output, records) =
        run_fixture_path_with_args("test_harness_reachability", &fixture, "lib", &["--test"]);
    assert!(
        output.status.success(),
        "driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let definitions = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => Some(definition),
            _ => None,
        })
        .collect::<Vec<_>>();
    let paths = definitions
        .iter()
        .map(|definition| (definition.compiler_id, definition.definition_path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let roots = records.iter().filter_map(|record| match &record.event {
        Event::Root(root) => Some(root),
        _ => None,
    });
    let references = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Reference(reference)
                if reference.kind != ReferenceKind::VisibilityRequirement =>
            {
                Some(reference)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut reachable = roots.map(|root| root.definition).collect::<BTreeSet<_>>();
    loop {
        let before = reachable.len();
        for reference in &references {
            if reachable.contains(&reference.from) {
                reachable.insert(reference.to);
            }
        }
        if reachable.len() == before {
            break;
        }
    }

    let reachable_paths = reachable
        .iter()
        .filter_map(|definition| paths.get(definition).copied())
        .collect::<BTreeSet<_>>();
    assert!(reachable_paths.contains("main"));
    assert!(reachable_paths.contains("selected_test"));
    assert!(reachable_paths.contains("helper"));
    assert!(!reachable_paths.contains("unrelated_dead"));
    assert_product(&records, Product::References, Availability::Complete);
}

#[test]
fn source_spans_address_original_crlf_bytes() {
    let source = b"pub fn first() {}\r\n\r\npub fn second() {\r\n    first();\r\n}\r\n";
    let records = collect_source("crlf", source);
    let body = records
        .iter()
        .find_map(|record| match &record.event {
            Event::Body(body) if body.definition_path == "second" => Some(body),
            _ => None,
        })
        .unwrap();
    let span = body.span.as_ref().unwrap();
    let source_file = records
        .iter()
        .find_map(|record| match &record.event {
            Event::SourceFile(source_file) if source_file.key == span.file => Some(source_file),
            _ => None,
        })
        .unwrap();

    assert_eq!(source_file.byte_len, u32::try_from(source.len()).unwrap());
    let body_bytes =
        &source[usize::try_from(span.start).unwrap()..usize::try_from(span.end).unwrap()];
    assert!(String::from_utf8_lossy(body_bytes).contains("second"));

    let definitions = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::Definition(definition) => Some(definition),
            _ => None,
        })
        .collect::<Vec<_>>();
    let first = definitions
        .iter()
        .find(|definition| definition.definition_path == "first")
        .unwrap();
    let reference = records
        .iter()
        .find_map(|record| match &record.event {
            Event::Reference(reference)
                if reference.from == body.compiler_id
                    && reference.to == first.compiler_id
                    && reference.kind == ReferenceKind::Body =>
            {
                Some(reference)
            }
            _ => None,
        })
        .unwrap();
    let reference_span = reference.span.as_ref().unwrap();
    let reference_bytes = &source[usize::try_from(reference_span.start).unwrap()
        ..usize::try_from(reference_span.end).unwrap()];
    assert_eq!(reference_bytes, b"first");
}

#[test]
fn entry_roots_are_complete_reference_facts_after_edge_collection() {
    let records = collect_source_with_crate_type("entry_root", b"fn main() {}\n", "bin");

    assert!(records.iter().any(|record| matches!(
        &record.event,
        Event::Root(root) if root.kind == RootKind::EntryPoint
    )));
    assert_product(&records, Product::References, Availability::Complete);
}

#[test]
fn compiler_failure_never_reports_complete_references() {
    let records = collect_failing_source("broken_references", b"pub fn broken( {\n");

    assert_product(&records, Product::References, Availability::Unavailable);
    assert!(
        !records
            .iter()
            .any(|record| matches!(&record.event, Event::Reference(_)))
    );
}

#[test]
fn bare_test_cfg_agrees_across_invocation_and_profile() {
    let records = collect_source_with_rustc_args(
        "cfg_test_mode",
        b"pub fn compiled_as_test() {}\n",
        &["--cfg", "test"],
    );
    let started = records
        .iter()
        .find_map(|record| match &record.event {
            Event::InvocationStarted(started) => Some(started),
            _ => None,
        })
        .unwrap();
    let profile = records
        .iter()
        .find_map(|record| match &record.event {
            Event::Profile(profile) => Some(profile),
            _ => None,
        })
        .unwrap();

    assert!(started.test_mode);
    assert!(profile.test_mode);
    assert_eq!(started.target_triple, profile.target_triple);
    assert_eq!(started.compilation_context, CompilationContext::Host);
}

#[test]
fn explicit_host_triple_is_a_target_context_on_the_wire() {
    let rustc = command_output(Command::new("rustup").args([
        "which",
        "--toolchain",
        "nightly-2026-08-27",
        "rustc",
    ]));
    let rustc = String::from_utf8(rustc.stdout).unwrap();
    let verbose = command_output(Command::new(rustc.trim()).arg("-vV"));
    let verbose = String::from_utf8(verbose.stdout).unwrap();
    let host = verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap();
    let target = format!("--target={host}");
    let records = collect_source_with_rustc_args(
        "explicit_host_target",
        b"pub fn target_context() {}\n",
        &[&target],
    );
    let started = records
        .iter()
        .find_map(|record| match &record.event {
            Event::InvocationStarted(started) => Some(started),
            _ => None,
        })
        .unwrap();
    let profile = records
        .iter()
        .find_map(|record| match &record.event {
            Event::Profile(profile) => Some(profile),
            _ => None,
        })
        .unwrap();

    assert_eq!(started.compilation_context, CompilationContext::Target);
    assert_eq!(started.target_triple, host);
    assert_eq!(profile.target_triple, host);
}

fn find_definition<'a>(
    definitions: &[&'a Definition],
    path: &str,
    kind: DefinitionKind,
) -> &'a Definition {
    definitions
        .iter()
        .copied()
        .find(|definition| definition.definition_path == path && definition.kind == kind)
        .unwrap_or_else(|| panic!("missing {kind:?} definition {path}"))
}

fn definition_kinds(definitions: &[&Definition], path: &str) -> BTreeSet<DefinitionKind> {
    definitions
        .iter()
        .filter(|definition| definition.definition_path == path)
        .map(|definition| definition.kind)
        .collect()
}

fn source_bytes<'a>(source: &'a [u8], span: &rot_compiler_protocol::SourceSpan) -> &'a [u8] {
    &source[usize::try_from(span.start).unwrap()..usize::try_from(span.end).unwrap()]
}

fn kind_counts(decisions: &[&rot_compiler_protocol::Decision]) -> Vec<(DecisionKind, usize)> {
    let mut counts = BTreeMap::new();
    for decision in decisions {
        *counts.entry(decision.kind).or_default() += 1;
    }
    counts.into_iter().collect()
}

fn assert_reference(
    references: &[&rot_compiler_protocol::Reference],
    paths: &BTreeMap<rot_compiler_protocol::CompilerDefId, &str>,
    from: &str,
    to: &str,
    kind: ReferenceKind,
) {
    let found = references.iter().any(|reference| {
        reference.kind == kind
            && paths.get(&reference.from) == Some(&from)
            && paths.get(&reference.to) == Some(&to)
    });
    let local_references = references
        .iter()
        .filter_map(|reference| {
            Some((
                paths.get(&reference.from)?,
                paths.get(&reference.to)?,
                reference.kind,
            ))
        })
        .collect::<Vec<_>>();
    assert!(
        found,
        "missing {kind:?} reference {from} -> {to}; local references: {local_references:#?}"
    );
}

fn assert_contiguous_group_ordinals(decisions: &[&rot_compiler_protocol::Decision]) {
    let mut groups = BTreeMap::new();
    for decision in decisions {
        groups
            .entry((
                decision.body,
                decision.attribution_callsite.clone(),
                decision.expansion_origin,
                decision.macro_kind,
                decision.macro_definition,
            ))
            .or_insert_with(Vec::new)
            .push(decision.ordinal);
    }
    for ordinals in groups.values_mut() {
        ordinals.sort_unstable();
        assert_eq!(
            ordinals,
            &(0..u32::try_from(ordinals.len()).unwrap()).collect::<Vec<_>>()
        );
    }
}

fn assert_product(records: &[Record], product: Product, availability: Availability) {
    assert!(records.iter().any(|record| matches!(
        &record.event,
        Event::ProductStatus(status)
            if status.product == product && status.availability == availability
    )));
}

fn is_macro_origin(origin: ExpansionOrigin) -> bool {
    matches!(
        origin,
        ExpansionOrigin::LocalMacro | ExpansionOrigin::ExternalMacro
    )
}

fn collect_fixture(name: &str) -> Vec<Record> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest_dir
        .join("tests/fixtures")
        .join(format!("{name}.rs"));
    collect_fixture_path(name, &fixture)
}

fn collect_source(name: &str, source: &[u8]) -> Vec<Record> {
    collect_source_with_crate_type(name, source, "lib")
}

fn collect_source_with_crate_type(name: &str, source: &[u8], crate_type: &str) -> Vec<Record> {
    let source_directory = temporary_directory(&format!("{name}-source"));
    let fixture = source_directory.join(format!("{name}.rs"));
    fs::write(&fixture, source).unwrap();
    let records = collect_fixture_path_with_crate_type(name, &fixture, crate_type);
    fs::remove_dir_all(source_directory).unwrap();
    records
}

fn collect_source_with_rustc_args(name: &str, source: &[u8], rustc_args: &[&str]) -> Vec<Record> {
    let source_directory = temporary_directory(&format!("{name}-source"));
    let fixture = source_directory.join(format!("{name}.rs"));
    fs::write(&fixture, source).unwrap();
    let (output, records) = run_fixture_path_with_args(name, &fixture, "lib", rustc_args);
    assert!(
        output.status.success(),
        "driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(source_directory).unwrap();
    records
}

fn collect_failing_source(name: &str, source: &[u8]) -> Vec<Record> {
    let source_directory = temporary_directory(&format!("{name}-source"));
    let fixture = source_directory.join(format!("{name}.rs"));
    fs::write(&fixture, source).unwrap();
    let (output, records) = run_fixture_path(name, &fixture, "lib");
    assert!(
        !output.status.success(),
        "invalid fixture unexpectedly compiled"
    );
    fs::remove_dir_all(source_directory).unwrap();
    records
}

fn collect_fixture_path(name: &str, fixture: &Path) -> Vec<Record> {
    collect_fixture_path_with_crate_type(name, fixture, "lib")
}

fn collect_fixture_path_with_crate_type(
    name: &str,
    fixture: &Path,
    crate_type: &str,
) -> Vec<Record> {
    let (output, records) = run_fixture_path(name, fixture, crate_type);
    assert!(
        output.status.success(),
        "driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    records
}

fn run_fixture_path(name: &str, fixture: &Path, crate_type: &str) -> (Output, Vec<Record>) {
    run_fixture_path_with_args(name, fixture, crate_type, &[])
}

fn run_fixture_path_with_args(
    name: &str,
    fixture: &Path,
    crate_type: &str,
    rustc_args: &[&str],
) -> (Output, Vec<Record>) {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = temporary_directory(name);
    let sidecar_dir = directory.join("sidecars");
    let out_dir = directory.join("out");
    let build_script_out_dir = directory.join("build-script-out");
    let target_dir = directory.join("target");
    let build_dir = directory.join("build");
    for path in [
        &sidecar_dir,
        &out_dir,
        &build_script_out_dir,
        &target_dir,
        &build_dir,
    ] {
        fs::create_dir(path).unwrap();
    }

    let rustc = command_output(Command::new("rustup").args([
        "which",
        "--toolchain",
        "nightly-2026-08-27",
        "rustc",
    ]));
    let rustc = String::from_utf8(rustc.stdout).unwrap();
    let rustc = rustc.trim();
    let sysroot = command_output(Command::new(rustc).args(["--print", "sysroot"]));
    let sysroot = String::from_utf8(sysroot.stdout).unwrap();
    let dynamic_libraries = Path::new(sysroot.trim()).join("lib");

    let output = Command::new(env!("CARGO_BIN_EXE_rot-rustc-driver"))
        .arg(rustc)
        .args(["--crate-name", name])
        .arg(&fixture)
        .args(["--crate-type", crate_type, "--edition", "2024", "--out-dir"])
        .arg(&out_dir)
        .args(["--emit", "metadata"])
        .args(rustc_args)
        .env(RUN_ID_ENV, name)
        .env(SIDECAR_DIR_ENV, &sidecar_dir)
        .env(
            SELECTED_MANIFEST_DIRS_ENV,
            env::join_paths([manifest_dir]).unwrap(),
        )
        .env("ROT_COMPILER_TARGET_DIR", &target_dir)
        .env("ROT_COMPILER_BUILD_DIR", &build_dir)
        .env("CARGO_MANIFEST_DIR", manifest_dir)
        .env("CARGO_PKG_NAME", name)
        .env("CARGO_PRIMARY_PACKAGE", "1")
        .env("OUT_DIR", &build_script_out_dir)
        .env("DYLD_LIBRARY_PATH", &dynamic_libraries)
        .env("LD_LIBRARY_PATH", &dynamic_libraries)
        .output()
        .unwrap();
    let sidecars = fs::read_dir(&sidecar_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert_eq!(sidecars.len(), 1);
    let records = fs::read_to_string(&sidecars[0])
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Record>(line).unwrap())
        .collect::<Vec<_>>();
    let started = records.iter().find_map(|record| match &record.event {
        Event::InvocationStarted(started) => Some(started),
        _ => None,
    });
    let profile = records.iter().find_map(|record| match &record.event {
        Event::Profile(profile) => Some(profile),
        _ => None,
    });
    let canonical_build_script_out_dir = fs::canonicalize(&build_script_out_dir)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    if let (Some(started), Some(profile)) = (started, profile) {
        assert_eq!(started.target_triple, profile.target_triple);
        assert_eq!(
            started.build_script_out_dir.as_deref(),
            Some(canonical_build_script_out_dir.as_str())
        );
    }
    fs::remove_dir_all(directory).unwrap();
    (output, records)
}

fn command_output(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn temporary_directory(label: &str) -> PathBuf {
    let sequence = NEXT_TEMPORARY_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = env::temp_dir().join(format!(
        "rot-rustc-driver-{label}-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir(&directory).unwrap();
    directory
}
