use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{
    cli::{AuditCli, OutputFormat},
    compiler::{
        self,
        api_surface::{
            self, ApiBindingReport, ApiChangeReport, ApiDefinitionReport, ApiDiffReport,
            ApiSurfaceReport, ApiUnitReport,
        },
    },
    model::{
        CompilerReport, Diagnostic, DiagnosticSeverity, ImpactDefinitionReport,
        ImpactProvenanceClass, ImpactReport, ImpactVisibilityDisposition, SelectedPathReport,
        SemanticStatus,
    },
    paths::{containing_directory, portable},
    revision::{Repository, WorkingState, validate_baseline_ref},
    workspace,
};

const AUDIT_SCHEMA_VERSION: u32 = 3;

#[derive(Serialize)]
struct SnapshotOutput<'a> {
    schema_version: u32,
    report_kind: &'static str,
    root: &'a str,
    profile: &'a AuditProfile,
    #[serde(flatten)]
    evidence: &'a CompilerReport,
    diagnostics: &'a [Diagnostic],
}

struct AuditSnapshot {
    root: String,
    profile: AuditProfile,
    report: CompilerReport,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct AuditProfile {
    toolchain: String,
    target: String,
    feature_mode: &'static str,
    requested_features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    forced_cfg: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CompilerIdentity {
    protocol_version: u32,
    driver_version: String,
    rustc_version: String,
    rustc_commit: String,
    rustc_commit_date: String,
    rustc_host: String,
}

impl From<&CompilerReport> for CompilerIdentity {
    fn from(report: &CompilerReport) -> Self {
        Self {
            protocol_version: report.protocol_version,
            driver_version: report.driver_version.clone(),
            rustc_version: report.rustc_version.clone(),
            rustc_commit: report.rustc_commit.clone(),
            rustc_commit_date: report.rustc_commit_date.clone(),
            rustc_host: report.rustc_host.clone(),
        }
    }
}

#[derive(Serialize)]
struct ComparisonOutput {
    schema_version: u32,
    report_kind: &'static str,
    root: String,
    selection: AuditSelection,
    profile: AuditProfile,
    status: SemanticStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    before: ComparisonEndpoint,
    after: ComparisonEndpoint,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_diff: Option<ApiDiffReport>,
}

#[derive(Serialize)]
struct AuditSelection {
    paths: Vec<SelectedPathReport>,
    meaning: &'static str,
}

#[derive(Serialize)]
struct ComparisonEndpoint {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    dirty: Option<bool>,
    status: SemanticStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    compiler: CompilerIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_surface: Option<ApiSurfaceReport>,
    diagnostics: Vec<Diagnostic>,
}

struct ComparisonContext {
    baseline_ref: String,
    baseline_commit: String,
    current_state: WorkingState,
    root: String,
    selection: AuditSelection,
    target: String,
}

pub fn run(cli: AuditCli) -> ExitCode {
    match execute(&cli) {
        Ok(complete) => {
            if complete {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            if !is_broken_pipe(&error) {
                eprintln!("rot-audit: error: {error:#}");
            }
            if is_broken_pipe(&error) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn execute(cli: &AuditCli) -> Result<bool> {
    match cli.baseline.as_deref() {
        Some(baseline) => execute_comparison(cli, baseline),
        None => execute_snapshot(cli),
    }
}

fn execute_snapshot(cli: &AuditCli) -> Result<bool> {
    let snapshot = collect_snapshot(cli)?;
    let status = snapshot.report.status;
    match cli.format {
        OutputFormat::Json => {
            let output = SnapshotOutput {
                schema_version: AUDIT_SCHEMA_VERSION,
                report_kind: "snapshot",
                root: &snapshot.root,
                profile: &snapshot.profile,
                evidence: &snapshot.report,
                diagnostics: &snapshot.diagnostics,
            };
            write_json(&output)?;
        }
        OutputFormat::Table => {
            render_snapshot_table(&snapshot.report, status, snapshot.report.reason.as_deref())?
        }
    }
    render_diagnostics(&snapshot.diagnostics);
    Ok(status == SemanticStatus::Complete
        && snapshot
            .report
            .impact
            .as_ref()
            .is_none_or(|impact| impact.status == SemanticStatus::Complete))
}

fn execute_comparison(cli: &AuditCli, baseline_ref: &str) -> Result<bool> {
    validate_baseline_ref(baseline_ref)?;
    let repository = Repository::discover(&cli.paths)?;
    let baseline_commit = repository.resolve_commit(baseline_ref)?;
    let selection = AuditSelection {
        paths: repository.selection(&cli.paths, false, false)?.paths,
        meaning: "complete Cargo packages selected by PATH",
    };
    let driver = canonical_driver(&cli.driver)?;
    let target = resolve_target_once(cli)?;

    let mut current_cli = cli.clone();
    current_cli.baseline = None;
    current_cli.driver = driver.clone();
    current_cli.target = Some(target.clone());

    let checkout = repository.materialize(&baseline_commit)?;
    let baseline_paths = repository.baseline_paths(&cli.paths, checkout.root())?;
    let mut baseline_cli = current_cli.clone();
    baseline_cli.paths = baseline_paths;

    let state_before = repository.working_state()?;
    let current_snapshot = collect_snapshot(&current_cli)?;
    let state_after = repository.working_state()?;
    ensure_unchanged_working_state(&state_before, &state_after)?;

    // The two compiler runs are intentionally sequential. Each collection
    // creates its own isolated Cargo target and protocol artifact directories.
    let mut baseline_snapshot = collect_snapshot(&baseline_cli)?;
    normalize_snapshot_paths(&mut baseline_snapshot, checkout.root(), repository.root());
    let state_at_completion = repository.working_state()?;
    ensure_unchanged_working_state(&state_after, &state_at_completion)?;

    let output = comparison_output(
        ComparisonContext {
            baseline_ref: baseline_ref.to_owned(),
            baseline_commit,
            current_state: state_at_completion,
            root: repository.root().to_string_lossy().into_owned(),
            selection,
            target,
        },
        baseline_snapshot,
        current_snapshot,
    );
    let complete = output.status == SemanticStatus::Complete;
    match cli.format {
        OutputFormat::Json => write_json(&output)?,
        OutputFormat::Table => render_comparison_table(&output)?,
    }
    render_endpoint_diagnostics("baseline", &output.before.diagnostics);
    render_endpoint_diagnostics("working tree", &output.after.diagnostics);
    Ok(complete)
}

fn collect_snapshot(cli: &AuditCli) -> Result<AuditSnapshot> {
    let inventory = workspace::audit_inventory(cli)?;
    let root = inventory.root.to_string_lossy().into_owned();
    let profile = AuditProfile {
        toolchain: cli.toolchain.clone(),
        target: inventory
            .audit_target
            .clone()
            .unwrap_or_else(|| "unknown-target".to_owned()),
        feature_mode: cli.feature_mode(false),
        requested_features: cli.features.clone(),
        all_features: cli.all_features,
        no_default_features: cli.no_default_features,
        forced_cfg: cli.cfg.clone(),
    };
    let mut diagnostics = inventory.diagnostics.clone();
    let outcome = compiler::collect(cli, &inventory);
    diagnostics.extend(outcome.diagnostics);
    sort_diagnostics(&mut diagnostics);
    Ok(AuditSnapshot {
        root,
        profile,
        report: outcome.report,
        diagnostics,
    })
}

fn comparison_output(
    context: ComparisonContext,
    baseline: AuditSnapshot,
    current: AuditSnapshot,
) -> ComparisonOutput {
    let baseline_identity = CompilerIdentity::from(&baseline.report);
    let current_identity = CompilerIdentity::from(&current.report);
    let same_compiler = baseline_identity == current_identity;
    let baseline_complete = baseline.report.status == SemanticStatus::Complete;
    let current_complete = current.report.status == SemanticStatus::Complete;
    let has_surfaces =
        baseline.report.api_surface.is_some() && current.report.api_surface.is_some();
    let api_diff =
        (baseline_complete && current_complete && same_compiler && has_surfaces).then(|| {
            api_surface::compare(
                baseline
                    .report
                    .api_surface
                    .as_ref()
                    .expect("surface presence was checked"),
                current
                    .report
                    .api_surface
                    .as_ref()
                    .expect("surface presence was checked"),
            )
        });
    let status = comparison_status(
        baseline.report.status,
        current.report.status,
        same_compiler,
        has_surfaces,
    );
    let reason = comparison_reason(&baseline, &current, same_compiler, has_surfaces);
    let profile = AuditProfile {
        target: context.target,
        ..current.profile.clone()
    };

    ComparisonOutput {
        schema_version: AUDIT_SCHEMA_VERSION,
        report_kind: "comparison",
        root: context.root,
        selection: context.selection,
        profile,
        status,
        reason,
        before: ComparisonEndpoint {
            kind: "git",
            revision: Some(context.baseline_ref),
            commit: context.baseline_commit,
            dirty: None,
            status: baseline.report.status,
            reason: baseline.report.reason.clone(),
            compiler: baseline_identity,
            api_surface: baseline.report.api_surface,
            diagnostics: baseline.diagnostics,
        },
        after: ComparisonEndpoint {
            kind: "working_tree",
            revision: None,
            commit: context.current_state.commit().to_owned(),
            dirty: Some(context.current_state.dirty()),
            status: current.report.status,
            reason: current.report.reason.clone(),
            compiler: current_identity,
            api_surface: current.report.api_surface,
            diagnostics: current.diagnostics,
        },
        api_diff,
    }
}

fn comparison_status(
    baseline: SemanticStatus,
    current: SemanticStatus,
    same_compiler: bool,
    has_surfaces: bool,
) -> SemanticStatus {
    if baseline == SemanticStatus::Complete
        && current == SemanticStatus::Complete
        && same_compiler
        && has_surfaces
    {
        SemanticStatus::Complete
    } else if baseline == SemanticStatus::Unavailable
        || current == SemanticStatus::Unavailable
        || !same_compiler
    {
        SemanticStatus::Unavailable
    } else {
        SemanticStatus::Partial
    }
}

fn comparison_reason(
    baseline: &AuditSnapshot,
    current: &AuditSnapshot,
    same_compiler: bool,
    has_surfaces: bool,
) -> Option<String> {
    let mut reasons = Vec::new();
    if baseline.report.status != SemanticStatus::Complete {
        reasons.push(format!(
            "baseline evidence is {}{}",
            status_name(baseline.report.status),
            baseline
                .report
                .reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(": {reason}")),
        ));
    }
    if current.report.status != SemanticStatus::Complete {
        reasons.push(format!(
            "working-tree evidence is {}{}",
            status_name(current.report.status),
            current
                .report
                .reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(": {reason}")),
        ));
    }
    if !same_compiler {
        reasons.push(
            "endpoint compiler identity differs; protocol, driver, and rustc must match exactly"
                .to_owned(),
        );
    }
    if !has_surfaces {
        reasons.push("one or both endpoints have no complete public API surface".to_owned());
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

fn canonical_driver(driver: &Path) -> Result<PathBuf> {
    if !driver.is_file() {
        bail!("compiler driver {} does not exist", driver.display());
    }
    fs::canonicalize(driver)
        .with_context(|| format!("cannot resolve compiler driver {}", driver.display()))
}

fn resolve_target_once(cli: &AuditCli) -> Result<String> {
    let first = cli.paths.first().context("at least one path is required")?;
    let first = fs::canonicalize(first)
        .with_context(|| format!("cannot resolve input path {}", first.display()))?;
    compiler::effective_target(cli, containing_directory(&first))
}

fn ensure_unchanged_working_state(before: &WorkingState, after: &WorkingState) -> Result<()> {
    if before == after {
        return Ok(());
    }
    if before.commit() != after.commit() {
        bail!(
            "working-tree HEAD changed during the current compiler audit ({} -> {}); rerun against a stable checkout",
            short_commit(before.commit()),
            short_commit(after.commit()),
        );
    }
    bail!(
        "working-tree files changed during the current compiler audit; rerun after the checkout is stable"
    )
}

fn normalize_snapshot_paths(
    snapshot: &mut AuditSnapshot,
    physical_root: &Path,
    logical_root: &Path,
) {
    let physical = physical_root.to_string_lossy();
    let logical = logical_root.to_string_lossy();
    snapshot.root = logical.to_string();
    if let Some(reason) = &mut snapshot.report.reason {
        *reason = reason.replace(physical.as_ref(), logical.as_ref());
    }
    for diagnostic in &mut snapshot.diagnostics {
        diagnostic.message = diagnostic
            .message
            .replace(physical.as_ref(), logical.as_ref());
        if let Some(path) = &mut diagnostic.path {
            let path_value = Path::new(path);
            if let Ok(relative) = path_value.strip_prefix(physical_root) {
                *path = portable(relative);
            } else if let Ok(relative) = path_value.strip_prefix(logical_root) {
                *path = portable(relative);
            } else {
                *path = path.replace(physical.as_ref(), logical.as_ref());
            }
        }
    }
    if let Some(surface) = &mut snapshot.report.api_surface {
        for definition in &mut surface.definitions {
            if let Some(span) = &mut definition.span {
                normalize_api_span_path(span, physical_root, logical_root);
            }
            if let Some(span) = &mut definition.attribution_callsite {
                normalize_api_span_path(span, physical_root, logical_root);
            }
        }
        for binding in &mut surface.bindings {
            if let Some(span) = &mut binding.span {
                normalize_api_span_path(span, physical_root, logical_root);
            }
        }
    }
    sort_diagnostics(&mut snapshot.diagnostics);
}

fn normalize_api_span_path(
    span: &mut crate::model::CompilerSourceSpanReport,
    physical_root: &Path,
    logical_root: &Path,
) {
    let path = Path::new(&span.path);
    if let Ok(relative) = path.strip_prefix(physical_root) {
        span.path = logical_root
            .join(relative)
            .to_string_lossy()
            .replace('\\', "/");
    }
}

fn sort_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
    diagnostics
        .sort_by(|left, right| (&left.path, &left.message).cmp(&(&right.path, &right.message)));
    diagnostics.dedup_by(|left, right| left.path == right.path && left.message == right.message);
}

fn write_json(output: &impl Serialize) -> Result<()> {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    serde_json::to_writer(&mut writer, output).context("cannot serialize audit JSON")?;
    writeln!(writer).context("cannot write audit JSON")
}

fn render_snapshot_table(
    report: &CompilerReport,
    status: SemanticStatus,
    reason: Option<&str>,
) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "Compiler audit: {} ({}/{} Cargo invocations correlated)",
        status_name(status),
        report.correlated_invocations,
        report.expected_invocations,
    )?;
    writeln!(
        output,
        "Compiler: {} ({} {} for {})",
        report.rustc_version, report.rustc_commit, report.rustc_commit_date, report.rustc_host,
    )?;
    if let Some(reason) = reason {
        writeln!(output, "Reason: {reason}")?;
    }
    if status != SemanticStatus::Complete {
        return Ok(());
    }
    if let Some(closed_world) = &report.closed_world {
        writeln!(output, "Scope: {}", closed_world.scope)?;
        writeln!(
            output,
            "Evidence excludes: {}",
            closed_world.evidence_exclusions.join(", ")
        )?;
    }
    let required = report
        .required_visibility
        .as_ref()
        .map_or(0, |required| required.definitions.len());
    let summary = report.closed_world.as_ref().map(|report| &report.summary);
    let can_narrow = summary.map_or(0, |summary| summary.unnecessary_public);
    let dead = summary.map_or(0, |summary| summary.dead_public);
    writeln!(output, "Required public: {required}")?;
    writeln!(output, "Can narrow unrestricted pub: {can_narrow}")?;
    writeln!(output, "Dead public: {dead}")?;
    if let Some(closed_world) = &report.closed_world
        && report.impact.is_none()
    {
        if !closed_world.findings.is_empty() {
            writeln!(output)?;
            writeln!(output, "Findings:")?;
        }
        for finding in &closed_world.findings {
            let location = finding
                .span
                .as_ref()
                .or(finding.attribution_callsite.as_ref())
                .map_or_else(
                    || "<unknown>".to_owned(),
                    |span| format!("{}:{}:{}", span.path, span.line, span.column),
                );
            let liveness = match (finding.production_live, finding.nonproduction_live) {
                (true, true) => "production+nonproduction",
                (true, false) => "production",
                (false, true) => "nonproduction",
                (false, false) => "unreachable",
            };
            writeln!(
                output,
                "{location}  {}  {}  [{liveness}]  {}",
                finding.definition_path, finding.kind, finding.reason,
            )?;
        }
    }
    if let Some(impact) = &report.impact {
        writeln!(output)?;
        render_impact(&mut output, impact)?;
    }
    Ok(())
}

fn render_impact(output: &mut impl Write, impact: &ImpactReport) -> Result<()> {
    writeln!(output, "Impact: {}", status_name(impact.status))?;
    writeln!(
        output,
        "Query: {}:{}",
        impact.query.package, impact.query.definition_path
    )?;
    if let Some(reason) = &impact.reason {
        writeln!(output, "Reason: {reason}")?;
    }
    if impact.status != SemanticStatus::Complete {
        if !impact.candidates.is_empty() {
            writeln!(output, "Candidates:")?;
            for candidate in &impact.candidates {
                writeln!(
                    output,
                    "  {}{}",
                    impact_definition_label(candidate),
                    impact_definition_location(candidate),
                )?;
            }
        }
        return Ok(());
    }

    if let Some(selected) = &impact.selected {
        writeln!(
            output,
            "Declaration: {}{}",
            impact_definition_label(selected),
            impact_definition_location(selected),
        )?;
    }
    if let Some(disposition) = impact.visibility_disposition {
        writeln!(output, "Visibility: {}", disposition_name(disposition))?;
    }
    if let Some(summary) = &impact.summary {
        let mut provenance = Vec::new();
        if summary.production {
            provenance.push("production");
        }
        if summary.nonproduction {
            provenance.push("nonproduction");
        }
        if summary.build_time {
            provenance.push("build-time");
        }
        if summary.public_interface {
            provenance.push("public-interface");
        }
        let provenance = if provenance.is_empty() {
            "none".to_owned()
        } else {
            provenance.join("+")
        };
        writeln!(
            output,
            "Consumers: {} direct relationships, {} transitive declarations [{provenance}]",
            summary.direct_reference_relationships, summary.transitive_consumers,
        )?;
    }

    const HUMAN_DIRECT_LIMIT: usize = 25;
    if !impact.direct_references.is_empty() {
        writeln!(output)?;
        writeln!(output, "Representative direct references:")?;
        let ordered = impact
            .direct_references
            .iter()
            .filter(|reference| reference.consumer.expansion_origin == "authored")
            .chain(
                impact
                    .direct_references
                    .iter()
                    .filter(|reference| reference.consumer.expansion_origin != "authored"),
            );
        for reference in ordered.take(HUMAN_DIRECT_LIMIT) {
            writeln!(
                output,
                "  {}{} --{}--> {} [{}]",
                impact_definition_label(&reference.consumer),
                span_location(reference.representative_span.as_ref()),
                reference.reference_kind,
                impact_definition_label(&reference.dependency),
                provenance_name(reference.provenance.class),
            )?;
        }
        if impact.direct_references.len() > HUMAN_DIRECT_LIMIT {
            writeln!(
                output,
                "  ... {} more relationships; use --format json for the complete list",
                impact.direct_references.len() - HUMAN_DIRECT_LIMIT,
            )?;
        }
    }

    if !impact.witnesses.is_empty() {
        writeln!(output)?;
        writeln!(output, "Shortest root witnesses:")?;
        for witness in &impact.witnesses {
            writeln!(
                output,
                "  [{}] {} ({})",
                provenance_name(witness.provenance.class),
                impact_definition_label(&witness.root),
                witness.root_reason,
            )?;
            for step in &witness.steps {
                writeln!(
                    output,
                    "    --{}{}--> {}",
                    step.reference_kind,
                    span_location(step.representative_span.as_ref()),
                    impact_definition_label(&step.to),
                )?;
            }
        }
    }
    Ok(())
}

fn impact_definition_label(definition: &ImpactDefinitionReport) -> String {
    let origin = if definition.expansion_origin == "authored" {
        String::new()
    } else {
        format!(", {}", definition.expansion_origin)
    };
    format!(
        "{}:{} {} [{}{origin}]",
        definition.package_name,
        definition.target_name,
        definition.definition_path,
        definition.definition_kind,
    )
}

fn impact_definition_location(definition: &ImpactDefinitionReport) -> String {
    span_location(
        definition
            .span
            .as_ref()
            .or(definition.attribution_callsite.as_ref()),
    )
}

fn disposition_name(disposition: ImpactVisibilityDisposition) -> &'static str {
    match disposition {
        ImpactVisibilityDisposition::RequiredPublic => "required-public",
        ImpactVisibilityDisposition::NarrowablePublic => "narrowable-public",
        ImpactVisibilityDisposition::DeadPublic => "dead-public",
        ImpactVisibilityDisposition::NotPublicCandidate => "not-a-public-candidate",
    }
}

fn provenance_name(provenance: ImpactProvenanceClass) -> &'static str {
    match provenance {
        ImpactProvenanceClass::Production => "production",
        ImpactProvenanceClass::Nonproduction => "nonproduction",
        ImpactProvenanceClass::BuildTime => "build-time",
        ImpactProvenanceClass::PublicInterface => "public-interface",
    }
}

fn render_comparison_table(report: &ComparisonOutput) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    render_comparison(&mut output, report)
}

