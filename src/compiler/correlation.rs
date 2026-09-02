use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    compiler::{
        cargo::{CargoArtifact, CargoFailure, CargoProfile, CargoRun},
        profile::{CompilerProfile, ExpectedUnit},
        sidecar::Invocation,
    },
    model::CompilerTargetReport,
    workspace::{AuditInventory, PackageInfo, PackageTargetInfo},
};

pub struct CorrelatedInvocation {
    pub invocation: Invocation,
    pub target: Option<CompilerTargetReport>,
    pub cargo_features: Option<Vec<String>>,
    pub cargo_profile: Option<CargoProfile>,
    pub issue: Option<String>,
}

pub struct Correlation {
    pub expected: usize,
    pub correlated: usize,
    pub invocations: Vec<CorrelatedInvocation>,
    pub errors: Vec<String>,
    pub missing_invoked_sidecars: usize,
}

pub fn correlate(
    mut invocations: Vec<Invocation>,
    cargo: &CargoRun,
    inventory: &AuditInventory,
    compiler_profile: &CompilerProfile,
) -> Correlation {
    let selected = inventory.selected_package_ids();
    let mut errors = Vec::new();
    let expected = expected_units(compiler_profile, &selected);
    let artifacts = cargo
        .artifacts
        .iter()
        .filter(|artifact| selected.contains(&artifact.package_id))
        .collect::<Vec<_>>();
    let package_by_id = inventory
        .packages
        .iter()
        .map(|package| (package.id.to_string(), package))
        .collect::<HashMap<_, _>>();
    let selected_roots = inventory
        .packages
        .iter()
        .filter(|package| selected.contains(&package.id.to_string()))
        .map(|package| canonical(&package.root))
        .collect::<HashSet<_>>();

    invocations.retain(|invocation| {
        invocation
            .started
            .manifest_dir
            .as_ref()
            .is_some_and(|path| selected_roots.contains(&canonical(Path::new(path))))
    });

    let local_id_counts = counts(invocations.iter().map(|invocation| invocation.id.0.clone()));
    let merge_key_counts = counts(
        invocations
            .iter()
            .map(|invocation| invocation.started.merge_key.0.clone()),
    );

    let mut used_artifacts = HashSet::new();
    let mut correlated = Vec::with_capacity(invocations.len());
    for invocation in invocations {
        let mut identity_issue = None;
        if local_id_counts[&invocation.id.0] > 1 {
            add_issue(
                &mut identity_issue,
                &format!(
                    "duplicate selected compiler invocation ID {}",
                    invocation.id.0
                ),
            );
        }
        if merge_key_counts[&invocation.started.merge_key.0] > 1 {
            add_issue(
                &mut identity_issue,
                &format!(
                    "duplicate selected compiler merge key {}",
                    invocation.started.merge_key.0
                ),
            );
        }
        let matches = artifacts
            .iter()
            .enumerate()
            .filter(|(_, artifact)| artifact_matches(&invocation, artifact))
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            let (index, artifact) = matches[0];
            let mut issue = identity_issue;
            if !used_artifacts.insert(index) {
                add_issue(
                    &mut issue,
                    &format!(
                        "multiple compiler sidecars match Cargo artifact {}:{}",
                        artifact.package_id, artifact.target_name
                    ),
                );
            }
            let unit_key = UnitKey::from_artifact(artifact, &invocation.started);
            if !expected.contains_key(&unit_key) {
                add_issue(
                    &mut issue,
                    &format!(
                        "Cargo artifact is absent from the selected unit graph: {}",
                        unit_key.render()
                    ),
                );
            }
            if let Some(issue) = &issue {
                errors.push(format!("{}: {issue}", invocation.started.merge_key.0));
            }
            let target = target_report(artifact, &invocation.started);
            correlated.push(CorrelatedInvocation {
                invocation,
                target: Some(target),
                cargo_features: Some(artifact.features.clone()),
                cargo_profile: Some(artifact.profile.clone()),
                issue,
            });
            continue;
        }

        let metadata_target = failed_target(&invocation, &cargo.failures, &package_by_id);
        let cargo_features = metadata_target.as_ref().and_then(|target| {
            expected_features_for_target(&expected, target, &invocation.started)
        });
        let correlation_issue = if matches.is_empty() {
            if metadata_target.is_some() && !invocation.finished.rustc_success {
                "failed rustc invocation has no Cargo artifact".to_owned()
            } else {
                "compiler sidecar does not match a unique Cargo artifact".to_owned()
            }
        } else {
            format!(
                "compiler sidecar ambiguously matches {} Cargo artifacts",
                matches.len()
            )
        };
        let mut issue = identity_issue;
        add_issue(&mut issue, &correlation_issue);
        if let Some(issue) = &issue {
            errors.push(format!("{}: {issue}", invocation.started.merge_key.0));
        }
        correlated.push(CorrelatedInvocation {
            invocation,
            target: metadata_target,
            cargo_features,
            cargo_profile: None,
            issue,
        });
    }

    let mut missing_artifact_sidecars = 0;
    for (index, artifact) in artifacts.iter().enumerate() {
        if !used_artifacts.contains(&index) {
            missing_artifact_sidecars += 1;
            errors.push(format!(
                "Cargo artifact has no matching compiler sidecar: {}:{} ({})",
                artifact.package_id,
                artifact.target_name,
                if artifact.fresh { "fresh" } else { "built" }
            ));
        }
    }

    let mut invocation_indices = BTreeMap::<UnitKey, Vec<usize>>::new();
    for (index, invocation) in correlated.iter_mut().enumerate() {
        let Some(key) = UnitKey::from_correlated(invocation) else {
            continue;
        };
        if expected.contains_key(&key) {
            invocation_indices.entry(key).or_default().push(index);
        } else {
            errors.push(format!(
                "compiler sidecar mapped to an unexpected Cargo unit: {}",
                key.render()
            ));
            add_issue(
                &mut invocation.issue,
                "compiler sidecar mapped outside the selected unit graph",
            );
        }
    }

    let mut uniquely_correlated = 0;
    for (key, expected_count) in &expected {
        let indices = invocation_indices
            .get(key)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if indices.len() == *expected_count {
            uniquely_correlated += indices.len();
        } else {
            let issue = format!(
                "selected Cargo unit graph expects {expected_count} compiler sidecar(s), observed {}: {}",
                indices.len(),
                key.render()
            );
            errors.push(issue.clone());
            for &index in indices {
                add_issue(&mut correlated[index].issue, &issue);
            }
        }
    }

    let missing_failed_sidecars = expected
        .iter()
        .filter(|(key, _)| {
            cargo
                .failures
                .iter()
                .any(|failure| failure_matches_target(failure, key))
        })
        .map(|(key, expected_count)| {
            expected_count.saturating_sub(
                invocation_indices
                    .get(key)
                    .map_or(0, |indices| indices.len()),
            )
        })
        .sum::<usize>();

    errors.sort();
    errors.dedup();
    correlated.sort_by(|left, right| {
        left.invocation
            .started
            .merge_key
            .cmp(&right.invocation.started.merge_key)
    });
    Correlation {
        expected: expected.values().sum(),
        correlated: uniquely_correlated,
        invocations: correlated,
        errors,
        missing_invoked_sidecars: missing_artifact_sidecars + missing_failed_sidecars,
    }
}

