use std::{
    fs,
    io::Cursor,
    path::PathBuf,
    process::{Command, ExitStatus},
};

use anyhow::{Context, Result};
use cargo_metadata::{Message, TargetKind, diagnostic::DiagnosticLevel};

#[derive(Clone, Debug)]
pub struct CargoArtifact {
    pub package_id: String,
    pub manifest_path: PathBuf,
    pub target_name: String,
    pub target_kinds: Vec<String>,
    pub target_crate_types: Vec<String>,
    pub target_source: PathBuf,
    pub profile_test: bool,
    pub profile: CargoProfile,
    pub features: Vec<String>,
    pub filenames: Vec<PathBuf>,
    pub fresh: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CargoProfile {
    pub opt_level: String,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
}

#[derive(Clone, Debug)]
pub struct CargoFailure {
    pub package_id: String,
    pub target_name: String,
    pub target_kinds: Vec<String>,
    pub target_crate_types: Vec<String>,
    pub target_source: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BuildScriptOutput {
    pub package_id: String,
    pub out_dir: PathBuf,
    pub cfg: Vec<String>,
}

pub struct CargoRun {
    pub status: ExitStatus,
    pub build_finished: Option<bool>,
    pub artifacts: Vec<CargoArtifact>,
    pub failures: Vec<CargoFailure>,
    pub build_script_outputs: Vec<BuildScriptOutput>,
    pub stderr: String,
    pub text_lines: Vec<String>,
}

pub fn run(command: &mut Command) -> Result<CargoRun> {
    let output = command
        .output()
        .context("cannot run selected Cargo compiler pass")?;
    let mut artifacts = Vec::new();
    let mut failures = Vec::new();
    let mut build_script_outputs = Vec::new();
    let mut text_lines = Vec::new();
    let mut build_finished = None;

    for message in Message::parse_stream(Cursor::new(&output.stdout)) {
        match message.context("cannot parse Cargo JSON message")? {
            Message::CompilerArtifact(artifact) => artifacts.push(CargoArtifact {
                package_id: artifact.package_id.to_string(),
                manifest_path: artifact.manifest_path.into_std_path_buf(),
                target_name: artifact.target.name,
                target_kinds: target_kinds(&artifact.target.kind),
                target_crate_types: crate_types(&artifact.target.crate_types),
                target_source: artifact.target.src_path.into_std_path_buf(),
                profile_test: artifact.profile.test,
                profile: CargoProfile {
                    opt_level: artifact.profile.opt_level,
                    debug_assertions: artifact.profile.debug_assertions,
                    overflow_checks: artifact.profile.overflow_checks,
                },
                features: sorted(artifact.features),
                filenames: artifact
                    .filenames
                    .into_iter()
                    .map(|path| path.into_std_path_buf())
                    .collect(),
                fresh: artifact.fresh,
            }),
            Message::CompilerMessage(message)
                if matches!(
                    message.message.level,
                    DiagnosticLevel::Ice | DiagnosticLevel::Error | DiagnosticLevel::FailureNote
                ) =>
            {
                failures.push(CargoFailure {
                    package_id: message.package_id.to_string(),
                    target_name: message.target.name,
                    target_kinds: target_kinds(&message.target.kind),
                    target_crate_types: crate_types(&message.target.crate_types),
                    target_source: message.target.src_path.into_std_path_buf(),
                    message: message.message.message,
                });
            }
            Message::BuildScriptExecuted(script) => {
                build_script_outputs.push(BuildScriptOutput {
                    package_id: script.package_id.to_string(),
                    out_dir: {
                        let path = script.out_dir.into_std_path_buf();
                        fs::canonicalize(&path).unwrap_or(path)
                    },
                    cfg: sorted(script.cfgs),
                });
            }
            Message::BuildFinished(finished) => build_finished = Some(finished.success),
            Message::TextLine(line) if text_lines.len() < 32 => {
                text_lines.push(line);
            }
            _ => {}
        }
    }
    artifacts.sort_by(|left, right| {
        (
            &left.package_id,
            &left.target_name,
            left.profile_test,
            &left.filenames,
        )
            .cmp(&(
                &right.package_id,
                &right.target_name,
                right.profile_test,
                &right.filenames,
            ))
    });
    failures.sort_by(|left, right| {
        (&left.package_id, &left.target_name, &left.message).cmp(&(
            &right.package_id,
            &right.target_name,
            &right.message,
        ))
    });
    build_script_outputs.sort();
    build_script_outputs.dedup();

    Ok(CargoRun {
        status: output.status,
        build_finished,
        artifacts,
        failures,
        build_script_outputs,
        stderr: truncate_utf8(&output.stderr, 16 * 1024),
        text_lines,
    })
}

impl CargoArtifact {
    pub fn role(&self) -> &'static str {
        if self.target_kinds.iter().any(|kind| kind == "test") {
            "test"
        } else if self.target_kinds.iter().any(|kind| kind == "bench") {
            "bench"
        } else if self.profile_test {
            "unit_test"
        } else if self.target_kinds.iter().any(|kind| kind == "example") {
            "example"
        } else if self.target_kinds.iter().any(|kind| kind == "custom-build") {
            "build"
        } else {
            "production"
        }
    }
}

fn target_kinds(kinds: &[TargetKind]) -> Vec<String> {
    kinds.iter().map(ToString::to_string).collect()
}

fn crate_types(types: &[cargo_metadata::CrateType]) -> Vec<String> {
    sorted(types.iter().map(ToString::to_string).collect())
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn truncate_utf8(bytes: &[u8], limit: usize) -> String {
    let bytes = &bytes[..bytes.len().min(limit)];
    String::from_utf8_lossy(bytes).into_owned()
}
