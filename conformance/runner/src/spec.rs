//! Where the specification lives, and which of its features this runner has ported.

use std::path::PathBuf;

/// The feature files this runner runs. Each one ported here is meant to leave the Scala runner's
/// suite list, so every feature has exactly one runner during the move; for this first batch that
/// holds once EtaCassiopeia/rift-conformance#237 lands, and until then 00/01/02/09 run under both.
/// Later batches: #1347, #1348, #1349, #1350.
pub const PORTED_FEATURES: &[&str] = &[
    "00-smoke.feature",
    "01-consistency-gate.feature",
    "02-core-serving.feature",
    "09-gateway-https-metrics.feature",
];

/// The conformance tree: `features/`, `rift-fixtures/`, `rift-lint-fixtures/`, `rift-scripts/`.
/// `RIFT_FEATURES_DIR` points it elsewhere (an unpacked `rift-conformance-<ver>.tar.gz`, say), with
/// the same meaning it has for the Scala runner; by default it is this repository's `conformance/`.
#[must_use]
pub fn conformance_root() -> PathBuf {
    match std::env::var_os("RIFT_FEATURES_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."),
    }
}

#[must_use]
pub fn features_dir() -> PathBuf {
    conformance_root().join("features").join("rift")
}

#[must_use]
pub fn fixtures_dir() -> PathBuf {
    conformance_root().join("rift-fixtures")
}

/// The JSON corpus: `RIFT_CORPUS_ROOT`, by default this repository's `sdk-conformance/corpus`.
/// Its `imposters/` are feature 01's subject, and the spawned engine runs with it as its working
/// directory so a fixture's `data/...` path resolves.
#[must_use]
pub fn default_corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk-conformance/corpus")
}