fn render_comparison(output: &mut impl Write, report: &ComparisonOutput) -> Result<()> {
    writeln!(
        output,
        "Public API comparison: {}",
        status_name(report.status)
    )?;
    writeln!(
        output,
        "Baseline: {} at {} ({})",
        report.before.revision.as_deref().unwrap_or("<commit>"),
        short_commit(&report.before.commit),
        status_name(report.before.status),
    )?;
    writeln!(
        output,
        "Working tree: {} at {} ({})",
        if report.after.dirty == Some(true) {
            "dirty"
        } else {
            "clean"
        },
        short_commit(&report.after.commit),
        status_name(report.after.status),
    )?;
    render_compiler_identity(output, report)?;
    if let Some(reason) = &report.reason {
        writeln!(output, "Reason: {reason}")?;
    }
    let Some(diff) = &report.api_diff else {
        writeln!(
            output,
            "API diff unavailable; incomplete evidence is not reported as zero changes."
        )?;
        return Ok(());
    };
    writeln!(
        output,
        "Changes: {} ({} definitions added, {} removed; {} bindings added, {} removed, {} retargeted)",
        diff.summary.total_changes,
        diff.summary.added_definitions,
        diff.summary.removed_definitions,
        diff.summary.added_bindings,
        diff.summary.removed_bindings,
        diff.summary.retargeted_bindings,
    )?;
    if diff.changes.is_empty() {
        writeln!(output, "No public API topology changes.")?;
        return Ok(());
    }
    writeln!(output)?;
    writeln!(output, "API changes:")?;
    for change in &diff.changes {
        render_api_change(output, change, report)?;
    }
    Ok(())
}

