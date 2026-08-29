mod cargo;
mod closed_world;
mod correlation;
mod effective_api;
mod environment;
mod macro_complexity;
mod profile;
mod sidecar;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use ra_ap_syntax::Edition;
use rot_compiler_protocol::{
    Availability, CfgValue, DRIVER_VERSION, HANDSHAKE_ARG, Handshake, PINNED_RUSTC_COMMIT,
    PINNED_RUSTC_RELEASE, PINNED_RUSTC_VERSION, PROTOCOL_VERSION, Product,
};

use crate::{
    cfg::{CfgProfile, PackageFeatures},
    cli::Cli,
    model::{
        CompilerArtifactReport, CompilerCodegenReport, CompilerInvocationReport, CompilerReport,
        CompilerSourceReport, ComplexityMetrics, Diagnostic, DiagnosticSeverity,
        GeneratedFileReport, LineCounts, ProductAvailabilityReport, SemanticStatus,
    },
    source::{ContentKind, analyze_bytes},
    workspace::Inventory,
};

use self::{
    cargo::CargoRun,
    correlation::{CorrelatedInvocation, Correlation, compilation_context_name},
    environment::CompilerEnvironment,
    sidecar::Invocation,
};

const DRIVER_ENV: &str = "ROT_COMPILER_DRIVER";

pub(crate) fn pinned_metadata(
    cli: &Cli,
    workspace: &Path,
    no_dependencies: bool,
) -> Result<cargo_metadata::Metadata> {
    profile::load_metadata(cli, workspace, no_dependencies)
}

pub(crate) fn validate_environment(workspace: &Path) -> Result<()> {
    environment::reject_compiler_overrides(workspace)
}

pub(crate) fn pinned_rustc_command() -> std::process::Command {
    environment::pinned_command("rustc")
}

pub(crate) fn effective_target(cli: &Cli, workspace: &Path) -> Result<String> {
    environment::effective_target(cli, workspace)
}

pub struct Outcome {
    pub report: CompilerReport,
    pub diagnostics: Vec<Diagnostic>,
    pub profile_incompatibilities: Vec<String>,
}

pub fn collect(cli: &Cli, inventory: &Inventory) -> Outcome {
    if !inventory.compiler_compatible {
        return unavailable(inventory.compiler_unavailable_reasons.join("; "));
    }
    if inventory.packages.is_empty() {
        return unavailable("compiler mode requires a Cargo workspace or package".to_owned());
    }
    let compiler_profile = match profile::resolve(cli, inventory) {
        Ok(profile) => profile,
        Err(error) => {
            return unavailable(format!("compiler analysis unavailable: {error:#}"));
        }
    };
    if !compiler_profile.incompatibilities().is_empty() {
        let reasons = compiler_profile.incompatibilities().to_vec();
        return unavailable_profile(reasons.join("; "), reasons);
    }
    match try_collect(cli, inventory, &compiler_profile) {
        Ok(outcome) => outcome,
        Err(error) => unavailable(format!("compiler analysis unavailable: {error:#}")),
    }
}

fn try_collect(
    cli: &Cli,
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

    Ok(build_outcome(cli, inventory, compiler_profile, run, retry))
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
    integrity_errors: Vec<String>,
    product_integrity_errors: BTreeMap<(String, Product), Vec<String>>,
    correlation: Correlation,
    generated_files: Vec<GeneratedFileReport>,
}

