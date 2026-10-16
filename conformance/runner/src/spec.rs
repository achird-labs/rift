//! Where the specification lives, and which of its features this runner has ported.

use std::path::PathBuf;

/// The feature files this runner runs. Each one ported here is also listed in the Scala runner's
/// `conformance/ported.txt` (EtaCassiopeia/rift-conformance#244), which drops it from that runner's
/// external, embedded and lint legs, so every feature has exactly one runner during the move.
/// Batch 1 (#1346) is 00, 01, 02 and 09, batch 2 (#1347) 06, 07, 11, 14, 17 and 18, batch 3 (#1348)
/// 03, 05, 10, 19 and 21; later batches: #1349, #1350. Embedded mode leaves out `@upstream-trust`
/// (09), `@front-door` (11) and `@rift-debug-env` (05, 19); container mode those, `@admin-isolated`
/// (21), `@full-corpus-verify` (01), `@reload-engine` (11), `@connect-refused` (21) and `@tcp-rst`
/// (03): see `harness::Mode::excludes`.
pub const PORTED_FEATURES: &[&str] = &[
    "00-smoke.feature",
    "01-consistency-gate.feature",
    "02-core-serving.feature",
    "09-gateway-https-metrics.feature",
    // Batch 2 (#1347).
    "06-behaviors.feature",
    "07-spaces-and-fsm.feature",
    "11-reload.feature",
    "14-behaviors-response-types.feature",
    "17-metrics.feature",
    "18-match-diagnostics.feature",
    // Batch 3 (#1348).
    "03-faults-transport.feature",
    "05-templating.feature",
    "10-proxy-record.feature",
    "19-flow-state-declarative.feature",
    "21-admin-rest.feature",
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
