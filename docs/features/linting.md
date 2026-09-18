---
layout: default
title: Configuration Linting
parent: Features
nav_order: 7
---

# Configuration Linting

Rift includes a powerful configuration linter (`rift-lint`) that validates imposter configuration files before loading them. This helps catch common issues early and ensures your configurations will work correctly.

It reads the same formats `--configfile` does: JSON (`.json`) and YAML (`.yaml`, `.yml`). A YAML config must be a **sequence of imposters** at the document root, which is the only shape `--configfile` loads — the single-imposter and `{"imposters": [...]}` forms are JSON-only, and using one in YAML is reported as [E046](#errors). As in the engine, the format is decided by the content, not the extension: a `.yaml` file whose text starts with `{` or `[` is read as JSON and may use any JSON shape.

---

## Installation

```bash
# Via Homebrew (macOS/Linux)
brew tap achird-labs/rift
brew install rift
# rift-lint is included

# Via crates.io
cargo install rift-lint

# Via Docker (for CI/CD)
docker pull zainalpour/rift-lint:latest
```

## Quick Start

```bash
# Lint a directory of imposters
rift-lint ./imposters/

# Lint with strict mode (warnings become errors)
rift-lint ./imposters/ --strict

# Using Docker
docker run --rm -v $(pwd):/imposters zainalpour/rift-lint .
```

---

## Why Use the Linter?

The linter catches issues that would otherwise cause problems at runtime:

- **Port conflicts**: Multiple imposters trying to use the same port
- **Invalid headers**: Header values that aren't strings (arrays, numbers, booleans)
- **Malformed predicates**: Invalid JSONPath selectors, bad regex patterns
- **JavaScript mistakes**: Syntax errors in function `wait` scripts (including the `{"inject": …}` spelling), JavaScript `decorate` scripts, `inject` responses, `inject` predicates and `proxy.predicateGenerators[].inject`, and JavaScript `_rift.script` bodies, parsed with the engine's own JavaScript engine (see [E028 and E040](#errors)). A Rhai `decorate` is not parsed as JavaScript
- **Missing fields**: Required configuration that's absent
- **Engine refusals**: Shapes the engine rejects at load, such as a repeated header name in a single-valued header object, a non-object `_behaviors`, or an EJS tag the config loader does not evaluate

---

## CLI Options

```bash
rift-lint [OPTIONS] <PATH>

Arguments:
  <PATH>  Path to an imposter file (.json, .yaml, .yml) or a directory of them
          (a directory is scanned one level deep, not recursively)

Options:
  -f, --fix          Auto-fix issues where possible
  -o, --output       Output format: text (default), json
  -e, --errors-only  Only show errors (hide warnings)
  -s, --strict       Treat warnings as errors
      --no-parse     Lint files verbatim, without rendering EJS tags (alias: --noParse)
  -h, --help         Print help
  -V, --version      Print version
```

With `-o json`, stdout carries only the JSON result (an empty result when no files are found); the
banner, progress and `--fix` messages go to stderr.

### Templated files

A JSON or YAML file with EJS `<% %>` tags is rendered before it is linted, exactly as `--configfile`
renders it: the same four tags, the same refusals, `include` and `stringify` paths relative to the
file, and `process.env` read from the environment `rift-lint` runs in. So the documented
`"port": <%= process.env.PORT || '4545' %>` lints clean, and an included file's imposters are
checked too.

- A tag the engine would refuse, or an included file it cannot read, is [E049](#errors), with the
  engine's own message. The rest of that file is not checked.
- A `process.env` tag with no default whose variable is unset, or any `process.env` tag whose
  variable is set to a value that is not valid Unicode, is [W013](#warnings). Run the lint with the
  same environment rift will have, or give the tag a default.
- `--no-parse` lints the text as it is, matching `rift --no-parse`. Use it for a `--datadir` and for
  JSON sent to `POST /imposters`, which the engine never preprocesses, so a literal `<%` there is
  data. The TUI validates imports this way.
- A line and column in `E001` or `W012` count in the rendered document, and the finding says so,
  because an `include` or a substitution moves them.
- `--fix` never rewrites a templated file, because it would write the rendered values over the tags.

---

## Validation Rules

### Errors

Errors indicate issues that will prevent the imposter from loading correctly.

| Code | Description | Example |
|:-----|:------------|:--------|
| E001 | File could not be read, or is not valid JSON or YAML (a multi-document YAML stream included) | Missing comma, unquoted string, unreadable path |
| E002 | Port conflict — more than one imposter declares the same port, inside one file (`{"imposters": [...]}` or `[...]`) or across files. An absent or `null` port is auto-assigned and never conflicts; `0` is reported as E005 instead | Two imposters on port 4545 |
| E003 | A required imposter field — `port`, `protocol` or `stubs` — is missing or `null` | No `port` or `stubs` field |
| E004 | Invalid protocol | Protocol is "ftp" instead of "http" |
| E005 | Port out of range, or `0` — the engine auto-assigns `0` like an absent port, but a config file must pin its ports | Port 70000 (max is 65535), port 0 |
| E010 | Unbalanced brackets in JSONPath | `$.user[0` missing `]` |
| E013 | Invalid regex | `[invalid(` |
| E018 | `is.headers` array contains a non-string element (a string array is legal, #238) | `"Accept": ["text/html", 1]` |
| E019 | Header is number | `"Content-Length": 256` |
| E006 | Stub missing `responses` field | A stub with `predicates` but no `responses` |
| E007 | Predicate is not an object | `"predicates": ["equals"]` |
| E008 | Predicate has no operator | `{"caseSensitive": true}` on its own |
| E009 | Unknown predicate operator | `{"equalz": {"path": "/a"}}` |
| E011 | JSONPath missing `selector` field | `"jsonpath": {}` |
| E014 | Response has no response type | Neither `is`, `proxy`, `inject`, `fault` nor `_rift` |
| E015 | Invalid HTTP status code | `"statusCode": 999` |
| E016 | `statusCode` is not a number or numeric string | `"statusCode": true` |
| E017 | Empty header name | `"headers": {"": "value"}` |
| E020 | Header value is a boolean, must be a string | `"X-Debug": true` |
| E021 | Headers is not an object | `"headers": []` |
| E022 | Proxy `to` URL does not start with `http://` or `https://` | `"to": "ftp://host/x"` |
| E023 | Proxy `to` is not a string URL | `"to": 8080` |
| E024 | Proxy missing required `to` field | `"proxy": {"mode": "proxyOnce"}` |
| E025 | Invalid `wait` behavior value — a bare number must be a non-negative integer of milliseconds; anything else makes the engine refuse the file (before 0.18.0 it loaded and ignored the block's behaviors, all but `repeat`, with only a log line). `null` counts as absent. A `behaviors` array is checked as the engine merges it, so only the last value for each key is checked and the finding names that element; `"_behaviors": null` falls back to `behaviors`. Also fires for a **`{min,max}` range with `min` greater than `max`**, and for an inverted stub-level `delayRange` entry (bounds may be numeric strings) — the engine refuses those outright. **The inverted-range check is the one exception to the merge rule above:** the engine validates every `behaviors` element and both blocks regardless of which it will evaluate, so the linter does too — an inverted range in a losing array element, or in a `behaviors` block shadowed by `_behaviors`, is still reported | `"wait": []`, `"wait": 500.5`, `"wait": {"min": 100, "max": 10}` |
| E026 | Unbalanced braces in JavaScript — the fallback when the script is not parsed: a build without the `javascript` feature, or a Rhai `decorate`. A parsed script reports E028 instead, so a `{` inside a string or comment is not miscounted | `function () { return 1;` |
| E027 | Unbalanced parentheses in JavaScript — the fallback when the script is not parsed, as for E026 | `function ( { return 1; }` |
| E028 | JavaScript syntax error, found by parsing the script (never running it). Every release artifact includes this check; a build that opts out of the `javascript` feature falls back to brace and parenthesis counting (E026/E027) and says so with [I004](#info). Covers `decorate` and function `wait` scripts, and every `inject`: a response (when it is the response type the engine uses — not beside an `is` or `proxy`), a predicate at any depth, and a `proxy.predicateGenerators` entry. An `inject` is parsed as the engine wraps it (`var __injectFn = <script>;`), so an arrow, `async` or named function is accepted, as the engine accepts it | A malformed `decorate` or `inject` function |
| E029 | Copy behavior item missing `from` | `{"into": "${token}"}` |
| E030 | Copy behavior item missing `into` | `{"from": "body"}` |
| E031 | Lookup behavior missing `key` | Lookup with only `fromDataSource` |
| E032 | Lookup behavior missing `fromDataSource` | Lookup with only `key` |
| E033 | Lookup behavior missing `into` | Lookup with `key` and `fromDataSource` only |
| E034 | More than one operator in a single predicate | `{"equals": {...}, "contains": {...}}` — split them under `and` |
| E035 | `repeat` behavior is not a positive integer no larger than 4294967295 (`null` counts as absent) | `"repeat": 0` |
| E036 | `script` must specify exactly one of `code`, `file` or `ref` | Both `code` and `file` given |
| E037 | Unknown script `ref` — no such entry in `_rift.scripts` | `"ref": "missing"` |
| E038 | Script `file` (via `ref`) could not be read | `"file": "no-such.js"` |
| E039 | A `_rift.scripts` entry uses `ref` itself (ref chains are not allowed) | `{"a": {"ref": "b"}}` |
| E040 | JavaScript syntax error in a JavaScript `_rift.script` (inline, `file` or `ref`). Every release artifact includes this check; a build without the `javascript` feature has no fallback for it and reports [I004](#info) instead | A malformed `_rift.script` body |
| E041 | Malformed `_rift.fault.tcp`: not a fault-type string or an object; an object form without a numeric `probability` or a string `type`; or a `probability` outside 0.0–1.0 | `"probability": 1.5`, `{"type": "RESET"}` |
| E043 | Single-valued header object names one header twice, in different case (`proxy.injectHeaders`, `_rift.fault.error.headers`) | `{"X-Id": "a", "x-id": "b"}` |
| E044 | Single-valued header object names one header twice, byte-identically (`proxy.injectHeaders`, `_rift.fault.error.headers`). `is.headers` is excluded: a repeat there is merged into two header lines on purpose | `{"X-Id": "a", "X-Id": "b"}` |
| E045 | Single-valued header object has a non-string value (`proxy.injectHeaders`, `_rift.fault.error.headers`) | `{"X-Id": 1}` |
| E046 | A YAML document's root is not a sequence of imposters — the engine's YAML loader accepts only a top-level list, unlike `--configfile`'s JSON, which also accepts a single imposter object or an `{"imposters": [...]}` wrapper | `port: 3000` at the document root |
| E047 | `port` is present but not a non-negative integer — the engine refuses the file at load (`expected u16`), and an integral float such as `3000.0` is no exception. `null` is reported as E003 instead, because the engine reads it as absent and auto-assigns a port | `"port": "3000"`, `"port": 3000.5` |
| E048 | A response's behaviors block has a shape the engine does not read: a `_behaviors` that is not an object, or a `behaviors` that is neither an object nor an array, is refused at load; a non-object, non-null element of a `behaviors` array is skipped | `"_behaviors": [null, null, null, null, "cmd"]`, `"behaviors": "wait"`, `"behaviors": [5]` |
| E049 | The engine would refuse to preprocess the file: an EJS tag it does not evaluate, or an `include`/`stringify` file that cannot be read. The message is the engine's own | `"body": "<% for (x) %>"`, `<% include 'missing.json' %>` |
| E050 | A config file's `intercept` block sets `returnCaKey: true`. The engine refuses the file, because a config file has no response to return the generated CA key in | `"intercept": {"returnCaKey": true}` |
| E051 | A behavior value the engine refuses the file for, on any response type: a `copy` or `lookup` item that is not an object; a missing or malformed `using` (an object with `method` `regex`, `jsonpath` or `xpath`, a string `selector`, boolean regex `options`); a `from` that is neither a field name nor an object of names; a non-string `into`; a `fromDataSource` without a `csv` object of string `path` and `keyColumn` and a one-character `delimiter`; a `decorate` that is not a string; a `shellTransform` that is neither a command string nor an array of them | `"copy": {"from": "path", "into": "${P}"}` |

### Warnings

Warnings indicate potential issues that may cause unexpected behavior.

| Code | Description | Example |
|:-----|:------------|:--------|
| W001 | Privileged port | Port 80 requires root access |
| W002 | Stub has no responses defined | `{"predicates": [...], "responses": []}` |
| W003 | Response has both `is` and `proxy` defined | `{"is": {...}, "proxy": {...}}` |
| W004 | Invalid JSON body | Body isn't JSON but Content-Type is application/json |
| W005 | Header value is null | `"X-Request-Id": null` |
| W006 | `Content-Length` header is a numeric string below 10 | `"Content-Length": "5"` |
| W007 | Unknown proxy mode | `"mode": "proxyEverything"` |
| W008 | `shellTransform` contains a potentially dangerous command | `"shellTransform": "rm -rf /tmp/x"` |
| W009 | Non-function behavior | `"wait": "return 100"` without function wrapper |
| W010 | Protocol `tcp` is not yet implemented and will fail at runtime | `"protocol": "tcp"` |
| W011 | Unknown TCP fault type — the fault will not fire at runtime | `{"type": "NONSENSE"}` |
| W012 | Number literal cannot be kept as written — the engine reads it as the nearest double (a `.yaml`/`.yml` file is not checked, even one holding JSON text) | `"body": {"big": 123456789012345678901234567890}` is served as `1.2345678901234568e29` |
| W013 | A `<%= process.env.VAR %>` tag cannot substitute its variable where `rift-lint` runs: the variable is unset and the tag has no default, so it renders empty, or it is set to a value that is not valid Unicode, so the tag renders its default or empty. The engine logs the same warning at load. The document is linted as rendered | `"port": <%= process.env.PORT %>` with `PORT` unset |
| W014 | A response's `_rift.script` uses `ctx.state` (or `flow_store`), or its `_rift.stateOps` is a non-empty array, but the imposter has no `_rift.flowState`. State is then auto-provisioned in memory — not persisted, not shared across a cluster. Formerly `E042` (renumbered in #1156: it was always a warning, and the letter now matches) | `ctx.state.get(...)` without `flowState` |
| W015 | A `_mode: "binary"` body — on an `is` response or on `defaultResponse` — that is not valid base64, or is not a string at all (a non-string body is serialized to JSON text first, so it can never decode). The engine serves it anyway, as the raw text with `x-rift-binary-error: true`, or as a `500` under `strictBehaviors`. Checked with the engine's own decoder, not an approximation | `"body": "not!valid!base64!", "_mode": "binary"` |
| W016 | `_rift.scriptEngine.defaultEngine` is not `rhai`, `javascript` or `js`. A script that names no engine, and whose `file` extension does not decide it, fails to build with it. The rest of the imposter loads, so a stale value with no such script is only a warning | `"defaultEngine": "lua"` |
| W017 | A key the engine parses and does not act on: `_rift.metrics`, `_rift.proxy`, `recordMatches: true`, `_rift` on a `proxy`, `inject` or `fault` response, or a `_behaviors`/`behaviors` block setting anything besides `repeat` on a `proxy`, `fault` or `_rift`-only response. The engine reports the same keys as `config_key_ignored` in `_rift.warnings` | `"recordMatches": true` |

### Info

Informational messages about configuration patterns.

| Code | Description |
|:-----|:------------|
| I001 | Mountebank slice notation detected (`[:0]`) |
| I002 | Proxy targets localhost |
| I003 | Response uses the Rift `_rift` extension (not Mountebank-compatible) |
| I004 | This build omits the `javascript` feature, so the run's JavaScript was not syntax-checked (no E028/E040). Reported once per run, not per script. Every release artifact has the feature; only a source build that opts out with `--no-default-features` can report this |
| I005 | `_rift.dataset` or `_rift.sequencing`: a carrier field that round-trips through the admin API for an embedder's extension. The standalone engine does not read it |

---

**Retired codes.** A code number is never reused. `E012` was never assigned. `E042` was renumbered
to `W014` in #1156 — it had always been reported as a warning, and a consumer filtering on the `E`
prefix saw a different rule set from one filtering on severity. A rule's letter now always matches
its severity.

## Auto-Fix

The `--fix` flag automatically corrects certain value shapes in `is.headers` (the E018, E019 and
E020 findings). It runs only when the lint found at least one error:

`--fix` rewrites JSON only. A `.yaml`/`.yml` file is reported and never rewritten:
re-serializing it would put JSON text under a YAML name, which the engine would then silently
read back as JSON.

- A number → the same number as a string
- A boolean → the same boolean as a string
- An array containing a non-string element → each element quoted in place (a string-only array is
  already legal and is left alone)

```bash
rift-lint ./imposters/ --fix
```

**A templated file is never rewritten** (see [Templated files](#templated-files)): the rewrite
would replace its `<% %>` tags with what they rendered to.

**A file that gives the same key twice anywhere is never rewritten.** `--fix` re-serializes the
whole document from its parsed form, and a repeated key does not survive parsing — the first value
is already gone. Rather than write that loss to disk, `--fix` skips the file and names the key it
would have dropped.

How to resolve it depends on where the repeat is:

- In **`is.headers`**, a repeated name is how a stub sends the same header twice, and the engine
  merges it on purpose — so the fix is *not* to delete one of them. Write the values as an array
  instead, which means the same thing and survives a rewrite:
  `"Set-Cookie": ["a=1", "b=2"]`.
- Anywhere else — `proxy.injectHeaders`, `_rift.fault.error.headers`, or an ordinary field such as
  `port` — a repeat is a mistake and only one of the values was ever going to be used. Keep the one
  you meant. In the two single-valued header objects this is also reported as
  [E044](#errors); an array is **not** a valid alternative there.

**A file with a number `--fix` cannot write back digit-for-digit is never rewritten either.** A
number wider than a 64-bit integer, or with more significant digits than a double can distinguish, is
held as the nearest double once parsed — so rewriting the file would change it:

| Written in the file | What a rewrite would write |
|---|---|
| `123456789012345678901234567890` | `1.2345678901234568e29` |
| `0.1000000000000000055511151231257827` | `0.1` |
| `0.30000000000000001` | `0.3` |

An ordinary float is not affected: any number a double holds in its shortest form — `7e23`,
`1.23e-30`, `0.10018513143495411` — is written back exactly.

`--fix` skips the file and names each such number with its line and column; every lint run, `--fix`
or not, also reports each one as [W012](#warnings). The engine reads the
file the same way, so it **already serves** the right-hand value for that number — the rewrite
would only have made the file agree with it, silently. Resolve it by writing the value you mean: the
rounded number if that is what you want served, or, if a response body must carry the exact digits,
give the body as a string, which the engine sends verbatim:
`"body": "{\"big\": 123456789012345678901234567890}"`. Set `Content-Type: application/json` in
`headers` yourself when you do — the engine adds it only for a body written as a JSON object or
array.

A number that only changes spelling — `0.10` → `0.1`, `1e2` → `100.0` — is formatting, not loss,
and does not stop the rewrite.

Two more things `--fix` does that are easy to miss: it rewrites the entire file, so object keys come
back in sorted order and the original formatting is not preserved; and it only repairs headers under
a top-level `stubs` array, so a config written in the `{"imposters": [...]}` wrapper or bare-array
form is reported but never rewritten. Formatting is all a rewrite changes beyond the headers it
reports: a file it would change in any other way — a repeated key, or one of the numbers above — is
skipped instead.

---

## CI/CD Integration

### GitHub Actions

```yaml
- name: Lint Imposters
  uses: docker://zainalpour/rift-lint:latest
  with:
    args: ./imposters/ --strict
```

Or with a direct command:

```yaml
- name: Lint Imposters
  run: |
    docker run --rm -v ${% raw %}{{ github.workspace }}{% endraw %}:/imposters \
      zainalpour/rift-lint:latest . --strict
```

### GitLab CI

```yaml
lint:
  image: zainalpour/rift-lint:latest
  script:
    - rift-lint ./imposters/ --output json > lint-results.json
  artifacts:
    reports:
      codequality: lint-results.json
```

### Pre-commit Hook

```bash
#!/bin/bash
# .git/hooks/pre-commit

if [ -d "imposters" ]; then
  rift-lint ./imposters/ --strict
  if [ $? -ne 0 ]; then
    echo "Imposter linting failed. Please fix errors before committing."
    exit 1
  fi
fi
```

---

## Common Issues and Fixes

### Header Values Must Be Strings

**Problem:**
```json
{
  "headers": {
    "Content-Length": 256,
    "X-Count": 10
  }
}
```

**Fix:**
```json
{
  "headers": {
    "Content-Length": "256",
    "X-Count": "10"
  }
}
```

### JavaScript Must Be Function Expression

**Problem:**
```json
{
  "wait": "return Math.random() * 1000"
}
```

**Fix:**
```json
{
  "wait": "function() { return Math.random() * 1000; }"
}
```

---

## Exit Codes

| Code | Meaning |
|:-----|:--------|
| 0 | No errors (warnings allowed unless `--strict`) |
| 1 | Errors found (or warnings in `--strict` mode) |

---

## Library Usage

The linter is also available as a Rust library for integration into other tools (like rift-tui):

Library entry points differ from the CLI in what they can see:

- **E002** (port conflicts) is reported by every entry point (issue #1156). `lint_file`,
  `lint_json`, `lint_yaml`, `lint_value` and `lint_document` report a port repeated inside the one
  document they lint; `lint_directory` reports a port shared across its files, always against the
  first in byte-wise sorted path order (case-sensitive, so `B.json` sorts before `a.json`). To lint several documents as one run yourself, record each in a
  `PortUses` and report `PortUses::conflicts()` once — and lint them with `lint_document_in_run`,
  which reports no `E002` of its own, so a within-file conflict is not reported twice.
- **E044**, **E046** and **W012** need the raw text, so `lint_value` (which starts from an
  already-parsed value) cannot report them. `lint_file`, `lint_json`, `lint_yaml`, and
  `parse_document`/`parse_yaml_document` + `lint_document` can.
- `lint_file`, `lint_json` and `lint_yaml` render EJS tags first, like the CLI, unless
  `LintOptions { no_parse: true }` is passed.

```rust
use rift_lint::{lint_file, lint_json, lint_value, LintOptions, LintResult};
use std::path::Path;

// Lint a file from disk
let result = lint_file(Path::new("imposter.json"), &LintOptions::default());

// Lint a JSON string (useful for in-memory validation)
let json = r#"{"port": 4545, "protocol": "http", "stubs": []}"#;
let result = lint_json(json, "inline", &LintOptions::default());

// Lint already-parsed JSON
let value: serde_json::Value = serde_json::from_str(json).unwrap();
let result = lint_value(&value, "inline", &LintOptions::default());

// Check results
if result.has_errors() {
    for issue in &result.issues {
        println!("[{}] {}: {}", issue.severity.label(), issue.code, issue.message);
    }
}
```

Add to your `Cargo.toml`:

```toml
[dependencies]
rift-lint = { path = "../rift-lint", default-features = false }
```

---

## See Also

- [rift-verify]({{ site.baseurl }}/features/stub-analysis/) - Test imposters by making requests
- [Mountebank Compatibility]({{ site.baseurl }}/mountebank/) - Configuration format reference
