//! `BUILD_INFO`: the build identity a staged `RIFT_BUILD_DIR` carries, written by
//! `.github/workflows/conformance.yml`'s staging step (and the release build script). Feature 01
//! asserts each binary's live `--version` still matches what this recorded.

use crate::HarnessError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub commit: String,
    pub branch: String,
    pub built_at: String,
    /// Binary name → sha256.
    pub binaries: BTreeMap<String, String>,
    /// Binary name → first line of its `--version`.
    pub versions: BTreeMap<String, String>,
}

impl BuildInfo {
    /// Read `<build_dir>/BUILD_INFO`. Absent or malformed is an error: it means the build dir was
    /// not staged the way the gate assumes, which must not be papered over.
    pub fn load(build_dir: &Path) -> Result<Self, HarnessError> {
        let path = build_dir.join("BUILD_INFO");
        let raw = std::fs::read_to_string(&path).map_err(|source| HarnessError::Io {
            action: "reading",
            path: path.clone(),
            source,
        })?;
        Self::parse(&raw).map_err(|source| HarnessError::BuildInfo { path, source })
    }

    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_staging_step_shape() {
        let info = BuildInfo::parse(
            r#"{"commit":"abc","branch":"b","builtAt":"2026-10-15T00:00:00Z",
                "binaries":{"rift":"00"},"versions":{"rift":"rift 0.22.0"}}"#,
        )
        .expect("valid BUILD_INFO");
        assert_eq!(info.commit, "abc");
        assert_eq!(info.versions["rift"], "rift 0.22.0");
    }

    #[test]
    fn a_missing_field_is_an_error_not_a_default() {
        assert!(BuildInfo::parse(r#"{"commit":"abc"}"#).is_err());
    }
}
