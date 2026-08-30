use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::{DependencyKind, Metadata, MetadataCommand, Package, PackageId};
use cargo_platform::{Cfg, Platform};
use ignore::WalkBuilder;

use crate::{
    cfg::PackageFeatures,
    cli::FastCli,
    model::{
        Activation, Contexts, Diagnostic, DiagnosticSeverity, ProfileReport, Reachability,
        TargetRole,
    },
};

#[cfg(feature = "audit")]
use crate::compiler::SelectedCompiler;

#[derive(Clone, Debug)]
pub struct PackageInfo {
    pub id: PackageId,
    pub name: String,
    pub root: PathBuf,
    pub edition: String,
    pub features: PackageFeatures,
    pub targets: Vec<PackageTargetInfo>,
}

#[derive(Clone, Debug)]
pub struct PackageTargetInfo {
    #[cfg(feature = "audit")]
    pub name: String,
    pub kinds: Vec<String>,
    #[cfg(feature = "audit")]
    pub crate_types: Vec<String>,
    pub source: PathBuf,
    pub required_features: Vec<String>,
    pub test_reachable: bool,
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
enum CompilationContext {
    Host,
    Target,
}

const COMPILATION_CONTEXTS: [CompilationContext; 2] =
    [CompilationContext::Host, CompilationContext::Target];

#[derive(Debug, Default)]
struct ContextFeatureState {
    active: bool,
    active_dependencies: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct FeatureState {
    host: ContextFeatureState,
    target: ContextFeatureState,
    enabled: BTreeSet<String>,
}

impl FeatureState {
    fn context(&self, context: CompilationContext) -> &ContextFeatureState {
        match context {
            CompilationContext::Host => &self.host,
            CompilationContext::Target => &self.target,
        }
    }

    fn context_mut(&mut self, context: CompilationContext) -> &mut ContextFeatureState {
        match context {
            CompilationContext::Host => &mut self.host,
            CompilationContext::Target => &mut self.target,
        }
    }

    fn active(&self) -> bool {
        self.host.active || self.target.active
    }
}

#[derive(Clone, Copy)]
struct FeatureSelection<'a> {
    features: &'a [String],
    all_features: bool,
    no_default_features: bool,
    excluded_features: &'a [String],
}

impl<'a> FeatureSelection<'a> {
    fn fast(cli: &'a FastCli) -> Self {
        Self {
            features: &cli.features,
            all_features: cli.all_features,
            no_default_features: cli.no_default_features,
            excluded_features: &cli.exclude_feature,
        }
    }

    fn mode(self) -> &'static str {
        if self.all_features {
            if self.excluded_features.is_empty() {
                "all"
            } else {
                "all_except"
            }
        } else if self.no_default_features {
            if self.features.is_empty() {
                "none"
            } else {
                "selected_without_defaults"
            }
        } else if self.features.is_empty() {
            "default"
        } else {
            "default_plus_selected"
        }
    }
}

struct RustcCfg {
    known_true: HashSet<String>,
    known_false: HashSet<String>,
    closed_world_names: HashSet<String>,
    target: String,
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
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    pub fn should_report(&self, path: &Path) -> bool {
        requested_contains(&self.requested, path) && self.reportable_sources.contains(path)
    }

