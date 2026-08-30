use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};
use rot_compiler_protocol::{
    BUILD_DIR_ENV, CompilerIdentity, RUN_ID_ENV, SELECTED_MANIFEST_DIRS_ENV, SIDECAR_DIR_ENV,
    TARGET_DIR_ENV,
};
use tempfile::{Builder, TempDir};

use crate::cli::AuditCli;

#[derive(Clone, Debug)]
pub(crate) struct SelectedCompiler {
    pub(crate) identity: CompilerIdentity,
    pub(crate) linked_version: String,
}

pub struct CompilerEnvironment {
    pub artifacts: ArtifactDirs,
    pub selected: SelectedCompiler,
    dynamic_library_path: OsString,
}

pub struct ArtifactDirs {
    _root: TempDir,
    pub target: PathBuf,
    pub build: PathBuf,
    pub events: PathBuf,
    pub run_id: String,
}

impl CompilerEnvironment {
    pub fn discover(cli: &AuditCli, workspace: &Path, selected: &SelectedCompiler) -> Result<Self> {
        reject_compiler_overrides(cli, workspace)?;
        let artifacts = ArtifactDirs::new(cli.scratch_dir.as_deref())?;
        let host = &selected.identity.host;

        let sysroot = toolchain_output(cli, workspace, "rustc", ["--print", "sysroot"])?;
        if !sysroot.status.success() {
            bail!(
                "cannot query selected rustc sysroot: {}",
                String::from_utf8_lossy(&sysroot.stderr).trim()
            );
        }
        let sysroot = PathBuf::from(
            String::from_utf8(sysroot.stdout)
                .context("rustc sysroot was not UTF-8")?
                .trim(),
        );
        let compiler_lib = sysroot.join("lib/rustlib").join(host).join("lib");
        if !compiler_lib.is_dir() {
            bail!(
                "selected rustc-dev library directory is missing: {}",
                compiler_lib.display()
            );
        }
        let dynamic_library_path = prepend_path(dynamic_library_variable(), &compiler_lib)?;

        Ok(Self {
            artifacts,
            selected: selected.clone(),
            dynamic_library_path,
        })
    }

    pub fn driver_command(&self, driver: &Path) -> Command {
        let mut command = Command::new(driver);
        command.env(dynamic_library_variable(), &self.dynamic_library_path);
        command
    }

    pub fn cargo_command(
        &self,
        workspace: &Path,
        driver: &Path,
        cli: &AuditCli,
        target: &str,
        selected_manifest_dirs: &OsStr,
    ) -> Result<Command> {
        let mut command = toolchain_command(cli, "cargo");
        command
            .current_dir(workspace)
            .arg("check")
            .arg("--workspace")
            .arg("--all-targets")
            .arg("--keep-going")
            .arg("--message-format=json")
            .arg("--target")
            .arg(target)
            .env("RUSTC_WORKSPACE_WRAPPER", driver)
            .env(SIDECAR_DIR_ENV, &self.artifacts.events)
            .env(RUN_ID_ENV, &self.artifacts.run_id)
            .env(SELECTED_MANIFEST_DIRS_ENV, selected_manifest_dirs)
            .env(TARGET_DIR_ENV, &self.artifacts.target)
            .env(BUILD_DIR_ENV, &self.artifacts.build)
            .env("CARGO_TARGET_DIR", &self.artifacts.target)
            .env("CARGO_BUILD_BUILD_DIR", &self.artifacts.build)
            .env_remove("RUSTC_BOOTSTRAP")
            .env(dynamic_library_variable(), &self.dynamic_library_path);

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
        if !cli.cfg.is_empty() {
            if cargo_target_rustflags_configured(cli, workspace)? {
                bail!(
                    "custom --cfg cannot be composed safely with Cargo target-specific rustflags"
                );
            }
            let mut flags = configured_build_rustflags(cli, workspace)?;
            flags.reserve(cli.cfg.len() * 2);
            for predicate in &cli.cfg {
                flags.push("--cfg".to_owned());
                flags.push(predicate.clone());
            }
            let flags = serde_json::to_string(&flags).expect("string arrays always serialize");
            command
                .arg("--config")
                .arg(format!("build.rustflags={flags}"));
        }
        Ok(command)
    }

    pub fn disable_ordinary_wrapper(command: &mut Command) {
        command.env("RUSTC_WRAPPER", "");
        command.env("CARGO_BUILD_RUSTC_WRAPPER", "");
    }
}

pub(super) fn selected_compiler(cli: &AuditCli, workspace: &Path) -> Result<SelectedCompiler> {
    let verbose = toolchain_output(cli, workspace, "rustc", ["-Vv"])?;
    if !verbose.status.success() {
        bail!(
            "cannot query selected rustc {}: {}",
            cli.toolchain,
            String::from_utf8_lossy(&verbose.stderr).trim()
        );
    }
    let verbose = String::from_utf8(verbose.stdout).context("rustc -Vv was not UTF-8")?;
    let linked_version = verbose
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("rustc "))
        .context("rustc -Vv omitted its version header")?
        .to_owned();
    let compiler = CompilerIdentity {
        release: field(&verbose, "release")
            .context("rustc -Vv omitted release")?
            .to_owned(),
        commit_hash: field(&verbose, "commit-hash")
            .context("rustc -Vv omitted commit-hash")?
            .to_owned(),
        commit_date: field(&verbose, "commit-date")
            .context("rustc -Vv omitted commit-date")?
            .to_owned(),
        host: field(&verbose, "host")
            .context("rustc -Vv omitted host")?
            .to_owned(),
    };
    super::support::validate(&compiler)?;
    Ok(SelectedCompiler {
        identity: compiler,
        linked_version,
    })
}

