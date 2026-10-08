//! Issue #1343: every key the SDK-conformance corpus or the documentation writes is a key the
//! engine reads.
//!
//! No imposter-grammar type refuses unknown fields, and none may: rift-cluster replays stored
//! config bytes, so a refusal added at decode would make admitted configs undecodable. A key the
//! engine does not read is therefore accepted and dropped without a word — `casesensitive` for
//! `caseSensitive`, an `ns` the engine did not implement yet (#1326). This test is where that is
//! caught instead, over the inputs people copy from: the corpus fixtures, the `docs/demo` and
//! `examples` configs, and every ```` ```json ```` block in `docs/**/*.md` that is grammar.
//!
//! Two passes, because neither sees everything:
//! - `serde_ignored` around the decode the config door runs, which reports every key a derived
//!   `Deserialize` skipped.
//! - A walk over the JSON for what `serde_ignored` cannot see: keys under a `#[serde(flatten)]`
//!   (predicates), inside a tagged or untagged enum (`wait`, `using`, `_rift.fault.tcp`,
//!   `_rift.conditional`, `stateOps`), or in a `Value` the engine reads by key
//!   (`predicateGenerators`), and the response fields a higher-priority variant shadows. The
//!   predicate and generator lists come from `rift-types`, where the engine reads them from; each
//!   list this file still keeps names the code it mirrors and is pinned to the generated schema
//!   (`the_hand_lists_here_match_the_schema`).
//!
//! Since #1342 the same inputs are also validated against `sdk-conformance/schema/imposter.schema.json`,
//! the grammar generated from the engine's types. The two gates overlap on purpose: the schema is
//! what a consumer validates against, so it must accept everything the engine reads here, and it
//! closes every object, so it refuses an unread key the walk above would have to be taught.
//!
//! The docs blocks are also linted, so an example that teaches an error is caught too. A block that
//! shows rejected input on purpose opts out with ```` ```json title="invalid" ```` or a
//! `<!-- rift-lint: skip -->` line above its fence.

use std::path::{Path, PathBuf};

use rift_http_proxy::front_door::RouteTable;
use rift_mock_core::behaviors::{CANONICAL_ORDER, ResponseBehaviors};
use rift_mock_core::imposter::ImposterConfig;
use rift_types::{
    PREDICATE_GENERATOR_KEYS as GENERATOR_KEYS, PREDICATE_OPERATORS as OPERATORS,
    PREDICATE_PARAMETERS,
};
use serde_json::{Value, json};

/// The wrapper keys `parse_document` reads (`rift-http-proxy/src/config_loader.rs`).
const WRAPPER_KEYS: [&str; 3] = ["imposters", "intercept", "routes"];

/// `RiftFlowStateConfig`'s typed fields. Anything else lands in its `extra` map, which only an
/// embedder's `FlowStoreProvider` reads; standalone Rift drops it. That includes a `redis` block
/// left over from before #1356.
const FLOW_STATE_KEYS: [&str; 3] = ["backend", "ttlSeconds", "flowIdSource"];

/// The `StubResponse` variants in the priority `TryFrom<StubResponseRaw>` picks them: the first
/// present one is read and the rest are not. A response with none of them is a `_rift`-only
/// response when it has a `_rift`, else the flat form.
const RESPONSE_VARIANTS: [&str; 4] = ["is", "proxy", "inject", "fault"];

/// The flat response form (issue #304), read only when no variant is present.
const FLAT_RESPONSE_KEYS: [&str; 4] = ["statusCode", "headers", "body", "_mode"];

/// `RiftResponseExtension`'s fields, which decide that a bare `_rift` fragment is a response's.
const RIFT_RESPONSE_KEYS: [&str; 6] = [
    "fault",
    "script",
    "templated",
    "stateOps",
    "dataset",
    "conditional",
];

/// Unread keys the gate accepts, as `(pattern, why)`. A pattern matches the trailing segments of
/// a reported path, `*` standing for any one segment. Every entry needs a reason a reader can
/// check, and a follow-up issue when the key is a defect rather than a choice.
const ALLOWED: &[(&str, &str)] = &[(
    "stubs.*.name",
    "a label for the reader, written throughout the corpus and the demos; the engine has no stub \
     name to honour, so dropping it changes nothing a request can observe",
)];

/// Lint errors the gate accepts on a docs example, as `(file, code, why)`: the file the example is
/// in, so the entry survives edits that move it.
const LINT_ALLOWED: &[(&str, &str, &str)] = &[
    (
        "docs/mountebank/responses.md",
        "E019",
        "the page documents that the engine coerces a number header value, as Mountebank does; \
     rift-lint still calls it an error (#1358)",
    ),
    (
        "docs/mountebank/responses.md",
        "E020",
        "the same coercion for a boolean header value (#1358)",
    ),
];

fn allowed(path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    ALLOWED.iter().any(|(pattern, _)| {
        let pattern: Vec<&str> = pattern.split('.').collect();
        segments.len() >= pattern.len()
            && segments[segments.len() - pattern.len()..]
                .iter()
                .zip(&pattern)
                .all(|(segment, want)| *want == "*" || segment == want)
    })
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root exists")
}

/// A document the config door would be handed, and where it came from.
#[derive(Debug)]
struct Example {
    origin: String,
    document: Value,
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// A `serde_ignored` path in this file's spelling: it writes a `?` segment for each `Option` it
/// passes through, which the walk below does not.
fn serde_path(prefix: &str, path: &serde_ignored::Path<'_>) -> String {
    let path = path.to_string();
    let segments: Vec<&str> = path.split('.').filter(|s| *s != "?").collect();
    join(prefix, &segments.join("."))
}

fn items(value: Option<&Value>) -> impl Iterator<Item = (usize, &Value)> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
}

