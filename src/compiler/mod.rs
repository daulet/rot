mod cargo;
mod closed_world;
mod correlation;
mod environment;
mod profile;
mod sidecar;

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rot_compiler_protocol::{
    Availability, CfgValue, DRIVER_VERSION, HANDSHAKE_ARG, Handshake, PINNED_RUSTC_COMMIT,
    PINNED_RUSTC_RELEASE, PINNED_RUSTC_VERSION, PROTOCOL_VERSION, Product,
};

use crate::{
    cli::AuditCli,
    model::{
        CompilerInvocationReport, CompilerReport, Diagnostic, DiagnosticSeverity, SemanticStatus,
    },
    workspace::Inventory,
};

use self::{
    cargo::CargoRun,
    correlation::{CorrelatedInvocation, Correlation, compilation_context_name},
    environment::CompilerEnvironment,
    sidecar::Invocation,
};

const DRIVER_ENV: &str = "ROT_AUDIT_DRIVER";

pub(crate) fn pinned_metadata(
    cli: &AuditCli,
    workspace: &Path,
    no_dependencies: bool,
) -> Result<cargo_metadata::Metadata> {
    profile::load_metadata(cli, workspace, no_dependencies)
}

pub(crate) fn effective_target(cli: &AuditCli, workspace: &Path) -> Result<String> {
    environment::effective_target(cli, workspace)
}

