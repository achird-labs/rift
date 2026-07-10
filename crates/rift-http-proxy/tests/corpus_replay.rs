//! Issue #460: the vendored SDK-conformance corpus — published per release as
//! `sdk-conformance-<version>.tar.gz` and replayed by every SDK's CI — must serve and its
//! `_verify` transcripts must hold on THIS engine commit. This is the engine-side invariant that
//! keeps the published artifact *verified*, not merely hoped: it mirrors the reference replayer the
//! README documents (`rift-verify --skip-dynamic --verify-dynamic`) over the whole corpus, guards
//! `manifest.json` against drift from the fixtures on disk, and proves the *packaged* tarball (not
//! just the source tree) still serves after `cp`/`jq`/re-tar.

use std::path::{Path, PathBuf};
use std::time::Duration;

const SERVER: &str = env!("CARGO_BIN_EXE_rift-http-proxy");
const VERIFY: &str = env!("CARGO_BIN_EXE_rift-verify");
/// The closed set of capability gates a fixture may declare in `manifest.json` `requires`
/// (extended additively). An SDK CI skips a fixture only when it lacks a declared capability.
const CLOSED_CAPS: &[&str] = &["injection", "proxy", "redis", "https", "shell"];

/// `<repo>/sdk-conformance` (this test lives in `<repo>/crates/rift-http-proxy`).
fn sdk_conformance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk-conformance")
        .canonicalize()
        .expect("sdk-conformance directory exists")
}

/// Every fixture's imposters/*.json path under `<corpus>/imposters`, sorted.
fn fixture_files(corpus: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus.join("imposters"))
        .expect("read imposters dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

#[test]
fn corpus_lints_clean() {
    let dir = sdk_conformance_dir().join("corpus/imposters");
    let result = rift_lint::lint_directory(&dir, &rift_lint::LintOptions::default());
    assert!(
        result.files_checked >= 15,
        "expected the full corpus (>=15 fixtures), linted {}",
        result.files_checked
    );
    assert!(
        !result.has_errors(),
        "corpus fixtures must lint clean, found {} error(s): {:#?}",
        result.errors,
        result
            .issues
            .iter()
            .filter(|i| matches!(i.severity, rift_lint::Severity::Error))
            .collect::<Vec<_>>()
    );
}

#[test]
fn manifest_is_consistent() {
    let root = sdk_conformance_dir();
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("manifest.json")).expect("read manifest"),
    )
    .expect("manifest is valid JSON");

    assert_eq!(manifest["schemaVersion"], 1, "schemaVersion must be 1");
    assert!(
        manifest["engineVersion"].is_string(),
        "engineVersion must be present (stamped at packaging time)"
    );
    let fixtures = manifest["fixtures"].as_array().expect("fixtures array");

    // Every fixture on disk is listed exactly once, and every listed file exists.
    let mut listed: Vec<String> = fixtures
        .iter()
        .map(|f| {
            f["file"]
                .as_str()
                .expect("fixture.file is a string")
                .to_string()
        })
        .collect();
    listed.sort();
    let corpus = root.join("corpus");
    let mut on_disk: Vec<String> = fixture_files(&corpus)
        .iter()
        .map(|p| {
            format!(
                "corpus/imposters/{}",
                p.file_name().unwrap().to_str().unwrap()
            )
        })
        .collect();
    on_disk.sort();
    assert_eq!(
        listed, on_disk,
        "manifest must list every corpus fixture exactly once"
    );

    // Structural + SEMANTIC checks: the manifest's port/requires/hasVerify must match the fixture's
    // actual content, so a fixture edited on one side without the other can't drift past the gate
    // (an SDK trusts these fields to decide what to skip and how to replay).
    let mut ports = std::collections::HashSet::new();
    for f in fixtures {
        let file = f["file"].as_str().unwrap();
        let path = root.join(file);
        assert!(
            path.exists(),
            "manifest references {file}, which does not exist"
        );
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let raw = serde_json::to_string(&fixture).unwrap();

        let port = f["port"].as_u64().expect("fixture.port is a number");
        assert!(ports.insert(port), "duplicate port {port} across fixtures");
        assert_eq!(
            Some(port),
            fixture["port"].as_u64(),
            "{file}: manifest port {port} disagrees with the fixture's own port"
        );

        for cap in f["requires"]
            .as_array()
            .expect("fixture.requires is an array")
        {
            let cap = cap.as_str().expect("capability is a string");
            assert!(
                CLOSED_CAPS.contains(&cap),
                "fixture {file} declares unknown capability {cap:?} (closed set: {CLOSED_CAPS:?})"
            );
        }
        let requires: Vec<&str> = f["requires"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();

        // hasVerify ⇔ the fixture actually carries a `_verify` block.
        let has_verify_field = f["hasVerify"].as_bool().expect("hasVerify is a bool");
        assert_eq!(
            has_verify_field,
            raw.contains("\"_verify\""),
            "{file}: manifest hasVerify={has_verify_field} disagrees with the fixture content"
        );

        // A scripting surface (inject/decorate/shellTransform/JS-wait/_rift.script) ⇒ requires
        // injection; a proxy response ⇒ requires proxy. Under-declaring these would make an SDK
        // lane wrongly RUN a fixture it can't support.
        let needs_injection = raw.contains("\"inject\"")
            || raw.contains("\"decorate\"")
            || raw.contains("\"shellTransform\"")
            || raw.contains("\"wait\": \"function")
            || (raw.contains("\"_rift\"") && raw.contains("\"script\""));
        if needs_injection {
            assert!(
                requires.contains(&"injection"),
                "{file} uses a scripting surface but does not declare requires:[injection]"
            );
        }
        if raw.contains("\"proxy\"") {
            assert!(
                requires.contains(&"proxy"),
                "{file} has a proxy response but does not declare requires:[proxy]"
            );
        }
    }
}