fn render_compiler_identity(output: &mut impl Write, report: &ComparisonOutput) -> Result<()> {
    let baseline = &report.before.compiler;
    let current = &report.after.compiler;
    if baseline == current {
        writeln!(
            output,
            "Compiler: protocol {}, driver {}, rustc {} ({} {} for {})",
            current.protocol_version,
            current.driver_version,
            current.rustc_version,
            current.rustc_commit,
            current.rustc_commit_date,
            current.rustc_host,
        )?;
    } else {
        writeln!(output, "Baseline compiler: {}", compiler_label(baseline))?;
        writeln!(output, "Working-tree compiler: {}", compiler_label(current))?;
    }
    Ok(())
}

fn compiler_label(identity: &CompilerIdentity) -> String {
    format!(
        "protocol {}, driver {}, rustc {} ({} {} for {})",
        identity.protocol_version,
        identity.driver_version,
        identity.rustc_version,
        identity.rustc_commit,
        identity.rustc_commit_date,
        identity.rustc_host,
    )
}

fn render_api_change(
    output: &mut impl Write,
    change: &ApiChangeReport,
    report: &ComparisonOutput,
) -> Result<()> {
    match change {
        ApiChangeReport::DefinitionAdded { definition } => {
            render_definition_change(output, "+", definition)
        }
        ApiChangeReport::DefinitionRemoved { definition } => {
            render_definition_change(output, "-", definition)
        }
        ApiChangeReport::BindingAdded { binding } => render_binding_change(output, "+", binding),
        ApiChangeReport::BindingRemoved { binding } => render_binding_change(output, "-", binding),
        ApiChangeReport::BindingRetargeted {
            unit,
            parent_definition_path,
            name,
            namespace,
            before_target_path,
            after_target_path,
        } => {
            let location = retarget_location(
                report,
                unit,
                parent_definition_path,
                name,
                namespace,
                after_target_path,
            );
            writeln!(
                output,
                "~ binding {} {} [{namespace}]: {before_target_path} -> {after_target_path}{location}",
                unit_label(unit),
                binding_path(parent_definition_path, name),
            )
            .map_err(Into::into)
        }
    }
}