fn run_once(
    cli: &Cli,
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
        &inventory.profile.target,
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
    // Generated source must be read while the isolated build directory is alive.
    let mut integrity_errors = Vec::new();
    let mut product_integrity_errors = BTreeMap::new();
    let generated = rescan_generated(inventory, &correlation.invocations);
    for error in generated.errors {
        let message = format!("{}: {}", error.report, error.error);
        for product in error.products {
            product_integrity_errors
                .entry((error.merge_key.clone(), product))
                .or_insert_with(Vec::new)
                .push(message.clone());
        }
        integrity_errors.push(message);
    }
    Ok(CollectedRun {
        handshake,
        cargo,
        sidecar_errors: sidecars.errors,
        integrity_errors,
        product_integrity_errors,
        correlation,
        generated_files: generated.files,
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
        bail!("compiler mode selected no Cargo package manifests");
    }
    if directories.len() > MAX_SELECTED_MANIFEST_DIRS {
        bail!(
            "compiler mode selected {} package manifests, exceeding the limit {MAX_SELECTED_MANIFEST_DIRS}",
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
    cli: &Cli,
    inventory: &Inventory,
    compiler_profile: &profile::CompilerProfile,
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
    // ledger, exact correlation, and per-product integrity checks establish
    // semantic completeness; unrelated extra files remain diagnostics only.
    let semantic_inputs_clean = true;
    let transport_clean = run.correlation.errors.is_empty();
    let mut transport_errors = std::mem::take(&mut run.sidecar_errors);
    transport_errors.append(&mut run.integrity_errors);
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
            &run.product_integrity_errors,
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
    let api_invocations = run
        .correlation
        .invocations
        .iter()
        .filter_map(|correlated| {
            let invocation = &correlated.invocation;
            let report = report_by_key.get(invocation.started.merge_key.0.as_str())?;
            let status = report
                .products
                .iter()
                .find(|product| product.product == "effective_api")
                .map_or(SemanticStatus::Unavailable, |product| product.status);
            Some(effective_api::ApiInvocation {
                target: correlated.target.as_ref(),
                owner: invocation
                    .started
                    .package_name
                    .as_deref()
                    .unwrap_or(&invocation.started.crate_name),
                crate_name: &invocation.started.crate_name,
                status,
                sources: &invocation.sources,
                definitions: &invocation.definitions,
                public_bindings: &invocation.public_bindings,
            })
        })
        .collect::<Vec<_>>();
    let api_aggregation = effective_api::aggregate(
        inventory,
        compiler_profile,
        semantic_inputs_clean,
        api_invocations,
    );
    let macro_expansion_complexity = macro_complexity::aggregate(
        semantic_inputs_clean,
        run.correlation.invocations.iter().filter_map(|correlated| {
            let invocation = &correlated.invocation;
            let report = report_by_key.get(invocation.started.merge_key.0.as_str())?;
            let product = report
                .products
                .iter()
                .find(|product| product.product == product_name(Product::ExpansionDecisions))?;
            Some(macro_complexity::MacroInvocation {
                key: &invocation.started.merge_key.0,
                target: correlated.target.as_ref(),
                crate_name: &invocation.started.crate_name,
                status: product.status,
                reason: product.reason.as_deref(),
                bodies: &invocation.bodies,
                decisions: &invocation.decisions,
            })
        }),
    );
    let expected = run.correlation.expected;
    let collected = run.correlation.invocations.len();
    let mut products = aggregate_products(&invocation_reports, expected, transport_clean);
    if let Some(product) = products
        .iter_mut()
        .find(|product| product.product == "effective_api")
    {
        *product = api_aggregation.product.clone();
    } else {
        products.push(api_aggregation.product.clone());
        products.sort_by(|left, right| left.product.cmp(&right.product));
    }
    let references_status = products
        .iter()
        .find(|product| product.product == "references")
        .map_or(SemanticStatus::Unavailable, |product| product.status);
    let graph_invocations = run
        .correlation
        .invocations
        .iter()
        .filter_map(|correlated| {
            let target = correlated.target.as_ref()?;
            let invocation = &correlated.invocation;
            let report = report_by_key.get(invocation.started.merge_key.0.as_str())?;
            let status = report
                .products
                .iter()
                .find(|product| product.product == "references")
                .map_or(SemanticStatus::Unavailable, |product| product.status);
            Some(closed_world::GraphInvocation {
                target,
                owner: invocation
                    .started
                    .package_name
                    .as_deref()
                    .unwrap_or(&invocation.started.crate_name),
                crate_name: &invocation.started.crate_name,
                status,
                invocation,
            })
        })
        .collect::<Vec<_>>();
    let graph_aggregation =
        closed_world::aggregate(&inventory.root, references_status, &graph_invocations);
    products.push(graph_aggregation.liveness_product.clone());
    products.push(graph_aggregation.required_visibility_product.clone());
    products.sort_by(|left, right| left.product.cmp(&right.product));
    invocation_reports.sort_by(|left, right| left.key.cmp(&right.key));
    let generated_files = std::mem::take(&mut run.generated_files);

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
            products,
            effective_api: api_aggregation.report,
            required_visibility: graph_aggregation.required_visibility,
            closed_world: graph_aggregation.closed_world,
            macro_expansion_complexity,
            generated_files,
        },
        diagnostics,
        profile_incompatibilities: Vec::new(),
    }
}

fn invocation_report(
    cli: &Cli,
    inventory: &Inventory,
    correlated: &CorrelatedInvocation,
    product_integrity_errors: &BTreeMap<(String, Product), Vec<String>>,
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

    let products = invocation
        .products
        .iter()
        .map(|raw| {
            let mut product_issues = issues.clone();
            if let Some(integrity) =
                product_integrity_errors.get(&(invocation.started.merge_key.0.clone(), raw.product))
            {
                product_issues.extend(integrity.iter().cloned());
                diagnostics.extend(integrity.iter().cloned().map(warning));
            }
            product_issues.sort();
            product_issues.dedup();
            let raw_status = availability(raw.availability);
            let status = if product_issues.is_empty() || raw_status == SemanticStatus::Unavailable {
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
                if raw_status != status || !product_issues.is_empty() {
                    reasons.extend(product_issues.iter().cloned());
                }
                reasons.sort();
                reasons.dedup();
                (!reasons.is_empty()).then(|| reasons.join("; "))
            };
            ProductAvailabilityReport {
                product: product_name(raw.product).to_owned(),
                status,
                reason,
            }
        })
        .collect::<Vec<_>>();
    let codegen = profile.map(|profile| CompilerCodegenReport {
        optimization: optimization_name(profile.codegen.optimization).to_owned(),
        panic: panic_name(profile.codegen.panic).to_owned(),
        debug_assertions: profile.codegen.debug_assertions,
        overflow_checks: profile.codegen.overflow_checks,
        codegen_units: profile.codegen.codegen_units,
        target_cpu: profile.codegen.target_cpu.clone(),
        target_features: profile.codegen.target_features.clone(),
    });
    let sources = invocation
        .sources
        .iter()
        .map(|source| CompilerSourceReport {
            path: if source.generated {
                let path = source
                    .local_path
                    .as_deref()
                    .unwrap_or(&source.remapped_path);
                generated_label(invocation, Path::new(path), &source.source_hash)
            } else {
                source.local_path.as_ref().map_or_else(
                    || source.remapped_path.clone(),
                    |path| inventory.display_path(Path::new(path)),
                )
            },
            source_hash_algorithm: source.source_hash_algorithm.clone(),
            source_hash: source.source_hash.clone(),
            bytes: u64::from(source.byte_len),
            generated: source.generated,
        })
        .collect();
    CompilerInvocationReport {
        key: invocation.started.merge_key.0.clone(),
        target: correlated.target.clone(),
        crate_name: invocation.started.crate_name.clone(),
        crate_types: invocation.started.artifact.crate_types.clone(),
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
        codegen,
        artifact: CompilerArtifactReport {
            extra_filename: invocation.started.artifact.extra_filename.clone(),
            metadata: invocation.started.artifact.metadata.clone(),
            emit: invocation.started.artifact.emit.clone(),
        },
        source_files: invocation.sources.len() as u64,
        sources,
        bodies: invocation.bodies.len() as u64,
        definitions: invocation.definitions.len() as u64,
        public_bindings: invocation.public_bindings.len() as u64,
        roots: invocation.roots.len() as u64,
        references: invocation.references.len() as u64,
        macro_expansion_decisions: invocation.decisions.len() as u64,
        products,
    }
}

fn product_has_facts(invocation: &Invocation, product: Product) -> bool {
    match product {
        Product::HirBodies => !invocation.sources.is_empty() || !invocation.bodies.is_empty(),
        Product::EffectiveApi => {
            !invocation.definitions.is_empty() || !invocation.public_bindings.is_empty()
        }
        Product::References => !invocation.references.is_empty() || !invocation.roots.is_empty(),
        Product::ExpansionDecisions => {
            !invocation.decisions.is_empty()
                || invocation.bodies.iter().any(|body| {
                    matches!(
                        body.expansion_origin,
                        rot_compiler_protocol::ExpansionOrigin::LocalMacro
                            | rot_compiler_protocol::ExpansionOrigin::ExternalMacro
                    )
                })
        }
    }
}

fn aggregate_products(
    invocations: &[CompilerInvocationReport],
    expected: usize,
    transport_clean: bool,
) -> Vec<ProductAvailabilityReport> {
    [
        Product::HirBodies,
        Product::EffectiveApi,
        Product::References,
        Product::ExpansionDecisions,
    ]
    .into_iter()
    .map(|product| {
        let statuses = invocations
            .iter()
            .filter_map(|invocation| {
                invocation
                    .products
                    .iter()
                    .find(|status| status.product == product_name(product))
            })
            .collect::<Vec<_>>();
        let complete_transport = transport_clean && expected > 0 && invocations.len() == expected;
        let any_complete = statuses
            .iter()
            .any(|status| status.status == SemanticStatus::Complete);
        let any_partial = statuses
            .iter()
            .any(|status| status.status == SemanticStatus::Partial);
        let all_complete = statuses.len() == expected
            && statuses
                .iter()
                .all(|status| status.status == SemanticStatus::Complete);
        let status = if complete_transport && all_complete {
            SemanticStatus::Complete
        } else if any_complete || any_partial {
            SemanticStatus::Partial
        } else {
            SemanticStatus::Unavailable
        };
        let reason = (status != SemanticStatus::Complete).then(|| {
            statuses
                .iter()
                .find_map(|status| status.reason.clone())
                .unwrap_or_else(|| "not complete for every expected Cargo invocation".to_owned())
        });
        ProductAvailabilityReport {
            product: product_name(product).to_owned(),
            status,
            reason,
        }
    })
    .collect()
}

fn invocation_issues(
    cli: &Cli,
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
            &inventory.profile.target
        };
        if &profile.target_triple != expected {
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

fn optimization_name(value: rot_compiler_protocol::OptimizationLevel) -> &'static str {
    use rot_compiler_protocol::OptimizationLevel;
    match value {
        OptimizationLevel::None => "none",
        OptimizationLevel::Less => "less",
        OptimizationLevel::More => "more",
        OptimizationLevel::Aggressive => "aggressive",
        OptimizationLevel::Size => "size",
        OptimizationLevel::SizeMin => "size_min",
    }
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

fn panic_name(value: rot_compiler_protocol::PanicStrategy) -> &'static str {
    use rot_compiler_protocol::PanicStrategy;
    match value {
        PanicStrategy::Unwind => "unwind",
        PanicStrategy::Abort => "abort",
        PanicStrategy::ImmediateAbort => "immediate_abort",
    }
}

fn availability(value: Availability) -> SemanticStatus {
    match value {
        Availability::Complete => SemanticStatus::Complete,
        Availability::Partial => SemanticStatus::Partial,
        Availability::Unavailable => SemanticStatus::Unavailable,
    }
}

fn product_name(product: Product) -> &'static str {
    match product {
        Product::HirBodies => "hir_bodies",
        Product::EffectiveApi => "effective_api",
        Product::References => "references",
        Product::ExpansionDecisions => "macro_expansion_cyclomatic_delta",
    }
}