// Serialized against `packaged_corpus_serves_and_verify_holds`: both serve the whole corpus on its
// fixtures' fixed ports (4501–4520), so running them concurrently races on those binds and flakes.
#[tokio::test]
#[serial_test::serial(corpus)]
async fn corpus_serves_and_verify_holds() {
    let corpus = sdk_conformance_dir().join("corpus");
    serve_and_verify(&corpus).await;
}

/// Issue #460 AC-f: the *packaged* tarball (not just the source tree) must still serve. Packaging
/// does real transforms — `cp -R`, a `jq` rewrite of `manifest.json`, and a re-tar under a
/// version-prefixed root — any of which could silently drop a fixture or a `data/`/injection file.
/// Run the packager, extract, and replay against the extracted `corpus/`.
#[tokio::test]
#[serial_test::serial(corpus)]
async fn packaged_corpus_serves_and_verify_holds() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/gen-sdk-conformance.sh");
    let work = tempfile::tempdir().expect("tempdir");
    let tarball = work.path().join("sdk-conformance-0.0.0-test.tar.gz");

    let pkg = std::process::Command::new("bash")
        .arg(&script)
        .arg("0.0.0-test")
        .arg(&tarball)
        .output()
        .expect("run gen-sdk-conformance.sh");
    assert!(
        pkg.status.success(),
        "packaging failed:\n{}",
        String::from_utf8_lossy(&pkg.stderr)
    );

    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(work.path())
        .status()
        .expect("extract tarball");
    assert!(status.success(), "tar extraction failed");

    let extracted_corpus = work.path().join("sdk-conformance-0.0.0-test/corpus");
    assert!(
        extracted_corpus.join("imposters").is_dir(),
        "packaged tarball is missing corpus/imposters"
    );
    serve_and_verify(&extracted_corpus).await;
}

/// Serve every fixture under `<corpus>/imposters` from cwd = `<corpus>` (so relative `data/` paths
/// resolve) with `--allowInjection`, then assert `rift-verify --skip-dynamic --verify-dynamic`
/// exits 0 and every `_verify` block on disk actually replayed (not a corpus-global no-op).
async fn serve_and_verify(corpus: &Path) {
    let files = fixture_files(corpus);
    let imposters: Vec<serde_json::Value> = files
        .iter()
        .map(|f| serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap())
        .collect();
    let expected = imposters.len();
    // The number of `_verify` blocks across the corpus; each replayed block prints a `_verify[0]`
    // step, so a fixture whose annotation stops being honored drops this count.
    let expected_verify_blocks: usize = files
        .iter()
        .map(|f| {
            std::fs::read_to_string(f)
                .unwrap()
                .matches("\"_verify\"")
                .count()
        })
        .sum();

    let config = serde_json::json!({ "imposters": imposters });
    let tmp = tempfile::NamedTempFile::new().expect("temp config");
    std::fs::write(tmp.path(), serde_json::to_vec(&config).unwrap()).unwrap();

    let admin_port = port_check::free_local_port().expect("a free local port");
    let admin_url = format!("http://127.0.0.1:{admin_port}");
    // Capture the server's own stdout+stderr so a startup crash is diagnosable, not a blind timeout.
    let log = tempfile::NamedTempFile::new().expect("server log file");
    let log_out = log.reopen().expect("reopen log");
    let log_err = log.reopen().expect("reopen log");

    let mut server = tokio::process::Command::new(SERVER)
        .arg("--configfile")
        .arg(tmp.path())
        .args(["--port", &admin_port.to_string(), "--allowInjection"])
        .current_dir(corpus)
        .stdout(std::process::Stdio::from(log_out))
        .stderr(std::process::Stdio::from(log_err))
        .kill_on_drop(true)
        .spawn()
        .expect("spawn rift server");

    let ready =
        wait_for_imposters(&mut server, &admin_url, expected, Duration::from_secs(30)).await;
    if !ready {
        let _ = server.kill().await;
        panic!(
            "server did not load {expected} imposters within 30s.\nserver log:\n{}",
            std::fs::read_to_string(log.path()).unwrap_or_default()
        );
    }

    let out = tokio::process::Command::new(VERIFY)
        .args([
            "--admin-url",
            &admin_url,
            "--skip-dynamic",
            "--verify-dynamic",
        ])
        .output()
        .await
        .expect("run rift-verify");
    let _ = server.kill().await;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "rift-verify must exit 0 over the corpus at {}.\nstdout:\n{stdout}\nstderr:\n{stderr}",
        corpus.display()
    );
    // Every `_verify` block on disk must have actually replayed — guards against a fixture's
    // annotation silently ceasing to be honored while other fixtures keep the run green.
    let replayed = format!("{stdout}{stderr}").matches("_verify[0]").count();
    assert!(
        replayed >= expected_verify_blocks,
        "expected {expected_verify_blocks} _verify block(s) to replay, saw {replayed}:\n{stdout}"
    );
}

/// Poll `GET /imposters` until at least `expected` imposters are loaded, failing fast if the server
/// process dies first (rather than blindly waiting out the timeout).
async fn wait_for_imposters(
    server: &mut tokio::process::Child,
    admin_url: &str,
    expected: usize,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(_status)) = server.try_wait() {
            return false; // server exited before it was ready — caller surfaces the log
        }
        if let Ok(resp) = reqwest::get(format!("{admin_url}/imposters")).await
            && let Ok(v) = resp.json::<serde_json::Value>().await
            && v["imposters"].as_array().map_or(0, Vec::len) >= expected
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}