pub(super) fn effective_target(cli: &AuditCli, workspace: &Path) -> Result<String> {
    if let Some(target) = &cli.target {
        return Ok(target.clone());
    }
    if let Some(target) = cargo_config_json(cli, workspace, "build.target")? {
        return configured_target(target);
    }
    let verbose = toolchain_output(cli, workspace, "rustc", ["-vV"])?;
    if !verbose.status.success() {
        bail!(
            "cannot query selected rustc target: {}",
            String::from_utf8_lossy(&verbose.stderr).trim()
        );
    }
    let verbose = String::from_utf8(verbose.stdout).context("rustc -vV was not UTF-8")?;
    field(&verbose, "host")
        .map(str::to_owned)
        .context("rustc -vV omitted host")
}

fn configured_target(value: serde_json::Value) -> Result<String> {
    match value {
        serde_json::Value::String(target) if !target.is_empty() => Ok(target),
        serde_json::Value::Array(mut targets) if targets.len() == 1 => targets
            .remove(0)
            .as_str()
            .filter(|target| !target.is_empty())
            .map(str::to_owned)
            .context("Cargo build.target contains an empty or non-string target"),
        serde_json::Value::Array(targets) => bail!(
            "visibility audit requires one effective Cargo build.target, found {}",
            targets.len()
        ),
        _ => bail!("Cargo build.target is neither a target string nor an array"),
    }
}

impl ArtifactDirs {
    fn new(parent: Option<&Path>) -> Result<Self> {
        let root = match parent {
            Some(parent) => {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "cannot create compiler artifact parent {}",
                        parent.display()
                    )
                })?;
                Builder::new().prefix("rot-compiler-").tempdir_in(parent)
            }
            None => Builder::new().prefix("rot-compiler-").tempdir(),
        }
        .context("cannot create isolated compiler artifact directory")?;
        let run_id = root
            .path()
            .file_name()
            .and_then(OsStr::to_str)
            .context("compiler artifact directory has no UTF-8 name")?
            .to_owned();
        let target = root.path().join("target");
        let build = root.path().join("build");
        let events = root.path().join("events");
        for directory in [&target, &build, &events] {
            fs::create_dir(directory).with_context(|| {
                format!("cannot create compiler directory {}", directory.display())
            })?;
        }
        Ok(Self {
            _root: root,
            target,
            build,
            events,
            run_id,
        })
    }
}

pub(super) fn reject_compiler_overrides(cli: &AuditCli, workspace: &Path) -> Result<()> {
    for variable in ["RUSTC", "CARGO_BUILD_RUSTC"] {
        if env::var_os(variable).is_some() {
            bail!("visibility audit rejects {variable}; it must use the exact selected rustc");
        }
    }
    for variable in [
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    ] {
        if env::var_os(variable).is_some_and(|value| !value.is_empty()) {
            bail!("visibility audit cannot compose the existing workspace wrapper in {variable}");
        }
    }
    for key in ["build.rustc", "build.rustc-workspace-wrapper"] {
        if let Some(value) = cargo_config(cli, workspace, key)? {
            bail!("visibility audit rejects Cargo {key}={value:?}");
        }
    }
    Ok(())
}

pub fn ordinary_wrapper_configured(cli: &AuditCli, workspace: &Path) -> Result<bool> {
    if env::var_os("RUSTC_WRAPPER").is_some_and(|value| !value.is_empty())
        || env::var_os("CARGO_BUILD_RUSTC_WRAPPER").is_some_and(|value| !value.is_empty())
    {
        return Ok(true);
    }
    Ok(cargo_config(cli, workspace, "build.rustc-wrapper")?.is_some())
}