fn expected_features_for_target(
    expected: &BTreeMap<UnitKey, usize>,
    target: &CompilerTargetReport,
    started: &rot_compiler_protocol::InvocationStarted,
) -> Option<Vec<String>> {
    let matches = expected
        .keys()
        .filter(|key| key.matches_target(target, started))
        .map(|key| key.features.clone())
        .collect::<BTreeSet<_>>();
    (matches.len() == 1).then(|| matches.into_iter().next().expect("one feature set"))
}

fn counts<T: Ord>(values: impl IntoIterator<Item = T>) -> BTreeMap<T, usize> {
    let mut counts = BTreeMap::new();
    for value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    counts
}

fn expected_units(
    compiler_profile: &CompilerProfile,
    selected: &HashSet<String>,
) -> BTreeMap<UnitKey, usize> {
    counts(
        compiler_profile
            .expected_units()
            .iter()
            .filter(|unit| selected.contains(&unit.package_id))
            .map(UnitKey::from_expected),
    )
}

fn failure_matches_target(failure: &CargoFailure, target: &UnitKey) -> bool {
    failure.package_id == target.package_id
        && failure.target_name == target.name
        && canonical(&failure.target_source) == target.source
        && sorted(failure.target_kinds.clone()) == target.kinds
        && sorted(failure.target_crate_types.clone()) == target.crate_types
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct UnitKey {
    package_id: String,
    name: String,
    source: PathBuf,
    kinds: Vec<String>,
    crate_types: Vec<String>,
    features: Vec<String>,
    platform: Option<String>,
}

impl UnitKey {
    fn from_expected(unit: &ExpectedUnit) -> Self {
        Self {
            package_id: unit.package_id.clone(),
            name: unit.target_name.clone(),
            source: canonical(&unit.target_source),
            kinds: sorted(unit.target_kinds.clone()),
            crate_types: sorted(unit.target_crate_types.clone()),
            features: sorted(unit.features.clone()),
            platform: unit.platform.clone(),
        }
    }

    fn from_artifact(
        artifact: &CargoArtifact,
        started: &rot_compiler_protocol::InvocationStarted,
    ) -> Self {
        Self {
            package_id: artifact.package_id.clone(),
            name: artifact.target_name.clone(),
            source: canonical(&artifact.target_source),
            kinds: sorted(artifact.target_kinds.clone()),
            crate_types: sorted(artifact.target_crate_types.clone()),
            features: sorted(artifact.features.clone()),
            platform: compilation_platform(started),
        }
    }

    fn from_correlated(invocation: &CorrelatedInvocation) -> Option<Self> {
        let target = invocation.target.as_ref()?;
        let features = invocation.cargo_features.clone().or_else(|| {
            invocation
                .invocation
                .profile
                .as_ref()
                .map(|profile| profile.features.clone())
        })?;
        Some(Self {
            package_id: target.package_id.clone(),
            name: target.name.clone(),
            source: canonical(Path::new(&target.source)),
            kinds: sorted(target.kinds.clone()),
            crate_types: sorted(target.crate_types.clone()),
            features: sorted(features),
            platform: compilation_platform(&invocation.invocation.started),
        })
    }

    fn render(&self) -> String {
        format!(
            "{}:{} [kinds={}; crate_types={}; features={}; platform={}] at {}",
            self.package_id,
            self.name,
            self.kinds.join(","),
            self.crate_types.join(","),
            self.features.join(","),
            self.platform.as_deref().unwrap_or("host"),
            self.source.display()
        )
    }

    fn matches_target(
        &self,
        target: &CompilerTargetReport,
        started: &rot_compiler_protocol::InvocationStarted,
    ) -> bool {
        self.package_id == target.package_id
            && self.name == target.name
            && self.source == canonical(Path::new(&target.source))
            && self.kinds == sorted(target.kinds.clone())
            && self.crate_types == sorted(target.crate_types.clone())
            && self.platform == compilation_platform(started)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TargetKey {
    package_id: String,
    name: String,
    source: PathBuf,
    kinds: Vec<String>,
    crate_types: Vec<String>,
    role: String,
    compilation_context: String,
}

impl TargetKey {
    fn from_report(target: &CompilerTargetReport) -> Self {
        Self {
            package_id: target.package_id.clone(),
            name: target.name.clone(),
            source: canonical(Path::new(&target.source)),
            kinds: sorted(target.kinds.clone()),
            crate_types: sorted(target.crate_types.clone()),
            role: target.role.clone(),
            compilation_context: target.compilation_context.clone(),
        }
    }
}

fn add_issue(current: &mut Option<String>, issue: &str) {
    match current {
        Some(current) => {
            current.push_str("; ");
            current.push_str(issue);
        }
        None => *current = Some(issue.to_owned()),
    }
}

fn artifact_matches(invocation: &Invocation, artifact: &CargoArtifact) -> bool {
    let started = &invocation.started;
    let Some(manifest_dir) = &started.manifest_dir else {
        return false;
    };
    if canonical(Path::new(manifest_dir))
        != canonical(
            artifact
                .manifest_path
                .parent()
                .unwrap_or(&artifact.manifest_path),
        )
    {
        return false;
    }
    let Some(input) = &started.input else {
        return false;
    };
    let input = resolve(&started.working_directory, input);
    if canonical(&input) != canonical(&artifact.target_source) {
        return false;
    }
    if !cargo_mode_matches(
        &artifact.target_kinds,
        started.test_mode,
        artifact.profile_test,
    ) {
        return false;
    }

    let identity = &started.artifact;
    if !invocation_crate_types_match(
        &identity.crate_types,
        started.test_mode,
        &artifact.target_crate_types,
    ) {
        return false;
    }
    let Some(out_dir) = &identity.out_dir else {
        return false;
    };
    let out_dir = canonical(Path::new(out_dir));
    let suffix = identity.extra_filename.as_deref().unwrap_or_default();
    let expected = format!("{}{}", identity.crate_name, suffix);
    artifact.filenames.iter().any(|filename| {
        filename
            .parent()
            .is_some_and(|parent| canonical(parent) == out_dir)
            && filename
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| {
                    stem == expected || stem.strip_prefix("lib") == Some(expected.as_str())
                })
    })
}

fn failed_target(
    invocation: &Invocation,
    failures: &[CargoFailure],
    packages: &HashMap<String, &PackageInfo>,
) -> Option<CompilerTargetReport> {
    let manifest_dir = invocation.started.manifest_dir.as_ref()?;
    let input = invocation
        .started
        .input
        .as_ref()
        .map(|input| canonical(&resolve(&invocation.started.working_directory, input)))?;
    let identity = &invocation.started.artifact;
    let mut metadata = packages
        .values()
        .filter(|package| canonical(&package.root) == canonical(Path::new(manifest_dir)))
        .flat_map(|package| {
            package
                .targets
                .iter()
                .filter(|target| {
                    canonical(&target.source) == input
                        && invocation_crate_types_match(
                            &identity.crate_types,
                            invocation.started.test_mode,
                            &target.crate_types,
                        )
                })
                .filter_map(|target| {
                    invocation_role(target, invocation.started.test_mode).map(|role| {
                        metadata_target(
                            package,
                            target,
                            role,
                            invocation.started.compilation_context,
                        )
                    })
                })
        })
        .collect::<Vec<_>>();
    let failure_keys = failures
        .iter()
        .filter(|failure| {
            packages.get(&failure.package_id).is_some_and(|package| {
                canonical(&package.root) == canonical(Path::new(manifest_dir))
                    && canonical(&failure.target_source) == input
                    && invocation_crate_types_match(
                        &identity.crate_types,
                        invocation.started.test_mode,
                        &failure.target_crate_types,
                    )
                    && package.targets.iter().any(|target| {
                        target.name == failure.target_name
                            && target.kinds == failure.target_kinds
                            && canonical(&target.source) == input
                            && same_crate_types(&target.crate_types, &failure.target_crate_types)
                    })
            })
        })
        .map(|failure| {
            (
                failure.package_id.clone(),
                failure.target_name.clone(),
                canonical(&failure.target_source),
                sorted(failure.target_kinds.clone()),
                sorted(failure.target_crate_types.clone()),
            )
        })
        .collect::<HashSet<_>>();
    if !failure_keys.is_empty() {
        metadata.retain(|target| {
            failure_keys.contains(&(
                target.package_id.clone(),
                target.name.clone(),
                canonical(Path::new(&target.source)),
                sorted(target.kinds.clone()),
                sorted(target.crate_types.clone()),
            ))
        });
    }
    metadata.sort_by_key(TargetKey::from_report);
    metadata.dedup_by(|left, right| TargetKey::from_report(left) == TargetKey::from_report(right));
    (metadata.len() == 1).then(|| metadata.remove(0))
}

fn metadata_target(
    package: &PackageInfo,
    target: &PackageTargetInfo,
    role: &str,
    compilation_context: rot_compiler_protocol::CompilationContext,
) -> CompilerTargetReport {
    CompilerTargetReport {
        package_id: package.id.to_string(),
        name: target.name.clone(),
        kinds: target.kinds.clone(),
        crate_types: target.crate_types.clone(),
        source: target.source.to_string_lossy().into_owned(),
        role: role.to_owned(),
        compilation_context: compilation_context_name(compilation_context).to_owned(),
    }
}

fn target_report(
    artifact: &CargoArtifact,
    started: &rot_compiler_protocol::InvocationStarted,
) -> CompilerTargetReport {
    CompilerTargetReport {
        package_id: artifact.package_id.clone(),
        name: artifact.target_name.clone(),
        kinds: artifact.target_kinds.clone(),
        crate_types: artifact.target_crate_types.clone(),
        source: artifact.target_source.to_string_lossy().into_owned(),
        role: artifact.role().to_owned(),
        compilation_context: compilation_context_name(started.compilation_context).to_owned(),
    }
}

fn compilation_platform(started: &rot_compiler_protocol::InvocationStarted) -> Option<String> {
    match started.compilation_context {
        rot_compiler_protocol::CompilationContext::Host => None,
        rot_compiler_protocol::CompilationContext::Target => Some(started.target_triple.clone()),
    }
}

pub(super) fn compilation_context_name(
    context: rot_compiler_protocol::CompilationContext,
) -> &'static str {
    match context {
        rot_compiler_protocol::CompilationContext::Host => "host",
        rot_compiler_protocol::CompilationContext::Target => "target",
    }
}

fn special_role(kinds: &[String]) -> Option<&'static str> {
    if kinds.iter().any(|kind| kind == "test") {
        Some("test")
    } else if kinds.iter().any(|kind| kind == "bench") {
        Some("bench")
    } else if kinds.iter().any(|kind| kind == "example") {
        Some("example")
    } else if kinds.iter().any(|kind| kind == "custom-build") {
        Some("build")
    } else {
        None
    }
}

