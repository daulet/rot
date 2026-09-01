use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Value, value};

use crate::git::Repository;
use crate::version::version;

const ROOT: &str = "Cargo.toml";
const ROOT_LOCK: &str = "Cargo.lock";
const PROTOCOL_MANIFEST: &str = "crates/rot-compiler-protocol/Cargo.toml";
const DRIVER_MANIFEST: &str = "compiler/rot-rustc-driver/Cargo.toml";
const DRIVER_LOCK: &str = "compiler/rot-rustc-driver/Cargo.lock";
const PROTOCOL: &str = "rot-compiler-protocol";
const FILES: [&str; 3] = [ROOT, ROOT_LOCK, DRIVER_LOCK];

struct VersionFiles {
    root: DocumentMut,
    protocol: DocumentMut,
    driver: DocumentMut,
    root_lock: DocumentMut,
    driver_lock: DocumentMut,
    location: String,
}

impl VersionFiles {
    fn read(root: &Path) -> Result<Self> {
        let load = |relative: &str| {
            let path = root.join(relative);
            let text = fs::read_to_string(&path)
                .with_context(|| format!("could not read {}", path.display()))?;
            parse(&text, &path.display().to_string())
        };
        Ok(Self {
            root: load(ROOT)?,
            protocol: load(PROTOCOL_MANIFEST)?,
            driver: load(DRIVER_MANIFEST)?,
            root_lock: load(ROOT_LOCK)?,
            driver_lock: load(DRIVER_LOCK)?,
            location: root.display().to_string(),
        })
    }

    fn at(repo: &Repository, commit: &str) -> Result<Self> {
        let load = |path| parse(&repo.file(commit, path)?, &format!("{commit}:{path}"));
        Ok(Self {
            root: load(ROOT)?,
            protocol: load(PROTOCOL_MANIFEST)?,
            driver: load(DRIVER_MANIFEST)?,
            root_lock: load(ROOT_LOCK)?,
            driver_lock: load(DRIVER_LOCK)?,
            location: commit.to_owned(),
        })
    }

    fn version(&self) -> Result<Version> {
        let found = root_version(&self.root, &self.path(ROOT))?;
        inherited(&self.root, "rot-metrics", &self.path(ROOT))?;
        workspace_dependency(&self.root, &self.path(ROOT))?;
        inherited(&self.protocol, PROTOCOL, &self.path(PROTOCOL_MANIFEST))?;
        driver_layout(&self.driver, &self.path(DRIVER_MANIFEST))?;
        lock_version(
            &self.root_lock,
            "rot-metrics",
            &found,
            &self.path(ROOT_LOCK),
        )?;
        lock_version(&self.root_lock, PROTOCOL, &found, &self.path(ROOT_LOCK))?;
        lock_version(&self.driver_lock, PROTOCOL, &found, &self.path(DRIVER_LOCK))?;
        Ok(found)
    }

    fn update(&mut self, new: &Version) -> Result<()> {
        let root = self.path(ROOT);
        let root_lock = self.path(ROOT_LOCK);
        let driver_lock = self.path(DRIVER_LOCK);
        self.root["workspace"]["package"]["version"] = value(new.to_string());
        set_dependency_version(&mut self.root, new, &root)?;
        set_lock_version(&mut self.root_lock, "rot-metrics", new, &root_lock)?;
        set_lock_version(&mut self.root_lock, PROTOCOL, new, &root_lock)?;
        set_lock_version(&mut self.driver_lock, PROTOCOL, new, &driver_lock)?;
        ensure!(self.version()? == *new, "version update did not validate");
        Ok(())
    }

    fn rendered(self) -> [String; 3] {
        [
            self.root.to_string(),
            self.root_lock.to_string(),
            self.driver_lock.to_string(),
        ]
    }

    fn path(&self, relative: &str) -> String {
        format!("{}:{relative}", self.location)
    }
}

pub fn set_version(root: &Path, new: Version) -> Result<()> {
    let mut files = VersionFiles::read(root)?;
    let old = files.version()?;
    if old == new {
        return Ok(());
    }
    files.update(&new)?;
    let rendered = files.rendered();
    for (relative, content) in FILES.into_iter().zip(rendered) {
        atomic_write(&root.join(relative), content.as_bytes())?;
    }
    Ok(())
}

pub(crate) fn current_version(root: &Path) -> Result<Version> {
    VersionFiles::read(root)?.version()
}

pub(crate) fn validate_materialized_release(
    repo: &Repository,
    commit: &str,
    expected: &Version,
    expected_paths: &[&str],
) -> Result<()> {
    let paths: BTreeSet<_> = repo
        .paths(&repo.parent(commit)?, commit)?
        .into_iter()
        .collect();
    if *expected == Version::new(0, 1, 0) && paths.is_empty() {
        return Ok(()); // The pre-shared-authority bootstrap release was intentionally empty.
    }
    let required: BTreeSet<_> = expected_paths.iter().map(ToString::to_string).collect();
    ensure!(
        paths == required,
        "generated release {commit} changes the wrong paths: expected={required:?}, actual={paths:?}"
    );
    let files = VersionFiles::at(repo, commit)?;
    let actual = files.version()?;
    ensure!(
        actual == *expected,
        "generated release {commit} says {expected}, but {ROOT} says {actual}"
    );
    Ok(())
}