pub fn custom_cfg_environment_is_safe() -> bool {
    env::vars_os().all(|(name, _)| {
        let name = name.to_string_lossy();
        !matches!(
            name.as_ref(),
            "RUSTFLAGS" | "CARGO_ENCODED_RUSTFLAGS" | "CARGO_BUILD_RUSTFLAGS"
        ) && !(name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS"))
    })
}

fn cargo_config(cli: &AuditCli, workspace: &Path, key: &str) -> Result<Option<String>> {
    let Some(current) = cargo_config_json(cli, workspace, key)? else {
        return Ok(None);
    };
    let value = current
        .as_str()
        .with_context(|| format!("Cargo config {key} is not a string"))?;
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn cargo_config_json(
    cli: &AuditCli,
    workspace: &Path,
    key: &str,
) -> Result<Option<serde_json::Value>> {
    let output = unstable_cargo_command(cli)
        .current_dir(workspace)
        .args([
            "-Z",
            "unstable-options",
            "config",
            "get",
            key,
            "--format=json",
        ])
        .output()
        .with_context(|| format!("cannot query Cargo configuration {key}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("is not set") {
            return Ok(None);
        }
        bail!("cannot query Cargo configuration {key}: {}", stderr.trim());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Cargo config JSON was malformed")?;
    let mut current = &value;
    for component in key.split('.') {
        current = current
            .get(component)
            .with_context(|| format!("Cargo config JSON omitted {key}"))?;
    }
    Ok(Some(current.clone()))
}

fn configured_build_rustflags(cli: &AuditCli, workspace: &Path) -> Result<Vec<String>> {
    let Some(value) = cargo_config_json(cli, workspace, "build.rustflags")? else {
        return Ok(Vec::new());
    };
    match value {
        serde_json::Value::String(flags) => {
            Ok(flags.split_ascii_whitespace().map(str::to_owned).collect())
        }
        serde_json::Value::Array(flags) => flags
            .into_iter()
            .map(|flag| {
                flag.as_str()
                    .map(str::to_owned)
                    .context("Cargo build.rustflags contains a non-string value")
            })
            .collect(),
        _ => bail!("Cargo build.rustflags is neither a string nor an array"),
    }
}

fn cargo_target_rustflags_configured(cli: &AuditCli, workspace: &Path) -> Result<bool> {
    let Some(value) = cargo_config_json(cli, workspace, "target")? else {
        return Ok(false);
    };
    Ok(contains_key(&value, "rustflags"))
}

fn contains_key(value: &serde_json::Value, key: &str) -> bool {
    match value {
        serde_json::Value::Object(values) => {
            values.contains_key(key) || values.values().any(|value| contains_key(value, key))
        }
        serde_json::Value::Array(values) => values.iter().any(|value| contains_key(value, key)),
        _ => false,
    }
}

fn toolchain_output<const N: usize>(
    cli: &AuditCli,
    workspace: &Path,
    program: &str,
    arguments: [&str; N],
) -> Result<Output> {
    toolchain_command(cli, program)
        .current_dir(workspace)
        .args(arguments)
        .output()
        .with_context(|| format!("cannot run {program} from toolchain {}", cli.toolchain))
}

pub(super) fn toolchain_command(cli: &AuditCli, program: &str) -> Command {
    let mut command = Command::new("rustup");
    command.args(["run", &cli.toolchain, program]);
    command
}

pub(super) fn unstable_cargo_command(cli: &AuditCli) -> Command {
    let mut command = toolchain_command(cli, "cargo");
    // Stable Cargo otherwise rejects these read-only preflights. The actual
    // Cargo build removes this injected process value; project config is kept.
    command.env("RUSTC_BOOTSTRAP", "1");
    command
}

fn field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

fn dynamic_library_variable() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(windows) {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

fn prepend_path(variable: &str, path: &Path) -> Result<OsString> {
    let existing = env::var_os(variable)
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    let paths = std::iter::once(path.to_path_buf()).chain(existing);
    env::join_paths(paths).with_context(|| format!("cannot construct {variable}"))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_verbose_rustc_fields() {
        let output = "rustc 1.100.0-nightly\ncommit-hash: abc\nhost: target\n";
        assert_eq!(field(output, "commit-hash"), Some("abc"));
        assert_eq!(field(output, "host"), Some("target"));
        assert_eq!(field(output, "missing"), None);
    }

    #[test]
    fn accepts_one_configured_target_and_rejects_multi_target_profiles() {
        assert_eq!(
            configured_target(serde_json::json!(["target"])).unwrap(),
            "target"
        );
        assert!(
            configured_target(serde_json::json!(["first", "second"]))
                .unwrap_err()
                .to_string()
                .contains("requires one")
        );
    }

    #[test]
    fn ambient_bootstrap_is_removed_from_the_project_cargo_command() {
        let cli =
            AuditCli::parse_from(["rot-audit", "--driver", "driver", "--toolchain", "1.98.0"]);
        let environment = CompilerEnvironment {
            artifacts: ArtifactDirs::new(None).unwrap(),
            selected: SelectedCompiler {
                identity: CompilerIdentity {
                    release: "1.98.0".to_owned(),
                    commit_hash: "commit".to_owned(),
                    commit_date: "date".to_owned(),
                    host: "host".to_owned(),
                },
                linked_version: "linked".to_owned(),
            },
            dynamic_library_path: OsString::new(),
        };
        let command = environment
            .cargo_command(
                Path::new("workspace"),
                Path::new("driver"),
                &cli,
                "target",
                OsStr::new("manifest"),
            )
            .unwrap();
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == "RUSTC_BOOTSTRAP")
                .map(|(_, value)| value),
            Some(None),
        );

        let preflight = unstable_cargo_command(&cli);
        assert_eq!(
            preflight
                .get_envs()
                .find(|(name, _)| *name == "RUSTC_BOOTSTRAP")
                .and_then(|(_, value)| value),
            Some(OsStr::new("1")),
        );
    }
}
