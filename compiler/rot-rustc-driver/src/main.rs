#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_lint_defs;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;
#[cfg(rot_crate_type_in_structures)]
extern crate rustc_structures;
extern crate rustc_target;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{self, ExitCode},
};

use rot_compiler_protocol::{
    ArtifactIdentity, Availability, BUILD_DIR_ENV, CfgValue, CodegenProfile, CompilationContext,
    CompilerDefId, CompilerIdentity, DRIVER_VERSION, Definition, DefinitionKind, Diagnostic,
    DiagnosticPhase, DiagnosticSeverity, Event, ExpansionOrigin, FactId, HANDSHAKE_ARG, Handshake,
    InvocationFinished, InvocationId, InvocationMergeKey, InvocationStarted, MAX_SIDECAR_BYTES,
    NominalVisibility, OptimizationLevel, PROTOCOL_VERSION, PanicStrategy, Product, ProductStatus,
    Profile, RUN_ID_ENV, Record, Reference, ReferenceKind, Root, RootKind, RunId,
    SELECTED_MANIFEST_DIRS_ENV, SIDECAR_DIR_ENV, SourceFile, SourceFileKey, SourceSpan,
    TARGET_DIR_ENV,
};
use rustc_driver::{Callbacks, Compilation};
use rustc_hir::{
    Expr, ExprKind, Node, Pat, PatExpr, PatExprKind, PatKind, QPath, StructTailExpr,
    def::{CtorOf, DefKind, Res},
    intravisit::{self, Visitor},
};
use rustc_interface::interface::Compiler;
use rustc_lint_defs::{Level as LintLevel, builtin::DEAD_CODE};
use rustc_middle::{
    metadata::Reexport,
    middle::codegen_fn_attrs::CodegenFnAttrFlags,
    ty::{self, TyCtxt, Visibility},
};
#[cfg(not(rot_crate_type_in_structures))]
use rustc_session::config::CrateType;
use rustc_session::config::{self, OptLevel};
use rustc_span::{
    FileName, Pos, Span,
    def_id::{CRATE_DEF_ID, DefId, LOCAL_CRATE, LocalDefId},
    hygiene::ExpnKind,
};
#[cfg(rot_crate_type_in_structures)]
use rustc_structures::CrateType;
use rustc_target::spec::PanicStrategy as RustcPanicStrategy;

const TRAILER_RESERVE_BYTES: usize = 64 * 1024;
const MAX_SELECTED_MANIFEST_DIRS: usize = 4096;
const BUILD_RUSTC_VERSION: &str = env!("ROT_BUILD_RUSTC_VERSION");

fn main() -> ExitCode {
    let args = match unicode_args() {
        Ok(args) => args,
        Err(argument) => {
            eprintln!(
                "rot-rustc-driver: argument is not valid UTF-8: {}",
                argument.to_string_lossy()
            );
            return ExitCode::FAILURE;
        }
    };

    if args.as_slice() == [HANDSHAKE_ARG] {
        return write_handshake();
    }

    let Some(rustc_path) = args.first() else {
        eprintln!("rot-rustc-driver: expected rustc path as the first wrapper argument");
        return ExitCode::FAILURE;
    };

    let invocation = InvocationArgs::parse(rustc_path.clone(), &args[1..]);
    let linked_compiler = linked_rustc_version();
    if linked_compiler.as_deref() != Some(BUILD_RUSTC_VERSION) {
        eprintln!(
            "rot-rustc-driver: refusing compiler fact collection: driver was built for rustc {BUILD_RUSTC_VERSION}, found {}",
            linked_compiler.as_deref().unwrap_or("unknown")
        );
    }
    let mut callbacks = DriverCallbacks {
        collection: (linked_compiler.as_deref() == Some(BUILD_RUSTC_VERSION))
            .then(|| Collection::from_environment(invocation))
            .flatten(),
    };
    let exit = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&args, &mut callbacks);
    });

    if let Some(collection) = &mut callbacks.collection {
        #[cfg(rot_driver_exit_code)]
        collection.finish(exit == ExitCode::SUCCESS);
        #[cfg(not(rot_driver_exit_code))]
        collection.finish(exit == 0);
        if let Err(error) = collection.write_sidecar() {
            eprintln!("rot-rustc-driver: cannot write compiler sidecar: {error}");
        }
    }

    #[cfg(rot_driver_exit_code)]
    let exit_code = exit;
    #[cfg(not(rot_driver_exit_code))]
    let exit_code = if exit == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(u8::try_from(exit).unwrap_or(1))
    };
    exit_code
}

fn unicode_args() -> Result<Vec<String>, OsString> {
    env::args_os()
        .skip(1)
        .map(|argument| argument.into_string())
        .collect()
}

