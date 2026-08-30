use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use cargo_metadata::{DependencyKind, Metadata, MetadataCommand, Package, TargetKind};
use cargo_platform::{Cfg, Platform};
use ignore::WalkBuilder;

use crate::{
    cfg::PackageFeatures,
    cli::FastCli,
    model::{
        Activation, Contexts, Diagnostic, DiagnosticSeverity, ProfileReport, Reachability,
        TargetRole,
    },
    paths::{canonical_or_original, containing_directory, portable},
};

#[cfg(feature = "audit")]
use crate::compiler::SelectedCompiler;
#[cfg(feature = "audit")]
use cargo_metadata::PackageId;

#[derive(Clone, Debug)]
pub struct PackageInfo {
    #[cfg(feature = "audit")]
    pub id: PackageId,
    pub name: String,
    pub root: PathBuf,
    pub edition: String,
    pub features: PackageFeatures,
    #[cfg(feature = "audit")]
    pub targets: Vec<PackageTargetInfo>,
}

#[cfg(feature = "audit")]
#[derive(Clone, Debug)]
pub struct PackageTargetInfo {
    pub name: String,
    pub kinds: Vec<String>,
    pub crate_types: Vec<String>,
    pub source: PathBuf,
}

#[derive(Clone, Debug)]
pub struct TargetSeed {
    pub path: PathBuf,
    pub contexts: Contexts,
}

#[derive(Debug)]
pub struct Inventory {
    pub root: PathBuf,
    pub requested: Vec<PathBuf>,
    pub sources: Vec<PathBuf>,
    reportable_sources: BTreeSet<PathBuf>,
    pub packages: Vec<PackageInfo>,
    pub targets: Vec<TargetSeed>,
    pub cfg_true: HashSet<String>,
    pub cfg_false: HashSet<String>,
    pub cfg_closed_world: HashSet<String>,
    pub profile: ProfileReport,
    #[cfg(feature = "audit")]
    pub audit_target: Option<String>,
    #[cfg(feature = "audit")]
    selected_compiler: Option<SelectedCompiler>,
    pub diagnostics: Vec<Diagnostic>,
}

struct PackageBuild {
    packages: Vec<PackageInfo>,
    targets: Vec<TargetSeed>,
    enabled_features: BTreeMap<String, Vec<String>>,
}

#[derive(Debug)]
struct FeatureDependency {
    alias: String,
    target: Option<usize>,
    kind: DependencyKind,
    platform: Option<Platform>,
    target_is_proc_macro: bool,
    optional: bool,
    uses_default_features: bool,
    features: Vec<String>,
}

impl FeatureDependency {
    fn matches(
        &self,
        parent: CompilationContext,
        platforms: &CargoPlatforms,
        parent_is_selected: bool,
    ) -> bool {
        if self.kind == DependencyKind::Development && !parent_is_selected {
            return false;
        }
        let context = if self.kind == DependencyKind::Build {
            CompilationContext::Host
        } else {
            parent
        };
        let (name, cfg) = match context {
            CompilationContext::Host => (&platforms.host, &platforms.host_cfg),
            CompilationContext::Target => (&platforms.target, &platforms.target_cfg),
        };
        self.platform
            .as_ref()
            .is_none_or(|platform| platform.matches(name, cfg))
    }

    fn child_context(&self, parent: CompilationContext) -> CompilationContext {
        if self.kind == DependencyKind::Build || self.target_is_proc_macro {
            CompilationContext::Host
        } else {
            parent
        }
    }
}

struct CargoPlatforms {
    host: String,
    host_cfg: Vec<Cfg>,
    target: String,
    target_cfg: Vec<Cfg>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(usize)]
enum CompilationContext {
    Host,
    Target,
}

