use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::Path,
};

use anyhow::{Context, Result, bail};
use rot_compiler_protocol::{
    Definition, DefinitionKind, Diagnostic, Event, Handshake, InvocationFinished, InvocationId,
    InvocationStarted, MAX_SIDECAR_BYTES, PROTOCOL_VERSION, Product, ProductStatus, Profile,
    PublicBinding, Record, Reference, Root, RunId, SourceFile,
};

const MAX_SIDECARS: usize = 10_000;
const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct Invocation {
    pub id: InvocationId,
    pub started: InvocationStarted,
    pub profile: Option<Profile>,
    pub sources: Vec<SourceFile>,
    pub products: Vec<ProductStatus>,
    pub diagnostics: Vec<Diagnostic>,
    pub definitions: Vec<Definition>,
    pub bindings: Vec<PublicBinding>,
    pub roots: Vec<Root>,
    pub references: Vec<Reference>,
    pub finished: InvocationFinished,
}

pub struct Sidecars {
    pub invocations: Vec<Invocation>,
    pub errors: Vec<String>,
}

pub fn read_all(directory: &Path, run_id: &str, handshake: &Handshake) -> Result<Sidecars> {
    let mut paths = fs::read_dir(directory)
        .with_context(|| {
            format!(
                "cannot read compiler sidecar directory {}",
                directory.display()
            )
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    paths.sort();
    if paths.len() > MAX_SIDECARS {
        bail!(
            "compiler emitted {} sidecars, exceeding the limit {MAX_SIDECARS}",
            paths.len()
        );
    }

    let mut invocations = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        match read_one(&path, run_id, handshake) {
            Ok(invocation) => invocations.push(invocation),
            Err(error) => errors.push(format!("{}: {error:#}", path.display())),
        }
    }

    invocations.sort_by(|left, right| {
        (&left.started.merge_key, &left.id).cmp(&(&right.started.merge_key, &right.id))
    });
    errors.sort();
    errors.dedup();
    Ok(Sidecars {
        invocations,
        errors,
    })
}

fn read_one(path: &Path, run_id: &str, handshake: &Handshake) -> Result<Invocation> {
    let file = File::open(path)
        .with_context(|| format!("cannot open compiler sidecar {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot inspect compiler sidecar {}", path.display()))?;
    if metadata.len() > MAX_SIDECAR_BYTES {
        bail!(
            "sidecar is {} bytes, exceeding the limit {MAX_SIDECAR_BYTES}",
            metadata.len()
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read compiler sidecar {}", path.display()))?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        bail!("sidecar grew beyond the limit {MAX_SIDECAR_BYTES}");
    }
    if bytes.is_empty() || !bytes.ends_with(b"\n") {
        bail!("sidecar is empty or truncated");
    }

    let mut records = Vec::new();
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_RECORD_BYTES {
            bail!("record {} exceeds {MAX_RECORD_BYTES} bytes", index + 1);
        }
        let record = serde_json::from_slice::<Record>(line)
            .with_context(|| format!("record {} is malformed", index + 1))?;
        records.push(record);
    }
    validate_records(records, run_id, handshake)
}

fn validate_records(
    records: Vec<Record>,
    expected_run_id: &str,
    handshake: &Handshake,
) -> Result<Invocation> {
    let first = records.first().context("sidecar has no records")?;
    let invocation_id = first.invocation_id.clone();
    for (index, record) in records.iter().enumerate() {
        if record.protocol_version != PROTOCOL_VERSION {
            bail!(
                "protocol mismatch: expected {PROTOCOL_VERSION}, found {}",
                record.protocol_version
            );
        }
        if record.run_id != RunId(expected_run_id.to_owned()) {
            bail!("sidecar belongs to a different compiler run");
        }
        if record.invocation_id != invocation_id {
            bail!("sidecar contains multiple invocation IDs");
        }
        if record.sequence != index as u64 {
            bail!(
                "record sequence is not contiguous: expected {index}, found {}",
                record.sequence
            );
        }
    }

    let mut events = records.into_iter().map(|record| record.event);
    let Some(Event::InvocationStarted(started)) = events.next() else {
        bail!("first record is not invocation_started");
    };
    if started.compiler != handshake.rustc {
        bail!("sidecar compiler identity does not match the handshake");
    }
    validate_started(&started)?;

    let mut profile = None;
    let mut sources = Vec::new();
    let mut products = Vec::new();
    let mut diagnostics = Vec::new();
    let mut definitions = Vec::new();
    let mut bindings = Vec::new();
    let mut roots = Vec::new();
    let mut references = Vec::new();
    let mut finished = None;
    for event in events {
        if finished.is_some() {
            bail!("records follow invocation_finished");
        }
        match event {
            Event::InvocationStarted(_) => bail!("duplicate invocation_started record"),
            Event::Profile(value) => {
                if profile.replace(value).is_some() {
                    bail!("duplicate profile record");
                }
            }
            Event::SourceFile(value) => sources.push(value),
            Event::ProductStatus(value) => products.push(value),
            Event::Diagnostic(value) => diagnostics.push(value),
            Event::InvocationFinished(value) => finished = Some(value),
            Event::Definition(value) => definitions.push(value),
            Event::PublicBinding(value) => bindings.push(value),
            Event::Root(value) => roots.push(value),
            Event::Reference(value) => references.push(value),
        }
    }
    let finished = finished.context("sidecar has no invocation_finished record")?;
    if profile
        .as_ref()
        .is_some_and(|profile| profile.test_mode != started.test_mode)
    {
        bail!("invocation/profile rustc test mode mismatch");
    }
    if profile
        .as_ref()
        .is_some_and(|profile| profile.target_triple != started.target_triple)
    {
        bail!("invocation/profile rustc target mismatch");
    }
    if let Some(profile) = &profile {
        validate_profile(profile, &started)?;
    }
    if finished.analysis_reached != profile.is_some() {
        bail!("compiler analysis/profile completion mismatch");
    }
    if !finished.analysis_reached
        && products
            .iter()
            .any(|status| status.availability == rot_compiler_protocol::Availability::Complete)
    {
        bail!("complete semantic product reported before compiler analysis was reached");
    }

    let mut product_names = BTreeSet::<Product>::new();
    for status in &products {
        if !product_names.insert(status.product) {
            bail!("duplicate product_status for {:?}", status.product);
        }
    }
    let expected_products = BTreeSet::from([Product::SemanticGraph]);
    if product_names != expected_products {
        bail!("sidecar does not report availability for every semantic product");
    }
    for source in &sources {
        validate_source(source)?;
    }
    validate_product_facts(
        &products,
        &definitions,
        !sources.is_empty()
            || !definitions.is_empty()
            || !bindings.is_empty()
            || !references.is_empty()
            || !roots.is_empty(),
    )?;
    sources.sort_by(|left, right| left.key.cmp(&right.key));
    products.sort_by_key(|status| status.product);
    definitions.sort_by(|left, right| left.id.cmp(&right.id));
    bindings.sort_by(|left, right| left.id.cmp(&right.id));
    roots.sort_by(|left, right| left.id.cmp(&right.id));
    references.sort_by(|left, right| left.id.cmp(&right.id));
    reject_duplicate("source file", sources.iter().map(|source| &source.key.0))?;
    let mut fact_ids = BTreeSet::new();
    for (kind, id) in definitions
        .iter()
        .map(|fact| ("definition", &fact.id.0))
        .chain(bindings.iter().map(|fact| ("public binding", &fact.id.0)))
        .chain(roots.iter().map(|fact| ("root", &fact.id.0)))
        .chain(references.iter().map(|fact| ("reference", &fact.id.0)))
    {
        if !fact_ids.insert(id) {
            bail!("duplicate semantic fact identity {id:?} ({kind})");
        }
    }
    let source_lengths = sources
        .iter()
        .map(|source| (source.key.clone(), source.byte_len))
        .collect::<BTreeMap<_, _>>();
    for span in definitions
        .iter()
        .flat_map(|fact| [fact.span.as_ref(), fact.attribution_callsite.as_ref()])
        .chain(bindings.iter().map(|fact| fact.span.as_ref()))
        .chain(references.iter().map(|fact| fact.span.as_ref()))
        .flatten()
    {
        let Some(byte_len) = source_lengths.get(&span.file) else {
            bail!("semantic fact references an unknown source file");
        };
        if span.start > span.end || span.end > *byte_len || span.line == 0 || span.column == 0 {
            bail!("semantic fact contains an invalid source span");
        }
    }
    let definitions_by_id = definition_ids(&definitions)?;
    let local_crates = definitions_by_id
        .iter()
        .map(|definition| definition.stable_crate_id)
        .collect::<BTreeSet<_>>();
    let unknown_local = |definition: &rot_compiler_protocol::CompilerDefId| {
        local_crates.contains(&definition.stable_crate_id)
            && !definitions_by_id.contains(definition)
    };
    validate_definition_boundaries(&definitions, &definitions_by_id)?;
    validate_public_bindings(&bindings, &definitions, &definitions_by_id, &local_crates)?;
    if roots
        .iter()
        .any(|root| !definitions_by_id.contains(&root.definition))
        || references.iter().any(|reference| {
            !definitions_by_id.contains(&reference.from) || unknown_local(&reference.to)
        })
    {
        bail!("semantic graph contains an unknown local definition endpoint");
    }

    Ok(Invocation {
        id: invocation_id,
        started,
        profile,
        sources,
        products,
        diagnostics,
        definitions,
        bindings,
        roots,
        references,
        finished,
    })
}

fn definition_ids(
    definitions: &[Definition],
) -> Result<BTreeSet<rot_compiler_protocol::CompilerDefId>> {
    let ids = definitions
        .iter()
        .map(|definition| definition.compiler_id)
        .collect::<BTreeSet<_>>();
    if ids.len() != definitions.len() {
        bail!("duplicate definition compiler identity");
    }
    Ok(ids)
}

fn validate_definition_boundaries(
    definitions: &[Definition],
    ids: &BTreeSet<rot_compiler_protocol::CompilerDefId>,
) -> Result<()> {
    let unknown = definitions.iter().any(|definition| {
        definition
            .parent
            .as_ref()
            .is_some_and(|parent| !ids.contains(parent))
            || match &definition.nominal_visibility {
                rot_compiler_protocol::NominalVisibility::Public => false,
                rot_compiler_protocol::NominalVisibility::Restricted(boundary) => {
                    !ids.contains(boundary)
                }
            }
    });
    if unknown {
        bail!("semantic definition has an unknown local parent or visibility boundary");
    }
    Ok(())
}

fn validate_public_bindings(
    bindings: &[PublicBinding],
    definitions: &[Definition],
    definition_ids: &BTreeSet<rot_compiler_protocol::CompilerDefId>,
    local_crates: &BTreeSet<u64>,
) -> Result<()> {
    let definitions_by_id = definitions
        .iter()
        .map(|definition| (definition.compiler_id, definition))
        .collect::<BTreeMap<_, _>>();
    let mut slots = BTreeSet::new();
    for binding in bindings {
        if binding.name.is_empty() || binding.resolved_target_path.is_empty() {
            bail!("public binding has an empty name or resolved target path");
        }
        let Some(parent) = definitions_by_id.get(&binding.parent) else {
            bail!("public binding has an unknown local parent");
        };
        if !matches!(parent.kind, DefinitionKind::Crate | DefinitionKind::Module) {
            bail!("public binding parent is not a crate or module");
        }
        if let Some(import) = binding.exposing_import {
            let Some(import) = definitions_by_id.get(&import) else {
                bail!("public binding has an unknown local exposing import");
            };
            if !matches!(
                import.kind,
                DefinitionKind::Import | DefinitionKind::ExternCrate
            ) {
                bail!("public binding exposing import is not an import or extern crate");
            }
        }
        if local_crates.contains(&binding.target.stable_crate_id) {
            if !definition_ids.contains(&binding.target) {
                bail!("public binding has an unknown local target");
            }
            let target = definitions_by_id
                .get(&binding.target)
                .expect("validated local target has a definition");
            if target.definition_path != binding.resolved_target_path {
                bail!("public binding resolved target path does not match its local target");
            }
        }
        if !slots.insert((binding.parent, binding.name.as_str(), binding.namespace)) {
            bail!("duplicate public binding namespace slot");
        }
    }
    Ok(())
}

fn validate_started(started: &InvocationStarted) -> Result<()> {
    let artifact = &started.artifact;
    if artifact.crate_name != started.crate_name {
        bail!("invocation/artifact crate name mismatch");
    }
    if started.target_triple.is_empty() {
        bail!("compiler invocation omitted its target triple");
    }
    if started.input.is_none()
        || started.manifest_dir.is_none()
        || artifact.out_dir.is_none()
        || artifact.crate_types.is_empty()
        || artifact.metadata.as_ref().is_none_or(String::is_empty)
        || artifact.emit.is_empty()
    {
        bail!("compiler invocation omitted required Cargo artifact identity");
    }
    if !is_sorted_unique(&artifact.crate_types) || !is_sorted_unique(&artifact.emit) {
        bail!("compiler artifact identity is not canonical");
    }
    Ok(())
}

fn validate_profile(profile: &Profile, started: &InvocationStarted) -> Result<()> {
    if profile.host_triple != started.compiler.host {
        bail!("compiler profile host does not match the compiler identity");
    }
    if profile.host_triple.is_empty()
        || profile.target_triple.is_empty()
        || profile.codegen.target_cpu.is_empty()
        || profile.codegen.codegen_units == 0
    {
        bail!("compiler profile omitted required target or codegen identity");
    }
    if !profile.cfg.windows(2).all(|pair| pair[0] < pair[1])
        || !is_sorted_unique(&profile.features)
        || !is_sorted_unique(&profile.codegen.target_features)
    {
        bail!("compiler profile is not canonical");
    }
    let cfg_features = profile
        .cfg
        .iter()
        .filter(|cfg| cfg.name == "feature")
        .map(|cfg| {
            cfg.value
                .as_ref()
                .context("compiler profile contains a valueless feature cfg")
        })
        .collect::<Result<Vec<_>>>()?;
    if cfg_features
        .iter()
        .map(|feature| feature.as_str())
        .ne(profile.features.iter().map(String::as_str))
    {
        bail!("compiler profile feature cfg does not match its feature list");
    }
    Ok(())
}

fn validate_source(source: &SourceFile) -> Result<()> {
    let digits = source
        .source_hash
        .strip_prefix(&format!("{}=", source.source_hash_algorithm))
        .context("source hash does not match its named algorithm")?;
    let expected_digits = match source.source_hash_algorithm.as_str() {
        "md5" => 32,
        "sha1" => 40,
        "sha256" | "blake3" => 64,
        algorithm => bail!("unsupported source hash algorithm {algorithm:?}"),
    };
    if digits.len() != expected_digits || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("source hash has an invalid digest");
    }
    Ok(())
}