/// `value`'s elements when it is an array, else `value` itself: the "object or list of objects"
/// shape `copy`, `lookup` and a behaviors block share.
fn one_or_many(value: &Value, path: &str) -> Vec<(String, Value)> {
    match value {
        Value::Array(list) => list
            .iter()
            .enumerate()
            .map(|(i, v)| (join(path, &i.to_string()), v.clone()))
            .collect(),
        other => vec![(path.to_owned(), other.clone())],
    }
}

fn present(object: &serde_json::Map<String, Value>, key: &str) -> bool {
    object.get(key).is_some_and(|v| !v.is_null())
}

fn unread_outside(value: &Value, allowed: &[&str], path: &str, unread: &mut Vec<String>) {
    if let Some(object) = value.as_object() {
        unread.extend(
            object
                .keys()
                .filter(|key| !allowed.contains(&key.as_str()))
                .map(|key| join(path, key)),
        );
    }
}

/// Every key in `document` no consumer reads, as dotted paths (`stubs.0.predicates.0.x`), sorted.
/// `Err` when the config door refuses the document outright.
fn unread_keys(document: &Value) -> Result<Vec<String>, String> {
    let mut unread = Vec::new();
    match document.get("imposters") {
        Some(imposters) => {
            unread_outside(document, &WRAPPER_KEYS, "", &mut unread);
            if let Some(routes) = document.get("routes") {
                serde_ignored::deserialize::<_, _, RouteTable>(routes, |path| {
                    unread.push(serde_path("routes", &path));
                })
                .map_err(|e| format!("routes: {e}"))?;
            }
            for (i, imposter) in items(Some(imposters)) {
                unread_in_imposter(imposter, &format!("imposters.{i}"), &mut unread)?;
            }
        }
        None => unread_in_imposter(document, "", &mut unread)?,
    }
    unread.sort();
    unread.dedup();
    Ok(unread)
}

fn unread_in_imposter(
    imposter: &Value,
    path: &str,
    unread: &mut Vec<String>,
) -> Result<(), String> {
    serde_ignored::deserialize::<_, _, ImposterConfig>(imposter, |ignored| {
        unread.push(serde_path(path, &ignored));
    })
    .map_err(|e| join(path, &format!("does not decode: {e}")))?;

    if let Some(flow_state) = imposter.pointer("/_rift/flowState") {
        unread_outside(
            flow_state,
            &FLOW_STATE_KEYS,
            &join(path, "_rift.flowState"),
            unread,
        );
    }
    for (i, stub) in items(imposter.get("stubs")) {
        let stub_path = join(path, &format!("stubs.{i}"));
        // `rules` is an alias, read only when `predicates` is empty.
        let has_predicates = stub
            .get("predicates")
            .and_then(Value::as_array)
            .is_some_and(|p| !p.is_empty());
        if has_predicates && stub.get("rules").is_some_and(|r| !r.is_null()) {
            unread.push(join(&stub_path, "rules"));
        }
        for key in ["predicates", "rules"] {
            for (j, predicate) in items(stub.get(key)) {
                unread_in_predicate(predicate, &format!("{stub_path}.{key}.{j}"), unread);
            }
        }
        for (j, response) in items(stub.get("responses")) {
            unread_in_response(response, &format!("{stub_path}.responses.{j}"), unread)?;
        }
    }
    Ok(())
}

/// `Predicate` flattens its parameters and its operation, so serde buffers every key and the ones
/// neither half claims vanish before `serde_ignored` could see them.
fn unread_in_predicate(predicate: &Value, path: &str, unread: &mut Vec<String>) {
    let Some(object) = predicate.as_object() else {
        return;
    };
    let mut operators = Vec::new();
    for key in object.keys() {
        if OPERATORS.contains(&key.as_str()) {
            operators.push(key.as_str());
        } else if !PREDICATE_PARAMETERS.contains(&key.as_str()) {
            unread.push(join(path, key));
        }
    }
    // The flattened enum takes one operator; any second one is never evaluated.
    if operators.len() > 1 {
        unread.push(format!(
            "{path} (operators {operators:?}: only one is read)"
        ));
    }
    // The selector is a flattened `Option` of one enum, so it too holds one of the two.
    if object.contains_key("jsonpath") && object.contains_key("xpath") {
        unread.push(format!(
            "{path} (selectors [\"jsonpath\", \"xpath\"]: only one is read)"
        ));
    }
    if let Some(jsonpath) = object.get("jsonpath") {
        unread_outside(jsonpath, &["selector"], &join(path, "jsonpath"), unread);
    }
    if let Some(xpath) = object.get("xpath") {
        unread_outside(xpath, &["selector", "ns"], &join(path, "xpath"), unread);
    }
    if let Some(inner) = object.get("not") {
        unread_in_predicate(inner, &join(path, "not"), unread);
    }
    for key in ["and", "or"] {
        for (i, inner) in items(object.get(key)) {
            unread_in_predicate(inner, &format!("{path}.{key}.{i}"), unread);
        }
    }
}