const COMPILATION_CONTEXTS: [CompilationContext; 2] =
    [CompilationContext::Host, CompilationContext::Target];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ContextFeatureState {
    active: bool,
    active_dependencies: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct FeatureState {
    contexts: [ContextFeatureState; 2],
    enabled: BTreeSet<String>,
    excluded: BTreeSet<String>,
}

impl FeatureState {
    fn context(&self, context: CompilationContext) -> &ContextFeatureState {
        &self.contexts[context as usize]
    }

    fn context_mut(&mut self, context: CompilationContext) -> &mut ContextFeatureState {
        &mut self.contexts[context as usize]
    }

    fn active(&self) -> bool {
        self.contexts.iter().any(|context| context.active)
    }
}

struct RustcCfg {
    known_true: HashSet<String>,
    known_false: HashSet<String>,
    closed_world_names: HashSet<String>,
    version: String,
    preset: &'static str,
    cargo_platforms: CargoPlatforms,
}

impl Inventory {
    pub fn package_for(&self, path: &Path) -> Option<&PackageInfo> {
        self.packages
            .iter()
            .filter(|package| path.starts_with(&package.root))
            .max_by_key(|package| package.root.components().count())
    }

    pub fn display_path(&self, path: &Path) -> String {
        portable(path.strip_prefix(&self.root).unwrap_or(path))
    }

    pub fn should_report(&self, path: &Path) -> bool {
        requested_contains(&self.requested, path) && self.reportable_sources.contains(path)
    }

    #[cfg(feature = "audit")]
    pub fn selected_package_ids(&self) -> HashSet<String> {
        self.packages
            .iter()
            .filter(|package| root_selected(&package.root, &self.requested))
            .map(|package| package.id.to_string())
            .collect()
    }

    #[cfg(feature = "audit")]
    pub(crate) fn selected_compiler(&self) -> &SelectedCompiler {
        self.selected_compiler
            .as_ref()
            .expect("audit inventory carries its selected compiler")
    }
}

pub fn inventory(cli: &FastCli) -> Result<Inventory> {
    if cli.threads == Some(0) {
        bail!("--threads must be greater than zero");
    }

    let requested = cli
        .paths
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .with_context(|| format!("cannot resolve input path {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let root = common_root(&requested);
    let mut diagnostics = Vec::new();
    let metadata = load_metadata(&requested, &mut diagnostics)?;
    if metadata.is_none()
        && (cli.all_features
            || cli.no_default_features
            || !cli.features.is_empty()
            || !cli.exclude_feature.is_empty())
    {
        bail!("Cargo feature options require a Cargo workspace or package");
    }
    let rustc_cfg = rustc_cfg(cli)?;
    let feature_mode = cli.feature_mode(!cli.exclude_feature.is_empty());
    let PackageBuild {
        packages,
        targets,
        enabled_features,
    } = build_packages(
        metadata.as_ref(),
        cli,
        &requested,
        &rustc_cfg.cargo_platforms,
    )?;
    let synthetic = !cli.exclude_feature.is_empty() || !cli.unset_cfg.is_empty();
    let reportable_sources = discover_sources(&requested, cli)?;
    let mut sources = reportable_sources.clone();
    for target in &targets {
        if target.path.is_file() {
            sources.insert(target.path.clone());
        }
    }

    let RustcCfg {
        known_true: cfg_true,
        known_false: cfg_false,
        closed_world_names: cfg_closed_world,
        version: rustc,
        preset: cfg_preset,
        cargo_platforms: CargoPlatforms { target, .. },
    } = rustc_cfg;
    let profile = ProfileReport {
        target,
        rustc,
        cfg_preset,
        cfg_resolution: "requested_target_global",
        feature_mode,
        feature_resolution: "workspace_package_union",
        enabled_features,
        excluded_features: sorted(
            cli.exclude_feature
                .iter()
                .map(|value| value.trim().to_owned()),
        ),
        active_cfg: sorted(cfg_true.iter().cloned()),
        forced_cfg: sorted(cli.cfg.iter().map(|value| normalize_predicate(value))),
        forced_unset_cfg: sorted(cli.unset_cfg.iter().map(|value| normalize_predicate(value))),
        additional_test_attributes: sorted(cli.test_attribute.iter().cloned()),
        synthetic,
    };

    Ok(Inventory {
        root,
        requested,
        sources: sources.into_iter().collect(),
        reportable_sources,
        packages,
        targets,
        cfg_true,
        cfg_false,
        cfg_closed_world,
        profile,
        #[cfg(feature = "audit")]
        audit_target: None,
        #[cfg(feature = "audit")]
        selected_compiler: None,
        diagnostics,
    })
}

#[cfg(feature = "audit")]
pub fn audit_inventory(cli: &crate::cli::AuditCli) -> Result<Inventory> {
    let requested = cli
        .paths
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .with_context(|| format!("cannot resolve input path {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let first = requested.first().context("at least one path is required")?;
    let current_dir = containing_directory(first);
    let selected_compiler = crate::compiler::selected_compiler(cli, current_dir)?;
    let metadata = crate::compiler::compiler_metadata(cli, current_dir, false)?;
    let root = fs::canonicalize(metadata.workspace_root.as_std_path())
        .unwrap_or_else(|_| metadata.workspace_root.as_std_path().to_path_buf());
    if let Some(outside) = requested.iter().find(|path| !path.starts_with(&root)) {
        bail!(
            "cannot mix Cargo workspace {} with outside path {}; run rot-audit once per workspace",
            root.display(),
            outside.display(),
        );
    }

    let packages = audit_packages(&metadata);
    let target = crate::compiler::effective_target(cli, &root)?;
    let feature_mode = cli.feature_mode(false);
    let profile = ProfileReport {
        target: target.clone(),
        rustc: cli.toolchain.clone(),
        cfg_preset: "cargo",
        cfg_resolution: "cargo_unit_graph",
        feature_mode,
        feature_resolution: "cargo_unit_graph",
        enabled_features: BTreeMap::new(),
        excluded_features: Vec::new(),
        active_cfg: Vec::new(),
        forced_cfg: sorted(cli.cfg.iter().map(|value| normalize_predicate(value))),
        forced_unset_cfg: Vec::new(),
        additional_test_attributes: Vec::new(),
        synthetic: false,
    };

    Ok(Inventory {
        root,
        requested,
        sources: Vec::new(),
        reportable_sources: BTreeSet::new(),
        packages,
        targets: Vec::new(),
        cfg_true: HashSet::new(),
        cfg_false: HashSet::new(),
        cfg_closed_world: HashSet::new(),
        profile,
        audit_target: Some(target),
        selected_compiler: Some(selected_compiler),
        diagnostics: Vec::new(),
    })
}

#[cfg(feature = "audit")]
fn audit_packages(metadata: &Metadata) -> Vec<PackageInfo> {
    metadata
        .workspace_packages()
        .into_iter()
        .map(|package| {
            let manifest = package.manifest_path.as_std_path();
            PackageInfo {
                id: package.id.clone(),
                name: package.name.to_string(),
                root: manifest.parent().unwrap_or(manifest).to_path_buf(),
                edition: package.edition.to_string(),
                features: PackageFeatures::default(),
                targets: package
                    .targets
                    .iter()
                    .map(|target| PackageTargetInfo {
                        name: target.name.clone(),
                        kinds: target.kind.iter().map(ToString::to_string).collect(),
                        crate_types: target.crate_types.iter().map(ToString::to_string).collect(),
                        source: target.src_path.as_std_path().to_path_buf(),
                    })
                    .collect(),
            }
        })
        .collect()
}

fn load_metadata(
    requested: &[PathBuf],
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<Metadata>> {
    let mut selected: Option<Metadata> = None;
    for input in requested {
        if selected
            .as_ref()
            .is_some_and(|metadata| input.starts_with(metadata.workspace_root.as_std_path()))
        {
            continue;
        }
        let current_dir = containing_directory(input);
        let loaded = ambient_metadata(current_dir);
        match loaded {
            Ok(metadata) => {
                if let Some(existing) = &selected
                    && existing.workspace_root != metadata.workspace_root
                {
                    bail!(
                        "multiple Cargo workspaces are not supported in one report: {} and {}; run rot once per workspace",
                        existing.workspace_root,
                        metadata.workspace_root,
                    );
                }
                selected = Some(metadata);
            }
            Err(error) => {
                if find_manifest(current_dir).is_some() {
                    diagnostics.push(Diagnostic {
                        severity: DiagnosticSeverity::Warning,
                        path: None,
                        message: format!(
                            "Cargo metadata unavailable; using standalone discovery: {error}"
                        ),
                    });
                }
            }
        }
    }

    if let Some(metadata) = &selected {
        let root = fs::canonicalize(metadata.workspace_root.as_std_path())
            .unwrap_or_else(|_| metadata.workspace_root.as_std_path().to_path_buf());
        if let Some(outside) = requested.iter().find(|path| !path.starts_with(&root)) {
            bail!(
                "cannot mix Cargo workspace {} with outside path {}; run rot separately for each source root",
                root.display(),
                outside.display(),
            );
        }
    }
    Ok(selected)
}

fn ambient_metadata(current_dir: &Path) -> Result<Metadata> {
    let mut command = MetadataCommand::new();
    command.current_dir(current_dir).no_deps();
    command.exec().map_err(anyhow::Error::from)
}

fn find_manifest(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|directory| directory.join("Cargo.toml"))
        .find(|manifest| manifest.is_file())
}

fn build_packages(
    metadata: Option<&Metadata>,
    cli: &FastCli,
    requested: &[PathBuf],
    platforms: &CargoPlatforms,
) -> Result<PackageBuild> {
    let Some(metadata) = metadata else {
        return Ok(PackageBuild {
            packages: Vec::new(),
            targets: requested
                .iter()
                .filter(|path| is_rust_file(path))
                .map(|path| TargetSeed {
                    path: path.clone(),
                    contexts: Contexts::production(),
                })
                .collect(),
            enabled_features: BTreeMap::new(),
        });
    };

    let workspace_packages = metadata.workspace_packages();
    let selectors = cli
        .features
        .iter()
        .map(|selector| FeatureSelector::parse(selector))
        .collect::<Result<Vec<_>>>()?;
    let exclusions = cli
        .exclude_feature
        .iter()
        .map(|selector| FeatureSelector::parse(selector))
        .collect::<Result<Vec<_>>>()?;
    let resolved_features = resolve_workspace_features(
        &workspace_packages,
        cli,
        &selectors,
        &exclusions,
        requested,
        platforms,
    )?;

    let mut packages = Vec::with_capacity(workspace_packages.len());
    let mut enabled_features = BTreeMap::new();
    let mut targets = Vec::new();
    for (package, features) in workspace_packages.iter().zip(resolved_features) {
        let manifest = package.manifest_path.as_std_path();
        let root = manifest.parent().unwrap_or(manifest).to_path_buf();
        let target_settings = TargetSettings::load(manifest)?;
        enabled_features.insert(
            package.name.to_string(),
            features.enabled.iter().cloned().collect(),
        );
        let selected = root_selected(&root, requested);
        for target in package.targets.iter().filter(|_| selected) {
            let enabled = target
                .required_features
                .iter()
                .all(|feature| features.enabled.contains(feature));
            let (role, reachability) =
                target_context(target, target.test || target_settings.bench_enabled(target));
            targets.push(TargetSeed {
                path: target.src_path.as_std_path().to_path_buf(),
                contexts: Contexts::seed(
                    role,
                    enabled
                        .then_some(reachability)
                        .unwrap_or(Reachability::NEVER),
                ),
            });
        }
        packages.push(PackageInfo {
            #[cfg(feature = "audit")]
            id: package.id.clone(),
            name: package.name.to_string(),
            root,
            edition: package.edition.to_string(),
            features,
            #[cfg(feature = "audit")]
            targets: package
                .targets
                .iter()
                .map(|target| PackageTargetInfo {
                    name: target.name.clone(),
                    kinds: target.kind.iter().map(ToString::to_string).collect(),
                    crate_types: target.crate_types.iter().map(ToString::to_string).collect(),
                    source: target.src_path.as_std_path().to_path_buf(),
                })
                .collect(),
        });
    }

    Ok(PackageBuild {
        packages,
        targets,
        enabled_features,
    })
}

struct TargetSettings {
    manifest_dir: PathBuf,
    document: toml::Value,
}

impl TargetSettings {
    fn load(manifest: &Path) -> Result<Self> {
        let text = fs::read_to_string(manifest)
            .with_context(|| format!("cannot read Cargo manifest {}", manifest.display()))?;
        let document = toml::from_str(&text)
            .with_context(|| format!("cannot parse Cargo manifest {}", manifest.display()))?;
        Ok(Self {
            manifest_dir: manifest.parent().unwrap_or(manifest).to_path_buf(),
            document,
        })
    }

    fn bench_enabled(&self, target: &cargo_metadata::Target) -> bool {
        self.target_table(target)
            .and_then(|table| table.get("bench"))
            .and_then(toml::Value::as_bool)
            .unwrap_or_else(|| {
                target.is_lib() || target.is_proc_macro() || target.is_bin() || target.is_bench()
            })
    }

    fn target_table<'a>(
        &'a self,
        target: &cargo_metadata::Target,
    ) -> Option<&'a toml::map::Map<String, toml::Value>> {
        let key = target.kind.iter().find_map(|kind| match kind {
            TargetKind::Lib | TargetKind::ProcMacro => Some("lib"),
            TargetKind::Bin => Some("bin"),
            TargetKind::Example => Some("example"),
            TargetKind::Test => Some("test"),
            TargetKind::Bench => Some("bench"),
            _ => None,
        })?;
        let value = self.document.get(key)?;
        if key == "lib" {
            return value.as_table();
        }
        value.as_array()?.iter().find_map(|entry| {
            let table = entry.as_table()?;
            (table.get("name").and_then(toml::Value::as_str) == Some(target.name.as_str())
                || table
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .map(|path| self.manifest_dir.join(path))
                    .is_some_and(|path| {
                        canonical_or_original(&path)
                            == canonical_or_original(target.src_path.as_std_path())
                    }))
            .then_some(table)
        })
    }
}

fn target_context(
    target: &cargo_metadata::Target,
    test_reachable: bool,
) -> (TargetRole, Reachability) {
    let role = match (
        target.is_test(),
        target.is_bench(),
        target.is_example(),
        target.is_custom_build(),
    ) {
        (true, _, _, _) => TargetRole::Test,
        (_, true, _, _) => TargetRole::Bench,
        (_, _, true, _) => TargetRole::Example,
        (_, _, _, true) => TargetRole::Build,
        _ => TargetRole::Production,
    };
    let reachability = match role {
        TargetRole::Test | TargetRole::Bench => Reachability::TEST,
        TargetRole::Build => Reachability::PRODUCTION,
        TargetRole::Production | TargetRole::Example => Reachability {
            production: Activation::Always,
            test: test_reachable.into(),
        },
    };
    (role, reachability)
}

#[derive(Clone, Debug)]
struct FeatureSelector {
    package: Option<String>,
    feature: String,
}

impl FeatureSelector {
    fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            bail!("feature names cannot be empty");
        }
        Ok(match value.split_once('/') {
            Some((package, feature)) if !package.is_empty() && !feature.is_empty() => Self {
                package: Some(package.to_owned()),
                feature: feature.to_owned(),
            },
            Some(_) => {
                bail!("invalid feature selector {value:?}; use FEATURE or QUALIFIER/FEATURE")
            }
            None => Self {
                package: None,
                feature: value.to_owned(),
            },
        })
    }

    fn rendered(&self) -> String {
        self.package.as_ref().map_or_else(
            || self.feature.clone(),
            |package| format!("{package}/{}", self.feature),
        )
    }
}

#[derive(Debug)]
struct QualifiedFeature {
    target: usize,
    contexts: BTreeSet<CompilationContext>,
    dependency_aliases: Vec<(usize, CompilationContext, String)>,
}

fn resolve_workspace_features(
    packages: &[&Package],
    cli: &FastCli,
    selectors: &[FeatureSelector],
    exclusions: &[FeatureSelector],
    requested: &[PathBuf],
    platforms: &CargoPlatforms,
) -> Result<Vec<PackageFeatures>> {
    let dependencies = feature_dependencies(packages);
    let selected = packages
        .iter()
        .map(|package| {
            let manifest = package.manifest_path.as_std_path();
            root_selected(manifest.parent().unwrap_or(manifest), requested)
        })
        .collect::<Vec<_>>();
    let selected_contexts = packages
        .iter()
        .enumerate()
        .map(|(index, package)| {
            if selected[index] {
                package_root_contexts(package)
            } else {
                BTreeSet::new()
            }
        })
        .collect::<Vec<_>>();
    let reachable = dependency_reachability(&dependencies, &selected_contexts, platforms);
    let resolve = |selectors, option| {
        resolve_qualified_selectors(
            packages,
            &dependencies,
            &selected_contexts,
            &reachable,
            selectors,
            option,
            platforms,
        )
    };
    let qualified_features = resolve(selectors, "--features")?;
    let qualified_exclusions = resolve(exclusions, "--exclude-feature")?;
    let mut states = vec![FeatureState::default(); packages.len()];

    for (index, package) in packages.iter().enumerate() {
        if selected[index] {
            for context in &selected_contexts[index] {
                states[index].context_mut(*context).active = true;
            }
            if cli.all_features {
                states[index]
                    .enabled
                    .extend(package.features.keys().cloned());
            } else if !cli.no_default_features && package.features.contains_key("default") {
                states[index].enabled.insert("default".to_owned());
            }
            states[index].enabled.extend(
                selectors
                    .iter()
                    .filter(|selector| {
                        selector.package.is_none()
                            && package.features.contains_key(&selector.feature)
                    })
                    .map(|selector| selector.feature.clone()),
            );
        }
    }
    for (selector, resolution) in selectors.iter().zip(&qualified_features) {
        if let Some(resolution) = resolution {
            states[resolution.target]
                .enabled
                .insert(selector.feature.clone());
            for context in &resolution.contexts {
                states[resolution.target].context_mut(*context).active = true;
            }
            for (parent, context, alias) in &resolution.dependency_aliases {
                states[*parent]
                    .context_mut(*context)
                    .active_dependencies
                    .insert(alias.clone());
            }
        }
    }

    loop {
        let previous = states.clone();
        for (index, package) in packages.iter().enumerate() {
            for context in COMPILATION_CONTEXTS {
                let context_state = previous[index].context(context);
                if !context_state.active {
                    continue;
                }

                states[index]
                    .context_mut(context)
                    .active_dependencies
                    .extend(
                        dependencies[index]
                            .iter()
                            .filter(|dependency| {
                                !dependency.optional
                                    && dependency.matches(context, platforms, selected[index])
                            })
                            .map(|dependency| dependency.alias.clone()),
                    );

                for feature in &previous[index].enabled {
                    let Some(members) = package.features.get(feature) else {
                        continue;
                    };
                    for member in members {
                        if let Some(alias) = member.strip_prefix("dep:") {
                            states[index]
                                .context_mut(context)
                                .active_dependencies
                                .insert(alias.to_owned());
                            continue;
                        }
                        let Some((dependency, dependency_feature)) = member.split_once('/') else {
                            if package.features.contains_key(member) {
                                states[index].enabled.insert(member.clone());
                            }
                            continue;
                        };
                        if let Some(alias) = dependency.strip_suffix('?') {
                            if context_state.active_dependencies.contains(alias) {
                                request_dependency_feature(
                                    &dependencies[index],
                                    context,
                                    platforms,
                                    selected[index],
                                    alias,
                                    dependency_feature,
                                    &mut states,
                                );
                            }
                        } else {
                            states[index]
                                .context_mut(context)
                                .active_dependencies
                                .insert(dependency.to_owned());
                            if package.features.contains_key(dependency)
                                && dependencies[index].iter().any(|candidate| {
                                    candidate.alias == dependency
                                        && candidate.optional
                                        && candidate.matches(context, platforms, selected[index])
                                })
                            {
                                states[index].enabled.insert(dependency.to_owned());
                            }
                            request_dependency_feature(
                                &dependencies[index],
                                context,
                                platforms,
                                selected[index],
                                dependency,
                                dependency_feature,
                                &mut states,
                            );
                        }
                    }
                }

                for dependency in dependencies[index]
                    .iter()
                    .filter(|dependency| dependency.matches(context, platforms, selected[index]))
                {
                    if dependency.optional
                        && !context_state
                            .active_dependencies
                            .contains(&dependency.alias)
                    {
                        continue;
                    }
                    let Some(target) = dependency.target else {
                        continue;
                    };
                    states[target]
                        .context_mut(dependency.child_context(context))
                        .active = true;
                    if dependency.uses_default_features
                        && packages[target].features.contains_key("default")
                    {
                        states[target].enabled.insert("default".to_owned());
                    }
                    states[target]
                        .enabled
                        .extend(dependency.features.iter().cloned());
                }
            }
        }
        if states == previous {
            break;
        }
    }

    for (selector, resolution) in exclusions.iter().zip(&qualified_exclusions) {
        if let Some(resolution) = resolution {
            if !states[resolution.target].active() {
                let rendered = selector.rendered();
                bail!(
                    "--exclude-feature selector {rendered:?} targets workspace package {:?}, which is not active in the selected feature profile; exclusions never activate dependencies",
                    packages[resolution.target].name,
                );
            }
            states[resolution.target]
                .excluded
                .insert(selector.feature.clone());
        } else {
            for (index, package) in packages.iter().enumerate() {
                if selected[index] && package.features.contains_key(&selector.feature) {
                    states[index].excluded.insert(selector.feature.clone());
                }
            }
        }
    }

    Ok(states
        .into_iter()
        .map(|mut state| {
            state
                .enabled
                .retain(|feature| !state.excluded.contains(feature));
            PackageFeatures {
                enabled: state.enabled,
                excluded: state.excluded,
            }
        })
        .collect())
}

fn resolve_qualified_selectors(
    packages: &[&Package],
    dependencies: &[Vec<FeatureDependency>],
    selected_contexts: &[BTreeSet<CompilationContext>],
    reachable: &[BTreeSet<CompilationContext>],
    selectors: &[FeatureSelector],
    option: &str,
    platforms: &CargoPlatforms,
) -> Result<Vec<Option<QualifiedFeature>>> {
    selectors
        .iter()
        .map(|selector| {
            let rendered = selector.rendered();
            let Some(qualifier) = selector.package.as_deref() else {
                if packages.iter().enumerate().any(|(index, package)| {
                    !selected_contexts[index].is_empty()
                        && package.features.contains_key(&selector.feature)
                }) {
                    return Ok(None);
                }
                bail!(
                    "{option} selector {rendered:?} does not match a selected PATH root feature; use QUALIFIER/FEATURE for a reachable workspace package or direct dependency"
                );
            };
            let mut candidates = BTreeMap::<_, (BTreeSet<_>, Vec<_>)>::new();
            for (target, _package) in packages.iter().enumerate().filter(|(target, package)| {
                !reachable[*target].is_empty()
                    && package.name.as_str() == qualifier
                    && package.features.contains_key(&selector.feature)
            }) {
                candidates
                    .entry(target)
                    .or_default()
                    .0
                    .extend(&reachable[target]);
            }
            for (parent, contexts) in selected_contexts.iter().enumerate() {
                for context in contexts {
                    for dependency in dependencies[parent].iter().filter(|dependency| {
                        dependency.alias == qualifier
                            && dependency.matches(*context, platforms, true)
                    }) {
                        let Some(target) = dependency.target.filter(|target| {
                            packages[*target].features.contains_key(&selector.feature)
                        }) else {
                            continue;
                        };
                        let candidate = candidates.entry(target).or_default();
                        candidate.0.insert(dependency.child_context(*context));
                        candidate
                            .1
                            .push((parent, *context, dependency.alias.clone()));
                    }
                }
            }
            if candidates.is_empty() {
                if packages.iter().any(|package| {
                    package.name.as_str() == qualifier
                        && package.features.contains_key(&selector.feature)
                }) {
                    bail!(
                        "{option} selector {rendered:?} targets a workspace package that is not reachable from the selected PATH roots"
                    );
                }
                bail!("{option} selector {rendered:?} does not match a reachable workspace package feature or selected-root dependency feature");
            }
            if candidates.len() != 1 {
                bail!(
                    "{option} selector {rendered:?} is ambiguous between multiple reachable workspace packages"
                );
            }
            let (target, (contexts, dependency_aliases)) =
                candidates.pop_first().expect("one qualified target");
            Ok(Some(QualifiedFeature {
                target,
                contexts,
                dependency_aliases,
            }))
        })
        .collect()
}

fn dependency_reachability(
    dependencies: &[Vec<FeatureDependency>],
    selected_contexts: &[BTreeSet<CompilationContext>],
    platforms: &CargoPlatforms,
) -> Vec<BTreeSet<CompilationContext>> {
    let mut queue = VecDeque::new();
    let mut visited = selected_contexts.to_vec();
    for (index, contexts) in selected_contexts.iter().enumerate() {
        for context in contexts {
            queue.push_back((index, *context));
        }
    }

    while let Some((parent, context)) = queue.pop_front() {
        for dependency in dependencies[parent].iter().filter(|dependency| {
            dependency.matches(context, platforms, !selected_contexts[parent].is_empty())
        }) {
            let Some(child) = dependency.target else {
                continue;
            };
            let child_context = dependency.child_context(context);
            if visited[child].insert(child_context) {
                queue.push_back((child, child_context));
            }
        }
    }
    visited
}

fn root_selected(root: &Path, requested: &[PathBuf]) -> bool {
    requested
        .iter()
        .any(|input| input.starts_with(root) || input.is_dir() && root.starts_with(input))
}

fn is_rust_file(path: &Path) -> bool {
    path.is_file() && path.extension().is_some_and(|extension| extension == "rs")
}

fn package_root_contexts(package: &Package) -> BTreeSet<CompilationContext> {
    let mut contexts = BTreeSet::new();
    for target in &package.targets {
        if target.is_proc_macro() {
            contexts.insert(CompilationContext::Host);
        } else if !target.is_custom_build() {
            contexts.insert(CompilationContext::Target);
        }
    }
    if contexts.is_empty() {
        contexts.insert(CompilationContext::Target);
    }
    contexts
}

fn package_is_proc_macro(package: &Package) -> bool {
    package.targets.iter().any(|target| target.is_proc_macro())
}

fn feature_dependencies(packages: &[&Package]) -> Vec<Vec<FeatureDependency>> {
    let by_root = packages
        .iter()
        .enumerate()
        .map(|(index, package)| {
            let manifest = package.manifest_path.as_std_path();
            (
                canonical_or_original(manifest.parent().unwrap_or(manifest)),
                index,
            )
        })
        .collect::<HashMap<_, _>>();

    packages
        .iter()
        .map(|package| {
            package
                .dependencies
                .iter()
                .map(|dependency| {
                    let target = dependency.path.as_ref().and_then(|path| {
                        by_root
                            .get(&canonical_or_original(path.as_std_path()))
                            .copied()
                    });
                    FeatureDependency {
                        alias: dependency
                            .rename
                            .clone()
                            .unwrap_or_else(|| dependency.name.clone()),
                        target,
                        kind: dependency.kind,
                        platform: dependency.target.clone(),
                        target_is_proc_macro: target
                            .is_some_and(|index| package_is_proc_macro(packages[index])),
                        optional: dependency.optional,
                        uses_default_features: dependency.uses_default_features,
                        features: dependency.features.clone(),
                    }
                })
                .collect()
        })
        .collect()
}

fn request_dependency_feature(
    dependencies: &[FeatureDependency],
    context: CompilationContext,
    platforms: &CargoPlatforms,
    parent_is_selected: bool,
    alias: &str,
    feature: &str,
    states: &mut [FeatureState],
) {
    for dependency in dependencies.iter().filter(|dependency| {
        dependency.alias == alias && dependency.matches(context, platforms, parent_is_selected)
    }) {
        if let Some(target) = dependency.target {
            states[target]
                .context_mut(dependency.child_context(context))
                .active = true;
            states[target].enabled.insert(feature.to_owned());
        }
    }
}

fn discover_sources(requested: &[PathBuf], cli: &FastCli) -> Result<BTreeSet<PathBuf>> {
    let mut sources = requested
        .iter()
        .filter(|input| is_rust_file(input))
        .cloned()
        .collect::<BTreeSet<_>>();

    // A positional directory is an explicit discovery boundary. WalkBuilder
    // never filters its depth-zero root, while `parents(false)` prevents
    // ignore files above that root from affecting its descendants.
    for walk_root in requested.iter().filter(|input| input.is_dir()) {
        let mut builder = WalkBuilder::new(walk_root);
        builder
            .parents(false)
            .hidden(!cli.hidden)
            .ignore(!cli.no_ignore)
            .git_ignore(!cli.no_ignore)
            .require_git(false)
            .git_global(!cli.no_ignore)
            .git_exclude(!cli.no_ignore)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git");
        for entry in builder.build() {
            let entry = entry.with_context(|| format!("cannot walk {}", walk_root.display()))?;
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "rs")
                && requested_contains(requested, entry.path())
            {
                sources.insert(entry.into_path());
            }
        }
    }

    Ok(sources)
}