fn write_handshake() -> ExitCode {
    let Some(linked_rustc_version) = linked_rustc_version() else {
        eprintln!("rot-rustc-driver: linked rustc did not report its version");
        return ExitCode::FAILURE;
    };
    if linked_rustc_version != BUILD_RUSTC_VERSION {
        eprintln!(
            "rot-rustc-driver: linked rustc mismatch: driver was built for {BUILD_RUSTC_VERSION}, found {linked_rustc_version}"
        );
        return ExitCode::FAILURE;
    }
    let handshake = Handshake {
        protocol_version: PROTOCOL_VERSION,
        driver_version: DRIVER_VERSION,
        linked_rustc_version,
        rustc: compiler_identity(),
        max_sidecar_bytes: MAX_SIDECAR_BYTES,
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if serde_json::to_writer(&mut output, &handshake)
        .and_then(|()| output.write_all(b"\n").map_err(serde_json::Error::io))
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn linked_rustc_version() -> Option<String> {
    rustc_interface::util::rustc_version_str().map(str::to_owned)
}

fn compiler_identity() -> CompilerIdentity {
    CompilerIdentity {
        release: env!("ROT_BUILD_RUSTC_RELEASE").to_owned(),
        commit_hash: env!("ROT_BUILD_RUSTC_COMMIT").to_owned(),
        commit_date: env!("ROT_BUILD_RUSTC_COMMIT_DATE").to_owned(),
        host: env!("ROT_BUILD_RUSTC_HOST").to_owned(),
    }
}

#[derive(Clone, Debug)]
struct InvocationArgs {
    rustc_path: String,
    working_directory: String,
    manifest_dir: Option<String>,
    build_script_out_dir: Option<String>,
    package_name: Option<String>,
    primary_package: bool,
    crate_name: String,
    input: Option<String>,
    artifact: ArtifactIdentity,
    target: String,
    compilation_context: CompilationContext,
    test_mode: bool,
    cfg: Vec<String>,
    relevant_codegen: Vec<String>,
    collectible: bool,
}

impl InvocationArgs {
    fn parse(rustc_path: String, args: &[String]) -> Self {
        let working_directory = canonical_string(&env::current_dir().unwrap_or_default());
        let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .map(|path| canonical_string(&path));
        let build_script_out_dir = env::var_os("OUT_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .map(|path| canonical_string(&path));
        let package_name = env::var("CARGO_PKG_NAME").ok();
        let explicit_crate_name = option_value(args, "--crate-name");
        let crate_name = explicit_crate_name
            .clone()
            .or_else(|| env::var("CARGO_CRATE_NAME").ok())
            .unwrap_or_else(|| "unknown".to_owned());
        let mut crate_types = option_values(args, "--crate-type")
            .into_iter()
            .flat_map(|types| types.split(',').map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if crate_types.is_empty() {
            crate_types.push("bin".to_owned());
        }
        crate_types.sort();
        crate_types.dedup();

        let input = args
            .iter()
            .find(|argument| {
                !argument.starts_with('-')
                    && Path::new(argument)
                        .extension()
                        .is_some_and(|ext| ext == "rs")
            })
            .map(PathBuf::from)
            .map(|path| canonical_string(&path));
        let out_dir = option_value(args, "--out-dir")
            .map(PathBuf::from)
            .map(|path| canonical_string(&path));
        let extra_filename =
            codegen_value(args, "extra-filename").filter(|value| !value.is_empty());
        let metadata = codegen_value(args, "metadata").filter(|value| !value.is_empty());
        let mut emit = option_values(args, "--emit")
            .into_iter()
            .flat_map(|values| values.split(',').map(str::to_owned).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        emit.sort();
        emit.dedup();

        let mut cfg = option_values(args, "--cfg");
        cfg.sort();
        cfg.dedup();
        let explicit_target = option_value(args, "--target");
        let compilation_context = if explicit_target.is_some() {
            CompilationContext::Target
        } else {
            CompilationContext::Host
        };
        let target = explicit_target.unwrap_or_else(|| config::host_tuple().to_owned());
        let test_mode = args.iter().any(|argument| argument == "--test")
            || cfg.iter().any(|configuration| configuration == "test");
        let relevant_codegen = [
            "debug-assertions",
            "codegen-units",
            "opt-level",
            "overflow-checks",
            "panic",
            "target-cpu",
            "target-feature",
        ]
        .into_iter()
        .flat_map(|name| {
            codegen_values(args, name)
                .into_iter()
                .map(move |value| format!("{name}={value}"))
        })
        .collect();
        let collectible = explicit_crate_name.is_some() && input.is_some() && out_dir.is_some();

        Self {
            rustc_path,
            working_directory,
            manifest_dir,
            build_script_out_dir,
            package_name,
            primary_package: env::var_os("CARGO_PRIMARY_PACKAGE").is_some(),
            crate_name: crate_name.clone(),
            input,
            artifact: ArtifactIdentity {
                out_dir,
                crate_name,
                crate_types: crate_types.clone(),
                extra_filename,
                metadata,
                emit,
            },
            target,
            compilation_context,
            test_mode,
            cfg,
            relevant_codegen,
            collectible,
        }
    }

    fn merge_key(&self) -> InvocationMergeKey {
        let mut parts = vec![
            self.manifest_dir.clone().unwrap_or_default(),
            self.package_name.clone().unwrap_or_default(),
            self.crate_name.clone(),
            self.artifact.crate_types.join(","),
            self.input.clone().unwrap_or_default(),
            self.target.clone(),
            match self.compilation_context {
                CompilationContext::Host => "host",
                CompilationContext::Target => "target",
            }
            .to_owned(),
            self.test_mode.to_string(),
            self.cfg.join("\u{1f}"),
            self.relevant_codegen.join("\u{1f}"),
            self.artifact.extra_filename.clone().unwrap_or_default(),
            self.artifact.metadata.clone().unwrap_or_default(),
            self.artifact.emit.join(","),
        ];
        parts.insert(0, "invocation-v3".to_owned());
        InvocationMergeKey(length_prefixed(&parts))
    }
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    option_values(args, name).into_iter().next()
}

fn option_values(args: &[String], name: &str) -> Vec<String> {
    let joined = format!("{name}=");
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == name {
            if let Some(value) = args.get(index + 1) {
                values.push(value.clone());
                index += 2;
                continue;
            }
        } else if let Some(value) = args[index].strip_prefix(&joined) {
            values.push(value.to_owned());
        }
        index += 1;
    }
    values
}

fn codegen_value(args: &[String], name: &str) -> Option<String> {
    codegen_values(args, name).pop()
}

fn codegen_values(args: &[String], name: &str) -> Vec<String> {
    let prefix = format!("{name}=");
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let option = if args[index] == "-C" {
            index += 1;
            args.get(index).map(String::as_str)
        } else {
            args[index].strip_prefix("-C")
        };
        if let Some(value) = option.and_then(|option| option.strip_prefix(&prefix)) {
            values.push(value.to_owned());
        }
        index += 1;
    }
    values
}

struct DriverCallbacks {
    collection: Option<Collection>,
}

impl Callbacks for DriverCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        if let Some(collection) = &mut self.collection {
            collection.collect(tcx);
        }
        Compilation::Continue
    }
}

struct Collection {
    sidecar_dir: PathBuf,
    roots: Vec<GeneratedRoot>,
    records: RecordBuffer,
    analysis_reached: bool,
    visibility_audit: ProductProgress,
}

#[derive(Default)]
struct ProductProgress {
    started: bool,
    rejected: bool,
}

impl ProductProgress {
    fn start(&mut self) {
        self.started = true;
    }

    fn reject(&mut self) {
        self.rejected = true;
    }

    fn availability(&self) -> Availability {
        match (self.started, self.rejected) {
            (false, _) => Availability::Unavailable,
            (true, false) => Availability::Complete,
            (true, true) => Availability::Partial,
        }
    }
}

impl Collection {
    fn from_environment(invocation: InvocationArgs) -> Option<Self> {
        if !invocation.collectible
            || !manifest_is_selected(
                invocation.manifest_dir.as_deref(),
                env::var_os(SELECTED_MANIFEST_DIRS_ENV).as_deref(),
            )
        {
            return None;
        }
        let run_id = env::var(RUN_ID_ENV).ok()?;
        let sidecar_dir = env::var_os(SIDECAR_DIR_ENV).map(PathBuf::from)?;
        if !valid_run_id(&run_id) {
            return None;
        }

        let invocation_id = InvocationId(format!(
            "{}-{}",
            process::id(),
            safe_filename(&invocation.crate_name)
        ));
        let build_script_out_dir = invocation.build_script_out_dir.clone();
        let mut records =
            RecordBuffer::new(RunId(run_id), invocation_id, MAX_SIDECAR_BYTES as usize);
        records.push(Event::InvocationStarted(InvocationStarted {
            merge_key: invocation.merge_key(),
            compiler: compiler_identity(),
            process_id: process::id(),
            rustc_path: invocation.rustc_path,
            working_directory: invocation.working_directory,
            manifest_dir: invocation.manifest_dir,
            build_script_out_dir: invocation.build_script_out_dir,
            package_name: invocation.package_name,
            primary_package: invocation.primary_package,
            test_mode: invocation.test_mode,
            target_triple: invocation.target,
            compilation_context: invocation.compilation_context,
            crate_name: invocation.crate_name,
            input: invocation.input,
            artifact: invocation.artifact,
        }));

        let roots = [("target", TARGET_DIR_ENV), ("build", BUILD_DIR_ENV)]
            .into_iter()
            .filter_map(|(label, variable)| {
                env::var_os(variable).map(|path| GeneratedRoot {
                    label,
                    path: canonical_path(Path::new(&path)),
                })
            })
            .chain(build_script_out_dir.map(|path| GeneratedRoot {
                label: "out",
                path: PathBuf::from(path),
            }))
            .collect();

        Some(Self {
            sidecar_dir,
            roots,
            records,
            analysis_reached: false,
            visibility_audit: ProductProgress::default(),
        })
    }

    fn collect<'tcx>(&mut self, tcx: TyCtxt<'tcx>) {
        self.analysis_reached = true;
        self.visibility_audit.start();
        if self.records.truncated {
            self.visibility_audit.reject();
        }
        if !self.records.push(Event::Profile(profile(tcx))) {
            self.visibility_audit.reject();
        }

        let facts = collect_facts(tcx, &self.roots);
        for source in facts.sources.into_values() {
            if !self.records.push(Event::SourceFile(source)) {
                self.visibility_audit.reject();
            }
        }
        for definition in facts.definitions {
            if !self.records.push(Event::Definition(definition)) {
                self.visibility_audit.reject();
            }
        }
        if self.records.truncated {
            self.visibility_audit.reject();
        }
        for root in facts.roots {
            if !self.records.push(Event::Root(root)) {
                self.visibility_audit.reject();
            }
        }
        for reference in facts.references {
            if !self.records.push(Event::Reference(reference)) {
                self.visibility_audit.reject();
            }
        }
    }

    fn finish(&mut self, rustc_success: bool) {
        if self.records.truncated {
            self.records.push_mandatory(Event::Diagnostic(Diagnostic {
                phase: DiagnosticPhase::Sidecar,
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "compiler facts exceeded the {} byte sidecar limit",
                    MAX_SIDECAR_BYTES
                ),
                span: None,
            }));
        }
        self.records
            .push_mandatory(Event::ProductStatus(ProductStatus {
                product: Product::VisibilityAudit,
                availability: self.visibility_audit.availability(),
                message: product_message(&self.visibility_audit),
            }));
        self.records
            .push_mandatory(Event::InvocationFinished(InvocationFinished {
                rustc_success,
                analysis_reached: self.analysis_reached,
            }));
    }

    fn write_sidecar(&self) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.sidecar_dir)?;
        self.records.write_atomic(&self.sidecar_dir)
    }
}

fn product_message(progress: &ProductProgress) -> Option<String> {
    match (progress.started, progress.rejected) {
        (false, _) => Some("rustc did not reach after_analysis".to_owned()),
        (true, true) => Some("compiler facts were truncated by the sidecar limit".to_owned()),
        (true, false) => None,
    }
}