fn unread_in_response(
    response: &Value,
    path: &str,
    unread: &mut Vec<String>,
) -> Result<(), String> {
    let Some(object) = response.as_object() else {
        return Ok(());
    };
    let variant = RESPONSE_VARIANTS
        .iter()
        .copied()
        .find(|key| present(object, key))
        .or_else(|| present(object, "_rift").then_some("_rift"));
    if let Some(read) = variant {
        for key in RESPONSE_VARIANTS {
            if key != read && present(object, key) {
                unread.push(join(path, key));
            }
        }
        // The flat fields, under the condition the engine reads them by: `headers` counts only
        // when it is non-empty.
        let flat = |key: &str| match key {
            "headers" => object
                .get("headers")
                .and_then(Value::as_object)
                .is_some_and(|h| !h.is_empty()),
            other => present(object, other),
        };
        unread.extend(
            FLAT_RESPONSE_KEYS
                .iter()
                .filter(|key| flat(key))
                .map(|key| join(path, key)),
        );
    }
    // A `proxy`, `inject` or `fault` response keeps its `_rift` only to report it ignored
    // (`ignored_rift`); `stub_analysis` warns `ConfigKeyIgnored` on it.
    let rift_read = matches!(variant, Some("is" | "_rift"));
    if !rift_read && present(object, "_rift") {
        unread.push(join(path, "_rift"));
    }
    // `_behaviors` wins over `behaviors` when both are set.
    let block = if present(object, "_behaviors") {
        if present(object, "behaviors") {
            unread.push(join(path, "behaviors"));
        }
        Some("_behaviors")
    } else {
        present(object, "behaviors").then_some("behaviors")
    };
    if let Some(key) = block {
        let block_path = join(path, key);
        if matches!(variant, Some("fault" | "_rift")) {
            // `ignored_behaviors`: only the cycler's `repeat` is read on these responses.
            for (element_path, element) in one_or_many(&object[key], &block_path) {
                unread_outside(&element, &["repeat"], &element_path, unread);
            }
        } else {
            unread_in_behaviors(&object[key], &block_path, unread)?;
        }
    }
    if let Some(proxy) = object.get("proxy") {
        for (i, generator) in items(proxy.get("predicateGenerators")) {
            let generator_path = join(path, &format!("proxy.predicateGenerators.{i}"));
            unread_outside(generator, &GENERATOR_KEYS, &generator_path, unread);
            if let Some(jsonpath) = generator.get("jsonpath") {
                unread_outside(
                    jsonpath,
                    &["selector"],
                    &join(&generator_path, "jsonpath"),
                    unread,
                );
            }
            if let Some(xpath) = generator.get("xpath") {
                unread_outside(
                    xpath,
                    &["selector", "ns"],
                    &join(&generator_path, "xpath"),
                    unread,
                );
            }
        }
    }
    if let Some(rift) = object.get("_rift").filter(|_| rift_read) {
        let rift_path = join(path, "_rift");
        if let Some(tcp) = rift.pointer("/fault/tcp") {
            // Read by hand from a `Value` (`RiftTcpFault`'s `Deserialize`).
            unread_outside(
                tcp,
                &["probability", "type"],
                &join(&rift_path, "fault.tcp"),
                unread,
            );
        }
        if let Some(conditional) = rift.get("conditional") {
            unread_outside(
                conditional,
                &["etag", "lastModified"],
                &join(&rift_path, "conditional"),
                unread,
            );
        }
        for (i, op) in items(rift.get("stateOps")) {
            // `StateOp` is internally tagged by `op`.
            let allowed: &[&str] = match op.get("op").and_then(Value::as_str) {
                Some("set") => &["op", "key", "value"],
                Some("increment") => &["op", "key", "by"],
                Some("delete") => &["op", "key"],
                _ => &["op"],
            };
            unread_outside(
                op,
                allowed,
                &join(&rift_path, &format!("stateOps.{i}")),
                unread,
            );
        }
        if let Some(key) = rift.pointer("/dataset/key") {
            unread_in_extraction(key, &join(&rift_path, "dataset.key"), unread);
        }
    }
    Ok(())
}

/// A behaviors block as `BehaviorProgram::from_elements` reads it: each element is a
/// `ResponseBehaviors`, whose derived keys `serde_ignored` sees, holding enums it cannot see into.
fn unread_in_behaviors(block: &Value, path: &str, unread: &mut Vec<String>) -> Result<(), String> {
    for (element_path, element) in one_or_many(block, path) {
        serde_ignored::deserialize::<_, _, ResponseBehaviors>(&element, |ignored| {
            unread.push(serde_path(&element_path, &ignored));
        })
        .map_err(|e| format!("{element_path}: behaviors do not decode: {e}"))?;

        if let Some(wait) = element.get("wait").filter(|w| w.is_object()) {
            // `WaitBehavior` is untagged and tries `{min, max}` before `{inject}`.
            let allowed: &[&str] = if wait.get("min").is_some() || wait.get("max").is_some() {
                &["min", "max"]
            } else {
                &["inject"]
            };
            unread_outside(wait, allowed, &join(&element_path, "wait"), unread);
        }
        if let Some(copy) = element.get("copy") {
            for (copy_path, copy) in one_or_many(copy, &join(&element_path, "copy")) {
                unread_in_extraction(&copy, &copy_path, unread);
            }
        }
        if let Some(lookup) = element.get("lookup") {
            for (lookup_path, lookup) in one_or_many(lookup, &join(&element_path, "lookup")) {
                if let Some(key) = lookup.get("key") {
                    unread_in_extraction(key, &join(&lookup_path, "key"), unread);
                }
            }
        }
    }
    Ok(())
}