fn cargo_mode_matches(kinds: &[String], rustc_test_mode: bool, cargo_test_profile: bool) -> bool {
    let _ = kinds;
    rustc_test_mode == cargo_test_profile
}

fn invocation_role(target: &PackageTargetInfo, test_mode: bool) -> Option<&'static str> {
    if test_mode {
        if target.kinds.iter().any(|kind| kind == "test") {
            Some("test")
        } else if target.kinds.iter().any(|kind| kind == "bench") {
            Some("bench")
        } else {
            Some("unit_test")
        }
    } else if let Some(role) = special_role(&target.kinds) {
        Some(role)
    } else {
        Some("production")
    }
}

fn same_crate_types(left: &[String], right: &[String]) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort();
    left.dedup();
    right.sort();
    right.dedup();
    left == right
}

fn invocation_crate_types_match(
    rustc_crate_types: &[String],
    rustc_test_mode: bool,
    cargo_target_crate_types: &[String],
) -> bool {
    let implicit_bin = ["bin".to_owned()];
    let rustc_crate_types = if rustc_crate_types.is_empty() {
        &implicit_bin
    } else {
        rustc_crate_types
    };
    if rustc_test_mode {
        same_crate_types(rustc_crate_types, &implicit_bin)
    } else {
        same_crate_types(rustc_crate_types, cargo_target_crate_types)
    }
}