#[derive(Default)]
struct CollectedFacts {
    sources: BTreeMap<SourceFileKey, SourceFile>,
    definitions: Vec<Definition>,
    bindings: Vec<VisibilityBinding>,
    roots: Vec<Root>,
    references: Vec<Reference>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum BindingNamespace {
    Type,
    Value,
    Macro,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum BindingExposure {
    Direct,
    SingleReexport,
    GlobReexport,
    ExternCrate,
    MacroUse,
    MacroExport,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct VisibilityBinding {
    target: CompilerDefId,
    namespace: BindingNamespace,
    exposure: BindingExposure,
    exposing_import: Option<CompilerDefId>,
}

impl CollectedFacts {
    fn remember_span(&mut self, located: &LocatedSpan) {
        self.sources
            .entry(located.source.key.clone())
            .or_insert_with(|| located.source.clone());
    }

    fn remember_attribution(&mut self, attribution: &SpanAttribution) {
        if let Some(span) = &attribution.span {
            self.remember_span(span);
        }
        if let Some(callsite) = &attribution.callsite {
            self.remember_span(callsite);
        }
    }

    fn push_reference(
        &mut self,
        tcx: TyCtxt<'_>,
        from: DefId,
        to: DefId,
        kind: ReferenceKind,
        span: Option<LocatedSpan>,
    ) {
        if let Some(span) = &span {
            self.remember_span(span);
        }
        self.references.push(Reference {
            id: FactId(String::new()),
            from: compiler_id(tcx, from),
            to: compiler_id(tcx, to),
            kind,
            span: span.map(|span| span.span),
        });
    }

    fn push_resolved_references(
        &mut self,
        tcx: TyCtxt<'_>,
        from: DefId,
        kind: ReferenceKind,
        references: &[ResolvedReference],
    ) {
        for reference in references {
            self.push_reference(tcx, from, reference.target, kind, reference.span.clone());
        }
    }
}

#[derive(Clone)]
struct LocatedSpan {
    source: SourceFile,
    span: SourceSpan,
}

struct SpanAttribution {
    span: Option<LocatedSpan>,
    callsite: Option<LocatedSpan>,
    origin: ExpansionOrigin,
}

fn collect_facts(tcx: TyCtxt<'_>, generated_roots: &[GeneratedRoot]) -> CollectedFacts {
    let mut facts = CollectedFacts::default();
    let local_definitions = tcx.iter_local_def_id().collect::<Vec<_>>();
    let emitted_definitions = local_definitions
        .iter()
        .copied()
        .filter(|local_def_id| {
            definition_kind(tcx.def_kind(*local_def_id), *local_def_id).is_some()
        })
        .collect::<HashSet<_>>();

    for local_def_id in emitted_definitions.iter().copied() {
        let kind = definition_kind(tcx.def_kind(local_def_id), local_def_id)
            .expect("emitted definitions have a supported kind");
        let attribution = span_attribution(tcx, tcx.def_span(local_def_id), generated_roots);
        facts.remember_attribution(&attribution);
        facts.definitions.push(Definition {
            id: FactId(String::new()),
            compiler_id: compiler_id(tcx, local_def_id.to_def_id()),
            parent: emitted_parent(tcx, local_def_id, &emitted_definitions),
            name: tcx
                .opt_item_name(local_def_id.to_def_id())
                .map(|name| name.to_string()),
            definition_path: tcx.def_path_str(local_def_id),
            kind,
            visibility_editable: visibility_editable(tcx, local_def_id),
            nominal_visibility: nominal_visibility(tcx, local_def_id.to_def_id()),
            externally_reachable: externally_reachable(tcx, local_def_id),
            span: attribution.span.as_ref().map(|span| span.span.clone()),
            attribution_callsite: attribution
                .callsite
                .as_ref()
                .map(|callsite| callsite.span.clone()),
            expansion_origin: attribution.origin,
        });
    }

    collect_visibility_bindings(tcx, &local_definitions, &mut facts);
    collect_references(
        tcx,
        generated_roots,
        &local_definitions,
        &emitted_definitions,
        &mut facts,
    );
    sort_and_identify_facts(&mut facts);
    facts
}

#[derive(Clone)]
struct ResolvedReference {
    target: DefId,
    span: Option<LocatedSpan>,
}

struct ReferenceVisitor<'roots, 'tcx> {
    tcx: TyCtxt<'tcx>,
    generated_roots: &'roots [GeneratedRoot],
    typeck_results: Option<&'tcx ty::TypeckResults<'tcx>>,
    targets: HashMap<DefId, Option<LocatedSpan>>,
}

impl<'roots, 'tcx> ReferenceVisitor<'roots, 'tcx> {
    fn new(
        tcx: TyCtxt<'tcx>,
        generated_roots: &'roots [GeneratedRoot],
        typeck_results: Option<&'tcx ty::TypeckResults<'tcx>>,
    ) -> Self {
        Self {
            tcx,
            generated_roots,
            typeck_results,
            targets: HashMap::new(),
        }
    }

    fn finish(self) -> Vec<ResolvedReference> {
        self.targets
            .into_iter()
            .map(|(target, span)| ResolvedReference { target, span })
            .collect()
    }

    fn record(&mut self, resolution: Res, span: Span) {
        match resolution {
            Res::Def(DefKind::Ctor(CtorOf::Struct, ..), constructor) => {
                let adt = self.tcx.parent(constructor);
                self.record_def(adt, span);
            }
            Res::Def(DefKind::Ctor(CtorOf::Variant, ..), constructor) => {
                self.record_variant(self.tcx.parent(constructor), span);
            }
            Res::Def(DefKind::Variant, variant) => self.record_variant(variant, span),
            Res::SelfCtor(implementation) => {
                let self_type = self.tcx.type_of(implementation).instantiate_identity();
                #[cfg(rot_type_of_unnormalized)]
                let self_type = self_type.skip_norm_wip();
                if let Some(adt) = self_type.ty_adt_def() {
                    self.record_def(adt.did(), span);
                    if !adt.is_enum() {
                        for field in &adt.non_enum_variant().fields {
                            self.record_def(field.did, span);
                        }
                    }
                }
            }
            Res::Def(_, def_id)
            | Res::SelfTyParam { trait_: def_id }
            | Res::SelfTyAlias {
                alias_to: def_id, ..
            } => self.record_def(def_id, span),
            _ => {}
        }
    }

    fn record_variant(&mut self, variant: DefId, span: Span) {
        self.record_def(variant, span);
    }

    fn record_def(&mut self, def_id: DefId, span: Span) {
        if def_id.as_local().is_some_and(|local_def_id| {
            definition_kind(self.tcx.def_kind(local_def_id), local_def_id).is_none()
        }) {
            return;
        }
        let attribution = span_attribution(self.tcx, span, self.generated_roots);
        let candidate = attribution.span.or(attribution.callsite);
        let retained = self.targets.entry(def_id).or_default();
        if let Some(candidate) = candidate
            && retained
                .as_ref()
                .is_none_or(|current| candidate.span < current.span)
        {
            *retained = Some(candidate);
        }
    }

    fn record_field(&mut self, variant: &ty::VariantDef, hir_id: rustc_hir::HirId, span: Span) {
        if let Some(typeck_results) = self.typeck_results
            && let Some(index) = typeck_results.opt_field_index(hir_id)
        {
            self.record_def(variant.fields[index].did, span);
        }
    }

    fn record_tuple_pattern_fields(&mut self, variant: &ty::VariantDef, span: Span) {
        // A tuple struct's constructor is inaccessible when any field is
        // private, including fields matched by `_` or omitted through `..`.
        for field in &variant.fields {
            self.record_def(field.did, span);
        }
    }

    fn visit_node(&mut self, node: Node<'tcx>) {
        match node {
            Node::Item(item) => self.visit_item(item),
            Node::ImplItem(item) => self.visit_impl_item(item),
            Node::TraitItem(item) => self.visit_trait_item(item),
            Node::ForeignItem(item) => self.visit_foreign_item(item),
            Node::Field(field) => self.visit_field_def(field),
            Node::Variant(variant) => self.visit_variant(variant),
            Node::OpaqueTy(opaque) => self.visit_opaque_ty(opaque),
            _ => {}
        }
    }
}

impl<'tcx> Visitor<'tcx> for ReferenceVisitor<'_, 'tcx> {
    fn visit_path(&mut self, path: &rustc_hir::Path<'tcx>, hir_id: rustc_hir::HirId) {
        self.record(path.res, path.span);
        intravisit::walk_path(self, path);
        let _ = hir_id;
    }

    fn visit_expr(&mut self, expression: &'tcx Expr<'tcx>) {
        if let Some(typeck_results) = self.typeck_results {
            match expression.kind {
                ExprKind::Path(ref qpath) => {
                    let resolution = typeck_results.qpath_res(qpath, expression.hir_id);
                    if matches!(qpath, QPath::TypeRelative(..)) {
                        self.record(resolution, expression.span);
                    }
                    if let Res::Def(DefKind::Ctor(owner, ..), constructor) = resolution {
                        let adt = match owner {
                            CtorOf::Struct => self.tcx.parent(constructor),
                            CtorOf::Variant => self.tcx.parent(self.tcx.parent(constructor)),
                        };
                        let adt = self.tcx.adt_def(adt);
                        for field in &adt.variant_with_ctor_id(constructor).fields {
                            self.record_def(field.did, expression.span);
                        }
                    }
                }
                ExprKind::Struct(qpath, fields, tail) => {
                    let resolution = typeck_results.qpath_res(qpath, expression.hir_id);
                    if matches!(qpath, QPath::TypeRelative(..)) {
                        self.record(resolution, expression.span);
                    }
                    if let Some(adt) = typeck_results.expr_ty(expression).ty_adt_def() {
                        let variant = adt.variant_of_res(resolution);
                        for field in fields {
                            self.record_field(variant, field.hir_id, field.ident.span);
                        }
                        if !matches!(tail, StructTailExpr::None) {
                            for field in &variant.fields {
                                self.record_def(field.did, expression.span);
                            }
                        }
                    }
                }
                ExprKind::Field(base, ident) => {
                    if let Some(adt) = typeck_results.expr_ty_adjusted(base).ty_adt_def()
                        && !adt.is_enum()
                    {
                        self.record_field(adt.non_enum_variant(), expression.hir_id, ident.span);
                    }
                }
                ExprKind::OffsetOf(..) => {
                    if let Some(fields) = typeck_results.offset_of_data().get(expression.hir_id) {
                        #[cfg(rot_flat_offset_of)]
                        for (container, variant, field) in fields {
                            if let ty::Adt(adt, _) = container.kind()
                                && !adt.is_enum()
                            {
                                self.record_def(
                                    adt.variant(*variant).fields[*field].did,
                                    expression.span,
                                );
                            }
                        }
                        #[cfg(not(rot_flat_offset_of))]
                        {
                            let (container, fields) = fields;
                            if let ty::Adt(adt, _) = container.kind()
                                && !adt.is_enum()
                            {
                                for (variant, field) in fields {
                                    self.record_def(
                                        adt.variant(*variant).fields[*field].did,
                                        expression.span,
                                    );
                                }
                            }
                        }
                    }
                }
                ExprKind::MethodCall(..) => {
                    if let Some(def_id) = typeck_results.type_dependent_def_id(expression.hir_id) {
                        self.record_def(def_id, expression.span);
                    }
                }
                _ => {}
            }
        }
        intravisit::walk_expr(self, expression);
    }

    fn visit_pat(&mut self, pattern: &'tcx Pat<'tcx>) {
        if let Some(typeck_results) = self.typeck_results {
            match pattern.kind {
                PatKind::Struct(ref qpath, fields, _) => {
                    let resolution = typeck_results.qpath_res(qpath, pattern.hir_id);
                    if matches!(qpath, QPath::TypeRelative(..)) {
                        self.record(resolution, pattern.span);
                    }
                    if let Some(adt) = typeck_results.pat_ty(pattern).ty_adt_def() {
                        let variant = adt.variant_of_res(resolution);
                        for field in fields {
                            self.record_field(variant, field.hir_id, field.ident.span);
                        }
                    }
                }
                PatKind::TupleStruct(ref qpath, ..) => {
                    let resolution = typeck_results.qpath_res(qpath, pattern.hir_id);
                    if matches!(qpath, QPath::TypeRelative(..)) {
                        self.record(resolution, pattern.span);
                    }
                    if let Some(adt) = typeck_results.pat_ty(pattern).ty_adt_def() {
                        self.record_tuple_pattern_fields(
                            adt.variant_of_res(resolution),
                            pattern.span,
                        );
                    }
                }
                _ => {}
            }
        }
        intravisit::walk_pat(self, pattern);
    }

    fn visit_pat_expr(&mut self, expression: &'tcx PatExpr<'tcx>) {
        if let Some(typeck_results) = self.typeck_results
            && let PatExprKind::Path(ref qpath @ QPath::TypeRelative(..)) = expression.kind
        {
            self.record(
                typeck_results.qpath_res(qpath, expression.hir_id),
                expression.span,
            );
        }
        intravisit::walk_pat_expr(self, expression);
    }
}

fn collect_references(
    tcx: TyCtxt<'_>,
    generated_roots: &[GeneratedRoot],
    local_definitions: &[LocalDefId],
    emitted_definitions: &HashSet<LocalDefId>,
    facts: &mut CollectedFacts,
) {
    for local_def_id in tcx.hir_body_owners() {
        let Some(source) = nearest_emitted_definition(tcx, local_def_id, emitted_definitions)
        else {
            continue;
        };
        let body = tcx.hir_body_owned_by(local_def_id);
        let mut visitor =
            ReferenceVisitor::new(tcx, generated_roots, Some(tcx.typeck_body(body.id())));
        visitor.visit_body(body);
        let references = visitor.finish();
        if tcx.def_kind(local_def_id) == DefKind::GlobalAsm {
            for target in references
                .iter()
                .filter_map(|reference| reference.target.as_local())
                .filter(|target| emitted_definitions.contains(target))
            {
                push_root(
                    tcx,
                    facts,
                    target,
                    RootKind::Conservative,
                    "global assembly retains this symbol",
                );
            }
        }
        facts.push_resolved_references(tcx, source.to_def_id(), ReferenceKind::Body, &references);
    }

    for local_def_id in emitted_definitions.iter().copied() {
        let kind = if tcx.def_kind(local_def_id) == DefKind::Use {
            ReferenceKind::Reexport
        } else {
            ReferenceKind::Interface
        };
        let mut visitor = ReferenceVisitor::new(tcx, generated_roots, None);
        visitor.visit_node(tcx.hir_node_by_def_id(local_def_id));
        let references = visitor.finish();
        facts.push_resolved_references(tcx, local_def_id.to_def_id(), kind, &references);

        let parent = tcx.opt_local_parent(local_def_id);
        #[cfg(rot_trait_item_of)]
        let trait_item = tcx.trait_item_of(local_def_id.to_def_id());
        #[cfg(not(rot_trait_item_of))]
        let trait_item = tcx
            .opt_associated_item(local_def_id.to_def_id())
            .and_then(|item| item.trait_item_def_id);
        if let Some(trait_item) = trait_item
            && parent.is_some_and(|parent| {
                matches!(tcx.def_kind(parent), DefKind::Impl { of_trait: true })
            })
        {
            facts.push_reference(
                tcx,
                local_def_id.to_def_id(),
                trait_item,
                ReferenceKind::Interface,
                definition_reference_span(tcx, local_def_id, generated_roots),
            );
            facts.push_resolved_references(
                tcx,
                local_def_id.to_def_id(),
                ReferenceKind::VisibilityRequirement,
                &references,
            );
        }

        if let Some(parent) = parent {
            match tcx.def_kind(parent) {
                DefKind::Trait
                    if matches!(
                        tcx.def_kind(local_def_id),
                        DefKind::AssocFn | DefKind::AssocConst { .. } | DefKind::AssocTy
                    ) =>
                {
                    facts.push_reference(
                        tcx,
                        local_def_id.to_def_id(),
                        parent.to_def_id(),
                        ReferenceKind::Interface,
                        definition_reference_span(tcx, local_def_id, generated_roots),
                    );
                    facts.push_resolved_references(
                        tcx,
                        parent.to_def_id(),
                        ReferenceKind::VisibilityRequirement,
                        &references,
                    );
                }
                DefKind::Impl { of_trait: false }
                    if matches!(
                        tcx.def_kind(local_def_id),
                        DefKind::AssocFn | DefKind::AssocConst { .. } | DefKind::AssocTy
                    ) =>
                {
                    let self_type = tcx.type_of(parent).instantiate_identity();
                    #[cfg(rot_type_of_unnormalized)]
                    let self_type = self_type.skip_norm_wip();
                    if let ty::Adt(adt, _) = self_type.kind() {
                        facts.push_reference(
                            tcx,
                            local_def_id.to_def_id(),
                            adt.did(),
                            ReferenceKind::Interface,
                            definition_reference_span(tcx, local_def_id, generated_roots),
                        );
                    }
                }
                _ => {}
            }
        }

        if let Some(module) = enclosing_module(tcx, local_def_id) {
            facts.push_reference(
                tcx,
                local_def_id.to_def_id(),
                module.to_def_id(),
                ReferenceKind::VisibilityParent,
                definition_reference_span(tcx, local_def_id, generated_roots),
            );
        }

        if matches!(
            tcx.def_kind(local_def_id),
            DefKind::Field | DefKind::Variant
        ) && let Some(adt) = containing_adt(tcx, local_def_id)
        {
            facts.push_reference(
                tcx,
                local_def_id.to_def_id(),
                adt.to_def_id(),
                ReferenceKind::Interface,
                definition_reference_span(tcx, local_def_id, generated_roots),
            );
        }
    }

    collect_reference_roots(tcx, local_definitions, emitted_definitions, facts);
}

fn definition_reference_span(
    tcx: TyCtxt<'_>,
    local_def_id: LocalDefId,
    generated_roots: &[GeneratedRoot],
) -> Option<LocatedSpan> {
    let attribution = span_attribution(tcx, tcx.def_span(local_def_id), generated_roots);
    attribution.span.or(attribution.callsite)
}

fn enclosing_module(tcx: TyCtxt<'_>, local_def_id: LocalDefId) -> Option<LocalDefId> {
    let parent = tcx.opt_local_parent(local_def_id)?;
    if parent != CRATE_DEF_ID && tcx.def_kind(parent) == DefKind::Mod {
        return Some(parent);
    }
    if tcx.def_kind(parent) != DefKind::ForeignMod {
        return None;
    }
    let module = tcx.opt_local_parent(parent)?;
    (module != CRATE_DEF_ID && tcx.def_kind(module) == DefKind::Mod).then_some(module)
}

fn containing_adt(tcx: TyCtxt<'_>, mut local_def_id: LocalDefId) -> Option<LocalDefId> {
    while let Some(parent) = tcx.opt_local_parent(local_def_id) {
        if matches!(
            tcx.def_kind(parent),
            DefKind::Struct | DefKind::Union | DefKind::Enum
        ) {
            return Some(parent);
        }
        local_def_id = parent;
    }
    None
}

fn collect_reference_roots(
    tcx: TyCtxt<'_>,
    local_definitions: &[LocalDefId],
    emitted_definitions: &HashSet<LocalDefId>,
    facts: &mut CollectedFacts,
) {
    if let Some((entry, _)) = tcx.entry_fn(())
        && entry
            .as_local()
            .is_some_and(|entry| emitted_definitions.contains(&entry))
    {
        facts.roots.push(Root {
            id: FactId(String::new()),
            definition: compiler_id(tcx, entry),
            kind: RootKind::EntryPoint,
            reason: "rustc entry point".to_owned(),
        });
    }
    if tcx.sess.opts.test {
        for local_def_id in emitted_definitions.iter().copied().filter(|local_def_id| {
            tcx.def_kind(*local_def_id) == DefKind::Fn
                && tcx.opt_local_parent(*local_def_id) == Some(CRATE_DEF_ID)
                && tcx
                    .opt_item_name(local_def_id.to_def_id())
                    .is_some_and(|name| name.as_str() == "main")
                && expansion_origin(tcx.def_span(*local_def_id))
                    == ExpansionOrigin::BuiltinDesugaring
        }) {
            push_root(
                tcx,
                facts,
                local_def_id,
                RootKind::EntryPoint,
                "rustc test harness entry point",
            );
        }
    }

    for local_def_id in tcx.hir_body_owners().filter(|local_def_id| {
        matches!(
            tcx.def_kind(*local_def_id),
            DefKind::AssocFn | DefKind::AssocConst { .. }
        ) && matches!(
            tcx.def_kind(tcx.local_parent(*local_def_id)),
            DefKind::Trait | DefKind::Impl { of_trait: true }
        )
    }) {
        push_root(
            tcx,
            facts,
            local_def_id,
            RootKind::Conservative,
            "trait-associated code may execute through indirect dispatch",
        );
    }

    for local_def_id in emitted_definitions.iter().copied() {
        let hir_id = tcx.local_def_id_to_hir_id(local_def_id);
        #[cfg(rot_lint_level_spec)]
        let dead_code_level = tcx.lint_level_spec_at_node(DEAD_CODE, hir_id).level();
        #[cfg(not(rot_lint_level_spec))]
        let dead_code_level = tcx.lint_level_at_node(DEAD_CODE, hir_id).level;
        if dead_code_level == LintLevel::Allow {
            push_root(
                tcx,
                facts,
                local_def_id,
                RootKind::Conservative,
                "dead_code is explicitly allowed for this definition",
            );
        }

        if matches!(
            tcx.def_kind(local_def_id),
            DefKind::Fn | DefKind::AssocFn | DefKind::Static { .. }
        ) {
            let attributes = tcx.codegen_fn_attrs(local_def_id.to_def_id());
            #[cfg(rot_codegen_symbol_name)]
            let has_exported_name = attributes.symbol_name.is_some();
            #[cfg(not(rot_codegen_symbol_name))]
            let has_exported_name = attributes.export_name.is_some();
            if attributes.flags.intersects(
                CodegenFnAttrFlags::NO_MANGLE
                    | CodegenFnAttrFlags::USED_COMPILER
                    | CodegenFnAttrFlags::USED_LINKER,
            ) || has_exported_name
            {
                push_root(
                    tcx,
                    facts,
                    local_def_id,
                    RootKind::Conservative,
                    "codegen attributes make this definition externally addressable or retained",
                );
            }
        }
    }

    let local_by_id = local_definitions
        .iter()
        .copied()
        .map(|local_def_id| (compiler_id(tcx, local_def_id.to_def_id()), local_def_id))
        .collect::<BTreeMap<_, _>>();
    let directly_exported_macros = facts
        .bindings
        .iter()
        .filter(|binding| {
            binding.namespace == BindingNamespace::Macro
                && binding.exposure == BindingExposure::Direct
        })
        .filter_map(|binding| local_by_id.get(&binding.target).copied())
        .filter(|local_def_id| matches!(tcx.def_kind(*local_def_id), DefKind::Macro(..)))
        .collect::<HashSet<_>>();
    for macro_definition in directly_exported_macros {
        push_root(
            tcx,
            facts,
            macro_definition,
            RootKind::RequiredPublic,
            "a directly exported macro may be invoked by a selected consumer",
        );
        if let Some(module) = enclosing_module(tcx, macro_definition) {
            push_root(
                tcx,
                facts,
                module,
                RootKind::RequiredPublic,
                "a directly exported macro requires its containing namespace",
            );
        }
    }
    for associated_type in emitted_definitions.iter().copied().filter(|local_def_id| {
        tcx.def_kind(*local_def_id) == DefKind::AssocTy
            && matches!(
                tcx.opt_local_parent(*local_def_id)
                    .map(|parent| tcx.def_kind(parent)),
                Some(DefKind::Impl { of_trait: false })
            )
            && tcx.visibility(local_def_id.to_def_id()).is_public()
            && externally_reachable(tcx, *local_def_id)
    }) {
        push_root(
            tcx,
            facts,
            associated_type,
            RootKind::RequiredPublic,
            "public inherent associated types are conservatively retained",
        );
    }
    let type_aliases = emitted_definitions
        .iter()
        .copied()
        .filter(|local_def_id| tcx.def_kind(*local_def_id) == DefKind::TyAlias)
        .map(|local_def_id| compiler_id(tcx, local_def_id.to_def_id()))
        .collect::<HashSet<_>>();
    let interface_targets = facts
        .references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Interface)
        .fold(
            BTreeMap::<CompilerDefId, Vec<CompilerDefId>>::new(),
            |mut targets, reference| {
                targets
                    .entry(reference.from)
                    .or_default()
                    .push(reference.to);
                targets
            },
        );
    let trait_impl_sources = emitted_definitions
        .iter()
        .copied()
        .filter(|local_def_id| {
            matches!(
                tcx.def_kind(*local_def_id),
                DefKind::AssocFn | DefKind::AssocConst { .. } | DefKind::AssocTy
            ) && matches!(
                tcx.def_kind(tcx.local_parent(*local_def_id)),
                DefKind::Impl { of_trait: true }
            ) && externally_reachable(tcx, tcx.local_parent(*local_def_id))
        })
        .map(|local_def_id| compiler_id(tcx, local_def_id.to_def_id()))
        .collect::<BTreeSet<_>>();
    let mut pending_required = facts
        .references
        .iter()
        .filter(|reference| {
            reference.kind == ReferenceKind::VisibilityRequirement
                && trait_impl_sources.contains(&reference.from)
        })
        .map(|reference| reference.to)
        .collect::<Vec<_>>();
    let mut examined = BTreeSet::new();
    while let Some(target) = pending_required.pop() {
        if !examined.insert(target) {
            continue;
        }
        if type_aliases.contains(&target) {
            pending_required.extend(
                interface_targets
                    .get(&target)
                    .into_iter()
                    .flatten()
                    .copied(),
            );
        } else if let Some(local_def_id) = local_by_id.get(&target)
            && emitted_definitions.contains(local_def_id)
        {
            push_root(
                tcx,
                facts,
                *local_def_id,
                RootKind::RequiredPublic,
                "reachable trait implementation exposes this type",
            );
        }
    }

    let public_imports = emitted_definitions
        .iter()
        .copied()
        .filter(|local_def_id| {
            matches!(
                tcx.def_kind(*local_def_id),
                DefKind::Use | DefKind::ExternCrate
            ) && tcx.visibility(local_def_id.to_def_id()).is_public()
                && tcx.effective_visibilities(()).is_exported(*local_def_id)
        })
        .collect::<Vec<_>>();
    let public_import_ids = public_imports
        .iter()
        .copied()
        .map(|local_def_id| compiler_id(tcx, local_def_id.to_def_id()))
        .collect::<BTreeSet<_>>();
    let reexport_targets = facts
        .bindings
        .iter()
        .filter(|binding| {
            binding
                .exposing_import
                .is_some_and(|import| public_import_ids.contains(&import))
                && matches!(
                    binding.exposure,
                    BindingExposure::SingleReexport
                        | BindingExposure::ExternCrate
                        | BindingExposure::MacroUse
                        | BindingExposure::MacroExport
                )
        })
        .filter_map(|binding| local_by_id.get(&binding.target).copied())
        .collect::<HashSet<_>>();
    for target in reexport_targets {
        if emitted_definitions.contains(&target) {
            push_root(
                tcx,
                facts,
                target,
                RootKind::RequiredPublic,
                "a public reexport requires this local target to remain public",
            );
        }
    }
    for module in public_imports
        .into_iter()
        .filter_map(|local_def_id| enclosing_module(tcx, local_def_id))
    {
        push_root(
            tcx,
            facts,
            module,
            RootKind::RequiredPublic,
            "a public reexport requires its containing namespace to remain public",
        );
    }

    if tcx.crate_types().contains(&CrateType::ProcMacro) {
        for local_def_id in emitted_definitions.iter().copied().filter(|local_def_id| {
            matches!(tcx.def_kind(*local_def_id), DefKind::Macro(..))
                && tcx.visibility(local_def_id.to_def_id()).is_public()
        }) {
            push_root(
                tcx,
                facts,
                local_def_id,
                RootKind::RequiredPublic,
                "rustc requires proc-macro entry points to remain public",
            );
        }
    }
}

fn nearest_emitted_definition(
    tcx: TyCtxt<'_>,
    mut local_def_id: LocalDefId,
    emitted_definitions: &HashSet<LocalDefId>,
) -> Option<LocalDefId> {
    loop {
        if emitted_definitions.contains(&local_def_id) {
            return Some(local_def_id);
        }
        local_def_id = tcx.opt_local_parent(local_def_id)?;
    }
}

fn push_root(
    tcx: TyCtxt<'_>,
    facts: &mut CollectedFacts,
    local_def_id: LocalDefId,
    kind: RootKind,
    reason: &str,
) {
    facts.roots.push(Root {
        id: FactId(String::new()),
        definition: compiler_id(tcx, local_def_id.to_def_id()),
        kind,
        reason: reason.to_owned(),
    });
}

fn sort_and_identify_facts(facts: &mut CollectedFacts) {
    facts.definitions.sort_by(|left, right| {
        (
            left.compiler_id,
            left.kind,
            &left.definition_path,
            left.parent,
            &left.name,
            &left.span,
            &left.attribution_callsite,
            left.expansion_origin,
        )
            .cmp(&(
                right.compiler_id,
                right.kind,
                &right.definition_path,
                right.parent,
                &right.name,
                &right.span,
                &right.attribution_callsite,
                right.expansion_origin,
            ))
    });
    for (index, definition) in facts.definitions.iter_mut().enumerate() {
        definition.id = FactId(format!("definition-{index}"));
    }

    facts.roots.sort_by(|left, right| {
        (left.definition, left.kind, &left.reason).cmp(&(
            right.definition,
            right.kind,
            &right.reason,
        ))
    });
    facts.roots.dedup_by(|left, right| {
        left.definition == right.definition
            && left.kind == right.kind
            && left.reason == right.reason
    });
    for (index, root) in facts.roots.iter_mut().enumerate() {
        root.id = FactId(format!("root-{index}"));
    }

    facts.references.sort_by(|left, right| {
        (
            left.from,
            left.to,
            left.kind,
            left.span.is_none(),
            &left.span,
        )
            .cmp(&(
                right.from,
                right.to,
                right.kind,
                right.span.is_none(),
                &right.span,
            ))
    });
    facts.references.dedup_by(|left, right| {
        left.from == right.from && left.to == right.to && left.kind == right.kind
    });
    for (index, reference) in facts.references.iter_mut().enumerate() {
        reference.id = FactId(format!("reference-{index}"));
    }
}

fn collect_visibility_bindings(
    tcx: TyCtxt<'_>,
    local_definitions: &[LocalDefId],
    facts: &mut CollectedFacts,
) {
    for module in local_definitions.iter().copied().filter(|definition| {
        tcx.def_kind(*definition) == DefKind::Mod
            && (*definition == CRATE_DEF_ID || externally_reachable(tcx, *definition))
    }) {
        for child in tcx.module_children_local(module) {
            if !child.vis.is_public() {
                continue;
            }
            let Res::Def(kind, target) = child.res else {
                continue;
            };
            if target
                .as_local()
                .is_some_and(|target| !externally_reachable(tcx, target))
            {
                continue;
            }
            let Some(namespace) = binding_namespace(kind) else {
                continue;
            };
            let (exposure, exposing_import) = binding_exposure(tcx, child.reexport_chain.first());
            facts.bindings.push(VisibilityBinding {
                target: compiler_id(tcx, target),
                namespace,
                exposure,
                exposing_import,
            });
        }
    }
}

fn compiler_id(tcx: TyCtxt<'_>, def_id: DefId) -> CompilerDefId {
    let hash = tcx.def_path_hash(def_id);
    CompilerDefId {
        stable_crate_id: hash.stable_crate_id().as_u64(),
        local_hash: hash.local_hash().as_u64(),
    }
}

fn emitted_parent(
    tcx: TyCtxt<'_>,
    local_def_id: LocalDefId,
    emitted_definitions: &HashSet<LocalDefId>,
) -> Option<CompilerDefId> {
    let mut parent = tcx.opt_local_parent(local_def_id)?;
    while !emitted_definitions.contains(&parent) {
        parent = tcx.opt_local_parent(parent)?;
    }
    Some(compiler_id(tcx, parent.to_def_id()))
}

fn definition_kind(kind: DefKind, local_def_id: LocalDefId) -> Option<DefinitionKind> {
    match kind {
        DefKind::Mod if local_def_id == CRATE_DEF_ID => Some(DefinitionKind::Crate),
        DefKind::Mod => Some(DefinitionKind::Module),
        DefKind::Struct => Some(DefinitionKind::Struct),
        DefKind::Union => Some(DefinitionKind::Union),
        DefKind::Enum => Some(DefinitionKind::Enum),
        DefKind::Variant => Some(DefinitionKind::Variant),
        DefKind::Trait => Some(DefinitionKind::Trait),
        DefKind::TyAlias => Some(DefinitionKind::TypeAlias),
        DefKind::ForeignTy => Some(DefinitionKind::ForeignType),
        DefKind::TraitAlias => Some(DefinitionKind::TraitAlias),
        DefKind::AssocTy => Some(DefinitionKind::AssociatedType),
        DefKind::Fn => Some(DefinitionKind::Function),
        DefKind::Const { .. } => Some(DefinitionKind::Constant),
        DefKind::Static { nested: false, .. } => Some(DefinitionKind::Static),
        DefKind::Ctor(..) => Some(DefinitionKind::Constructor),
        DefKind::AssocFn => Some(DefinitionKind::AssociatedFunction),
        DefKind::AssocConst { .. } => Some(DefinitionKind::AssociatedConstant),
        DefKind::Macro(..) => Some(DefinitionKind::Macro),
        DefKind::ExternCrate => Some(DefinitionKind::ExternCrate),
        DefKind::Use => Some(DefinitionKind::Import),
        DefKind::ForeignMod => Some(DefinitionKind::ForeignModule),
        DefKind::OpaqueTy => Some(DefinitionKind::OpaqueType),
        DefKind::Field => Some(DefinitionKind::Field),
        DefKind::Impl { .. } => Some(DefinitionKind::Implementation),
        DefKind::TyParam
        | DefKind::ConstParam
        | DefKind::LifetimeParam
        | DefKind::AnonConst
        | DefKind::GlobalAsm
        | DefKind::Closure
        | DefKind::SyntheticCoroutineBody
        | DefKind::Static { nested: true, .. } => None,
        #[cfg(rot_inline_const_def_kind)]
        DefKind::InlineConst => None,
        #[cfg(rot_test_binder_constraints)]
        DefKind::TestBinderConstraints => None,
    }
}

fn nominal_visibility(tcx: TyCtxt<'_>, def_id: DefId) -> NominalVisibility {
    match tcx.visibility(def_id) {
        Visibility::Public => NominalVisibility::Public,
        #[cfg(rot_local_module_visibility)]
        Visibility::Restricted(module) => {
            NominalVisibility::Restricted(compiler_id(tcx, module.to_def_id()))
        }
        #[cfg(not(rot_local_module_visibility))]
        Visibility::Restricted(module) => NominalVisibility::Restricted(compiler_id(tcx, module)),
    }
}

fn visibility_editable(tcx: TyCtxt<'_>, local_def_id: LocalDefId) -> bool {
    match tcx.def_kind(local_def_id) {
        DefKind::AssocFn | DefKind::AssocConst { .. } | DefKind::AssocTy => {
            tcx.opt_local_parent(local_def_id).is_some_and(|parent| {
                matches!(tcx.def_kind(parent), DefKind::Impl { of_trait: false })
            })
        }
        DefKind::Mod => local_def_id != CRATE_DEF_ID,
        DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Trait
        | DefKind::TyAlias
        | DefKind::TraitAlias
        | DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::Static { nested: false, .. }
        | DefKind::ExternCrate
        | DefKind::Use => true,
        DefKind::Field => containing_adt(tcx, local_def_id)
            .is_some_and(|adt| !matches!(tcx.def_kind(adt), DefKind::Enum)),
        _ => false,
    }
}

fn externally_reachable(tcx: TyCtxt<'_>, local_def_id: LocalDefId) -> bool {
    tcx.effective_visibilities(())
        .public_at_level(local_def_id)
        .is_some()
}

fn binding_namespace(kind: DefKind) -> Option<BindingNamespace> {
    match kind {
        DefKind::Macro(..) => Some(BindingNamespace::Macro),
        DefKind::Fn
        | DefKind::Const { .. }
        | DefKind::Static { .. }
        | DefKind::Ctor(..)
        | DefKind::AssocFn
        | DefKind::AssocConst { .. } => Some(BindingNamespace::Value),
        DefKind::Mod
        | DefKind::Struct
        | DefKind::Union
        | DefKind::Enum
        | DefKind::Variant
        | DefKind::Trait
        | DefKind::TyAlias
        | DefKind::ForeignTy
        | DefKind::TraitAlias
        | DefKind::AssocTy
        | DefKind::ExternCrate
        | DefKind::ForeignMod
        | DefKind::OpaqueTy => Some(BindingNamespace::Type),
        DefKind::TyParam
        | DefKind::ConstParam
        | DefKind::Use
        | DefKind::Field
        | DefKind::LifetimeParam
        | DefKind::AnonConst
        | DefKind::GlobalAsm
        | DefKind::Impl { .. }
        | DefKind::Closure
        | DefKind::SyntheticCoroutineBody => None,
        #[cfg(rot_inline_const_def_kind)]
        DefKind::InlineConst => None,
        #[cfg(rot_test_binder_constraints)]
        DefKind::TestBinderConstraints => None,
    }
}

fn binding_exposure(
    tcx: TyCtxt<'_>,
    reexport: Option<&Reexport>,
) -> (BindingExposure, Option<CompilerDefId>) {
    match reexport {
        None => (BindingExposure::Direct, None),
        Some(Reexport::Single(import)) => (
            BindingExposure::SingleReexport,
            Some(compiler_id(tcx, *import)),
        ),
        Some(Reexport::Glob(import)) => (
            BindingExposure::GlobReexport,
            Some(compiler_id(tcx, *import)),
        ),
        Some(Reexport::ExternCrate(import)) => (
            BindingExposure::ExternCrate,
            Some(compiler_id(tcx, *import)),
        ),
        Some(Reexport::MacroUse) => (BindingExposure::MacroUse, None),
        Some(Reexport::MacroExport) => (BindingExposure::MacroExport, None),
    }
}

fn profile(tcx: TyCtxt<'_>) -> Profile {
    #[cfg(rot_session_config)]
    let session_cfg = &tcx.sess.config;
    #[cfg(not(rot_session_config))]
    let session_cfg = &tcx.sess.psess.config;
    let mut cfg = session_cfg
        .iter()
        .map(|(name, value)| CfgValue {
            name: name.to_string(),
            value: value.map(|value| value.to_string()),
        })
        .collect::<Vec<_>>();
    cfg.sort();
    cfg.dedup();
    let test_mode = tcx.sess.opts.test
        || cfg
            .iter()
            .any(|configuration| configuration.name == "test" && configuration.value.is_none());
    let features = cfg
        .iter()
        .filter(|cfg| cfg.name == "feature")
        .filter_map(|cfg| cfg.value.clone())
        .collect();
    #[cfg(rot_internal_target_features)]
    let target_features = &tcx.sess.internal_target_features;
    #[cfg(not(rot_internal_target_features))]
    let target_features = &tcx.sess.unstable_target_features;
    let mut target_features = target_features
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    target_features.sort();
    target_features.dedup();

    Profile {
        host_triple: config::host_tuple().to_owned(),
        target_triple: tcx.sess.opts.target_triple.to_string(),
        test_mode,
        cfg,
        features,
        codegen: CodegenProfile {
            optimization: match tcx.sess.opts.optimize {
                OptLevel::No => OptimizationLevel::None,
                OptLevel::Less => OptimizationLevel::Less,
                OptLevel::More => OptimizationLevel::More,
                OptLevel::Aggressive => OptimizationLevel::Aggressive,
                OptLevel::Size => OptimizationLevel::Size,
                OptLevel::SizeMin => OptimizationLevel::SizeMin,
            },
            panic: match tcx.sess.panic_strategy() {
                RustcPanicStrategy::Unwind => PanicStrategy::Unwind,
                RustcPanicStrategy::Abort => PanicStrategy::Abort,
                #[cfg(rot_immediate_abort)]
                RustcPanicStrategy::ImmediateAbort => PanicStrategy::ImmediateAbort,
            },
            debug_assertions: tcx.sess.opts.debug_assertions,
            overflow_checks: tcx.sess.overflow_checks(),
            codegen_units: tcx.sess.codegen_units().as_usize(),
            target_cpu: tcx
                .sess
                .opts
                .cg
                .target_cpu
                .clone()
                .unwrap_or_else(|| tcx.sess.target.options.cpu.to_string()),
            target_features,
        },
    }
}

fn span_attribution(
    tcx: TyCtxt<'_>,
    span: Span,
    generated_roots: &[GeneratedRoot],
) -> SpanAttribution {
    let origin = expansion_origin(span);
    let raw = source_span(tcx, span, generated_roots);
    let callsite = (origin != ExpansionOrigin::Authored)
        .then(|| source_span(tcx, span.source_callsite(), generated_roots))
        .flatten();
    let raw_is_trustworthy = origin == ExpansionOrigin::Authored
        || raw.as_ref().is_some_and(|raw| {
            raw.source.generated
                || callsite
                    .as_ref()
                    .is_none_or(|callsite| callsite.source.key != raw.source.key)
        });

    SpanAttribution {
        span: raw_is_trustworthy.then_some(raw).flatten(),
        callsite,
        origin,
    }
}

fn expansion_origin(span: Span) -> ExpansionOrigin {
    if span.ctxt().is_root() {
        return ExpansionOrigin::Authored;
    }

    let mut current = span;
    let mut builtin_desugaring = false;
    while !current.ctxt().is_root() {
        let expansion = current.ctxt().outer_expn_data();
        match expansion.kind {
            ExpnKind::Macro(..) => {
                let origin = if expansion.macro_def_id.is_some_and(DefId::is_local) {
                    ExpansionOrigin::LocalMacro
                } else {
                    ExpansionOrigin::ExternalMacro
                };
                return origin;
            }
            ExpnKind::AstPass(..) | ExpnKind::Desugaring(..) => builtin_desugaring = true,
            ExpnKind::Root => {}
        }
        current = expansion.call_site;
    }

    if builtin_desugaring {
        ExpansionOrigin::BuiltinDesugaring
    } else {
        ExpansionOrigin::Authored
    }
}

fn source_span(
    tcx: TyCtxt<'_>,
    span: Span,
    generated_roots: &[GeneratedRoot],
) -> Option<LocatedSpan> {
    if span.is_dummy() {
        return None;
    }
    let source_map = tcx.sess.source_map();
    let file = source_map.lookup_source_file(span.lo());
    if file.cnum != LOCAL_CRATE || span.hi() > file.end_position() {
        return None;
    }
    let (local_path, remapped_path) = match &file.name {
        FileName::Real(name) => (
            name.local_path().map(canonical_string),
            file.name.prefer_remapped_unconditionally().to_string(),
        ),
        _ => return None,
    };
    let physical_path = local_path.as_deref().unwrap_or(&remapped_path);
    let rust_source = Path::new(physical_path)
        .extension()
        .is_some_and(|extension| extension == "rs");
    if !rust_source {
        return None;
    }

    let source_hash = file.src_hash.to_string();
    let source_hash_algorithm = file.src_hash.kind.to_string();
    let generated_identity = local_path
        .as_deref()
        .and_then(|path| generated_identity(Path::new(path), generated_roots));
    let identity_path = generated_identity.as_deref().unwrap_or(physical_path);
    let key = SourceFileKey(length_prefixed(&[
        "source-v2".to_owned(),
        identity_path.to_owned(),
        source_hash_algorithm.clone(),
        source_hash.clone(),
    ]));
    let source = SourceFile {
        key: key.clone(),
        generated: generated_identity.is_some(),
        local_path,
        remapped_path,
        source_hash_algorithm,
        source_hash,
        byte_len: file
            .original_relative_byte_pos(file.end_position())
            .to_u32(),
    };
    let location = source_map.lookup_char_pos(span.lo());
    let source_span = SourceSpan {
        file: key,
        start: file.original_relative_byte_pos(span.lo()).to_u32(),
        end: file.original_relative_byte_pos(span.hi()).to_u32(),
        line: location.line.try_into().ok()?,
        column: location.col.to_usize().checked_add(1)?.try_into().ok()?,
    };
    Some(LocatedSpan {
        source,
        span: source_span,
    })
}

struct GeneratedRoot {
    label: &'static str,
    path: PathBuf,
}

fn generated_identity(path: &Path, roots: &[GeneratedRoot]) -> Option<String> {
    roots
        .iter()
        .filter_map(|root| {
            path.strip_prefix(&root.path)
                .ok()
                .map(|relative| (root, relative))
        })
        .max_by_key(|(root, _)| root.path.components().count())
        .map(|(root, relative)| {
            format!("<generated:{}>/{}", root.label, relative.to_string_lossy())
        })
}

fn length_prefixed(parts: &[String]) -> String {
    let mut key = String::new();
    for part in parts {
        key.push_str(&part.len().to_string());
        key.push(':');
        key.push_str(part);
    }
    key
}

fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir().unwrap_or_default().join(path)
        }
    })
}