    #[cfg(feature = "audit")]
    pub fn selected_package_ids(&self) -> HashSet<String> {
        self.packages
            .iter()
            .filter(|package| {
                self.requested.iter().any(|input| {
                    input.starts_with(&package.root)
                        || input.is_dir() && package.root.starts_with(input)
                })
            })
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
    let cargo_aware = metadata.is_some();
    let rustc_cfg = rustc_cfg(cli)?;
    let feature_selection = FeatureSelection::fast(cli);
    let feature_mode = feature_selection.mode().to_owned();
    let PackageBuild {
        packages,
        mut targets,
        enabled_features,
    } = build_packages(
        metadata.as_ref(),
        feature_selection,
        &requested,
        &rustc_cfg.cargo_platforms,
    )?;
    let synthetic = !feature_selection.excluded_features.is_empty() || !cli.unset_cfg.is_empty();
    for input in requested.iter().filter(|path| {
        path.is_file() && path.extension().is_some_and(|extension| extension == "rs")
    }) {
        if !cargo_aware && !targets.iter().any(|target| target.path == *input) {
            targets.push(TargetSeed {
                path: input.clone(),
                contexts: Contexts::seed(TargetRole::Production, Reachability::BOTH),
            });
        }
    }

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
        target,
        version: rustc,
        preset: cfg_preset,
        cargo_platforms: _,
    } = rustc_cfg;
    let profile = ProfileReport {
        target,
        rustc,
        cfg_preset: cfg_preset.to_owned(),
        cfg_resolution: "requested_target_global".to_owned(),
        feature_mode,
        feature_resolution: "workspace_package_union".to_owned(),
        enabled_features,
        excluded_features: sorted_trimmed(&cli.exclude_feature),
        active_cfg: sorted(&cfg_true),
        forced_cfg: sorted_normalized(&cli.cfg),
        forced_unset_cfg: sorted_normalized(&cli.unset_cfg),
        additional_test_attributes: {
            let mut attributes = cli.test_attribute.clone();
            attributes.sort();
            attributes.dedup();
            attributes
        },
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
    let current_dir = if first.is_dir() {
        first.as_path()
    } else {
        first.parent().unwrap_or(first)
    };
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
    let feature_mode = if cli.all_features {
        "all"
    } else if cli.no_default_features {
        if cli.features.is_empty() {
            "none"
        } else {
            "selected_without_defaults"
        }
    } else if cli.features.is_empty() {
        "default"
    } else {
        "default_plus_selected"
    };
    let profile = ProfileReport {
        target: target.clone(),
        rustc: cli.toolchain.clone(),
        cfg_preset: "cargo".to_owned(),
        cfg_resolution: "cargo_unit_graph".to_owned(),
        feature_mode: feature_mode.to_owned(),
        feature_resolution: "cargo_unit_graph".to_owned(),
        enabled_features: BTreeMap::new(),
        excluded_features: Vec::new(),
        active_cfg: Vec::new(),
        forced_cfg: sorted_normalized(&cli.cfg),
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
    let workspace_members = metadata.workspace_members.iter().collect::<HashSet<_>>();
    metadata
        .packages
        .iter()
        .filter(|package| workspace_members.contains(&package.id))
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
                        required_features: target.required_features.clone(),
                        test_reachable: target.test,
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
        let current_dir = if input.is_dir() {
            input.as_path()
        } else {
            input.parent().unwrap_or(input)
        };
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
    selection: FeatureSelection<'_>,
    requested: &[PathBuf],
    platforms: &CargoPlatforms,
) -> Result<PackageBuild> {
    let Some(metadata) = metadata else {
        return Ok(PackageBuild {
            packages: Vec::new(),
            targets: standalone_seeds(requested),
            enabled_features: BTreeMap::new(),
        });
    };

    let workspace_members = metadata.workspace_members.iter().collect::<HashSet<_>>();
    let workspace_packages = metadata
        .packages
        .iter()
        .filter(|package| workspace_members.contains(&package.id))
        .collect::<Vec<_>>();
    let selectors = selection
        .features
        .iter()
        .map(|selector| FeatureSelector::parse(selector))
        .collect::<Result<Vec<_>>>()?;
    let exclusions = selection
        .excluded_features
        .iter()
        .map(|selector| FeatureSelector::parse(selector))
        .collect::<Result<Vec<_>>>()?;
    let resolved_features = resolve_workspace_features(
        &workspace_packages,
        selection,
        &selectors,
        &exclusions,
        requested,
        platforms,
    )?;

    let mut packages = Vec::with_capacity(workspace_packages.len());
    let mut enabled_features = BTreeMap::new();
    for (package, features) in workspace_packages.iter().zip(resolved_features) {
        let target_settings = TargetSettings::load(package.manifest_path.as_std_path())?;
        enabled_features.insert(
            package.name.to_string(),
            features.enabled.iter().cloned().collect(),
        );
        let manifest = package.manifest_path.as_std_path();
        packages.push(PackageInfo {
            id: package.id.clone(),
            name: package.name.to_string(),
            root: manifest.parent().unwrap_or(manifest).to_path_buf(),
            edition: package.edition.to_string(),
            features,
            targets: package
                .targets
                .iter()
                .map(|target| PackageTargetInfo {
                    #[cfg(feature = "audit")]
                    name: target.name.clone(),
                    kinds: target.kind.iter().map(ToString::to_string).collect(),
                    #[cfg(feature = "audit")]
                    crate_types: target.crate_types.iter().map(ToString::to_string).collect(),
                    source: target.src_path.as_std_path().to_path_buf(),
                    required_features: target.required_features.clone(),
                    test_reachable: target.test || target_settings.bench_enabled(target),
                })
                .collect(),
        });
    }

    let by_id = packages
        .iter()
        .enumerate()
        .map(|(index, package)| (&package.id, index))
        .collect::<HashMap<_, _>>();
    let mut targets = Vec::new();
    for package in workspace_packages {
        let package_index = by_id[&package.id];
        if !requested.iter().any(|input| {
            input.starts_with(&packages[package_index].root)
                || input.is_dir() && packages[package_index].root.starts_with(input)
        }) {
            continue;
        }
        for target in &packages[package_index].targets {
            let path = target.source.clone();
            let enabled = target
                .required_features
                .iter()
                .all(|feature| packages[package_index].features.enabled.contains(feature));
            let (role, mut reachability) = target_context(target);
            if !enabled {
                reachability = Reachability::NEVER;
            }
            targets.push(TargetSeed {
                path,
                contexts: Contexts::seed(role, reachability),
            });
        }
    }

    Ok(PackageBuild {
        packages,
        targets,
        enabled_features,
    })
}

fn standalone_seeds(requested: &[PathBuf]) -> Vec<TargetSeed> {
    requested
        .iter()
        .filter(|path| {
            path.is_file() && path.extension().is_some_and(|extension| extension == "rs")
        })
        .map(|path| TargetSeed {
            path: path.clone(),
            contexts: Contexts::seed(TargetRole::Production, Reachability::BOTH),
        })
        .collect()
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
        let default = target.kind.iter().any(|kind| {
            matches!(
                kind.to_string().as_str(),
                "lib" | "proc-macro" | "bin" | "bench"
            )
        });
        self.target_table(target)
            .and_then(|table| table.get("bench"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(default)
    }

    fn target_table<'a>(
        &'a self,
        target: &cargo_metadata::Target,
    ) -> Option<&'a toml::map::Map<String, toml::Value>> {
        let key = target
            .kind
            .iter()
            .find_map(|kind| match kind.to_string().as_str() {
                "lib" | "proc-macro" => Some("lib"),
                "bin" => Some("bin"),
                "example" => Some("example"),
                "test" => Some("test"),
                "bench" => Some("bench"),
                _ => None,
            })?;
        let value = self.document.get(key)?;
        if key == "lib" {
            return value.as_table();
        }
        value.as_array()?.iter().find_map(|entry| {
            let table = entry.as_table()?;
            let name_matches = table
                .get("name")
                .and_then(toml::Value::as_str)
                .is_some_and(|name| name == target.name);
            let path_matches = table
                .get("path")
                .and_then(toml::Value::as_str)
                .map(|path| self.manifest_dir.join(path))
                .is_some_and(|path| {
                    canonical_path(&path) == canonical_path(target.src_path.as_std_path())
                });
            (name_matches || path_matches).then_some(table)
        })
    }
}

fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn target_context(target: &PackageTargetInfo) -> (TargetRole, Reachability) {
    if target.kinds.iter().any(|kind| kind == "test") {
        return (TargetRole::Test, Reachability::TEST);
    }
    if target.kinds.iter().any(|kind| kind == "bench") {
        return (TargetRole::Bench, Reachability::TEST);
    }
    if target.kinds.iter().any(|kind| kind == "example") {
        return (
            TargetRole::Example,
            Reachability {
                production: Activation::Always,
                test: if target.test_reachable {
                    Activation::Always
                } else {
                    Activation::Never
                },
            },
        );
    }
    if target.kinds.iter().any(|kind| kind == "custom-build") {
        return (TargetRole::Build, Reachability::PRODUCTION);
    }

    let reachability = Reachability {
        production: Activation::Always,
        test: if target.test_reachable {
            Activation::Always
        } else {
            Activation::Never
        },
    };
    (TargetRole::Production, reachability)
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
    selection: FeatureSelection<'_>,
    selectors: &[FeatureSelector],
    exclusions: &[FeatureSelector],
    requested: &[PathBuf],
    platforms: &CargoPlatforms,
) -> Result<Vec<PackageFeatures>> {
    let dependencies = feature_dependencies(packages);
    let selected = packages
        .iter()
        .map(|package| package_selected(package, requested))
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
    validate_unqualified_selectors(packages, &selected, selectors, "--features")?;
    validate_unqualified_selectors(packages, &selected, exclusions, "--exclude-feature")?;
    let qualified_features = resolve_qualified_selectors(
        packages,
        &dependencies,
        &selected_contexts,
        selectors,
        "--features",
        platforms,
    )?;
    let qualified_exclusions = resolve_qualified_selectors(
        packages,
        &dependencies,
        &selected_contexts,
        exclusions,
        "--exclude-feature",
        platforms,
    )?;
    let mut states = (0..packages.len())
        .map(|_| FeatureState::default())
        .collect::<Vec<_>>();

    for (index, package) in packages.iter().enumerate() {
        if selected[index] {
            for context in &selected_contexts[index] {
                states[index].context_mut(*context).active = true;
            }
            if selection.all_features {
                states[index]
                    .enabled
                    .extend(package.features.keys().cloned());
            } else if !selection.no_default_features && package.features.contains_key("default") {
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
        let mut activate_packages = BTreeSet::new();
        let mut activate_dependencies = BTreeSet::new();
        let mut enable_features = BTreeSet::new();

        for (index, package) in packages.iter().enumerate() {
            for context in COMPILATION_CONTEXTS {
                let context_state = states[index].context(context);
                if !context_state.active {
                    continue;
                }

                for dependency in dependencies[index].iter().filter(|dependency| {
                    !dependency.optional && dependency.matches(context, platforms, selected[index])
                }) {
                    activate_dependencies.insert((index, context, dependency.alias.clone()));
                }

                for feature in &states[index].enabled {
                    let Some(members) = package.features.get(feature) else {
                        continue;
                    };
                    for member in members {
                        if let Some(alias) = member.strip_prefix("dep:") {
                            activate_dependencies.insert((index, context, alias.to_owned()));
                            continue;
                        }
                        let Some((dependency, dependency_feature)) = member.split_once('/') else {
                            if package.features.contains_key(member) {
                                enable_features.insert((index, member.clone()));
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
                                    FeatureChanges {
                                        activate_packages: &mut activate_packages,
                                        enable_features: &mut enable_features,
                                    },
                                );
                            }
                        } else {
                            activate_dependencies.insert((index, context, dependency.to_owned()));
                            if package.features.contains_key(dependency)
                                && dependencies[index].iter().any(|candidate| {
                                    candidate.alias == dependency
                                        && candidate.optional
                                        && candidate.matches(context, platforms, selected[index])
                                })
                            {
                                enable_features.insert((index, dependency.to_owned()));
                            }
                            request_dependency_feature(
                                &dependencies[index],
                                context,
                                platforms,
                                selected[index],
                                dependency,
                                dependency_feature,
                                FeatureChanges {
                                    activate_packages: &mut activate_packages,
                                    enable_features: &mut enable_features,
                                },
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
                    let child_context = dependency.child_context(context);
                    activate_packages.insert((target, child_context));
                    if dependency.uses_default_features
                        && packages[target].features.contains_key("default")
                    {
                        enable_features.insert((target, "default".to_owned()));
                    }
                    enable_features.extend(
                        dependency
                            .features
                            .iter()
                            .cloned()
                            .map(|feature| (target, feature)),
                    );
                }
            }
        }

        let mut changed = false;
        for (package, context) in activate_packages {
            changed |= !std::mem::replace(&mut states[package].context_mut(context).active, true);
        }
        for (package, context, dependency) in activate_dependencies {
            changed |= states[package]
                .context_mut(context)
                .active_dependencies
                .insert(dependency);
        }
        for (package, feature) in enable_features {
            changed |= states[package].enabled.insert(feature);
        }
        if !changed {
            break;
        }
    }

    validate_active_exclusions(packages, &states, exclusions, &qualified_exclusions)?;

    let mut excluded_features = (0..packages.len())
        .map(|_| BTreeSet::new())
        .collect::<Vec<_>>();
    for selector in exclusions
        .iter()
        .filter(|selector| selector.package.is_none())
    {
        for (index, package) in packages.iter().enumerate() {
            if selected[index] && package.features.contains_key(&selector.feature) {
                excluded_features[index].insert(selector.feature.clone());
            }
        }
    }
    for (selector, resolution) in exclusions.iter().zip(&qualified_exclusions) {
        if let Some(resolution) = resolution {
            excluded_features[resolution.target].insert(selector.feature.clone());
        }
    }

    Ok(packages
        .iter()
        .enumerate()
        .zip(states)
        .map(|((index, _package), mut state)| {
            let excluded = std::mem::take(&mut excluded_features[index]);
            for feature in &excluded {
                state.enabled.remove(feature);
            }
            PackageFeatures {
                enabled: state.enabled,
                excluded,
            }
        })
        .collect())
}

fn validate_unqualified_selectors(
    packages: &[&Package],
    selected: &[bool],
    selectors: &[FeatureSelector],
    option: &str,
) -> Result<()> {
    for selector in selectors
        .iter()
        .filter(|selector| selector.package.is_none())
    {
        let matched = packages.iter().enumerate().any(|(index, package)| {
            selected[index] && package.features.contains_key(&selector.feature)
        });
        if matched {
            continue;
        }
        let rendered = selector.rendered();
        bail!(
            "{option} selector {rendered:?} does not match a selected PATH root feature; use QUALIFIER/FEATURE for a reachable workspace package or direct dependency"
        );
    }
    Ok(())
}

fn resolve_qualified_selectors(
    packages: &[&Package],
    dependencies: &[Vec<FeatureDependency>],
    selected_contexts: &[BTreeSet<CompilationContext>],
    selectors: &[FeatureSelector],
    option: &str,
    platforms: &CargoPlatforms,
) -> Result<Vec<Option<QualifiedFeature>>> {
    let reachable = dependency_reachability(dependencies, selected_contexts, platforms);
    selectors
        .iter()
        .map(|selector| {
            let Some(qualifier) = selector.package.as_deref() else {
                return Ok(None);
            };
            let package_target = packages.iter().enumerate().find_map(|(index, package)| {
                (!reachable[index].is_empty()
                    && package.name.as_str() == qualifier
                    && package.features.contains_key(&selector.feature))
                .then_some(index)
            });
            let dependency_aliases = selected_contexts
                .iter()
                .enumerate()
                .flat_map(|(parent, contexts)| {
                    contexts.iter().flat_map(move |context| {
                        dependencies[parent]
                            .iter()
                            .filter(move |dependency| {
                                dependency.alias == qualifier
                                    && dependency.matches(*context, platforms, true)
                            })
                            .filter_map(move |dependency| {
                                let target = dependency.target?;
                                packages[target]
                                    .features
                                    .contains_key(&selector.feature)
                                    .then(|| {
                                        (
                                            target,
                                            dependency.child_context(*context),
                                            (parent, *context, dependency.alias.clone()),
                                        )
                                    })
                            })
                        })
                })
                .collect::<Vec<_>>();
            let targets = package_target
                .into_iter()
                .chain(
                    dependency_aliases
                        .iter()
                        .map(|(target, _context, _activation)| *target),
                )
                .collect::<BTreeSet<_>>();
            if targets.is_empty() {
                let rendered = selector.rendered();
                let workspace_match = packages.iter().any(|package| {
                    package.name.as_str() == qualifier
                        && package.features.contains_key(&selector.feature)
                });
                if workspace_match {
                    bail!(
                        "{option} selector {rendered:?} targets a workspace package that is not reachable from the selected PATH roots"
                    );
                }
                bail!("{option} selector {rendered:?} does not match a reachable workspace package feature or selected-root dependency feature");
            }
            if targets.len() != 1 {
                bail!(
                    "{option} selector {:?} is ambiguous between multiple reachable workspace packages",
                    selector.rendered()
                );
            }
            let target = *targets.first().expect("one qualified target");
            let contexts = package_target
                .filter(|candidate| *candidate == target)
                .into_iter()
                .flat_map(|_| reachable[target].iter().copied())
                .chain(dependency_aliases.iter().filter_map(
                    |(candidate, context, _activation)| {
                        (*candidate == target).then_some(*context)
                    },
                ))
                .collect();
            Ok(Some(QualifiedFeature {
                target,
                contexts,
                dependency_aliases: dependency_aliases
                    .into_iter()
                    .filter_map(|(candidate, _context, activation)| {
                        (candidate == target).then_some(activation)
                    })
                    .collect(),
            }))
        })
        .collect()
}

fn validate_active_exclusions(
    packages: &[&Package],
    states: &[FeatureState],
    selectors: &[FeatureSelector],
    resolutions: &[Option<QualifiedFeature>],
) -> Result<()> {
    for (selector, resolution) in selectors.iter().zip(resolutions) {
        if let Some(resolution) = resolution
            && !states[resolution.target].active()
        {
            let rendered = selector.rendered();
            bail!(
                "--exclude-feature selector {rendered:?} targets workspace package {:?}, which is not active in the selected feature profile; exclusions never activate dependencies",
                packages[resolution.target].name,
            );
        }
    }
    Ok(())
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

fn package_selected(package: &Package, requested: &[PathBuf]) -> bool {
    let manifest = package.manifest_path.as_std_path();
    let root = manifest.parent().unwrap_or(manifest);
    requested
        .iter()
        .any(|input| input.starts_with(root) || input.is_dir() && root.starts_with(input))
}

fn package_root_contexts(package: &Package) -> BTreeSet<CompilationContext> {
    let mut contexts = BTreeSet::new();
    for target in &package.targets {
        if target
            .kind
            .iter()
            .any(|kind| kind.to_string() == "proc-macro")
        {
            contexts.insert(CompilationContext::Host);
        } else if !target
            .kind
            .iter()
            .any(|kind| kind.to_string() == "custom-build")
        {
            contexts.insert(CompilationContext::Target);
        }
    }
    if contexts.is_empty() {
        contexts.insert(CompilationContext::Target);
    }
    contexts
}

fn package_is_proc_macro(package: &Package) -> bool {
    package.targets.iter().any(|target| {
        target
            .kind
            .iter()
            .any(|kind| kind.to_string() == "proc-macro")
    })
}

fn feature_dependencies(packages: &[&Package]) -> Vec<Vec<FeatureDependency>> {
    let by_root = packages
        .iter()
        .enumerate()
        .map(|(index, package)| {
            let manifest = package.manifest_path.as_std_path();
            (canonical_path(manifest.parent().unwrap_or(manifest)), index)
        })
        .collect::<HashMap<_, _>>();

    packages
        .iter()
        .map(|package| {
            package
                .dependencies
                .iter()
                .map(|dependency| {
                    let target = dependency
                        .path
                        .as_ref()
                        .and_then(|path| by_root.get(&canonical_path(path.as_std_path())).copied());
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

struct FeatureChanges<'a> {
    activate_packages: &'a mut BTreeSet<(usize, CompilationContext)>,
    enable_features: &'a mut BTreeSet<(usize, String)>,
}

fn request_dependency_feature(
    dependencies: &[FeatureDependency],
    context: CompilationContext,
    platforms: &CargoPlatforms,
    parent_is_selected: bool,
    alias: &str,
    feature: &str,
    changes: FeatureChanges<'_>,
) {
    for dependency in dependencies.iter().filter(|dependency| {
        dependency.alias == alias && dependency.matches(context, platforms, parent_is_selected)
    }) {
        if let Some(target) = dependency.target {
            changes
                .activate_packages
                .insert((target, dependency.child_context(context)));
            changes.enable_features.insert((target, feature.to_owned()));
        }
    }
}

fn discover_sources(requested: &[PathBuf], cli: &FastCli) -> Result<BTreeSet<PathBuf>> {
    let mut sources = BTreeSet::new();
    for input in requested {
        if input.is_file() && input.extension().is_some_and(|extension| extension == "rs") {
            sources.insert(input.clone());
        }
    }

    let directories = requested
        .iter()
        .filter(|input| input.is_dir())
        .cloned()
        .collect::<Vec<_>>();
    if directories.is_empty() {
        return Ok(sources);
    }

    // A positional directory is an explicit discovery boundary. WalkBuilder
    // never filters its depth-zero root, while `parents(false)` prevents
    // ignore files above that root from affecting its descendants.
    for walk_root in directories {
        let mut builder = WalkBuilder::new(&walk_root);
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

    let mut version_command = rustc_command();
    let version_output = version_command
        .arg("-vV")
        .output()
        .context("failed to run rustc -vV")?;
    if !version_output.status.success() {
        return Err(anyhow!(
            "rustc -vV failed: {}",
            String::from_utf8_lossy(&version_output.stderr).trim()
        ));
    }
    let verbose = String::from_utf8(version_output.stdout)
        .context("rustc emitted non-UTF-8 version output")?;
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
    let source_cfg_text = rustc_print_cfg(Some(&target), Some(debug_assertions))?;
    let target_cfg_text = rustc_print_cfg(Some(&target), None)?;
    let host_cfg_text = if target == host {
        target_cfg_text.clone()
    } else {
        rustc_print_cfg(Some(&host), None)?
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
        target: target.clone(),
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

fn rustc_print_cfg(target: Option<&str>, debug_assertions: Option<&str>) -> Result<String> {
    let mut command = rustc_command();
    command.args(["--print", "cfg"]);
    if let Some(debug_assertions) = debug_assertions {
        command.args(["-C", debug_assertions]);
    }
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    let output = command
        .output()
        .context("failed to run rustc --print cfg")?;
    if !output.status.success() {
        bail!(
            "rustc --print cfg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("rustc emitted non-UTF-8 cfg output")
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

fn sorted(values: &HashSet<String>) -> Vec<String> {
    let mut values = values.iter().cloned().collect::<Vec<_>>();
    values.sort();
    values
}

fn sorted_normalized(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| normalize_predicate(value))
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn sorted_trimmed(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_owned())
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
    let first = if first.is_dir() {
        first.clone()
    } else {
        first.parent().unwrap_or(first).to_path_buf()
    };
    first
        .ancestors()
        .find(|ancestor| paths.iter().all(|path| path.starts_with(ancestor)))
        .unwrap_or(&first)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
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
        let workspace_members = metadata.workspace_members.iter().collect::<HashSet<_>>();
        let packages = metadata
            .packages
            .iter()
            .filter(|package| workspace_members.contains(&package.id))
            .collect::<Vec<_>>();
        let selected = ["strong_dependency_feature".to_owned()];
        let excluded = ["strong_dependency_feature".to_owned()];
        let selection = FeatureSelection {
            features: &selected,
            all_features: false,
            no_default_features: true,
            excluded_features: &excluded,
        };
        let selectors = selected
            .iter()
            .map(|value| FeatureSelector::parse(value).unwrap())
            .collect::<Vec<_>>();
        let exclusions = excluded
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
            selection,
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
        let excluded = ["unstable".to_owned()];
        let selection = FeatureSelection {
            features: &[],
            all_features: true,
            no_default_features: false,
            excluded_features: &excluded,
        };

        assert_eq!(selection.mode(), "all_except");
        assert_eq!(
            sorted_trimmed(&[
                "crate_b/unstable".to_owned(),
                " crate_a/unstable ".to_owned(),
                "crate_b/unstable".to_owned(),
            ]),
            ["crate_a/unstable", "crate_b/unstable"]
        );
    }
}
