use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::{Metadata, MetadataCommand, Package, PackageId};
use ignore::WalkBuilder;

use crate::{
    cfg::PackageFeatures,
    cli::FastCli,
    model::{
        Activation, Contexts, Diagnostic, DiagnosticSeverity, ProfileReport, Reachability,
        TargetRole,
    },
};

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
    pub packages: Vec<PackageInfo>,
    pub targets: Vec<TargetSeed>,
    pub cfg_true: HashSet<String>,
    pub cfg_false: HashSet<String>,
    pub cfg_closed_world: HashSet<String>,
    pub profile: ProfileReport,
    #[cfg(feature = "audit")]
    pub audit_target: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

struct PackageBuild {
    packages: Vec<PackageInfo>,
    targets: Vec<TargetSeed>,
    enabled_features: BTreeMap<String, Vec<String>>,
    synthetic: bool,
}

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
}

struct RustcCfg {
    known_true: HashSet<String>,
    known_false: HashSet<String>,
    closed_world_names: HashSet<String>,
    target: String,
    version: String,
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
        requested_contains(&self.requested, path)
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
    let PackageBuild {
        packages,
        mut targets,
        enabled_features,
        synthetic,
    } = build_packages(
        metadata.as_ref(),
        FeatureSelection::fast(cli),
        &requested,
        &mut diagnostics,
    )?;
    let synthetic = synthetic || !cli.unset_cfg.is_empty();
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

    let mut sources = discover_sources(&requested, cli)?;
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
    } = rustc_cfg(cli)?;
    let feature_mode = if cli.all_features {
        "all".to_owned()
    } else if cli.no_default_features {
        if cli.features.is_empty() {
            "none".to_owned()
        } else {
            "selected_without_defaults".to_owned()
        }
    } else if cli.features.is_empty() {
        "default".to_owned()
    } else {
        "default_plus_selected".to_owned()
    };

    let profile = ProfileReport {
        target,
        rustc,
        feature_mode,
        enabled_features,
        excluded_features: cli.exclude_feature.clone(),
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
        packages,
        targets,
        cfg_true,
        cfg_false,
        cfg_closed_world,
        profile,
        #[cfg(feature = "audit")]
        audit_target: None,
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
    let metadata = crate::compiler::pinned_metadata(cli, current_dir, false)?;
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
        rustc: rot_compiler_protocol::PINNED_RUSTC_RELEASE.to_owned(),
        feature_mode: feature_mode.to_owned(),
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
        packages,
        targets: Vec::new(),
        cfg_true: HashSet::new(),
        cfg_false: HashSet::new(),
        cfg_closed_world: HashSet::new(),
        profile,
        audit_target: Some(target),
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
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<PackageBuild> {
    let Some(metadata) = metadata else {
        return Ok(PackageBuild {
            packages: Vec::new(),
            targets: standalone_seeds(requested),
            enabled_features: BTreeMap::new(),
            synthetic: false,
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
    validate_feature_selectors(&workspace_packages, &selectors, "--features")?;
    validate_feature_selectors(&workspace_packages, &exclusions, "--exclude-feature")?;

    let mut packages = Vec::with_capacity(workspace_packages.len());
    let mut enabled_features = BTreeMap::new();
    let mut synthetic = false;
    for package in &workspace_packages {
        let target_settings = TargetSettings::load(package.manifest_path.as_std_path())?;
        let (features, broken) = resolve_features(package, &selection, &selectors, &exclusions);
        for message in broken {
            synthetic = true;
            diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                path: Some(package.manifest_path.to_string()),
                message,
            });
        }
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
        synthetic,
    })
}

fn validate_feature_selectors(
    packages: &[&Package],
    selectors: &[FeatureSelector],
    option: &str,
) -> Result<()> {
    for selector in selectors {
        let matched = packages.iter().any(|package| {
            selector
                .package
                .as_ref()
                .is_none_or(|name| name == package.name.as_str())
                && package.features.contains_key(&selector.feature)
        });
        if !matched {
            let rendered = selector.package.as_ref().map_or_else(
                || selector.feature.clone(),
                |package| format!("{package}/{}", selector.feature),
            );
            bail!("{option} selector {rendered:?} does not match a workspace feature");
        }
    }
    Ok(())
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
            Some(_) => bail!("invalid feature selector {value:?}; use FEATURE or PACKAGE/FEATURE"),
            None => Self {
                package: None,
                feature: value.to_owned(),
            },
        })
    }

    fn matches(&self, package: &Package, feature: &str) -> bool {
        self.feature == feature
            && self
                .package
                .as_ref()
                .is_none_or(|name| name == package.name.as_str())
    }
}