fn canonical_string(path: &Path) -> String {
    canonical_path(path).to_string_lossy().into_owned()
}

fn manifest_is_selected(manifest_dir: Option<&str>, allowlist: Option<&OsStr>) -> bool {
    let (Some(manifest_dir), Some(allowlist)) = (manifest_dir, allowlist) else {
        return false;
    };
    let manifest_dir = canonical_path(Path::new(manifest_dir));
    let selected = env::split_paths(allowlist)
        .filter(|path| !path.as_os_str().is_empty())
        .take(MAX_SELECTED_MANIFEST_DIRS + 1)
        .map(|path| canonical_path(&path))
        .collect::<Vec<_>>();
    selected.len() <= MAX_SELECTED_MANIFEST_DIRS && selected.contains(&manifest_dir)
}

fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 128
        && run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn safe_filename(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(80)
        .collect::<String>();
    if value.is_empty() {
        "unknown".to_owned()
    } else {
        value
    }
}

struct RecordBuffer {
    run_id: RunId,
    invocation_id: InvocationId,
    next_sequence: u64,
    records: Vec<Vec<u8>>,
    bytes: usize,
    limit: usize,
    truncated: bool,
}

impl RecordBuffer {
    fn new(run_id: RunId, invocation_id: InvocationId, limit: usize) -> Self {
        Self {
            run_id,
            invocation_id,
            next_sequence: 0,
            records: Vec::new(),
            bytes: 0,
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, event: Event) -> bool {
        let maximum = self.limit.saturating_sub(TRAILER_RESERVE_BYTES);
        self.push_with_limit(event, maximum)
    }

    fn push_mandatory(&mut self, event: Event) -> bool {
        self.push_with_limit(event, self.limit)
    }

    fn push_with_limit(&mut self, event: Event, limit: usize) -> bool {
        let record = Record {
            protocol_version: PROTOCOL_VERSION,
            run_id: self.run_id.clone(),
            invocation_id: self.invocation_id.clone(),
            sequence: self.next_sequence,
            event,
        };
        let Ok(mut encoded) = serde_json::to_vec(&record) else {
            self.truncated = true;
            return false;
        };
        encoded.push(b'\n');
        if self.bytes.saturating_add(encoded.len()) > limit {
            self.truncated = true;
            return false;
        }
        self.bytes += encoded.len();
        self.next_sequence += 1;
        self.records.push(encoded);
        true
    }

    fn write_atomic(&self, directory: &Path) -> io::Result<PathBuf> {
        let name = safe_filename(&self.invocation_id.0);
        let final_path = directory.join(format!("{name}.jsonl"));
        let temporary_path = directory.join(format!(".{name}.tmp"));
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)?;
        for record in &self.records {
            output.write_all(record)?;
        }
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary_path, &final_path)?;
        sync_directory(directory)?;
        Ok(final_path)
    }
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(directory)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn compiler_identity_matches_the_driver_build() {
        assert_eq!(linked_rustc_version().as_deref(), Some(BUILD_RUSTC_VERSION));
        assert_eq!(
            compiler_identity(),
            CompilerIdentity {
                release: env!("ROT_BUILD_RUSTC_RELEASE").to_owned(),
                commit_hash: env!("ROT_BUILD_RUSTC_COMMIT").to_owned(),
                commit_date: env!("ROT_BUILD_RUSTC_COMMIT_DATE").to_owned(),
                host: env!("ROT_BUILD_RUSTC_HOST").to_owned(),
            }
        );
        assert_eq!(config::host_tuple(), env!("ROT_BUILD_RUSTC_HOST"));
    }

    #[test]
    fn parses_cargo_rustc_invocation_identity() {
        let args = vec![
            "--crate-name".to_owned(),
            "sample".to_owned(),
            "src/lib.rs".to_owned(),
            "--crate-type=lib,rlib".to_owned(),
            "--out-dir".to_owned(),
            "target/check/deps".to_owned(),
            "-C".to_owned(),
            "extra-filename=-1234".to_owned(),
            "-Cmetadata=abcd".to_owned(),
            "--emit=dep-info,metadata".to_owned(),
            "--cfg".to_owned(),
            "feature=\"alpha\"".to_owned(),
            "--test".to_owned(),
            "--target=aarch64-apple-darwin".to_owned(),
        ];

        let invocation = InvocationArgs::parse("/toolchain/bin/rustc".to_owned(), &args);

        assert_eq!(invocation.crate_name, "sample");
        assert_eq!(invocation.artifact.crate_types, ["lib", "rlib"]);
        assert_eq!(invocation.artifact.extra_filename.as_deref(), Some("-1234"));
        assert_eq!(invocation.artifact.metadata.as_deref(), Some("abcd"));
        assert_eq!(invocation.artifact.emit, ["dep-info", "metadata"]);
        assert_eq!(invocation.cfg, ["feature=\"alpha\""]);
        assert!(invocation.test_mode);
        assert_eq!(invocation.target, "aarch64-apple-darwin");
        assert_eq!(invocation.compilation_context, CompilationContext::Target);
    }

    #[test]
    fn explicit_host_target_remains_a_target_context() {
        let base = vec![
            "--crate-name=sample".to_owned(),
            "src/lib.rs".to_owned(),
            "--out-dir=target/check".to_owned(),
        ];
        let host = InvocationArgs::parse("rustc".to_owned(), &base);
        let mut explicit_args = base;
        explicit_args.push(format!("--target={}", config::host_tuple()));
        let explicit = InvocationArgs::parse("rustc".to_owned(), &explicit_args);

        assert_eq!(host.target, explicit.target);
        assert_eq!(host.compilation_context, CompilationContext::Host);
        assert_eq!(explicit.compilation_context, CompilationContext::Target);
        assert_ne!(host.merge_key(), explicit.merge_key());
    }

    #[test]
    fn record_buffer_is_bounded_and_sequences_only_written_records() {
        let mut records = RecordBuffer::new(
            RunId("run".to_owned()),
            InvocationId("invocation".to_owned()),
            TRAILER_RESERVE_BYTES + 300,
        );
        assert!(records.push(Event::Diagnostic(Diagnostic {
            phase: DiagnosticPhase::Analysis,
            severity: DiagnosticSeverity::Warning,
            message: "short".to_owned(),
            span: None,
        })));
        assert!(!records.push(Event::Diagnostic(Diagnostic {
            phase: DiagnosticPhase::Analysis,
            severity: DiagnosticSeverity::Warning,
            message: "x".repeat(1_000),
            span: None,
        })));
        assert!(
            records.push_mandatory(Event::InvocationFinished(InvocationFinished {
                rustc_success: true,
                analysis_reached: true,
            },))
        );

        let decoded = records
            .records
            .iter()
            .map(|record| serde_json::from_slice::<Record>(record).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].sequence, 0);
        assert_eq!(decoded[1].sequence, 1);
        assert!(records.bytes <= records.limit);
    }

    #[test]
    fn atomic_sidecar_has_no_visible_temp_file() {
        let directory = temporary_directory();
        let mut records = RecordBuffer::new(
            RunId("run".to_owned()),
            InvocationId("invocation".to_owned()),
            MAX_SIDECAR_BYTES as usize,
        );
        assert!(records.push(Event::InvocationFinished(InvocationFinished {
            rustc_success: true,
            analysis_reached: true,
        })));

        let path = records.write_atomic(&directory).unwrap();

        assert_eq!(path.file_name().unwrap(), "invocation.jsonl");
        assert!(path.is_file());
        assert!(!directory.join(".invocation.tmp").exists());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn run_ids_and_sidecar_names_reject_path_syntax() {
        assert!(valid_run_id("run-1_a.b"));
        assert!(!valid_run_id("../escape"));
        assert_eq!(safe_filename("crate/name"), "crate_name");
        assert_eq!(safe_filename(""), "unknown");
    }

    #[test]
    fn manifest_allowlist_is_bounded_and_fail_closed() {
        let allowed = temporary_directory();
        let unselected = temporary_directory();
        let allowlist = env::join_paths([&allowed]).unwrap();

        assert!(manifest_is_selected(
            Some(&canonical_string(&allowed)),
            Some(&allowlist)
        ));
        assert!(!manifest_is_selected(
            Some(&canonical_string(&unselected)),
            Some(&allowlist)
        ));
        assert!(!manifest_is_selected(
            Some(&canonical_string(&allowed)),
            None
        ));

        let oversized = env::join_paths(std::iter::repeat_n(
            &allowed,
            MAX_SELECTED_MANIFEST_DIRS + 1,
        ))
        .unwrap();
        assert!(!manifest_is_selected(
            Some(&canonical_string(&allowed)),
            Some(&oversized)
        ));

        fs::remove_dir_all(allowed).unwrap();
        fs::remove_dir_all(unselected).unwrap();
    }

    #[test]
    fn effective_codegen_value_is_the_last_occurrence() {
        let args = vec![
            "-Cmetadata=first".to_owned(),
            "-C".to_owned(),
            "metadata=last".to_owned(),
        ];

        assert_eq!(codegen_value(&args, "metadata").as_deref(), Some("last"));
        assert_eq!(codegen_values(&args, "metadata"), ["first", "last"]);
    }

    #[test]
    fn omitted_crate_type_uses_rustc_bin_default() {
        let args = vec![
            "--crate-name=sample_test".to_owned(),
            "src/lib.rs".to_owned(),
            "--out-dir=target/test".to_owned(),
            "--test".to_owned(),
        ];

        let invocation = InvocationArgs::parse("/toolchain/bin/rustc".to_owned(), &args);

        assert_eq!(invocation.artifact.crate_types, ["bin"]);
        assert!(invocation.test_mode);
    }

    #[test]
    fn bare_test_cfg_marks_harness_free_targets_as_test_mode() {
        let args = vec![
            "--crate-name=sample_test".to_owned(),
            "src/lib.rs".to_owned(),
            "--out-dir=target/test".to_owned(),
            "--cfg".to_owned(),
            "test".to_owned(),
        ];

        let invocation = InvocationArgs::parse("/toolchain/bin/rustc".to_owned(), &args);

        assert!(invocation.test_mode);
    }

    #[test]
    fn visibility_audit_progress_is_atomic() {
        let mut audit = ProductProgress::default();

        assert_eq!(audit.availability(), Availability::Unavailable);
        audit.start();
        assert_eq!(audit.availability(), Availability::Complete);
        audit.reject();

        assert_eq!(audit.availability(), Availability::Partial);
        assert_eq!(
            product_message(&audit).as_deref(),
            Some("compiler facts were truncated by the sidecar limit")
        );
    }

    #[test]
    fn generated_identity_prefers_the_most_specific_root() {
        let build = PathBuf::from("run/build");
        let out = build.join("package/out");
        let roots = vec![
            GeneratedRoot {
                label: "build",
                path: build,
            },
            GeneratedRoot {
                label: "out",
                path: out.clone(),
            },
        ];

        assert_eq!(
            generated_identity(&out.join("generated.rs"), &roots).as_deref(),
            Some("<generated:out>/generated.rs")
        );
        assert_eq!(
            generated_identity(Path::new("workspace/src/lib.rs"), &roots),
            None
        );
    }

    fn temporary_directory() -> PathBuf {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let directory = env::temp_dir().join(format!(
            "rot-rustc-driver-test-{}-{sequence}",
            process::id()
        ));
        fs::create_dir(&directory).unwrap();
        directory
    }
}
