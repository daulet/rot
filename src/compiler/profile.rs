use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};
use cargo_metadata::Metadata;
use serde::Deserialize;

use crate::{cli::AuditCli, workspace::Inventory};

use super::environment;

pub(super) struct CompilerProfile {
    pub(super) expected_units: Vec<ExpectedUnit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExpectedUnit {
    pub package_id: String,
    pub target_name: String,
    pub target_kinds: Vec<String>,
    pub target_crate_types: Vec<String>,
    pub target_source: std::path::PathBuf,
    pub features: Vec<String>,
    pub platform: Option<String>,
}

#[derive(Deserialize)]
struct UnitGraph {
    version: u32,
    units: Vec<RawUnit>,
}

#[derive(Deserialize)]
struct RawUnit {
    pkg_id: String,
    target: RawUnitTarget,
    mode: String,
    features: Vec<String>,
    platform: Option<String>,
}

#[derive(Deserialize)]
struct RawUnitTarget {
    kind: Vec<String>,
    crate_types: Vec<String>,
    name: String,
    src_path: std::path::PathBuf,
}

impl CompilerProfile {
    pub(super) fn expected_units(&self) -> &[ExpectedUnit] {
        &self.expected_units
    }
}

pub(super) fn resolve(cli: &AuditCli, inventory: &Inventory) -> Result<CompilerProfile> {
    environment::reject_compiler_overrides(&inventory.root)?;
    let target = inventory
        .audit_target
        .as_deref()
        .context("audit target was not resolved")?;
    let expected_units = load_unit_graph(cli, &inventory.root, target)?;
    Ok(CompilerProfile { expected_units })
}

fn load_unit_graph(cli: &AuditCli, workspace: &Path, target: &str) -> Result<Vec<ExpectedUnit>> {
    let mut command = environment::pinned_command("cargo");
    command.current_dir(workspace).args([
        "check",
        "-Z",
        "unstable-options",
        "--unit-graph",
        "--workspace",
        "--all-targets",
        "--target",
        target,
    ]);
    append_profile_options(&mut command, cli);
    let output = command
        .output()
        .context("cannot query pinned Cargo unit graph")?;
    if !output.status.success() {
        bail!(
            "pinned Cargo unit-graph preflight failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let graph: UnitGraph =
        serde_json::from_slice(&output.stdout).context("pinned Cargo unit graph was malformed")?;
    if graph.version != 1 {
        bail!(
            "unsupported pinned Cargo unit-graph version {}",
            graph.version
        );
    }
    let mut units = graph
        .units
        .into_iter()
        .filter(|unit| unit.mode != "run-custom-build")
        .map(|unit| {
            let mut kinds = unit.target.kind;
            kinds.sort();
            kinds.dedup();
            let mut crate_types = unit.target.crate_types;
            crate_types.sort();
            crate_types.dedup();
            let mut features = unit.features;
            features.sort();
            features.dedup();
            ExpectedUnit {
                package_id: unit.pkg_id,
                target_name: unit.target.name,
                target_kinds: kinds,
                target_crate_types: crate_types,
                target_source: unit.target.src_path,
                features,
                platform: unit.platform,
            }
        })
        .collect::<Vec<_>>();
    units.sort_by(|left, right| {
        (
            &left.package_id,
            &left.target_name,
            &left.target_kinds,
            &left.target_crate_types,
            &left.target_source,
            &left.features,
            &left.platform,
        )
            .cmp(&(
                &right.package_id,
                &right.target_name,
                &right.target_kinds,
                &right.target_crate_types,
                &right.target_source,
                &right.features,
                &right.platform,
            ))
    });
    Ok(units)
}

pub(crate) fn load_metadata(
    cli: &AuditCli,
    workspace: &Path,
    no_dependencies: bool,
) -> Result<Metadata> {
    let mut command = metadata_command(cli, workspace, no_dependencies)?;
    let output = command
        .output()
        .context("cannot run pinned Cargo metadata preflight")?;
    if !output.status.success() {
        bail!(
            "pinned Cargo metadata preflight failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("pinned Cargo metadata was malformed")
}

fn metadata_command(cli: &AuditCli, workspace: &Path, no_dependencies: bool) -> Result<Command> {
    let mut command = environment::pinned_command("cargo");
    command
        .current_dir(workspace)
        .args(["metadata", "--format-version", "1"]);
    if no_dependencies {
        command.arg("--no-deps");
    }
    append_profile_options(&mut command, cli);
    command
        .arg("--filter-platform")
        .arg(environment::effective_target(cli, workspace)?);
    Ok(command)
}

fn append_profile_options(command: &mut Command, cli: &AuditCli) {
    if cli.locked {
        command.arg("--locked");
    }
    if cli.offline {
        command.arg("--offline");
    }
    if cli.all_features {
        command.arg("--all-features");
    } else {
        if cli.no_default_features {
            command.arg("--no-default-features");
        }
        if !cli.features.is_empty() {
            command.arg("--features").arg(cli.features.join(","));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::*;

    #[test]
    fn metadata_preflight_uses_compiler_profile_controls() {
        let cli = AuditCli::parse_from([
            "rot-audit",
            "--features",
            "member/selected",
            "--no-default-features",
            "--locked",
            "--offline",
            "--target",
            "aarch64-unknown-linux-gnu",
        ]);
        let command = metadata_command(&cli, Path::new("workspace"), false).unwrap();
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "run",
                environment::TOOLCHAIN,
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--offline",
                "--no-default-features",
                "--features",
                "member/selected",
                "--filter-platform",
                "aarch64-unknown-linux-gnu",
            ]
        );
        assert_eq!(
            command.get_current_dir(),
            Some(PathBuf::from("workspace").as_path())
        );
    }
}