fn rustc_cfg(cli: &FastCli) -> Result<RustcCfg> {
    let forced_true = cli
        .cfg
        .iter()
        .map(|value| normalize_predicate(value))
        .collect::<HashSet<_>>();
    let forced_false = cli
        .unset_cfg
        .iter()
        .map(|value| normalize_predicate(value))
        .collect::<HashSet<_>>();
    if let Some(conflict) = forced_true.intersection(&forced_false).next() {
        bail!("cfg predicate {conflict:?} is both enabled and disabled explicitly");
    }

    let verbose = rustc_text(rustc_command().arg("-vV"), "rustc -vV", "version")?;
    let rustc = verbose
        .lines()
        .next()
        .unwrap_or("rustc (unknown)")
        .to_owned();
    let host = verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown-host")
        .to_owned();
    let target = cli.target.clone().unwrap_or_else(|| host.clone());
    let (preset, debug_assertions) = if cli.release {
        ("release", "debug-assertions=no")
    } else {
        ("dev", "debug-assertions=yes")
    };
    let source_cfg_text = rustc_print_cfg(&target, Some(debug_assertions))?;
    let target_cfg_text = rustc_print_cfg(&target, None)?;
    let host_cfg_text = if target == host {
        target_cfg_text.clone()
    } else {
        rustc_print_cfg(&host, None)?
    };
    let target_cfg = cargo_dependency_cfg(&target_cfg_text)?;
    let host_cfg = cargo_dependency_cfg(&host_cfg_text)?;
    let mut known_true = source_cfg_text
        .lines()
        .map(normalize_predicate)
        .collect::<HashSet<_>>();
    let closed_world_names = known_true
        .iter()
        .map(|predicate| {
            predicate
                .split_once('=')
                .map_or_else(|| predicate.clone(), |(name, _)| name.to_owned())
        })
        .collect();
    for predicate in &forced_false {
        known_true.remove(predicate);
    }
    known_true.extend(forced_true);

    Ok(RustcCfg {
        known_true,
        known_false: forced_false,
        closed_world_names,
        version: rustc,
        preset,
        cargo_platforms: CargoPlatforms {
            host,
            host_cfg,
            target,
            target_cfg,
        },
    })
}