fn render_definition_change(
    output: &mut impl Write,
    marker: &str,
    definition: &ApiDefinitionReport,
) -> Result<()> {
    writeln!(
        output,
        "{marker} definition {} {} [{}]{}",
        unit_label(&definition.unit),
        definition.definition_path,
        definition.kind,
        definition_location(definition),
    )
    .map_err(Into::into)
}

fn render_binding_change(
    output: &mut impl Write,
    marker: &str,
    binding: &ApiBindingReport,
) -> Result<()> {
    writeln!(
        output,
        "{marker} binding {} {} [{}] -> {} ({}){}",
        unit_label(&binding.unit),
        binding_path(&binding.parent_definition_path, &binding.name),
        binding.namespace,
        binding.resolved_target_path,
        binding.exposure,
        span_location(binding.span.as_ref()),
    )
    .map_err(Into::into)
}

fn binding_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}::{name}")
    }
}

fn definition_location(definition: &ApiDefinitionReport) -> String {
    span_location(
        definition
            .span
            .as_ref()
            .or(definition.attribution_callsite.as_ref()),
    )
}

fn span_location(span: Option<&crate::model::CompilerSourceSpanReport>) -> String {
    span.map_or_else(String::new, |span| {
        format!(" at {}:{}:{}", span.path, span.line, span.column)
    })
}