/// The `from` and `using` a `copy` or a lookup `key` shares.
fn unread_in_extraction(holder: &Value, path: &str, unread: &mut Vec<String>) {
    if let Some(from) = holder.get("from") {
        // `CopySource::Nested` reads `query`, else `headers`.
        let from_path = join(path, "from");
        unread_outside(from, &["query", "headers"], &from_path, unread);
        if from.get("query").is_some() && from.get("headers").is_some() {
            unread.push(join(&from_path, "headers"));
        }
    }
    if let Some(using) = holder.get("using") {
        // `ExtractionMethod` is internally tagged by `method`.
        let allowed: &[&str] = match using.get("method").and_then(Value::as_str) {
            Some("regex") => &["method", "selector", "options"],
            Some("xpath") => &["method", "selector", "ns"],
            _ => &["method", "selector"],
        };
        let using_path = join(path, "using");
        unread_outside(using, allowed, &using_path, unread);
        if let Some(options) = using.get("options") {
            unread_outside(
                options,
                &["ignoreCase", "multiline"],
                &join(&using_path, "options"),
                unread,
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Docs extraction
// ---------------------------------------------------------------------------------------------

/// One ```` ```json ```` fence: its 1-based line, its body, and whether it opted out.
#[derive(Debug, PartialEq)]
struct Block {
    line: usize,
    body: String,
    skip: bool,
}

const SKIP_MARKER: &str = "<!-- rift-lint: skip -->";

fn json_blocks(markdown: &str) -> Vec<Block> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let indent = line.len() - line.trim_start().len();
        let Some(info) = line.trim_start().strip_prefix("```json") else {
            i += 1;
            continue;
        };
        // ```jsonc, ```json5 and friends are not JSON.
        if !(info.is_empty() || info.starts_with(char::is_whitespace)) {
            i += 1;
            continue;
        }
        let previous = lines[..i].iter().rev().find(|l| !l.trim().is_empty());
        let skip =
            info.contains("title=\"invalid\"") || previous.is_some_and(|l| l.trim() == SKIP_MARKER);
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && lines[end].trim() != "```" {
            end += 1;
        }
        let body = lines[start..end]
            .iter()
            .map(|l| {
                l.get(indent..)
                    .filter(|_| l[..indent].trim().is_empty())
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        blocks.push(Block {
            line: i + 1,
            body,
            skip,
        });
        i = end + 1;
    }
    blocks
}

fn is_predicate(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .keys()
            .any(|k| k != "inject" && OPERATORS.contains(&k.as_str()))
            || (object.contains_key("inject")
                && object
                    .keys()
                    .any(|k| PREDICATE_PARAMETERS.contains(&k.as_str())))
    })
}

fn imposter_of(stub: Value) -> Value {
    json!({ "protocol": "http", "stubs": [stub] })
}

/// The document the config door would read for a docs block, or `None` when the block is not
/// imposter grammar (an API response, an ECS task definition). A fragment — a stub, a response, a
/// predicate, a behaviors block — is wrapped in the smallest imposter that holds it.
fn as_document(value: Value) -> Option<Value> {
    if let Value::Array(list) = &value {
        return (!list.is_empty() && list.iter().all(is_predicate))
            .then(|| imposter_of(json!({ "predicates": value })));
    }
    let object = value.as_object()?;
    if object.get("imposters").is_some_and(Value::is_array) || object.contains_key("stubs") {
        return Some(value);
    }
    if ["responses", "predicates", "rules"]
        .iter()
        .any(|k| object.contains_key(*k))
    {
        return Some(imposter_of(value));
    }
    if is_predicate(&value) {
        return Some(imposter_of(json!({ "predicates": [value] })));
    }
    if ["is", "proxy", "inject", "fault", "_behaviors", "behaviors"]
        .iter()
        .any(|k| object.contains_key(*k))
    {
        // A behaviors fragment elides the response it belongs to; give it one, so the shadowing
        // check and the linter see a whole response rather than an empty one.
        let typed = RESPONSE_VARIANTS
            .iter()
            .chain(FLAT_RESPONSE_KEYS.iter())
            .chain(["_rift"].iter())
            .any(|k| object.contains_key(*k));
        let mut response = value;
        if !typed {
            response["is"] = json!({});
        }
        return Some(imposter_of(json!({ "responses": [response] })));
    }
    if object.contains_key("predicateGenerators") {
        let mut proxy = object.clone();
        proxy
            .entry("to")
            .or_insert_with(|| json!("http://127.0.0.1:1"));
        return Some(imposter_of(json!({ "responses": [{ "proxy": proxy }] })));
    }
    if let Some(rift) = object.get("_rift").filter(|_| object.len() == 1) {
        let response_level = rift
            .as_object()
            .is_some_and(|r| r.keys().any(|k| RIFT_RESPONSE_KEYS.contains(&k.as_str())));
        return Some(if response_level {
            imposter_of(json!({ "responses": [value] }))
        } else {
            json!({ "protocol": "http", "_rift": rift, "stubs": [] })
        });
    }
    let behavior_keys = || CANONICAL_ORDER.iter().copied().chain(["repeat"]);
    if !object.is_empty() && object.keys().all(|k| behavior_keys().any(|b| b == k)) {
        return Some(imposter_of(
            json!({ "responses": [{ "is": {}, "_behaviors": value }] }),
        ));
    }
    None
}

/// Every grammar example a markdown file carries, and how many blocks it opted out or could not
/// be parsed as JSON (a block with `...` or comments in it).
fn markdown_examples(markdown: &str, file: &str) -> (Vec<Example>, usize) {
    let mut examples = Vec::new();
    let mut not_checked = 0;
    for block in json_blocks(markdown) {
        let parsed = serde_json::from_str::<Value>(&block.body).ok();
        match parsed.filter(|_| !block.skip).and_then(as_document) {
            Some(document) => examples.push(Example {
                origin: format!("{file}:{}", block.line),
                document,
            }),
            None => not_checked += 1,
        }
    }
    (examples, not_checked)
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
}

fn file_examples(dir: &Path, root: &Path) -> Vec<Example> {
    json_files(dir)
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("read example");
            Example {
                origin: path
                    .strip_prefix(root)
                    .expect("under the repo")
                    .display()
                    .to_string(),
                document: serde_json::from_str(&text)
                    .unwrap_or_else(|e| panic!("{}: not JSON: {e}", path.display())),
            }
        })
        .collect()
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read docs dir") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "md") {
            out.push(path);
        }
    }
}

