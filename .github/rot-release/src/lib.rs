mod git;
mod plan;
mod version;
mod workspace;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, ensure};

pub use plan::plan_release;
pub use semver::Version;
pub use version::parse_version;
pub use workspace::set_version;

pub type Result<T> = anyhow::Result<T>;
pub type Outputs = BTreeMap<String, String>;

pub(crate) fn outputs<const N: usize>(values: [(&str, &str); N]) -> Outputs {
    values
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

pub fn emit_plan(outputs: &Outputs, github_output: Option<&Path>) -> Result<()> {
    if let Some(path) = github_output {
        let mut destination = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        for (key, value) in outputs {
            ensure!(
                !value.contains(['\r', '\n']),
                "output {key:?} contains a newline"
            );
            writeln!(destination, "{key}={value}")
                .with_context(|| format!("could not write {}", path.display()))?;
        }
        return Ok(());
    }

    let json = serde_json::to_string(outputs).context("could not serialize release plan")?;
    println!("{json}");
    Ok(())
}