pub struct Outcome {
    pub report: CompilerReport,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn collect(cli: &AuditCli, inventory: &Inventory) -> Outcome {
    if inventory.packages.is_empty() {
        return unavailable("visibility audit requires a Cargo workspace or package".to_owned());
    }
    let compiler_profile = match profile::resolve(cli, inventory) {
        Ok(profile) => profile,
        Err(error) => {
            return unavailable(format!("compiler analysis unavailable: {error:#}"));
        }
    };
    match try_collect(cli, inventory, &compiler_profile) {
        Ok(outcome) => outcome,
        Err(error) => unavailable(format!("compiler analysis unavailable: {error:#}")),
    }
}

fn try_collect(
    cli: &AuditCli,
    inventory: &Inventory,
    compiler_profile: &profile::CompilerProfile,
) -> Result<Outcome> {
    if !cli.cfg.is_empty() && !environment::custom_cfg_environment_is_safe() {
        bail!("custom --cfg cannot be composed with configured rustflag environment variables");
    }

    let driver = locate_driver(cli)?;
    let ordinary_wrapper = environment::ordinary_wrapper_configured(&inventory.root)?;
    let first = run_once(cli, inventory, compiler_profile, &driver, false)?;
    let retry = ordinary_wrapper
        && (first.correlation.missing_invoked_sidecars > 0
            || wrapper_probe_failed(&first, &driver));
    let run = if retry {
        run_once(cli, inventory, compiler_profile, &driver, true)
            .context("compiler retry without the ordinary Cargo wrapper failed")?
    } else {
        first
    };

    Ok(build_outcome(cli, inventory, run, retry))
}

fn wrapper_probe_failed(run: &CollectedRun, driver: &Path) -> bool {
    run.cargo.artifacts.is_empty()
        && run.cargo.failures.is_empty()
        && run.correlation.invocations.is_empty()
        && run
            .cargo
            .stderr
            .contains(&driver.to_string_lossy().into_owned())
        && (run.cargo.stderr.contains(" -vV") || run.cargo.stderr.contains(" -Vv"))
}

struct CollectedRun {
    handshake: Handshake,
    cargo: CargoRun,
    sidecar_errors: Vec<String>,
    correlation: Correlation,
}

fn run_once(
    cli: &AuditCli,
    inventory: &Inventory,
    compiler_profile: &profile::CompilerProfile,
    driver: &Path,
    disable_ordinary_wrapper: bool,
) -> Result<CollectedRun> {
    let environment = CompilerEnvironment::discover(cli, &inventory.root)?;
    let handshake = handshake(&environment, driver)?;
    let selected_manifest_dirs = selected_manifest_dirs(inventory)?;
    let mut command = environment.cargo_command(
        &inventory.root,
        driver,
        cli,
        inventory
            .audit_target
            .as_deref()
            .unwrap_or("unknown-target"),
        &selected_manifest_dirs,
    )?;
    if disable_ordinary_wrapper {
        CompilerEnvironment::disable_ordinary_wrapper(&mut command);
    }
    let cargo = cargo::run(&mut command)?;
    let sidecars = sidecar::read_all(
        &environment.artifacts.events,
        &environment.artifacts.run_id,
        &handshake,
    )?;
    let correlation =
        correlation::correlate(sidecars.invocations, &cargo, inventory, compiler_profile);
    Ok(CollectedRun {
        handshake,
        cargo,
        sidecar_errors: sidecars.errors,
        correlation,
    })
}

fn selected_manifest_dirs(inventory: &Inventory) -> Result<std::ffi::OsString> {
    const MAX_SELECTED_MANIFEST_DIRS: usize = 4096;

    let selected = inventory.selected_package_ids();
    let mut directories = inventory
        .packages
        .iter()
        .filter(|package| selected.contains(&package.id.to_string()))
        .map(|package| fs::canonicalize(&package.root).unwrap_or_else(|_| package.root.clone()))
        .collect::<Vec<_>>();
    directories.sort();
    directories.dedup();
    if directories.is_empty() {
        bail!("visibility audit selected no Cargo package manifests");
    }
    if directories.len() > MAX_SELECTED_MANIFEST_DIRS {
        bail!(
            "visibility audit selected {} package manifests, exceeding the limit {MAX_SELECTED_MANIFEST_DIRS}",
            directories.len()
        );
    }
    env::join_paths(&directories).context("selected Cargo manifest paths cannot be encoded")
}

fn handshake(environment: &CompilerEnvironment, driver: &Path) -> Result<Handshake> {
    let output = environment
        .driver_command(driver)
        .arg(HANDSHAKE_ARG)
        .output()
        .with_context(|| format!("cannot execute compiler driver {}", driver.display()))?;
    if !output.status.success() {
        bail!(
            "compiler driver handshake failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let handshake: Handshake = serde_json::from_slice(&output.stdout)
        .context("compiler driver handshake was malformed")?;
    if handshake.protocol_version != PROTOCOL_VERSION {
        bail!(
            "compiler protocol mismatch: rot expects {PROTOCOL_VERSION}, driver reports {}",
            handshake.protocol_version
        );
    }
    if handshake.driver_version != DRIVER_VERSION {
        bail!(
            "compiler driver mismatch: rot expects {DRIVER_VERSION}, driver reports {}",
            handshake.driver_version
        );
    }
    if handshake.linked_rustc_version != PINNED_RUSTC_VERSION {
        bail!(
            "compiler driver linked-rustc mismatch: expected {PINNED_RUSTC_VERSION}, found {}",
            handshake.linked_rustc_version
        );
    }
    if handshake.rustc.commit_hash != PINNED_RUSTC_COMMIT
        || handshake.rustc.release != PINNED_RUSTC_RELEASE
    {
        bail!(
            "compiler toolchain mismatch: expected {PINNED_RUSTC_RELEASE} ({PINNED_RUSTC_COMMIT}), found {} ({})",
            handshake.rustc.release,
            handshake.rustc.commit_hash
        );
    }
    Ok(handshake)
}

fn build_outcome(
    cli: &AuditCli,
    inventory: &Inventory,
    mut run: CollectedRun,
    retried_without_wrapper: bool,
) -> Outcome {
    let mut diagnostics = Vec::new();
    let selected_cargo_incomplete = run.correlation.correlated != run.correlation.expected
        || run
            .correlation
            .invocations
            .iter()
            .any(|invocation| !invocation.invocation.finished.rustc_success);
    // Malformed sidecars cannot be attributed safely. The selected Cargo unit
    // ledger, exact correlation, and visibility-fact integrity checks establish
    // semantic completeness; unrelated extra files remain diagnostics only.
    let transport_clean = run.correlation.errors.is_empty();
    let mut transport_errors = std::mem::take(&mut run.sidecar_errors);
    transport_errors.append(&mut run.correlation.errors);
    transport_errors.sort();
    transport_errors.dedup();
    diagnostics.extend(transport_errors.into_iter().map(warning));
    if retried_without_wrapper {
        diagnostics.push(warning(
            "the configured ordinary Cargo rustc wrapper suppressed compiler sidecars; collection was retried once with that wrapper disabled"
                .to_owned(),
        ));
    }
    if selected_cargo_incomplete
        && (!run.cargo.status.success() || run.cargo.build_finished != Some(true))
    {
        let selected = inventory.selected_package_ids();
        let detail = run
            .cargo
            .failures
            .iter()
            .find(|failure| selected.contains(&failure.package_id))
            .or_else(|| run.cargo.failures.first())
            .map(|failure| failure.message.as_str())
            .or_else(|| run.cargo.text_lines.first().map(String::as_str))
            .or_else(|| (!run.cargo.stderr.trim().is_empty()).then(|| run.cargo.stderr.trim()))
            .unwrap_or("Cargo check did not complete successfully");
        diagnostics.push(warning(format!(
            "compiler Cargo pass was incomplete: {detail}"
        )));
    }
    let build_cfg_issues = apply_build_script_cfg_issues(&mut run);
    diagnostics.extend(build_cfg_issues.iter().cloned().map(warning));

    let mut invocation_reports = Vec::new();
    for correlated in &run.correlation.invocations {
        invocation_reports.push(invocation_report(
            cli,
            inventory,
            correlated,
            &mut diagnostics,
        ));
        for diagnostic in &correlated.invocation.diagnostics {
            diagnostics.push(Diagnostic {
                severity: match diagnostic.severity {
                    rot_compiler_protocol::DiagnosticSeverity::Warning => {
                        DiagnosticSeverity::Warning
                    }
                    rot_compiler_protocol::DiagnosticSeverity::Error => DiagnosticSeverity::Error,
                },
                path: None,
                message: format!(
                    "compiler {}: {}",
                    correlated.invocation.started.crate_name, diagnostic.message
                ),
            });
        }
    }
    let report_by_key = invocation_reports
        .iter()
        .map(|report| (report.key.as_str(), report))
        .collect::<BTreeMap<_, _>>();
    let expected = run.correlation.expected;
    let collected = run.correlation.invocations.len();
    let (mut status, mut reason) = aggregate_status(&invocation_reports, expected, transport_clean);
    let graph_invocations = run
        .correlation
        .invocations
        .iter()
        .filter_map(|correlated| {
            let target = correlated.target.as_ref()?;
            let invocation = &correlated.invocation;
            let report = report_by_key.get(invocation.started.merge_key.0.as_str())?;
            Some(closed_world::GraphInvocation {
                target,
                owner: invocation
                    .started
                    .package_name
                    .as_deref()
                    .unwrap_or(&invocation.started.crate_name),
                crate_name: &invocation.started.crate_name,
                status: report.status,
                invocation,
            })
        })
        .collect::<Vec<_>>();
    let graph_aggregation = closed_world::aggregate(&inventory.root, status, &graph_invocations);
    status = graph_aggregation.status;
    if graph_aggregation.reason.is_some() {
        reason = graph_aggregation.reason.clone();
    }
    invocation_reports.sort_by(|left, right| left.key.cmp(&right.key));
    diagnostics
        .sort_by(|left, right| (&left.path, &left.message).cmp(&(&right.path, &right.message)));
    diagnostics.dedup_by(|left, right| left.path == right.path && left.message == right.message);

    Outcome {
        report: CompilerReport {
            protocol_version: run.handshake.protocol_version,
            driver_version: run.handshake.driver_version.to_string(),
            rustc_version: run.handshake.rustc.release,
            rustc_commit: run.handshake.rustc.commit_hash,
            expected_invocations: expected as u64,
            collected_invocations: collected as u64,
            correlated_invocations: run.correlation.correlated as u64,
            invocations: invocation_reports,
            status,
            reason,
            required_visibility: graph_aggregation.required_visibility,
            closed_world: graph_aggregation.closed_world,
        },
        diagnostics,
    }
}

fn invocation_report(
    cli: &AuditCli,
    inventory: &Inventory,
    correlated: &CorrelatedInvocation,
    diagnostics: &mut Vec<Diagnostic>,
) -> CompilerInvocationReport {
    let invocation = &correlated.invocation;
    let profile = invocation.profile.as_ref();
    let issues = invocation_issues(cli, inventory, correlated);
    let observed_cfg = profile.map_or_else(Vec::new, |profile| render_cfg(&profile.cfg));
    for issue in &issues {
        diagnostics.push(warning(format!(
            "compiler {}: {issue}",
            invocation.started.crate_name
        )));
    }

    let raw = invocation
        .products
        .first()
        .expect("validated sidecars contain visibility audit status");
    let raw_status = availability(raw.availability);
    let status = if issues.is_empty() || raw_status == SemanticStatus::Unavailable {
        raw_status
    } else if product_has_facts(invocation, raw.product) {
        SemanticStatus::Partial
    } else {
        SemanticStatus::Unavailable
    };
    let reason = if status == SemanticStatus::Complete {
        None
    } else {
        let mut reasons = raw.message.iter().cloned().collect::<Vec<_>>();
        if raw_status != status || !issues.is_empty() {
            reasons.extend(issues.iter().cloned());
        }
        reasons.sort();
        reasons.dedup();
        (!reasons.is_empty()).then(|| reasons.join("; "))
    };
    CompilerInvocationReport {
        key: invocation.started.merge_key.0.clone(),
        target: correlated.target.clone(),
        crate_name: invocation.started.crate_name.clone(),
        target_triple: profile.map_or_else(
            || invocation.started.target_triple.clone(),
            |profile| profile.target_triple.clone(),
        ),
        compilation_context: compilation_context_name(invocation.started.compilation_context)
            .to_owned(),
        test: invocation.started.test_mode,
        features: profile.map_or_else(
            || correlated.cargo_features.clone().unwrap_or_default(),
            |profile| profile.features.clone(),
        ),
        cfg: observed_cfg,
        definitions: invocation.definitions.len() as u64,
        roots: invocation.roots.len() as u64,
        references: invocation.references.len() as u64,
        status,
        reason,
    }
}

fn product_has_facts(invocation: &Invocation, product: Product) -> bool {
    match product {
        Product::VisibilityAudit => {
            !invocation.sources.is_empty()
                || !invocation.definitions.is_empty()
                || !invocation.references.is_empty()
                || !invocation.roots.is_empty()
        }
    }
}

fn aggregate_status(
    invocations: &[CompilerInvocationReport],
    expected: usize,
    transport_clean: bool,
) -> (SemanticStatus, Option<String>) {
    let complete_transport = transport_clean && expected > 0 && invocations.len() == expected;
    let any_complete = invocations
        .iter()
        .any(|invocation| invocation.status == SemanticStatus::Complete);
    let any_partial = invocations
        .iter()
        .any(|invocation| invocation.status == SemanticStatus::Partial);
    let all_complete = invocations.len() == expected
        && invocations
            .iter()
            .all(|invocation| invocation.status == SemanticStatus::Complete);
    let status = if complete_transport && all_complete {
        SemanticStatus::Complete
    } else if any_complete || any_partial {
        SemanticStatus::Partial
    } else {
        SemanticStatus::Unavailable
    };
    let reason = (status != SemanticStatus::Complete).then(|| {
        invocations
            .iter()
            .find_map(|invocation| invocation.reason.clone())
            .unwrap_or_else(|| "not complete for every expected Cargo invocation".to_owned())
    });
    (status, reason)
}

fn invocation_issues(
    cli: &AuditCli,
    inventory: &Inventory,
    correlated: &CorrelatedInvocation,
) -> Vec<String> {
    let invocation = &correlated.invocation;
    let profile = invocation.profile.as_ref();
    let mut issues = correlated.issue.iter().cloned().collect::<Vec<_>>();
    if profile.is_none() {
        issues.push("compiler invocation has no concrete profile".to_owned());
    }
    if !invocation.finished.rustc_success {
        issues.push("rustc invocation failed".to_owned());
    }
    if let Some(cargo_features) = &correlated.cargo_features
        && profile.is_some_and(|profile| &profile.features != cargo_features)
    {
        issues.push(format!(
            "Cargo/rustc feature mismatch: Cargo={cargo_features:?}, rustc={:?}",
            profile.map(|profile| &profile.features)
        ));
    }
    if let (Some(profile), Some(cargo_profile)) = (profile, &correlated.cargo_profile) {
        issues.extend(cargo_codegen_issues(&profile.codegen, cargo_profile));
    }
    let host_only = is_host_only(correlated);
    if let Some(profile) = profile {
        let expected = if host_only {
            &profile.host_triple
        } else {
            inventory
                .audit_target
                .as_deref()
                .unwrap_or("unknown-target")
        };
        if profile.target_triple != expected {
            issues.push(format!(
                "rustc target mismatch: expected {expected}, observed {}",
                profile.target_triple
            ));
        }
    }
    if !host_only {
        let observed_cfg = profile.map_or_else(Vec::new, |profile| render_cfg(&profile.cfg));
        for requested in cli.cfg.iter().map(|value| normalize_cfg(value)) {
            if !observed_cfg.iter().any(|observed| observed == &requested) {
                issues.push(format!(
                    "requested cfg {requested:?} was not observed by rustc"
                ));
            }
        }
    }
    issues.sort();
    issues.dedup();
    issues
}

fn is_host_only(correlated: &CorrelatedInvocation) -> bool {
    matches!(
        correlated.invocation.started.compilation_context,
        rot_compiler_protocol::CompilationContext::Host
    )
}

fn cargo_codegen_issues(
    observed: &rot_compiler_protocol::CodegenProfile,
    expected: &cargo::CargoProfile,
) -> Vec<String> {
    let mut issues = Vec::new();
    let observed_opt = match observed.optimization {
        rot_compiler_protocol::OptimizationLevel::None => "0",
        rot_compiler_protocol::OptimizationLevel::Less => "1",
        rot_compiler_protocol::OptimizationLevel::More => "2",
        rot_compiler_protocol::OptimizationLevel::Aggressive => "3",
        rot_compiler_protocol::OptimizationLevel::Size => "s",
        rot_compiler_protocol::OptimizationLevel::SizeMin => "z",
    };
    if observed_opt != expected.opt_level {
        issues.push(format!(
            "Cargo/rustc optimization mismatch: Cargo={:?}, rustc={observed_opt:?}",
            expected.opt_level
        ));
    }
    if observed.debug_assertions != expected.debug_assertions {
        issues.push(format!(
            "Cargo/rustc debug-assertions mismatch: Cargo={}, rustc={}",
            expected.debug_assertions, observed.debug_assertions
        ));
    }
    if observed.overflow_checks != expected.overflow_checks {
        issues.push(format!(
            "Cargo/rustc overflow-checks mismatch: Cargo={}, rustc={}",
            expected.overflow_checks, observed.overflow_checks
        ));
    }
    issues
}

fn availability(value: Availability) -> SemanticStatus {
    match value {
        Availability::Complete => SemanticStatus::Complete,
        Availability::Partial => SemanticStatus::Partial,
        Availability::Unavailable => SemanticStatus::Unavailable,
    }
}

pub(super) fn generated_source_label(owner: &str, path: &Path, source_hash: &str) -> String {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("generated.rs");
    let (algorithm, digest) = source_hash.split_once('=').unwrap_or(("hash", source_hash));
    let digest = digest.chars().take(16).collect::<String>();
    format!("<generated>/{owner}/{algorithm}-{digest}/{filename}")
}

fn render_cfg(values: &[CfgValue]) -> Vec<String> {
    let mut rendered = values
        .iter()
        .map(|cfg| {
            cfg.value
                .as_ref()
                .map_or_else(|| cfg.name.clone(), |value| format!("{}={value}", cfg.name))
        })
        .collect::<Vec<_>>();
    rendered.sort();
    rendered.dedup();
    rendered
}

fn normalize_cfg(value: &str) -> String {
    let compact = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    compact
        .split_once('=')
        .map_or(compact.clone(), |(name, value)| {
            format!("{name}={}", value.trim_matches('"'))
        })
}

fn locate_driver(cli: &AuditCli) -> Result<PathBuf> {
    if let Some(explicit) = cli
        .driver
        .clone()
        .or_else(|| env::var_os(DRIVER_ENV).map(PathBuf::from))
    {
        if !explicit.is_file() {
            bail!("compiler driver {} does not exist", explicit.display());
        }
        return fs::canonicalize(&explicit)
            .with_context(|| format!("cannot resolve compiler driver {}", explicit.display()));
    }
    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join(driver_filename()));
    }
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(
        repository
            .join("compiler/rot-rustc-driver/target/release")
            .join(driver_filename()),
    );
    candidates.push(
        repository
            .join("compiler/rot-rustc-driver/target/debug")
            .join(driver_filename()),
    );
    for candidate in candidates {
        if candidate.is_file() {
            return fs::canonicalize(&candidate).with_context(|| {
                format!("cannot resolve compiler driver {}", candidate.display())
            });
        }
    }
    bail!("rot-rustc-driver was not found; build compiler/rot-rustc-driver or pass --driver PATH")
}

fn apply_build_script_cfg_issues(run: &mut CollectedRun) -> Vec<String> {
    let mut outputs = BTreeMap::<String, Vec<(PathBuf, Vec<String>)>>::new();
    for output in &run.cargo.build_script_outputs {
        outputs
            .entry(output.package_id.clone())
            .or_default()
            .push((canonical_or_owned(&output.out_dir), output.cfg.clone()));
    }
    let mut issues = Vec::new();
    for invocation in &mut run.correlation.invocations {
        let Some(target) = invocation
            .target
            .as_ref()
            .filter(|target| target.role != "build")
        else {
            continue;
        };
        let Some(package_outputs) = outputs.get(&target.package_id) else {
            continue;
        };
        let Some(out_dir) = invocation
            .invocation
            .started
            .build_script_out_dir
            .as_deref()
            .map(Path::new)
            .map(canonical_or_owned)
        else {
            let issue = format!(
                "compiler invocation {} omitted Cargo build-script OUT_DIR for {}",
                invocation.invocation.started.crate_name, target.package_id
            );
            append_issue(&mut invocation.issue, &issue);
            issues.push(issue);
            continue;
        };
        let matched = package_outputs
            .iter()
            .filter(|(candidate, _)| candidate == &out_dir)
            .collect::<Vec<_>>();
        if matched.len() != 1 {
            let issue = format!(
                "compiler invocation {} has an unmatched Cargo build-script OUT_DIR for {}",
                invocation.invocation.started.crate_name, target.package_id
            );
            append_issue(&mut invocation.issue, &issue);
            issues.push(issue);
            continue;
        }
        let observed = invocation
            .invocation
            .profile
            .as_ref()
            .map(|profile| render_cfg(&profile.cfg).into_iter().collect::<HashSet<_>>())
            .unwrap_or_default();
        for cfg in matched[0].1.iter().map(|cfg| normalize_cfg(cfg)) {
            if !observed.contains(&cfg) {
                let issue = format!(
                    "Cargo build-script cfg {cfg:?} for {} was not observed by {}",
                    target.package_id, invocation.invocation.started.crate_name
                );
                append_issue(&mut invocation.issue, &issue);
                issues.push(issue);
            }
        }
    }
    issues.sort();
    issues.dedup();
    issues
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn append_issue(current: &mut Option<String>, issue: &str) {
    match current {
        Some(current) => {
            current.push_str("; ");
            current.push_str(issue);
        }
        None => *current = Some(issue.to_owned()),
    }
}

fn driver_filename() -> &'static str {
    if cfg!(windows) {
        "rot-rustc-driver.exe"
    } else {
        "rot-rustc-driver"
    }
}

fn unavailable(reason: String) -> Outcome {
    Outcome {
        report: CompilerReport {
            protocol_version: PROTOCOL_VERSION,
            driver_version: DRIVER_VERSION.to_string(),
            rustc_version: PINNED_RUSTC_RELEASE.to_owned(),
            rustc_commit: PINNED_RUSTC_COMMIT.to_owned(),
            expected_invocations: 0,
            collected_invocations: 0,
            correlated_invocations: 0,
            invocations: Vec::new(),
            status: SemanticStatus::Unavailable,
            reason: Some(reason.clone()),
            required_visibility: None,
            closed_world: None,
        },
        diagnostics: vec![warning(reason)],
    }
}

fn warning(message: String) -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Warning,
        path: None,
        message,
    }
}

#[cfg(test)]
mod tests {
    use rot_compiler_protocol::{CodegenProfile, OptimizationLevel, PanicStrategy};

    use super::{cargo::CargoProfile, cargo_codegen_issues};

    #[test]
    fn cargo_codegen_profile_mismatches_are_semantic_issues() {
        let observed = CodegenProfile {
            optimization: OptimizationLevel::More,
            panic: PanicStrategy::Unwind,
            debug_assertions: false,
            overflow_checks: false,
            codegen_units: 1,
            target_cpu: "generic".to_owned(),
            target_features: Vec::new(),
        };
        let expected = CargoProfile {
            opt_level: "0".to_owned(),
            debug_assertions: true,
            overflow_checks: true,
        };

        let issues = cargo_codegen_issues(&observed, &expected);
        assert_eq!(issues.len(), 3);
        assert!(issues.iter().any(|issue| issue.contains("optimization")));
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("debug-assertions"))
        );
        assert!(issues.iter().any(|issue| issue.contains("overflow-checks")));
    }
}