fn docs_examples() -> Vec<Example> {
    let root = repo_root();
    let mut files = Vec::new();
    markdown_files(&root.join("docs"), &mut files);
    files.sort();
    let mut examples = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read markdown");
        let name = path
            .strip_prefix(&root)
            .expect("under the repo")
            .display()
            .to_string();
        examples.extend(markdown_examples(&text, &name).0);
    }
    examples.extend(file_examples(&root.join("docs/demo"), &root));
    examples.extend(file_examples(&root.join("examples"), &root));
    examples
}

fn corpus_examples() -> Vec<Example> {
    let root = repo_root();
    file_examples(&root.join("sdk-conformance/corpus/imposters"), &root)
}

/// Every unread key across `examples` not in [`ALLOWED`], one line each.
fn failures(examples: &[Example]) -> Vec<String> {
    let mut failures = Vec::new();
    for example in examples {
        match unread_keys(&example.document) {
            Ok(unread) => failures.extend(
                unread
                    .into_iter()
                    .filter(|path| !allowed(path))
                    .map(|path| format!("{}: {path}", example.origin)),
            ),
            Err(e) => failures.push(format!("{}: {e}", example.origin)),
        }
    }
    failures
}

// ---------------------------------------------------------------------------------------------
// The schema (issue #1342)
// ---------------------------------------------------------------------------------------------

const SCHEMA: &str = "sdk-conformance/schema/imposter.schema.json";

fn schema() -> Value {
    let path = repo_root().join(SCHEMA);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{SCHEMA} is not JSON: {e}"))
}

fn schema_validator() -> jsonschema::Validator {
    jsonschema::validator_for(&schema()).expect("the checked-in schema compiles")
}

/// `value` without the keys [`ALLOWED`] excuses: the walk above reports and excuses them, but the
/// schema refuses them outright, so they are dropped before validation for the same reasons.
fn strip_allowed(value: &mut Value, path: &str) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| !allowed(&join(path, key)));
            for (key, child) in object.iter_mut() {
                strip_allowed(child, &join(path, key));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                strip_allowed(item, &join(path, &i.to_string()));
            }
        }
        _ => {}
    }
}

/// The imposters a document holds — several under a wrapper's `imposters`, else the document
/// itself — each stripped of the keys [`ALLOWED`] excuses.
fn imposters_of(document: &Value) -> Vec<Value> {
    let imposters = match document.get("imposters").and_then(Value::as_array) {
        Some(imposters) => imposters.clone(),
        None => vec![document.clone()],
    };
    imposters
        .into_iter()
        .map(|mut imposter| {
            strip_allowed(&mut imposter, "");
            imposter
        })
        .collect()
}

/// Every schema violation across `examples`, one line each: origin, instance path, message.
fn schema_failures(examples: &[Example]) -> Vec<String> {
    let validator = schema_validator();
    let mut failures = Vec::new();
    for example in examples {
        for (i, imposter) in imposters_of(&example.document).iter().enumerate() {
            failures.extend(validator.iter_errors(imposter).map(|error| {
                format!(
                    "{}: imposter {i} at {}: {error}",
                    example.origin,
                    error.instance_path()
                )
            }));
        }
    }
    failures
}

#[test]
fn corpus_fixtures_validate_against_the_schema() {
    let examples = corpus_examples();
    assert!(
        examples.len() >= 15,
        "expected the full corpus, found {}",
        examples.len()
    );
    let failures = schema_failures(&examples);
    assert!(
        failures.is_empty(),
        "corpus fixtures do not validate against {SCHEMA} — fix the fixture, or the type the \
         schema is generated from:\n{}",
        failures.join("\n")
    );
}

#[test]
fn docs_examples_validate_against_the_schema() {
    let examples = docs_examples();
    assert!(
        examples.len() >= 150,
        "expected the docs examples, found {}",
        examples.len()
    );
    let failures = schema_failures(&examples);
    assert!(
        failures.is_empty(),
        "docs examples do not validate against {SCHEMA} — fix the example, mark a deliberately \
         invalid one `<!-- rift-lint: skip -->`, or fix the type the schema is generated from:\n{}",
        failures.join("\n")
    );
}

/// The schema refuses what this gate reports, at the same path: an unread key is a validation
/// error naming it, and an allowed one is stripped rather than refused.
#[test]
fn the_schema_refuses_an_unread_key_and_excuses_an_allowed_one() {
    let misspelled = json!({ "protocol": "http", "stubs": [{
        "name": "excused",
        "predicates": [{ "equals": { "path": "/x" }, "casesensitive": true }],
        "responses": [{ "is": { "statusCode": 200 }, "_behaviors": { "waitt": 5 } }]
    }] });
    let failures = schema_failures(&[Example {
        origin: "inline".to_owned(),
        document: misspelled,
    }]);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(
        failures[0].contains("/stubs/0/predicates/0"),
        "{}",
        failures[0]
    );
    assert!(failures[0].contains("casesensitive"), "{}", failures[0]);
    assert!(
        failures[1].contains("/stubs/0/responses/0/_behaviors"),
        "{}",
        failures[1]
    );
    assert!(failures[1].contains("waitt"), "{}", failures[1]);
    assert!(!failures.iter().any(|f| f.contains("name")), "{failures:?}");
}