fn resolve(working_directory: &str, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(working_directory).join(path)
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, source: &str, kind: &str, crate_type: &str) -> PackageTargetInfo {
        PackageTargetInfo {
            name: name.to_owned(),
            kinds: vec![kind.to_owned()],
            crate_types: vec![crate_type.to_owned()],
            source: PathBuf::from(source),
        }
    }

    #[test]
    fn selected_unit_ledger_preserves_cargo_multiplicity() {
        let unit = ExpectedUnit {
            package_id: "package".to_owned(),
            target_name: "sample".to_owned(),
            target_kinds: vec!["lib".to_owned()],
            target_crate_types: vec!["lib".to_owned()],
            target_source: PathBuf::from("/workspace/src/lib.rs"),
            features: vec!["default".to_owned()],
            platform: Some("host".to_owned()),
        };
        let profile = CompilerProfile {
            expected_units: vec![unit.clone(), unit],
        };

        let selected = HashSet::from(["package".to_owned()]);
        let ledger = expected_units(&profile, &selected);
        assert_eq!(ledger.values().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn crate_type_identity_is_exact_but_order_independent() {
        assert!(same_crate_types(
            &["rlib".to_owned(), "cdylib".to_owned()],
            &["cdylib".to_owned(), "rlib".to_owned()]
        ));
        assert!(!same_crate_types(&["lib".to_owned()], &["rlib".to_owned()]));
        assert!(!same_crate_types(&[], &["bin".to_owned()]));
        assert!(invocation_crate_types_match(
            &[],
            false,
            &["bin".to_owned()]
        ));
        assert!(invocation_crate_types_match(
            &["bin".to_owned()],
            true,
            &["lib".to_owned()]
        ));
        assert!(!invocation_crate_types_match(
            &["lib".to_owned()],
            true,
            &["lib".to_owned()]
        ));
    }

    #[test]
    fn test_mode_only_maps_to_a_unit_test_when_target_is_testable() {
        let testable = target("sample", "src/lib.rs", "lib", "lib");
        let not_testable = target("sample", "src/main.rs", "bin", "bin");
        let integration = target("it", "tests/it.rs", "test", "bin");

        assert_eq!(invocation_role(&testable, false), Some("production"));
        assert_eq!(invocation_role(&testable, true), Some("unit_test"));
        assert_eq!(invocation_role(&not_testable, true), Some("unit_test"));
        assert_eq!(invocation_role(&integration, true), Some("test"));
    }

    #[test]
    fn special_targets_do_not_confuse_the_cargo_test_profile_with_rustc_test_mode() {
        assert!(cargo_mode_matches(&["lib".to_owned()], true, true));
        assert!(!cargo_mode_matches(&["lib".to_owned()], false, true));
        assert!(!cargo_mode_matches(&["bench".to_owned()], false, true));
        assert!(cargo_mode_matches(&["bench".to_owned()], true, true));
    }
}