fn validate_product_facts(
    products: &[ProductStatus],
    definitions: &[Definition],
    has_facts: bool,
) -> Result<()> {
    let status = products
        .iter()
        .find(|status| status.product == Product::SemanticGraph)
        .context("semantic graph status is missing")?;
    if has_facts && status.availability == rot_compiler_protocol::Availability::Unavailable {
        bail!("unavailable semantic graph contains facts");
    }
    if status.availability != rot_compiler_protocol::Availability::Unavailable {
        let crate_definitions = definitions
            .iter()
            .filter(|definition| definition.kind == DefinitionKind::Crate)
            .count();
        if crate_definitions != 1 {
            bail!(
                "semantic graph facts contain {crate_definitions} crate definitions; expected exactly one"
            );
        }
    }
    Ok(())
}

fn is_sorted_unique(values: &[String]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn reject_duplicate<'a>(kind: &str, mut values: impl Iterator<Item = &'a String>) -> Result<()> {
    let mut seen = BTreeSet::new();
    if let Some(value) = values.find(|value| !seen.insert((*value).clone())) {
        bail!("duplicate {kind} identity {value:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rot_compiler_protocol::{
        ArtifactIdentity, Availability, CfgValue, CodegenProfile, CompilationContext,
        CompilerDefId, CompilerIdentity, DRIVER_VERSION, DefinitionKind, ExpansionOrigin, Exposure,
        FactId, InvocationMergeKey, Namespace, NominalVisibility, OptimizationLevel, PanicStrategy,
        ProductStatus, SourceFileKey, SourceSpan,
    };

    fn handshake() -> Handshake {
        Handshake {
            protocol_version: PROTOCOL_VERSION,
            driver_version: DRIVER_VERSION,
            linked_rustc_version: "1.100.0-nightly (bff8e12ff 2026-08-26)".to_owned(),
            rustc: CompilerIdentity {
                release: "nightly".to_owned(),
                commit_hash: "bff8e12ff5e6bcd53dfb1dbccdcec80a60a856ed".to_owned(),
                commit_date: "date".to_owned(),
                host: "host".to_owned(),
            },
            max_sidecar_bytes: MAX_SIDECAR_BYTES,
        }
    }

    fn record(sequence: u64, event: Event) -> Record {
        Record {
            protocol_version: PROTOCOL_VERSION,
            run_id: RunId("run".to_owned()),
            invocation_id: InvocationId("local".to_owned()),
            sequence,
            event,
        }
    }

    fn started(compiler: CompilerIdentity) -> Event {
        Event::InvocationStarted(InvocationStarted {
            merge_key: InvocationMergeKey("merge".to_owned()),
            compiler,
            process_id: 1,
            rustc_path: "rustc".to_owned(),
            working_directory: "workspace".to_owned(),
            manifest_dir: Some("workspace".to_owned()),
            build_script_out_dir: None,
            package_name: None,
            primary_package: true,
            crate_name: "crate".to_owned(),
            test_mode: false,
            target_triple: "host".to_owned(),
            compilation_context: CompilationContext::Host,
            input: Some("src/lib.rs".to_owned()),
            artifact: ArtifactIdentity {
                out_dir: Some("target".to_owned()),
                crate_name: "crate".to_owned(),
                crate_types: vec!["lib".to_owned()],
                extra_filename: Some("-hash".to_owned()),
                metadata: Some("hash".to_owned()),
                emit: vec!["metadata".to_owned()],
            },
        })
    }

    fn profile() -> Event {
        Event::Profile(Profile {
            host_triple: "host".to_owned(),
            target_triple: "host".to_owned(),
            test_mode: false,
            cfg: Vec::new(),
            features: Vec::new(),
            codegen: CodegenProfile {
                optimization: OptimizationLevel::None,
                panic: PanicStrategy::Unwind,
                debug_assertions: true,
                overflow_checks: true,
                codegen_units: 1,
                target_cpu: "generic".to_owned(),
                target_features: Vec::new(),
            },
        })
    }

    fn definition(id: CompilerDefId, visibility: NominalVisibility) -> Definition {
        Definition {
            id: FactId(format!("{}:{}", id.stable_crate_id, id.local_hash)),
            compiler_id: id,
            parent: None,
            name: Some("item".to_owned()),
            definition_path: "crate::item".to_owned(),
            kind: DefinitionKind::Function,
            visibility_editable: true,
            nominal_visibility: visibility,
            externally_reachable: true,
            span: None,
            attribution_callsite: None,
            expansion_origin: ExpansionOrigin::Authored,
        }
    }

    fn crate_definition() -> Definition {
        let mut definition = definition(
            CompilerDefId {
                stable_crate_id: 1,
                local_hash: 1,
            },
            NominalVisibility::Public,
        );
        definition.name = Some("crate".to_owned());
        definition.definition_path = "crate".to_owned();
        definition.kind = DefinitionKind::Crate;
        definition.visibility_editable = false;
        definition
    }

    fn public_binding(target: CompilerDefId, resolved_target_path: &str) -> PublicBinding {
        PublicBinding {
            id: FactId("binding".to_owned()),
            parent: CompilerDefId {
                stable_crate_id: 1,
                local_hash: 1,
            },
            target,
            name: "Alias".to_owned(),
            namespace: Namespace::Type,
            exposure: Exposure::SingleReexport,
            exposing_import: None,
            span: None,
            resolved_target_path: resolved_target_path.to_owned(),
        }
    }

    #[test]
    fn validates_complete_contiguous_sidecar() {
        let compiler = handshake().rustc.clone();
        let records = vec![
            record(0, started(compiler)),
            record(1, profile()),
            record(2, Event::Definition(crate_definition())),
            record(
                3,
                Event::PublicBinding(public_binding(
                    CompilerDefId {
                        stable_crate_id: 2,
                        local_hash: 1,
                    },
                    "dependency::Item",
                )),
            ),
            record(
                4,
                Event::ProductStatus(ProductStatus {
                    product: Product::SemanticGraph,
                    availability: Availability::Complete,
                    message: None,
                }),
            ),
            record(
                5,
                Event::InvocationFinished(InvocationFinished {
                    rustc_success: true,
                    analysis_reached: true,
                }),
            ),
        ];
        let invocation = validate_records(records, "run", &handshake()).unwrap();
        assert_eq!(invocation.started.merge_key.0, "merge");
        assert!(invocation.finished.rustc_success);
        assert_eq!(invocation.bindings.len(), 1);
        assert_eq!(
            invocation.bindings[0].resolved_target_path,
            "dependency::Item"
        );
    }

    #[test]
    fn rejects_complete_semantic_graph_without_crate_definition() {
        let records = vec![
            record(0, started(handshake().rustc.clone())),
            record(1, profile()),
            record(
                2,
                Event::ProductStatus(ProductStatus {
                    product: Product::SemanticGraph,
                    availability: Availability::Complete,
                    message: None,
                }),
            ),
            record(
                3,
                Event::InvocationFinished(InvocationFinished {
                    rustc_success: true,
                    analysis_reached: true,
                }),
            ),
        ];

        let error = validate_records(records, "run", &handshake()).unwrap_err();
        assert!(error.to_string().contains("expected exactly one"));
    }

    #[test]
    fn rejects_gaps_and_cross_run_records() {
        let compiler = handshake().rustc.clone();
        let started = started(compiler);
        assert!(validate_records(vec![record(1, started.clone())], "run", &handshake()).is_err());
        let mut wrong_run = record(0, started);
        wrong_run.run_id = RunId("other".to_owned());
        assert!(validate_records(vec![wrong_run], "run", &handshake()).is_err());
    }

    #[test]
    fn rejects_complete_product_without_analysis() {
        let records = vec![
            record(0, started(handshake().rustc.clone())),
            record(
                1,
                Event::ProductStatus(ProductStatus {
                    product: Product::SemanticGraph,
                    availability: Availability::Complete,
                    message: None,
                }),
            ),
            record(
                2,
                Event::InvocationFinished(InvocationFinished {
                    rustc_success: true,
                    analysis_reached: false,
                }),
            ),
        ];

        let error = validate_records(records, "run", &handshake()).unwrap_err();
        assert!(error.to_string().contains("before compiler analysis"));
    }

    #[test]
    fn rejects_duplicate_definition_compiler_identity() {
        let mut definition = definition(
            CompilerDefId {
                stable_crate_id: 1,
                local_hash: 2,
            },
            NominalVisibility::Public,
        );
        definition.id = FactId("first".to_owned());
        let mut duplicate = definition.clone();
        duplicate.id = FactId("second".to_owned());

        assert!(
            definition_ids(&[definition, duplicate])
                .unwrap_err()
                .to_string()
                .contains("duplicate definition compiler identity")
        );
    }

    #[test]
    fn rejects_noncanonical_or_inconsistent_profile() {
        let Event::Profile(mut profile) = profile() else {
            unreachable!()
        };
        profile.cfg = vec![
            CfgValue {
                name: "feature".to_owned(),
                value: Some("z".to_owned()),
            },
            CfgValue {
                name: "feature".to_owned(),
                value: Some("a".to_owned()),
            },
        ];
        profile.features = vec!["a".to_owned(), "z".to_owned()];
        let Event::InvocationStarted(started) = started(handshake().rustc.clone()) else {
            unreachable!()
        };
        assert!(validate_profile(&profile, &started).is_err());

        profile.cfg.sort();
        profile.features = vec!["a".to_owned()];
        assert!(
            validate_profile(&profile, &started)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );

        profile.features = vec!["a".to_owned(), "z".to_owned()];
        profile.codegen.target_features = vec!["sse4.2".to_owned(), "avx".to_owned()];
        assert!(validate_profile(&profile, &started).is_err());

        profile.codegen.target_features.clear();
        profile.host_triple = "corrupt-host".to_owned();
        assert!(
            validate_profile(&profile, &started)
                .unwrap_err()
                .to_string()
                .contains("compiler identity")
        );
    }

    #[test]
    fn rejects_unknown_restricted_visibility_boundary() {
        let item_id = CompilerDefId {
            stable_crate_id: 1,
            local_hash: 2,
        };
        let missing_boundary = CompilerDefId {
            stable_crate_id: 1,
            local_hash: 3,
        };
        let definitions = vec![definition(
            item_id,
            NominalVisibility::Restricted(missing_boundary),
        )];
        let ids = definition_ids(&definitions).unwrap();

        assert!(
            validate_definition_boundaries(&definitions, &ids)
                .unwrap_err()
                .to_string()
                .contains("visibility boundary")
        );
    }

    #[test]
    fn validates_public_binding_endpoints_and_unique_slots() {
        let crate_id = CompilerDefId {
            stable_crate_id: 1,
            local_hash: 1,
        };
        let target_id = CompilerDefId {
            stable_crate_id: 1,
            local_hash: 2,
        };
        let import_id = CompilerDefId {
            stable_crate_id: 1,
            local_hash: 3,
        };
        let mut target = definition(target_id, NominalVisibility::Public);
        target.definition_path = "crate::Item".to_owned();
        target.kind = DefinitionKind::Struct;
        let mut import = definition(import_id, NominalVisibility::Public);
        import.definition_path = "crate::{use#0}".to_owned();
        import.kind = DefinitionKind::Import;
        let definitions = vec![crate_definition(), target, import];
        let ids = definition_ids(&definitions).unwrap();
        let local_crates = BTreeSet::from([crate_id.stable_crate_id]);
        let mut binding = public_binding(target_id, "crate::Item");
        binding.exposing_import = Some(import_id);

        validate_public_bindings(&[binding.clone()], &definitions, &ids, &local_crates).unwrap();

        let missing_id = CompilerDefId {
            stable_crate_id: 1,
            local_hash: 99,
        };
        let mut missing_parent = binding.clone();
        missing_parent.parent = missing_id;
        assert!(
            validate_public_bindings(&[missing_parent], &definitions, &ids, &local_crates)
                .unwrap_err()
                .to_string()
                .contains("unknown local parent")
        );

        let mut missing_target = binding.clone();
        missing_target.target = missing_id;
        assert!(
            validate_public_bindings(&[missing_target], &definitions, &ids, &local_crates)
                .unwrap_err()
                .to_string()
                .contains("unknown local target")
        );

        let mut missing_import = binding.clone();
        missing_import.exposing_import = Some(missing_id);
        assert!(
            validate_public_bindings(&[missing_import], &definitions, &ids, &local_crates)
                .unwrap_err()
                .to_string()
                .contains("unknown local exposing import")
        );

        let mut wrong_parent = binding.clone();
        wrong_parent.parent = target_id;
        assert!(
            validate_public_bindings(&[wrong_parent], &definitions, &ids, &local_crates)
                .unwrap_err()
                .to_string()
                .contains("not a crate or module")
        );

        let mut wrong_import = binding.clone();
        wrong_import.exposing_import = Some(target_id);
        assert!(
            validate_public_bindings(&[wrong_import], &definitions, &ids, &local_crates)
                .unwrap_err()
                .to_string()
                .contains("not an import or extern crate")
        );

        let mut mismatched_path = binding.clone();
        mismatched_path.resolved_target_path = "crate::Other".to_owned();
        assert!(
            validate_public_bindings(&[mismatched_path], &definitions, &ids, &local_crates)
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );

        let mut duplicate = binding.clone();
        duplicate.id = FactId("other-binding".to_owned());
        assert!(
            validate_public_bindings(&[binding, duplicate], &definitions, &ids, &local_crates)
                .unwrap_err()
                .to_string()
                .contains("duplicate public binding namespace slot")
        );
    }

    #[test]
    fn rejects_public_binding_fact_id_collision() {
        let mut binding = public_binding(
            CompilerDefId {
                stable_crate_id: 2,
                local_hash: 1,
            },
            "dependency::Item",
        );
        binding.id = crate_definition().id;
        let records = vec![
            record(0, started(handshake().rustc.clone())),
            record(1, profile()),
            record(2, Event::Definition(crate_definition())),
            record(3, Event::PublicBinding(binding)),
            record(
                4,
                Event::ProductStatus(ProductStatus {
                    product: Product::SemanticGraph,
                    availability: Availability::Complete,
                    message: None,
                }),
            ),
            record(
                5,
                Event::InvocationFinished(InvocationFinished {
                    rustc_success: true,
                    analysis_reached: true,
                }),
            ),
        ];

        assert!(
            validate_records(records, "run", &handshake())
                .unwrap_err()
                .to_string()
                .contains("duplicate semantic fact identity")
        );
    }

    #[test]
    fn rejects_public_binding_span_without_its_source_record() {
        let mut binding = public_binding(
            CompilerDefId {
                stable_crate_id: 2,
                local_hash: 1,
            },
            "dependency::Item",
        );
        binding.span = Some(SourceSpan {
            file: SourceFileKey("missing".to_owned()),
            start: 0,
            end: 1,
            line: 1,
            column: 1,
        });
        let records = vec![
            record(0, started(handshake().rustc.clone())),
            record(1, profile()),
            record(2, Event::Definition(crate_definition())),
            record(3, Event::PublicBinding(binding)),
            record(
                4,
                Event::ProductStatus(ProductStatus {
                    product: Product::SemanticGraph,
                    availability: Availability::Complete,
                    message: None,
                }),
            ),
            record(
                5,
                Event::InvocationFinished(InvocationFinished {
                    rustc_success: true,
                    analysis_reached: true,
                }),
            ),
        ];

        assert!(
            validate_records(records, "run", &handshake())
                .unwrap_err()
                .to_string()
                .contains("unknown source file")
        );
    }
}