fn root_version(document: &DocumentMut, path: &str) -> Result<Version> {
    let raw = at(
        document.as_item(),
        &["workspace", "package", "version"],
        path,
    )?
    .as_str()
    .ok_or_else(|| anyhow::anyhow!("{path} has no workspace package version"))?;
    let found = version(raw)?;
    let dependency = at(
        document.as_item(),
        &["workspace", "dependencies", PROTOCOL],
        path,
    )?;
    ensure!(
        dependency_field(dependency, "path") == Some("crates/rot-compiler-protocol"),
        "{path} workspace dependency {PROTOCOL} has the wrong path"
    );
    let exact = dependency_field(dependency, "version")
        .ok_or_else(|| anyhow::anyhow!("{path} workspace dependency {PROTOCOL} has no version"))?;
    ensure!(
        exact == format!("={found}"),
        "{path} workspace version and exact {PROTOCOL} dependency disagree: version={found}, dependency={exact:?}"
    );
    Ok(found)
}

fn set_dependency_version(document: &mut DocumentMut, new: &Version, path: &str) -> Result<()> {
    let dependency = &mut document["workspace"]["dependencies"][PROTOCOL];
    let exact = format!("={new}");
    if let Some(table) = dependency.as_inline_table_mut() {
        *table
            .get_mut("version")
            .ok_or_else(|| anyhow::anyhow!("{path} dependency has no version"))? =
            Value::from(exact);
    } else if let Some(table) = dependency.as_table_mut() {
        table["version"] = value(exact);
    } else {
        bail!("{path} dependency is not a TOML table");
    }
    Ok(())
}

fn dependency_field<'a>(dependency: &'a Item, field: &str) -> Option<&'a str> {
    dependency
        .as_inline_table()
        .and_then(|table| table.get(field))
        .and_then(Value::as_str)
        .or_else(|| dependency.as_table()?.get(field)?.as_str())
}

fn package_index(document: &DocumentMut, name: &str, path: &str) -> Result<usize> {
    let packages = document
        .get("package")
        .and_then(Item::as_array_of_tables)
        .ok_or_else(|| anyhow::anyhow!("{path} has no package array"))?;
    let matches: Vec<_> = packages
        .iter()
        .enumerate()
        .filter(|(_, package)| package.get("name").and_then(Item::as_str) == Some(name))
        .map(|(index, _)| index)
        .collect();
    ensure!(
        matches.len() == 1,
        "{path} contains {} entries for {name}, expected one",
        matches.len()
    );
    Ok(matches[0])
}

fn lock_version(document: &DocumentMut, name: &str, expected: &Version, path: &str) -> Result<()> {
    let index = package_index(document, name, path)?;
    let actual = document
        .get("package")
        .and_then(Item::as_array_of_tables)
        .and_then(|packages| packages.get(index))
        .and_then(|package| package.get("version"))
        .and_then(Item::as_str);
    ensure!(
        actual == Some(&expected.to_string()),
        "{path} has {name} at {actual:?}, expected {expected}"
    );
    Ok(())
}

fn set_lock_version(
    document: &mut DocumentMut,
    name: &str,
    new: &Version,
    path: &str,
) -> Result<()> {
    let index = package_index(document, name, path)?;
    document["package"]
        .as_array_of_tables_mut()
        .and_then(|packages| packages.get_mut(index))
        .expect("validated package index")["version"] = value(new.to_string());
    Ok(())
}

fn inherited(document: &DocumentMut, name: &str, path: &str) -> Result<()> {
    ensure!(
        at(document.as_item(), &["package", "name"], path)?.as_str() == Some(name),
        "{path} is not package {name}"
    );
    ensure!(
        at(
            document.as_item(),
            &["package", "version", "workspace"],
            path
        )?
        .as_bool()
            == Some(true),
        "{path} package version does not inherit from the workspace"
    );
    Ok(())
}

fn workspace_dependency(document: &DocumentMut, path: &str) -> Result<()> {
    let dependency = at(document.as_item(), &["dependencies", PROTOCOL], path)?;
    let workspace = dependency
        .as_inline_table()
        .and_then(|table| table.get("workspace"))
        .and_then(Value::as_bool)
        .or_else(|| dependency.as_table()?.get("workspace")?.as_bool());
    ensure!(
        workspace == Some(true),
        "{path} package dependency {PROTOCOL} does not inherit from the workspace"
    );
    Ok(())
}

fn driver_layout(document: &DocumentMut, path: &str) -> Result<()> {
    ensure!(
        at(document.as_item(), &["package", "name"], path)?.as_str() == Some("rot-rustc-driver"),
        "{path} is not the rustc driver manifest"
    );
    version(
        at(document.as_item(), &["package", "version"], path)?
            .as_str()
            .unwrap_or_default(),
    )?;
    let dependency = at(document.as_item(), &["dependencies", PROTOCOL], path)?;
    ensure!(
        dependency_field(dependency, "path") == Some("../../crates/rot-compiler-protocol")
            && dependency_field(dependency, "version").is_none(),
        "{path} must use a path-only {PROTOCOL} dependency"
    );
    Ok(())
}

fn at<'a>(mut item: &'a Item, keys: &[&str], path: &str) -> Result<&'a Item> {
    for key in keys {
        item = item
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("{path} has no TOML field {key}"))?;
    }
    Ok(item)
}

fn parse(text: &str, path: &str) -> Result<DocumentMut> {
    text.parse()
        .with_context(|| format!("could not parse {path}"))
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let permissions = fs::metadata(path)
        .with_context(|| format!("could not inspect {}", path.display()))?
        .permissions();
    let parent = path
        .parent()
        .context("version file has no parent directory")?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create temporary file in {}", parent.display()))?;
    temporary
        .write_all(content)
        .and_then(|()| temporary.flush())
        .with_context(|| format!("could not write temporary file for {}", path.display()))?;
    temporary
        .as_file()
        .set_permissions(permissions)
        .with_context(|| format!("could not preserve permissions for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}
