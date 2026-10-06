//! Issue #1281: every vendor-mock template under `templates/` must load on the engine that ships
//! it, stay declarative, and keep to its reserved port block.
//!
//! The entrypoint is rendered with [`rift_ejs::render`] — the same call the config loader makes for
//! a local `--configfile` — so `<%- stringify('fixtures/…') %>` and env knobs are exercised, then each
//! imposter is deserialized as [`ImposterConfig`]. Booting the real binary on each template and
//! running its `smoke.sh` is `scripts/verify-templates.sh`; this test is the fast, cargo-level half.
//!
//! Templates are discovered by glob, so a new template is gated without editing this file.

use rift_mock_core::imposter::ImposterConfig;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The range the catalog reserves for templates, and the size of each template's block.
const RESERVED: std::ops::RangeInclusive<u16> = 4600..=4999;
const BLOCK: u16 = 20;

/// Keys that make a document depend on `--allow-injection`.
const INJECTION_KEYS: [&str; 3] = ["inject", "decorate", "shellTransform"];

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/rift-mock-core
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

struct Template {
    name: String,
    dir: PathBuf,
    manifest: Value,
    /// The entrypoint after EJS rendering, parsed as JSON.
    document: Value,
    imposters: Vec<ImposterConfig>,
}

impl Template {
    /// Every port the template binds: its imposters plus its intercept listener.
    fn ports(&self) -> BTreeSet<u16> {
        let mut ports: BTreeSet<u16> = self
            .imposters
            .iter()
            .map(|i| {
                i.port
                    .unwrap_or_else(|| panic!("{}: an imposter declares no port", self.name))
            })
            .collect();
        if let Some(port) = self.document.pointer("/intercept/port") {
            ports.insert(as_port(port, &format!("{}: intercept.port", self.name)));
        }
        ports
    }
}

fn as_port(v: &Value, what: &str) -> u16 {
    v.as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or_else(|| panic!("{what} is not a port: {v}"))
}

fn load_template(dir: &Path) -> Template {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .expect("utf8 template dir")
        .to_owned();
    let manifest_path = dir.join("template.json");
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("{name}: read template.json: {e}")),
    )
    .unwrap_or_else(|e| panic!("{name}: template.json is not JSON: {e}"));

    let entrypoint = manifest["entrypoint"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: template.json has no string `entrypoint`"));
    let entry_path = dir.join(entrypoint);
    let raw = std::fs::read_to_string(&entry_path)
        .unwrap_or_else(|e| panic!("{name}: read {entrypoint}: {e}"));
    let rendered = rift_ejs::render(&raw, &entry_path, rift_ejs::FileAccess::Allowed)
        .unwrap_or_else(|e| panic!("{name}: {entrypoint} does not render: {e}"));
    // A knob must carry a default, so the template loads with no environment set up.
    assert!(
        rendered.unset_env.is_empty(),
        "{name}: env knobs without a default: {:?}",
        rendered
            .unset_env
            .iter()
            .map(rift_ejs::UnsetEnv::describe)
            .collect::<Vec<_>>()
    );
    let document: Value = serde_json::from_str(&rendered.text)
        .unwrap_or_else(|e| panic!("{name}: {entrypoint} is not JSON after rendering: {e}"));
    let imposters = document["imposters"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: {entrypoint} is not an `imposters` wrapper"))
        .iter()
        .enumerate()
        .map(|(i, imposter)| {
            serde_json::from_value(imposter.clone())
                .unwrap_or_else(|e| panic!("{name}: imposters[{i}] does not deserialize: {e}"))
        })
        .collect();

    Template {
        name,
        dir: dir.to_path_buf(),
        manifest,
        document,
        imposters,
    }
}

fn templates() -> Vec<Template> {
    let root = repo_root().join("templates");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("templates/ exists")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "no template under {}", root.display());
    dirs.iter().map(|d| load_template(d)).collect()
}

/// Paths (as `a.b[2].c`) of every injection key in `v`, including `_rift.script`.
fn injection_sites(v: &Value, at: &str, found: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (key, child) in map {
                let here = format!("{at}.{key}");
                if INJECTION_KEYS.contains(&key.as_str()) {
                    found.push(here.clone());
                }
                if key == "_rift" && child.get("script").is_some() {
                    found.push(format!("{here}.script"));
                }
                // A string `wait` is a JavaScript function, not a number of milliseconds.
                if key == "_behaviors" && child.get("wait").is_some_and(Value::is_string) {
                    found.push(format!("{here}.wait"));
                }
                injection_sites(child, &here, found);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                injection_sites(child, &format!("{at}[{i}]"), found);
            }
        }
        _ => {}
    }
}

#[test]
fn every_shipped_template_loads() {
    for t in templates() {
        assert_eq!(
            t.manifest["name"].as_str(),
            Some(t.name.as_str()),
            "{}: template.json `name` must equal the directory name",
            t.name
        );
        assert!(!t.imposters.is_empty(), "{}: declares no imposter", t.name);
        assert!(
            t.dir.join("smoke.sh").is_file(),
            "{}: smoke.sh is required",
            t.name
        );
        assert!(
            t.dir.join("README.md").is_file(),
            "{}: README.md is required",
            t.name
        );
        for (i, imposter) in t.imposters.iter().enumerate() {
            assert!(
                imposter.record_requests,
                "{}: imposters[{i}] must set recordRequests: true",
                t.name
            );
            assert!(
                imposter.name.as_deref().is_some_and(|n| !n.is_empty()),
                "{}: imposters[{i}] must be named after the vendor host it stands in for",
                t.name
            );
        }
    }
}