/// Each list this file keeps by hand equals the set the generated schema carries for it.
#[test]
fn the_hand_lists_here_match_the_schema() {
    let schema = schema();
    let defs = &schema["$defs"];
    let keys = |object: &Value| -> Vec<String> {
        let mut keys: Vec<String> = object["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("no properties in {object}"))
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    let sorted = |list: &[&str]| -> Vec<String> {
        let mut list: Vec<String> = list.iter().map(|s| (*s).to_owned()).collect();
        list.sort();
        list
    };
    assert_eq!(keys(&defs["RiftFlowStateConfig"]), sorted(&FLOW_STATE_KEYS));
    assert_eq!(
        defs["StubResponse"]["x-rift-response-variants"],
        json!(RESPONSE_VARIANTS)
    );
    // The flat form mirrors an `is` response field for field.
    assert_eq!(keys(&defs["IsResponse"]), sorted(&FLAT_RESPONSE_KEYS));
    assert_eq!(
        keys(&defs["RiftResponseExtension"]),
        sorted(&RIFT_RESPONSE_KEYS)
    );
    assert_eq!(
        defs["ResponseBehaviors"]["x-rift-canonical-order"],
        json!(CANONICAL_ORDER)
    );
    let generator = &defs["ProxyResponse"]["properties"]["predicateGenerators"]["items"];
    assert_eq!(generator["x-rift-known-keys"], json!(GENERATOR_KEYS));
    // Predicates: the operators are the `oneOf` branches, the parameters the other properties.
    let mut operators: Vec<String> = defs["Predicate"]["oneOf"]
        .as_array()
        .expect("oneOf")
        .iter()
        .map(|branch| branch["required"][0].as_str().expect("required").to_owned())
        .collect();
    operators.sort();
    assert_eq!(operators, sorted(&OPERATORS));
    let parameters: Vec<String> = keys(&defs["Predicate"])
        .into_iter()
        .filter(|key| !operators.contains(key))
        .collect();
    assert_eq!(parameters, sorted(&PREDICATE_PARAMETERS));
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

#[test]
fn corpus_fixtures_have_no_unread_keys() {
    let examples = corpus_examples();
    assert!(
        examples.len() >= 15,
        "expected the full corpus, found {}",
        examples.len()
    );
    let failures = failures(&examples);
    assert!(
        failures.is_empty(),
        "corpus fixtures carry keys the engine does not read — fix the fixture, or allow-list the \
         path in ALLOWED with the reason:\n{}",
        failures.join("\n")
    );
}

#[test]
fn docs_examples_have_no_unread_keys() {
    let examples = docs_examples();
    // A floor, so an extractor that silently finds nothing cannot pass.
    assert!(
        examples.len() >= 150,
        "expected the docs examples, found {}",
        examples.len()
    );
    let failures = failures(&examples);
    assert!(
        failures.is_empty(),
        "docs examples carry keys the engine does not read — fix the example, mark a deliberately \
         invalid one `<!-- rift-lint: skip -->`, or allow-list the path in ALLOWED with the \
         reason:\n{}",
        failures.join("\n")
    );
}

/// `document` with what a docs fragment leaves out to keep the example short filled in, so the
/// linter judges what the example says rather than what it elides: an imposter's `port` and
/// `protocol`, and a stub's `responses`.
fn lintable(document: &Value) -> Value {
    fn fill(imposter: &mut Value) {
        let Some(object) = imposter.as_object_mut() else {
            return;
        };
        object.entry("port").or_insert(json!(4545));
        object.entry("protocol").or_insert(json!("http"));
        if let Some(stubs) = object.get_mut("stubs").and_then(Value::as_array_mut) {
            for stub in stubs.iter_mut().filter_map(Value::as_object_mut) {
                stub.entry("responses").or_insert(json!([{ "is": {} }]));
            }
        }
    }
    let mut document = document.clone();
    match document.get_mut("imposters").and_then(Value::as_array_mut) {
        Some(imposters) => imposters.iter_mut().for_each(fill),
        None => fill(&mut document),
    }
    document
}

/// The lint errors `example` raises that no rule here excuses, one line each.
fn lint_errors(example: &Example, dir: &Path) -> Vec<String> {
    let file = dir.join("example.json");
    std::fs::write(&file, lintable(&example.document).to_string()).expect("write example");
    // One file at a time: docs reuse ports across examples, which a directory lint reports.
    let result = rift_lint::lint_file(&file, &rift_lint::LintOptions::default());
    let source = example.origin.split(':').next();
    result
        .issues
        .iter()
        .filter(|i| matches!(i.severity, rift_lint::Severity::Error))
        // An EJS `include`/`stringify` names a file beside the doc's config, which an extracted
        // copy has no way to have.
        .filter(|i| i.code != "E049")
        .filter(|i| {
            !LINT_ALLOWED
                .iter()
                .any(|(file, code, _)| source == Some(*file) && i.code == *code)
        })
        .map(|i| {
            format!(
                "{}: {} {} at {}",
                example.origin,
                i.code,
                i.message,
                i.location.as_deref().unwrap_or("-")
            )
        })
        .collect()
}

#[test]
fn docs_examples_lint_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let errors: Vec<String> = docs_examples()
        .iter()
        .flat_map(|example| lint_errors(example, dir.path()))
        .collect();
    assert!(
        errors.is_empty(),
        "docs examples lint red — fix the example, or mark a deliberately invalid one \
         `<!-- rift-lint: skip -->`:\n{}",
        errors.join("\n")
    );
}

// ---------------------------------------------------------------------------------------------
// The gate's own tests: each class of unread key it claims to catch, caught
// ---------------------------------------------------------------------------------------------

fn unread(document: Value) -> Vec<String> {
    unread_keys(&document).expect("document decodes")
}

#[test]
fn a_misspelled_predicate_parameter_is_reported() {
    let document = imposter_of(json!({
        "predicates": [{ "equals": { "path": "/x" }, "casesensitive": true }],
        "responses": [{ "is": {} }]
    }));
    assert_eq!(unread(document), vec!["stubs.0.predicates.0.casesensitive"]);
}

#[test]
fn a_nested_predicate_and_its_selector_are_walked() {
    let document = imposter_of(json!({
        "predicates": [{ "and": [
            { "not": { "equals": { "body": "x" }, "keycasesensitive": true } },
            { "equals": { "body": "y" }, "jsonpath": { "selector": "$.a", "ns": {} } }
        ] }]
    }));
    assert_eq!(
        unread(document),
        vec![
            "stubs.0.predicates.0.and.0.not.keycasesensitive",
            "stubs.0.predicates.0.and.1.jsonpath.ns",
        ]
    );
}

#[test]
fn a_second_operator_in_one_predicate_is_reported() {
    let document = imposter_of(json!({
        "predicates": [{ "equals": { "path": "/x" }, "contains": { "body": "y" } }]
    }));
    let reported = unread(document);
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(
        reported[0].starts_with("stubs.0.predicates.0 (operators"),
        "{reported:?}"
    );
}

#[test]
fn a_misspelled_imposter_key_is_reported_by_serde() {
    let document = json!({ "protocol": "http", "recordRequest": true, "stubs": [] });
    assert_eq!(unread(document), vec!["recordRequest"]);
}

#[test]
fn a_misspelled_behavior_and_extraction_key_are_reported() {
    let document = imposter_of(json!({ "responses": [{
        "is": { "body": "${X}" },
        "_behaviors": {
            "waitt": 10,
            "copy": [{ "from": "path", "into": "${X}",
                       "using": { "method": "regex", "selector": ".*",
                                  "options": { "ignorecase": true } } }],
            "lookup": { "key": { "from": "path", "using": { "method": "jsonpath", "selector": "$" },
                                 "indx": 0 },
                        "fromDataSource": { "csv": { "path": "x.csv", "keyColumn": "k", "delim": "," } },
                        "into": "${Y}" },
            "wait": { "min": 1, "max": 2, "jitter": 3 }
        }
    }] }));
    assert_eq!(
        unread(document),
        vec![
            "stubs.0.responses.0._behaviors.copy.0.using.options.ignorecase",
            "stubs.0.responses.0._behaviors.lookup.fromDataSource.csv.delim",
            "stubs.0.responses.0._behaviors.lookup.key.indx",
            "stubs.0.responses.0._behaviors.wait.jitter",
            "stubs.0.responses.0._behaviors.waitt",
        ]
    );
}

#[test]
fn a_generator_key_the_proxy_does_not_read_is_reported() {
    let document = imposter_of(json!({ "responses": [{ "proxy": {
        "to": "http://127.0.0.1:1",
        "predicateGenerators": [{ "matches": { "path": true }, "keyCaseSensitive": true }]
    } }] }));
    assert_eq!(
        unread(document),
        vec!["stubs.0.responses.0.proxy.predicateGenerators.0.keyCaseSensitive"]
    );
}

#[test]
fn a_shadowed_response_variant_and_behaviors_block_are_reported() {
    let document = imposter_of(json!({ "responses": [{
        "is": { "statusCode": 200 },
        "proxy": { "to": "http://127.0.0.1:1" },
        "statusCode": 404,
        "_behaviors": { "wait": 1 },
        "behaviors": [{ "wait": 2 }]
    }] }));
    assert_eq!(
        unread(document),
        vec![
            "stubs.0.responses.0.behaviors",
            "stubs.0.responses.0.proxy",
            "stubs.0.responses.0.statusCode",
        ]
    );
}

#[test]
fn rift_extension_enums_are_walked() {
    let document = json!({ "protocol": "http",
        "_rift": { "flowState": { "backend": "inmemory", "ttlSecond": 5 } },
        "stubs": [{ "responses": [{ "is": {}, "_rift": {
            "fault": { "tcp": { "probability": 0.5, "type": "CONNECTION_RESET_BY_PEER", "chance": 1 } },
            "conditional": { "etag": true, "lastModifed": "load" },
            "stateOps": [{ "op": "delete", "key": "k", "value": "v" }]
        } }] }]
    });
    assert_eq!(
        unread(document),
        vec![
            "_rift.flowState.ttlSecond",
            "stubs.0.responses.0._rift.conditional.lastModifed",
            "stubs.0.responses.0._rift.fault.tcp.chance",
            "stubs.0.responses.0._rift.stateOps.0.value",
        ]
    );
}

#[test]
fn a_wrapper_key_the_loader_does_not_read_is_reported() {
    let document = json!({ "imposters": [{ "protocol": "http", "stub": [] }], "intercepts": {} });
    assert_eq!(unread(document), vec!["imposters.0.stub", "intercepts"]);
}

#[test]
fn a_clean_document_reports_nothing() {
    let document = imposter_of(json!({
        "predicates": [{ "equals": { "path": "/x" }, "caseSensitive": true,
                         "xpath": { "selector": "//a", "ns": { "a": "urn:a" } } }],
        "responses": [{ "is": { "statusCode": 200 }, "repeat": 2,
                        "_behaviors": { "wait": { "min": 1, "max": 2 } } }]
    }));
    assert_eq!(unread(document), Vec::<String>::new());
}

#[test]
fn an_allowed_pattern_matches_trailing_segments_only() {
    assert!(allowed("stubs.3.name"));
    assert!(allowed("imposters.0.stubs.12.name"));
    assert!(!allowed("stubs.3.names"));
    assert!(!allowed("stubs.3.responses.0.name"));
    assert!(!allowed("name"));
}

#[test]
fn a_failure_names_the_origin_and_the_path() {
    let examples = [
        Example {
            origin: "docs/x.md:7".to_owned(),
            document: imposter_of(
                json!({ "predicates": [{ "equals": {}, "casesensitive": true }] }),
            ),
        },
        Example {
            origin: "docs/x.md:9".to_owned(),
            document: json!({ "protocol": "http", "stubs": "not a list" }),
        },
        Example {
            origin: "fixture.json".to_owned(),
            document: imposter_of(json!({ "name": "allowed", "responses": [{ "is": {} }] })),
        },
    ];
    let failures = failures(&examples);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert_eq!(
        failures[0],
        "docs/x.md:7: stubs.0.predicates.0.casesensitive"
    );
    assert!(
        failures[1].starts_with("docs/x.md:9: does not decode:"),
        "{failures:?}"
    );
}

#[test]
fn a_lint_error_on_a_docs_example_is_reported_unless_excused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bad_header =
        imposter_of(json!({ "responses": [{ "is": { "headers": { "X-Count": 5 } } }] }));
    let reported = lint_errors(
        &Example {
            origin: "docs/features/x.md:3".to_owned(),
            document: bad_header.clone(),
        },
        dir.path(),
    );
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(
        reported[0].starts_with("docs/features/x.md:3: E019"),
        "{reported:?}"
    );
    let excused = lint_errors(
        &Example {
            origin: "docs/mountebank/responses.md:122".to_owned(),
            document: bad_header,
        },
        dir.path(),
    );
    assert_eq!(excused, Vec::<String>::new());
    // A fragment's elided port, protocol and responses are not what the example says.
    let fragment = lint_errors(
        &Example {
            origin: "docs/x.md:1".to_owned(),
            document: json!({ "stubs": [{ "predicates": [{ "equals": { "path": "/" } }] }] }),
        },
        dir.path(),
    );
    assert_eq!(fragment, Vec::<String>::new());
}

#[test]
fn rift_and_behaviors_a_response_keeps_only_to_report_are_unread() {
    let document = imposter_of(json!({ "responses": [
        { "fault": "CONNECTION_RESET_BY_PEER", "_rift": { "templated": true },
          "_behaviors": { "wait": 5, "repeat": 2 } },
        { "_rift": { "script": { "code": "fn respond(ctx) {}" } }, "statusCode": 404,
          "headers": {} },
        { "inject": "function () { return {}; }", "_behaviors": { "wait": 5 } }
    ] }));
    assert_eq!(
        unread(document),
        vec![
            "stubs.0.responses.0._behaviors.wait",
            "stubs.0.responses.0._rift",
            "stubs.0.responses.1.statusCode",
        ]
    );
}

#[test]
fn alternatives_the_engine_reads_only_one_of_are_reported() {
    let document = imposter_of(json!({
        "predicates": [{ "equals": { "body": "x" },
                         "jsonpath": { "selector": "$.a" }, "xpath": { "selector": "//a" } }],
        "rules": [{ "equals": { "path": "/" } }],
        "responses": [{ "is": { "body": "${X}" }, "_behaviors": {
            "wait": { "min": 1, "max": 2, "inject": "function () { return 1; }" },
            "copy": { "from": { "query": "q", "headers": "h" }, "into": "${X}",
                      "using": { "method": "regex", "selector": ".*" } }
        } }]
    }));
    let reported = unread(document);
    assert_eq!(
        reported[1..],
        [
            "stubs.0.responses.0._behaviors.copy.from.headers",
            "stubs.0.responses.0._behaviors.wait.inject",
            "stubs.0.rules",
        ],
        "{reported:?}"
    );
    assert!(
        reported[0].starts_with("stubs.0.predicates.0 (selectors"),
        "{reported:?}"
    );
}

#[test]
fn a_serde_reported_path_under_an_option_has_no_placeholder_segment() {
    let document = json!({ "protocol": "http", "_rift": { "metricz": {} }, "stubs": [] });
    assert_eq!(unread(document), vec!["_rift.metricz"]);
}

#[test]
fn a_document_the_door_refuses_is_an_error_not_a_pass() {
    let document = json!({ "protocol": "http", "stubs": "not a list" });
    assert!(unread_keys(&document).is_err());
}

#[test]
fn the_extractor_finds_indented_fences_and_honours_opt_outs() {
    let markdown = r#"Intro.

```json
{ "equals": { "path": "/a" } }
```

1. A list item:

   ```json
   { "is": { "statusCode": 201 } }
   ```

<!-- rift-lint: skip -->

```json
{ "equals": { "path": "/skipped" }, "bogus": 1 }
```

```json title="invalid"
{ "equals": { "path": "/also-skipped" } }
```

```jsonc
{ "is": {} }
```

```json
{ "version": "1.0", "commit": "abc123" }
```
"#;
    let (examples, not_checked) = markdown_examples(markdown, "doc.md");
    let origins: Vec<&str> = examples.iter().map(|e| e.origin.as_str()).collect();
    assert_eq!(origins, vec!["doc.md:3", "doc.md:9"]);
    assert_eq!(
        examples[1].document,
        json!({ "protocol": "http", "stubs": [{ "responses": [{ "is": { "statusCode": 201 } }] }] })
    );
    // The two opt-outs and the non-grammar block; ```jsonc is not a json fence at all.
    assert_eq!(not_checked, 3);
}