fn retarget_location(
    report: &ComparisonOutput,
    unit: &ApiUnitReport,
    parent_definition_path: &str,
    name: &str,
    namespace: &str,
    after_target_path: &str,
) -> String {
    report
        .after
        .api_surface
        .as_ref()
        .into_iter()
        .flat_map(|surface| &surface.bindings)
        .find(|binding| {
            &binding.unit == unit
                && binding.parent_definition_path == parent_definition_path
                && binding.name == name
                && binding.namespace == namespace
                && binding.resolved_target_path == after_target_path
        })
        .map_or_else(String::new, |binding| span_location(binding.span.as_ref()))
}

fn unit_label(unit: &ApiUnitReport) -> String {
    if unit.package_path == "." {
        format!("{}:{}", unit.package, unit.target)
    } else {
        format!("{}:{} ({})", unit.package, unit.target, unit.package_path)
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn status_name(status: SemanticStatus) -> &'static str {
    match status {
        SemanticStatus::Complete => "complete",
        SemanticStatus::Partial => "partial",
        SemanticStatus::Unavailable => "unavailable",
    }
}

fn render_diagnostics(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        render_diagnostic(None, diagnostic);
    }
}

fn render_endpoint_diagnostics(endpoint: &str, diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        render_diagnostic(Some(endpoint), diagnostic);
    }
}