#[test]
fn shipped_templates_are_declarative() {
    for t in templates() {
        assert_eq!(
            t.manifest.pointer("/requires/flags"),
            Some(&Value::Array(vec![])),
            "{}: requires.flags must be [] — a template loads on a plain `rift --configfile`",
            t.name
        );
        let mut found = Vec::new();
        injection_sites(&t.document, "$", &mut found);
        assert!(
            found.is_empty(),
            "{}: injection surface in the entrypoint: {found:?}",
            t.name
        );
    }
}

#[test]
fn shipped_templates_stay_in_their_port_block() {
    for t in templates() {
        let first = as_port(
            &t.manifest["ports"]["first"],
            &format!("{}: ports.first", t.name),
        );
        let last = as_port(
            &t.manifest["ports"]["last"],
            &format!("{}: ports.last", t.name),
        );
        assert!(
            RESERVED.contains(&first) && RESERVED.contains(&last) && last - first + 1 == BLOCK,
            "{}: ports {first}–{last} must be one {BLOCK}-port block inside {RESERVED:?}",
            t.name
        );
        assert_eq!(
            (first - RESERVED.start()) % BLOCK,
            0,
            "{}: block {first}–{last} must start on a {BLOCK}-port boundary of {RESERVED:?}, so \
             blocks tile the range instead of overlapping",
            t.name
        );
        let ports = t.ports();
        for port in &ports {
            assert!(
                (first..=last).contains(port),
                "{}: port {port} is outside the template's block {first}–{last}",
                t.name
            );
        }
        // The manifest's port table is what the catalog page shows; it must not drift.
        let listed: BTreeSet<u16> = t.manifest["imposters"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: template.json has no `imposters` list", t.name))
            .iter()
            .map(|i| {
                as_port(
                    &i["port"],
                    &format!("{}: template.json imposters[].port", t.name),
                )
            })
            .collect();
        assert_eq!(
            listed, ports,
            "{}: template.json lists different ports than the entrypoint binds",
            t.name
        );
        // Every intercept forward must land on one of the template's own imposters.
        let imposter_ports: BTreeSet<u16> = t.imposters.iter().filter_map(|i| i.port).collect();
        for rule in t
            .document
            .pointer("/intercept/rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(port) = rule.pointer("/action/forward/port") {
                let port = as_port(port, &format!("{}: intercept forward port", t.name));
                assert!(
                    imposter_ports.contains(&port),
                    "{}: intercept rule {rule} forwards to {port}, which no imposter binds",
                    t.name
                );
            }
        }
    }
}

/// Ports bound by the single-file configs under `dir` (imposter ports and intercept listeners).
fn ports_in_dir(dir: &str) -> BTreeMap<u16, String> {
    let mut ports = BTreeMap::new();
    for entry in std::fs::read_dir(repo_root().join(dir)).expect("fixture dir exists") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let file = format!("{dir}/{}", path.file_name().expect("file name").display());
        let doc: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {file}: {e}")),
        )
        .unwrap_or_else(|e| panic!("{file} is not JSON: {e}"));
        let imposters = match (&doc, doc.get("imposters")) {
            (_, Some(Value::Array(list))) => list.clone(),
            (Value::Array(list), None) => list.clone(),
            (single, None) => vec![single.clone()],
            (_, Some(other)) => panic!("{file}: `imposters` is not a list: {other}"),
        };
        let intercept = doc.pointer("/intercept/port").cloned();
        for port in imposters
            .iter()
            .filter_map(|i| i.get("port").cloned())
            .chain(intercept)
        {
            ports.insert(as_port(&port, &file), file.clone());
        }
    }
    ports
}

/// Templates, `examples/` and `docs/demo/` are meant to run side by side, so a template port may be
/// bound by nothing else. (`examples/` and `docs/demo/` reuse ports among themselves — each is
/// loaded alone — which is why only template ports are checked against the others.)
#[test]
fn shipped_template_ports_are_unique() {
    let mut owner: BTreeMap<u16, String> = BTreeMap::new();
    for t in templates() {
        for port in t.ports() {
            if let Some(other) = owner.insert(port, t.name.clone()) {
                panic!(
                    "port {port} is used by both templates `{other}` and `{}`",
                    t.name
                );
            }
        }
    }
    for dir in ["examples", "docs/demo"] {
        let others = ports_in_dir(dir);
        assert!(!others.is_empty(), "{dir}/ moved or emptied?");
        for (port, file) in others {
            if let Some(template) = owner.get(&port) {
                panic!("port {port} is used by template `{template}` and by {file}");
            }
        }
    }
}

/// Two templates may not claim the same block, even if neither binds every port in it.
#[test]
fn shipped_template_port_blocks_do_not_overlap() {
    let mut owners: std::collections::BTreeMap<u16, String> = std::collections::BTreeMap::new();
    for t in templates() {
        let first = as_port(
            &t.manifest["ports"]["first"],
            &format!("{}: ports.first", t.name),
        );
        if let Some(other) = owners.insert(first, t.name.clone()) {
            panic!(
                "{} and {other} both claim the port block starting at {first}",
                t.name
            );
        }
    }
}
