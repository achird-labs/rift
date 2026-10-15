//! Issue #1339: `conformance/manifest.json` lists exactly the Plane A feature files on disk.
//!
//! The manifest is what a consumer of `rift-conformance-<version>.tar.gz` reads to know which
//! suites a release specifies, so a feature added without its entry, or an entry whose file was
//! deleted or renamed, would publish a manifest that lies about the tarball. Gherkin validity is
//! not checked here: the runner's `lintFeatures` dry-run is that gate (#1340).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn conformance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../conformance")
        .canonicalize()
        .expect("conformance/ exists at the repository root")
}

/// The `Feature:` title of a feature file, as the manifest records it.
fn feature_title(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Feature:"))
        .map(str::trim)
}

#[test]
fn the_manifest_lists_exactly_the_feature_files_on_disk() {
    let root = conformance_dir();
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("manifest.json")).expect("read manifest.json"),
    )
    .expect("manifest.json is JSON");
    assert_eq!(manifest["schemaVersion"], 1);
    assert_eq!(
        manifest["engineVersion"], "0.0.0-dev",
        "the checked-in manifest carries the placeholder; the packager stamps the real version"
    );

    let listed: BTreeMap<String, String> = manifest["features"]
        .as_array()
        .expect("features is an array")
        .iter()
        .map(|f| {
            (
                f["file"].as_str().expect("file is a string").to_owned(),
                f["name"].as_str().expect("name is a string").to_owned(),
            )
        })
        .collect();

    let mut on_disk: BTreeMap<String, String> = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("features/rift")).expect("read features/rift") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_some_and(|e| e == "feature") {
            let text = std::fs::read_to_string(&path).expect("read feature");
            let title = feature_title(&text)
                .unwrap_or_else(|| panic!("{} has no `Feature:` line", path.display()));
            let name = path.file_name().expect("file name").to_string_lossy();
            on_disk.insert(format!("features/rift/{name}"), title.to_owned());
        }
    }

    // Anti-vacuity: an empty tree and an empty manifest would agree.
    assert!(
        on_disk.len() > 20,
        "expected the Plane A suites under conformance/features/rift, found {}",
        on_disk.len()
    );
    assert_eq!(
        listed, on_disk,
        "conformance/manifest.json must list every feature file under conformance/features/rift, \
         with its `Feature:` title as `name`, and nothing else"
    );
}

/// The TLS and intercept suites load certificates and keys that the repo-wide `*.pem`/`*.key`
/// ignore rules would silently leave out of every checkout (#1339): the tree would still look
/// whole to every count-based check. Pin a sample of them so a missing one fails here.
#[test]
fn the_tls_and_intercept_key_material_is_in_the_tree() {
    let fixtures = conformance_dir().join("rift-fixtures");
    for file in [
        "intercept/ca-cert.pem",
        "intercept/ca-key.pem",
        "tls/server-key.pem",
    ] {
        assert!(
            fixtures.join(file).is_file(),
            "conformance/rift-fixtures/{file} is missing — is .gitignore excluding it?"
        );
    }
}