fn render_diagnostic(endpoint: Option<&str>, diagnostic: &Diagnostic) {
    let severity = match diagnostic.severity {
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Error => "error",
    };
    let endpoint = endpoint.map_or_else(String::new, |endpoint| format!("{endpoint}: "));
    match &diagnostic.path {
        Some(path) => eprintln!(
            "rot-audit: {severity}: {endpoint}{path}: {}",
            diagnostic.message
        ),
        None => eprintln!("rot-audit: {severity}: {endpoint}{}", diagnostic.message),
    }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(version: &str) -> CompilerIdentity {
        CompilerIdentity {
            protocol_version: 1,
            driver_version: version.to_owned(),
            rustc_version: "1.98.0-nightly".to_owned(),
            rustc_commit: "0123456789abcdef".to_owned(),
            rustc_commit_date: "2026-08-27".to_owned(),
            rustc_host: "aarch64-apple-darwin".to_owned(),
        }
    }

    #[test]
    fn compiler_identity_includes_protocol_driver_and_full_rustc_identity() {
        let expected = identity("driver-a");
        let mut changed = expected.clone();
        changed.driver_version = "driver-b".to_owned();
        assert_ne!(expected, changed);
        changed = expected.clone();
        changed.rustc_host = "x86_64-unknown-linux-gnu".to_owned();
        assert_ne!(expected, changed);
        changed = expected.clone();
        changed.protocol_version += 1;
        assert_ne!(expected, changed);
    }

    #[test]
    fn incomplete_endpoint_never_becomes_a_complete_comparison() {
        assert_eq!(
            comparison_status(
                SemanticStatus::Complete,
                SemanticStatus::Partial,
                true,
                true,
            ),
            SemanticStatus::Partial
        );
        assert_eq!(
            comparison_status(
                SemanticStatus::Complete,
                SemanticStatus::Unavailable,
                true,
                true,
            ),
            SemanticStatus::Unavailable
        );
        assert_eq!(
            comparison_status(
                SemanticStatus::Complete,
                SemanticStatus::Complete,
                false,
                true,
            ),
            SemanticStatus::Unavailable
        );
    }

    #[test]
    fn baseline_diagnostics_do_not_expose_the_temporary_checkout() {
        let physical = Path::new("/tmp/.rot-baseline-fixture");
        let logical = Path::new("/work/project");
        let mut snapshot = synthetic_snapshot();
        snapshot.root = physical.display().to_string();
        snapshot.report.reason = Some(format!("failed in {}", physical.join("crate").display()));
        snapshot.diagnostics.push(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            path: Some(physical.join("crate/src/lib.rs").display().to_string()),
            message: format!("failed in {}", physical.join("crate").display()),
        });
        let unit = ApiUnitReport {
            package: "member".to_owned(),
            package_path: "crate".to_owned(),
            target: "member".to_owned(),
            kind: "library".to_owned(),
        };
        let outside_workspace = compiler_span(
            &physical
                .join("shared.rs")
                .to_string_lossy()
                .replace('\\', "/"),
        );
        snapshot.report.api_surface = Some(ApiSurfaceReport {
            scope: "test".to_owned(),
            limitations: Vec::new(),
            units: vec![unit.clone()],
            definitions: vec![ApiDefinitionReport {
                unit: unit.clone(),
                definition_path: "member::Shared".to_owned(),
                kind: "struct".to_owned(),
                expansion_origin: "authored".to_owned(),
                span: Some(outside_workspace.clone()),
                attribution_callsite: Some(outside_workspace.clone()),
            }],
            bindings: vec![ApiBindingReport {
                unit,
                parent_definition_path: "member".to_owned(),
                name: "Shared".to_owned(),
                namespace: "type".to_owned(),
                resolved_target_path: "member::Shared".to_owned(),
                exposure: "direct".to_owned(),
                span: Some(outside_workspace),
            }],
        });

        normalize_snapshot_paths(&mut snapshot, physical, logical);

        assert_eq!(snapshot.root, "/work/project");
        assert_eq!(
            snapshot.report.reason.as_deref(),
            Some("failed in /work/project/crate")
        );
        assert_eq!(
            snapshot.diagnostics[0].path.as_deref(),
            Some("crate/src/lib.rs")
        );
        assert_eq!(
            snapshot.diagnostics[0].message,
            "failed in /work/project/crate"
        );
        let surface = snapshot.report.api_surface.as_ref().unwrap();
        assert_eq!(
            surface.definitions[0].span.as_ref().unwrap().path,
            "/work/project/shared.rs"
        );
        assert_eq!(
            surface.definitions[0]
                .attribution_callsite
                .as_ref()
                .unwrap()
                .path,
            "/work/project/shared.rs"
        );
        assert_eq!(
            surface.bindings[0].span.as_ref().unwrap().path,
            "/work/project/shared.rs"
        );
    }

    fn compiler_span(path: &str) -> crate::model::CompilerSourceSpanReport {
        crate::model::CompilerSourceSpanReport {
            path: path.to_owned(),
            source_hash: format!("sha256={}", "0".repeat(64)),
            generated: false,
            start_byte: 0,
            end_byte: 1,
            line: 1,
            column: 1,
        }
    }

    fn synthetic_snapshot() -> AuditSnapshot {
        AuditSnapshot {
            root: String::new(),
            profile: AuditProfile {
                toolchain: "nightly-test".to_owned(),
                target: "test-target".to_owned(),
                feature_mode: "default",
                requested_features: Vec::new(),
                all_features: false,
                no_default_features: false,
                forced_cfg: Vec::new(),
            },
            report: CompilerReport {
                protocol_version: 1,
                driver_version: "driver".to_owned(),
                rustc_version: "rustc".to_owned(),
                rustc_commit: "commit".to_owned(),
                rustc_commit_date: "date".to_owned(),
                rustc_host: "host".to_owned(),
                expected_invocations: 0,
                collected_invocations: 0,
                correlated_invocations: 0,
                invocations: Vec::new(),
                status: SemanticStatus::Unavailable,
                reason: None,
                required_visibility: None,
                closed_world: None,
                api_surface: None,
                impact: None,
            },
            diagnostics: Vec::new(),
        }
    }
}