fn resolve_features(
    package: &Package,
    selection: &FeatureSelection<'_>,
    selectors: &[FeatureSelector],
    exclusions: &[FeatureSelector],
) -> (PackageFeatures, Vec<String>) {
    let mut enabled = BTreeSet::new();
    let mut queue = VecDeque::new();
    if selection.all_features {
        queue.extend(package.features.keys().cloned());
    } else {
        if !selection.no_default_features && package.features.contains_key("default") {
            queue.push_back("default".to_owned());
        }
        queue.extend(
            selectors
                .iter()
                .filter(|selector| {
                    selector
                        .package
                        .as_ref()
                        .is_some_and(|name| name == package.name.as_str())
                        || selector.package.is_none()
                            && package.features.contains_key(&selector.feature)
                })
                .map(|selector| selector.feature.clone()),
        );
    }

    while let Some(feature) = queue.pop_front() {
        if !enabled.insert(feature.clone()) {
            continue;
        }
        let Some(members) = package.features.get(&feature) else {
            continue;
        };
        for member in members {
            if let Some(local) = activated_local_feature(member, &package.features) {
                queue.push_back(local.to_owned());
            }
        }
    }

    let excluded = package
        .features
        .keys()
        .filter(|feature| {
            exclusions
                .iter()
                .any(|selector| selector.matches(package, feature))
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut broken = Vec::new();
    for feature in &excluded {
        if enabled.remove(feature) {
            broken.push(format!(
                "feature {feature:?} was enabled by the selected Cargo profile and is now hard-excluded"
            ));
        }
    }
    for parent in &enabled {
        if let Some(members) = package.features.get(parent) {
            for excluded_feature in &excluded {
                if members.iter().any(|member| member == excluded_feature) {
                    broken.push(format!(
                        "enabled feature {parent:?} implies excluded feature {excluded_feature:?}; profile is synthetic"
                    ));
                }
            }
        }
    }

    (PackageFeatures { enabled, excluded }, broken)
}

fn activated_local_feature<'a>(
    member: &'a str,
    features: &BTreeMap<String, Vec<String>>,
) -> Option<&'a str> {
    if member.starts_with("dep:") {
        return None;
    }
    let feature = match member.split_once('/') {
        Some((dependency, _)) if !dependency.ends_with('?') => dependency,
        Some(_) => return None,
        None => member,
    };
    features.contains_key(feature).then_some(feature)
}

fn discover_sources(requested: &[PathBuf], cli: &FastCli) -> Result<BTreeSet<PathBuf>> {
    let mut sources = BTreeSet::new();
    for input in requested {
        if input.is_file() {
            if input.extension().is_some_and(|extension| extension == "rs") {
                sources.insert(input.clone());
            }
            continue;
        }

        let mut builder = WalkBuilder::new(input);
        builder
            .hidden(!cli.hidden)
            .ignore(!cli.no_ignore)
            .git_ignore(!cli.no_ignore)
            .git_global(!cli.no_ignore)
            .git_exclude(!cli.no_ignore)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git");
        for entry in builder.build() {
            let entry = entry.with_context(|| format!("cannot walk {}", input.display()))?;
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "rs")
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

    let mut command = rustc_command();
    command.args(["--print", "cfg"]);
    let requested_target = cli.target.as_deref();
    if let Some(target) = requested_target {
        command.args(["--target", target]);
    }
    let output = command
        .output()
        .context("failed to run rustc --print cfg")?;
    if !output.status.success() {
        return Err(anyhow!(
            "rustc --print cfg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout).context("rustc emitted non-UTF-8 cfg output")?;
    let mut known_true = stdout
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
    let target = requested_target.map(str::to_owned).unwrap_or(host);
    Ok(RustcCfg {
        known_true,
        known_false: forced_false,
        closed_world_names,
        target,
        version: rustc,
    })
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
}
