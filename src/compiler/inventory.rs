use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, PackageId};

use super::environment::SelectedCompiler;
use crate::{cli::AuditCli, paths::containing_directory, workspace::root_selected};

/// The Cargo workspace, selected packages, target, and exact compiler that one
/// `rot-audit` run compiles.
#[derive(Debug)]
pub struct AuditInventory {
    pub root: PathBuf,
    pub requested: Vec<PathBuf>,
    pub packages: Vec<AuditPackage>,
    pub audit_target: String,
    pub(super) selected_compiler: SelectedCompiler,
}

#[derive(Clone, Debug)]
pub struct AuditPackage {
    pub id: PackageId,
    pub name: String,
    pub root: PathBuf,
    pub targets: Vec<PackageTargetInfo>,
}

#[derive(Clone, Debug)]
pub struct PackageTargetInfo {
    pub name: String,
    pub kinds: Vec<String>,
    pub crate_types: Vec<String>,
    pub source: PathBuf,
}

impl AuditInventory {
    pub fn selected_package_ids(&self) -> HashSet<String> {
        self.packages
            .iter()
            .filter(|package| root_selected(&package.root, &self.requested))
            .map(|package| package.id.to_string())
            .collect()
    }
}

pub fn audit_inventory(cli: &AuditCli) -> Result<AuditInventory> {
    let requested = cli
        .cargo
        .paths
        .iter()
        .map(|path| {
            fs::canonicalize(path)
                .with_context(|| format!("cannot resolve input path {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let first = requested.first().context("at least one path is required")?;
    let current_dir = containing_directory(first);
    let selected_compiler = super::environment::selected_compiler(cli, current_dir)?;
    let metadata = super::profile::load_metadata(cli, current_dir, false)?;
    let root = fs::canonicalize(metadata.workspace_root.as_std_path())
        .unwrap_or_else(|_| metadata.workspace_root.as_std_path().to_path_buf());
    if let Some(outside) = requested.iter().find(|path| !path.starts_with(&root)) {
        bail!(
            "cannot mix Cargo workspace {} with outside path {}; run rot-audit once per workspace",
            root.display(),
            outside.display(),
        );
    }
    let audit_target = super::environment::effective_target(cli, &root)?;

    Ok(AuditInventory {
        root,
        requested,
        packages: audit_packages(&metadata),
        audit_target,
        selected_compiler,
    })
}

fn audit_packages(metadata: &Metadata) -> Vec<AuditPackage> {
    metadata
        .workspace_packages()
        .into_iter()
        .map(|package| {
            let manifest: &Path = package.manifest_path.as_std_path();
            AuditPackage {
                id: package.id.clone(),
                name: package.name.to_string(),
                root: manifest.parent().unwrap_or(manifest).to_path_buf(),
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
