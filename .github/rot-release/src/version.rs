use anyhow::{Result, anyhow};
use semver::{BuildMetadata, Prerelease, Version};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Bump {
    Minor,
    Patch,
}

impl Bump {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Minor => "minor",
            Self::Patch => "patch",
        }
    }
}

pub fn parse_version(raw: &str) -> std::result::Result<Version, String> {
    let parsed = Version::parse(raw).map_err(|_| invalid(raw))?;
    if parsed.pre != Prerelease::EMPTY || parsed.build != BuildMetadata::EMPTY {
        return Err(invalid(raw));
    }
    Ok(parsed)
}

pub(crate) fn version(raw: &str) -> Result<Version> {
    parse_version(raw).map_err(|message| anyhow!(message))
}

pub(crate) fn next(mut version: Version, bump: Bump) -> Result<Version> {
    match bump {
        Bump::Minor => {
            version.minor = version
                .minor
                .checked_add(1)
                .ok_or_else(|| anyhow!("minor version overflowed"))?;
            version.patch = 0;
        }
        Bump::Patch => {
            version.patch = version
                .patch
                .checked_add(1)
                .ok_or_else(|| anyhow!("patch version overflowed"))?;
        }
    }
    Ok(version)
}

pub(crate) fn feature_subject(subject: &str) -> bool {
    let Some(rest) = subject.strip_prefix("feat") else {
        return false;
    };
    if rest.starts_with(':') || rest.starts_with("!:") {
        return true;
    }
    let Some((scope, suffix)) = rest.strip_prefix('(').and_then(|rest| rest.split_once(')')) else {
        return false;
    };
    !scope.is_empty()
        && !scope.contains(['(', ')'])
        && (suffix.starts_with(':') || suffix.starts_with("!:"))
}

fn invalid(raw: &str) -> String {
    format!("expected MAJOR.MINOR.PATCH, got {raw:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_versions() {
        assert_eq!(parse_version("12.3.4").unwrap(), Version::new(12, 3, 4));
        for raw in ["1", "1.2", "1.2.3.4", "01.2.3", "1.2.3-a", "v1.2.3"] {
            assert!(parse_version(raw).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn conventional_feature_grammar() {
        for subject in ["feat: x", "feat!: x", "feat(cli): x", "feat(cli)!: x"] {
            assert!(feature_subject(subject));
        }
        for subject in [
            "Feat: x",
            "feature: x",
            "feat(): x",
            "feat(a(b)): x",
            "fix: x",
        ] {
            assert!(!feature_subject(subject));
        }
    }
}