fn rustc_print_cfg(target: &str, debug_assertions: Option<&str>) -> Result<String> {
    let mut command = rustc_command();
    command.args(["--print", "cfg", "--target", target]);
    if let Some(debug_assertions) = debug_assertions {
        command.args(["-C", debug_assertions]);
    }
    rustc_text(&mut command, "rustc --print cfg", "cfg")
}

fn rustc_text(command: &mut Command, action: &str, output_name: &str) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("failed to run {action}"))?;
    if !output.status.success() {
        bail!(
            "{action} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)
        .with_context(|| format!("rustc emitted non-UTF-8 {output_name} output"))
}

fn cargo_dependency_cfg(output: &str) -> Result<Vec<Cfg>> {
    output
        .lines()
        .map(|line| {
            line.parse::<Cfg>()
                .with_context(|| format!("cannot parse rustc target cfg {line:?}"))
        })
        .collect()
}

fn rustc_command() -> Command {
    std::env::var_os("RUSTC").map_or_else(|| Command::new("rustc"), Command::new)
}

fn sorted(values: impl IntoIterator<Item = String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_predicate(value: &str) -> String {
    let compact = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    match compact.split_once('=') {
        Some((name, raw_value)) => format!("{name}={}", raw_value.trim_matches('"')),
        None => compact,
    }
}

fn requested_contains(requested: &[PathBuf], path: &Path) -> bool {
    requested.iter().any(|input| {
        if input.is_dir() {
            path.starts_with(input)
        } else {
            path == input
        }
    })
}

fn common_root(paths: &[PathBuf]) -> PathBuf {
    let Some(first) = paths.first() else {
        return PathBuf::from(".");
    };
    let first = containing_directory(first).to_path_buf();
    first
        .ancestors()
        .find(|ancestor| paths.iter().all(|path| path.starts_with(ancestor)))
        .unwrap_or(&first)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn cfg_predicates_are_normalized() {
        assert_eq!(
            normalize_predicate("target_os = \"linux\""),
            "target_os=linux"
        );
        assert_eq!(normalize_predicate("unix"), "unix");
    }

    #[test]
    fn feature_exclusion_is_applied_after_feature_closure() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace");
        let metadata = ambient_metadata(&fixture).expect("load fixture metadata");
        let packages = metadata.workspace_packages();
        let cli = FastCli::parse_from([
            "rot",
            "--no-default-features",
            "--features",
            "strong_dependency_feature",
            "--exclude-feature",
            "strong_dependency_feature",
        ]);
        let selectors = cli
            .features
            .iter()
            .map(|value| FeatureSelector::parse(value).unwrap())
            .collect::<Vec<_>>();
        let exclusions = cli
            .exclude_feature
            .iter()
            .map(|value| FeatureSelector::parse(value).unwrap())
            .collect::<Vec<_>>();

        let platforms = CargoPlatforms {
            host: "test-host".to_owned(),
            host_cfg: Vec::new(),
            target: "test-target".to_owned(),
            target_cfg: Vec::new(),
        };
        let features = resolve_workspace_features(
            &packages,
            &cli,
            &selectors,
            &exclusions,
            &[fixture],
            &platforms,
        )
        .unwrap()
        .into_iter()
        .next()
        .expect("fixture features");

        assert!(!features.enabled.contains("strong_dependency_feature"));
        assert!(features.excluded.contains("strong_dependency_feature"));
        assert!(
            features.enabled.contains("fixture-helper"),
            "excluding a feature predicate must not reverse its resolved implications"
        );
    }

    #[test]
    fn all_features_with_exclusions_has_an_explicit_mode() {
        let cli = FastCli::parse_from(["rot", "--all-features", "--exclude-feature", "unstable"]);

        assert_eq!(cli.feature_mode(true), "all_except");
        assert_eq!(
            sorted(
                ["crate_b/unstable", " crate_a/unstable ", "crate_b/unstable"]
                    .map(|value| value.trim().to_owned()),
            ),
            ["crate_a/unstable", "crate_b/unstable"]
        );
    }
}