struct GeneratedRescan {
    files: Vec<GeneratedFileReport>,
    errors: Vec<GeneratedRescanError>,
}

struct GeneratedRescanError {
    merge_key: String,
    report: String,
    error: String,
    products: BTreeSet<Product>,
}

fn rescan_generated(
    inventory: &Inventory,
    invocations: &[CorrelatedInvocation],
) -> GeneratedRescan {
    let mut candidates = BTreeMap::<
        (String, String),
        Vec<(&Invocation, &rot_compiler_protocol::SourceFile, PathBuf)>,
    >::new();
    for correlated in invocations {
        let owners = owning_source_keys(&correlated.invocation);
        for source in &correlated.invocation.sources {
            let Some(local_path) = source.local_path.as_ref() else {
                continue;
            };
            let path = PathBuf::from(local_path);
            if source.generated
                && owners.contains(&source.key.0)
                && path.extension().is_some_and(|extension| extension == "rs")
            {
                candidates
                    .entry((source.source_hash.clone(), canonical_string(&path)))
                    .or_default()
                    .push((&correlated.invocation, source, path));
            }
        }
    }

    let mut files = Vec::new();
    let mut errors = Vec::new();
    for ((source_hash, _), owners) in candidates {
        let (invocation, compiler_source, path) = &owners[0];
        let bytes = match read_verified_source(path, compiler_source) {
            Ok(bytes) => bytes,
            Err(error) => {
                for (invocation, source, path) in owners {
                    errors.push(GeneratedRescanError {
                        merge_key: invocation.started.merge_key.0.clone(),
                        report: path.display().to_string(),
                        error: error.to_string(),
                        products: products_for_source(invocation, &source.key.0),
                    });
                }
                continue;
            }
        };
        let profile = invocation.profile.as_ref();
        let known_true = profile
            .map(|profile| render_cfg(&profile.cfg).into_iter().collect())
            .unwrap_or_default();
        let cfg = CfgProfile::new(known_true, HashSet::new(), HashSet::new(), &[]);
        let features = profile.map(|profile| PackageFeatures {
            enabled: profile.features.iter().cloned().collect(),
            excluded: BTreeSet::new(),
        });
        let edition = invocation
            .started
            .manifest_dir
            .as_ref()
            .and_then(|root| {
                inventory.packages.iter().find(|package| {
                    canonical_string(&package.root) == canonical_string(Path::new(root))
                })
            })
            .and_then(|package| package.edition.parse::<Edition>().ok())
            .unwrap_or(Edition::CURRENT);
        let source = analyze_bytes(path.clone(), &bytes, edition, &cfg, features.as_ref());
        let (lines, metrics) = source_totals(&source);
        files.push(GeneratedFileReport {
            path: generated_label(invocation, path, &source_hash),
            source_hash,
            bytes: source.bytes,
            lines,
            metrics,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    GeneratedRescan { files, errors }
}

fn read_verified_source(
    path: &Path,
    source: &rot_compiler_protocol::SourceFile,
) -> Result<Vec<u8>> {
    use md5::Digest as _;

    let bytes = fs::read(path)
        .with_context(|| format!("cannot read compiler-observed source {}", path.display()))?;
    if bytes.len() != source.byte_len as usize {
        bail!(
            "compiler-observed source changed size: expected {} bytes, found {}",
            source.byte_len,
            bytes.len()
        );
    }
    let digest = match source.source_hash_algorithm.as_str() {
        "md5" => md5::Md5::digest(&bytes).to_vec(),
        "sha1" => sha1::Sha1::digest(&bytes).to_vec(),
        "sha256" => sha2::Sha256::digest(&bytes).to_vec(),
        "blake3" => blake3::hash(&bytes).as_bytes().to_vec(),
        algorithm => bail!("unsupported rustc source hash algorithm {algorithm:?}"),
    };
    let actual = format!(
        "{}={}",
        source.source_hash_algorithm,
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    if actual != source.source_hash {
        bail!(
            "compiler-observed source hash changed: expected {}, found {actual}",
            source.source_hash
        );
    }
    Ok(bytes)
}

fn owning_source_keys(invocation: &Invocation) -> HashSet<String> {
    invocation
        .sources
        .iter()
        .filter(|source| !products_for_source(invocation, &source.key.0).is_empty())
        .map(|source| source.key.0.clone())
        .collect()
}

fn products_for_source(invocation: &Invocation, source: &str) -> BTreeSet<Product> {
    let belongs = |span: Option<&rot_compiler_protocol::SourceSpan>| {
        span.is_some_and(|span| span.file.0 == source)
    };
    let mut products = BTreeSet::new();
    if invocation
        .bodies
        .iter()
        .any(|body| belongs(body.span.as_ref()) || belongs(body.attribution_callsite.as_ref()))
    {
        products.insert(Product::HirBodies);
    }
    if invocation.definitions.iter().any(|definition| {
        belongs(definition.span.as_ref()) || belongs(definition.attribution_callsite.as_ref())
    }) || invocation
        .public_bindings
        .iter()
        .any(|binding| belongs(binding.span.as_ref()))
    {
        products.insert(Product::EffectiveApi);
    }
    if invocation
        .references
        .iter()
        .any(|reference| belongs(reference.span.as_ref()))
    {
        products.insert(Product::References);
    }
    if invocation.bodies.iter().any(|body| {
        matches!(
            body.expansion_origin,
            rot_compiler_protocol::ExpansionOrigin::LocalMacro
                | rot_compiler_protocol::ExpansionOrigin::ExternalMacro
        ) && (belongs(body.span.as_ref()) || belongs(body.attribution_callsite.as_ref()))
    }) || invocation.decisions.iter().any(|decision| {
        belongs(decision.generated_span.as_ref()) || belongs(decision.attribution_callsite.as_ref())
    }) {
        products.insert(Product::ExpansionDecisions);
    }
    products
}

fn source_totals(source: &crate::source::LocalFile) -> (LineCounts, ComplexityMetrics) {
    let mut lines = LineCounts::default();
    let mut metrics = ComplexityMetrics::default();
    for line in &source.lines {
        lines.physical += 1;
        match line.kind {
            ContentKind::Code => lines.code += 1,
            ContentKind::Comment => {
                lines.comments += 1;
                lines.docs += u64::from(line.doc);
            }
            ContentKind::Blank => lines.blank += 1,
        }
        metrics.lexical_complexity += line
            .lexical_complexity
            .iter()
            .map(|count| u64::from(*count))
            .sum::<u64>();
    }
    for fact in &source.authored_facts {
        metrics.add(fact.metrics());
    }
    (lines, metrics)
}

fn generated_label(invocation: &Invocation, path: &Path, source_hash: &str) -> String {
    let owner = invocation
        .started
        .package_name
        .as_deref()
        .unwrap_or(&invocation.started.crate_name);
    generated_source_label(owner, path, source_hash)
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

fn locate_driver(cli: &Cli) -> Result<PathBuf> {
    if let Some(explicit) = cli
        .compiler_driver
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
    bail!(
        "rot-rustc-driver was not found; build compiler/rot-rustc-driver or pass --compiler-driver PATH"
    )
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
    unavailable_profile(reason, Vec::new())
}

fn unavailable_profile(reason: String, profile_incompatibilities: Vec<String>) -> Outcome {
    let products = [
        "hir_bodies",
        "effective_api",
        "references",
        "required_visibility",
        "closed_world_liveness",
        "macro_expansion_cyclomatic_delta",
    ]
    .into_iter()
    .map(|product| ProductAvailabilityReport {
        product: product.to_owned(),
        status: SemanticStatus::Unavailable,
        reason: Some(reason.clone()),
    })
    .collect();
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
            products,
            effective_api: None,
            required_visibility: None,
            closed_world: None,
            macro_expansion_complexity: None,
            generated_files: Vec::new(),
        },
        diagnostics: vec![warning(reason)],
        profile_incompatibilities,
    }
}

fn warning(message: String) -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Warning,
        path: None,
        message,
    }
}

fn canonical_string(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, Write};

    use rot_compiler_protocol::{
        CodegenProfile, OptimizationLevel, PanicStrategy, SourceFile, SourceFileKey,
    };
    use tempfile::NamedTempFile;

    use super::{cargo::CargoProfile, cargo_codegen_issues, read_verified_source};

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

    #[test]
    fn generated_rescan_requires_exact_compiler_observed_bytes() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"abc").unwrap();
        file.flush().unwrap();
        let source = SourceFile {
            key: SourceFileKey("source".to_owned()),
            local_path: Some(file.path().to_string_lossy().into_owned()),
            remapped_path: "generated.rs".to_owned(),
            source_hash_algorithm: "sha256".to_owned(),
            source_hash: "sha256=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                .to_owned(),
            byte_len: 3,
            generated: true,
        };

        assert_eq!(read_verified_source(file.path(), &source).unwrap(), b"abc");
        file.as_file_mut().set_len(0).unwrap();
        file.as_file_mut().rewind().unwrap();
        file.write_all(b"abd").unwrap();
        file.flush().unwrap();
        let error = read_verified_source(file.path(), &source).unwrap_err();
        assert!(error.to_string().contains("source hash changed"));
    }
}
