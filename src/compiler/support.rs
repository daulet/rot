use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use rot_compiler_protocol::CompilerIdentity;
use serde::Deserialize;

const SUPPORT_MANIFEST: &str = include_str!("../../compiler/supported-rustc.toml");

#[derive(Debug, Deserialize)]
struct SupportManifest {
    verified_on: String,
    window_start: String,
    window_days: u16,
    verified_host: String,
    toolchains: Vec<SupportedToolchain>,
}

#[derive(Debug, Deserialize)]
struct SupportedToolchain {
    kind: String,
    release: String,
    release_date: Option<String>,
    commit_hash: String,
    commit_date: String,
}

impl SupportManifest {
    fn validate(&self) -> Result<()> {
        if self.window_days != 365
            || self.window_start.len() != 10
            || self.verified_on.len() != 10
            || self.window_start > self.verified_on
        {
            bail!("rustc support manifest has an invalid verification window");
        }
        if self.verified_host.is_empty() {
            bail!("rustc support manifest has no verified host");
        }

        let mut releases = BTreeSet::new();
        let mut previous_stable = None;
        for toolchain in &self.toolchains {
            if !releases.insert(&toolchain.release) {
                bail!(
                    "rustc support manifest repeats release {}",
                    toolchain.release
                );
            }
            let release_version = release_version(&toolchain.release)?;
            if toolchain.commit_hash.len() != 40
                || !toolchain
                    .commit_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || toolchain.commit_date.len() != 10
            {
                bail!(
                    "rustc support manifest has an invalid identity for {}",
                    toolchain.release
                );
            }
            match (toolchain.kind.as_str(), toolchain.release_date.as_deref()) {
                ("stable", Some(date))
                    if date >= self.window_start.as_str() && date <= self.verified_on.as_str() =>
                {
                    if previous_stable.is_some_and(|previous| previous >= release_version) {
                        bail!(
                            "rustc support manifest stable releases are not numerically sorted at {}",
                            toolchain.release
                        );
                    }
                    previous_stable = Some(release_version);
                }
                ("development", None) => {}
                _ => bail!(
                    "rustc support manifest has invalid release provenance for {}",
                    toolchain.release
                ),
            }
        }
        Ok(())
    }
}

fn release_version(release: &str) -> Result<(u32, u32, u32)> {
    let numeric = release
        .split_once('-')
        .map_or(release, |(numeric, _)| numeric);
    let components = numeric
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<Vec<u32>, _>>()
        .with_context(|| format!("rustc support manifest has invalid release {release}"))?;
    match components.as_slice() {
        [major, minor, patch] => Ok((*major, *minor, *patch)),
        _ => bail!("rustc support manifest has invalid release {release}"),
    }
}

pub(super) fn validate(compiler: &CompilerIdentity) -> Result<()> {
    let manifest = manifest()?;
    let exact = manifest.toolchains.iter().any(|toolchain| {
        toolchain.release == compiler.release
            && toolchain.commit_hash == compiler.commit_hash
            && toolchain.commit_date == compiler.commit_date
            && manifest.verified_host == compiler.host
    });
    if exact {
        return Ok(());
    }

    let supported = manifest
        .toolchains
        .iter()
        .map(|toolchain| toolchain.release.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "rustc {} ({} for {}) is outside rot-audit's exact compatibility set verified on {}; supported compiler releases: {}",
        compiler.release,
        compiler.commit_hash,
        compiler.host,
        manifest.verified_on,
        supported,
    )
}

fn manifest() -> Result<SupportManifest> {
    let manifest: SupportManifest =
        toml::from_str(SUPPORT_MANIFEST).context("embedded rustc support manifest is malformed")?;
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_manifest_is_a_sorted_exact_one_year_ledger() {
        let manifest = manifest().unwrap();
        assert_eq!(manifest.verified_on, "2026-08-29");
        assert_eq!(manifest.window_start, "2025-08-29");
        assert_eq!(manifest.window_days, 365);
        assert_eq!(manifest.verified_host, "aarch64-apple-darwin");

        let stable = manifest
            .toolchains
            .iter()
            .filter(|toolchain| toolchain.kind == "stable")
            .collect::<Vec<_>>();
        assert_eq!(stable.len(), 14);
        assert_eq!(stable.first().unwrap().release, "1.90.0");
        assert_eq!(stable.last().unwrap().release, "1.98.0");
        assert!(
            stable
                .windows(2)
                .all(|pair| release_version(&pair[0].release).unwrap()
                    < release_version(&pair[1].release).unwrap())
        );
        assert!(stable.iter().all(|toolchain| {
            toolchain.release_date.as_deref().is_some_and(|date| {
                date >= manifest.window_start.as_str() && date <= manifest.verified_on.as_str()
            })
        }));

        let releases = manifest
            .toolchains
            .iter()
            .map(|toolchain| &toolchain.release)
            .collect::<BTreeSet<_>>();
        assert_eq!(releases.len(), manifest.toolchains.len());
        assert!(manifest.toolchains.iter().all(|toolchain| {
            toolchain.commit_hash.len() == 40
                && toolchain
                    .commit_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && toolchain.commit_date.len() == 10
        }));
    }

    #[test]
    fn release_order_is_numeric_across_three_digit_minors() {
        assert!(release_version("1.99.0").unwrap() < release_version("1.100.0").unwrap());
    }

    #[test]
    fn exact_identity_and_host_are_required() {
        let manifest = manifest().unwrap();
        for toolchain in &manifest.toolchains {
            let compiler = CompilerIdentity {
                release: toolchain.release.clone(),
                commit_hash: toolchain.commit_hash.clone(),
                commit_date: toolchain.commit_date.clone(),
                host: manifest.verified_host.clone(),
            };
            validate(&compiler).unwrap();
        }

        let toolchain = &manifest.toolchains[1];
        let compiler = CompilerIdentity {
            release: toolchain.release.clone(),
            commit_hash: toolchain.commit_hash.clone(),
            commit_date: toolchain.commit_date.clone(),
            host: manifest.verified_host.clone(),
        };
        let mut wrong_host = compiler.clone();
        wrong_host.host = "x86_64-unknown-linux-gnu".to_owned();
        assert!(validate(&wrong_host).is_err());
    }
}
