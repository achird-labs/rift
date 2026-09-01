---
layout: default
title: Configuration Linting
parent: Features
nav_order: 7
---

# Configuration Linting

Rift includes a powerful configuration linter (`rift-lint`) that validates imposter configuration files before loading them. This helps catch common issues early and ensures your configurations will work correctly.

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
- **JavaScript errors**: Syntax errors in wait/decorate behaviors
- **Missing fields**: Required configuration that's absent

---

## CLI Options

```bash
rift-lint [OPTIONS] <PATH>

Arguments:
  <PATH>  Path to imposter file or directory

Options:
  -f, --fix          Auto-fix issues where possible
  -o, --output       Output format: text (default), json
  -e, --errors-only  Only show errors (hide warnings)
  -v, --verbose      Verbose output
  -s, --strict       Treat warnings as errors
  -h, --help         Print help
  -V, --version      Print version
```

---

## Validation Rules

### Errors

Errors indicate issues that will prevent the imposter from loading correctly.

| Code | Description | Example |
|:-----|:------------|:--------|
| E001 | File could not be read, or is not valid JSON | Missing comma, unquoted string, unreadable path |
| E002 | Port conflict | Two imposters on port 4545 |
| E003 | Missing required field | No `port` or `stubs` field |
| E004 | Invalid protocol | Protocol is "ftp" instead of "http" |
| E005 | Port out of range | Port 70000 (max is 65535) |
| E010 | Unbalanced brackets in JSONPath | `$.user[0` missing `]` |
| E013 | Invalid regex | `[invalid(` |
| E018 | Header is array | `"Accept": ["text/html", "application/json"]` |
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
| E025 | Invalid `wait` behavior value | `"wait": []` |
| E026 | Unbalanced braces in JavaScript | `function () { return 1;` |
| E027 | Unbalanced parentheses in JavaScript | `function ( { return 1; }` |
| E028 | JavaScript syntax error | A malformed `inject` function |
| E029 | Copy behavior item missing `from` | `{"into": "${token}"}` |
| E030 | Copy behavior item missing `into` | `{"from": "body"}` |
| E031 | Lookup behavior missing `key` | Lookup with only `fromDataSource` |
| E032 | Lookup behavior missing `fromDataSource` | Lookup with only `key` |
| E033 | Lookup behavior missing `into` | Lookup with `key` and `fromDataSource` only |
| E034 | More than one operator in a single predicate | `{"equals": {...}, "contains": {...}}` — split them under `and` |
| E035 | `repeat` behavior is not a positive integer | `"repeat": 0` |
| E036 | `script` must specify exactly one of `code`, `file` or `ref` | Both `code` and `file` given |
| E037 | Unknown script `ref` — no such entry in `_rift.scripts` | `"ref": "missing"` |
| E038 | Script `file` (via `ref`) could not be read | `"file": "no-such.js"` |
| E039 | A `_rift.scripts` entry uses `ref` itself (ref chains are not allowed) | `{"a": {"ref": "b"}}` |
| E040 | JavaScript syntax error in `_rift.script` | A malformed `_rift.script` body |
| E041 | `_rift.fault.tcp` `probability` is outside 0.0–1.0 | `"probability": 1.5` |
| E042 | Script uses `ctx.state` but no `_rift.flowState` is configured | `ctx.state.get(...)` without `flowState` |

### Warnings

Warnings indicate potential issues that may cause unexpected behavior.

| Code | Description | Example |
|:-----|:------------|:--------|
| W001 | Privileged port | Port 80 requires root access |
| W002 | Stub has no responses defined | `{"predicates": [...], "responses": []}` |
| W003 | Response has both `is` and `proxy` defined | `{"is": {...}, "proxy": {...}}` |
| W004 | Invalid JSON body | Body isn't JSON but Content-Type is application/json |
| W005 | Header value is null | `"X-Request-Id": null` |
| W006 | Small Content-Length | `"Content-Length": "5"` with large body |
| W007 | Unknown proxy mode | `"mode": "proxyEverything"` |
| W008 | `shellTransform` contains a potentially dangerous command | `"shellTransform": "rm -rf /tmp/x"` |
| W009 | Non-function behavior | `"wait": "return 100"` without function wrapper |
| W010 | Protocol `tcp` is not yet implemented and will fail at runtime | `"protocol": "tcp"` |
| W011 | Unknown TCP fault type — the fault will not fire at runtime | `{"type": "NONSENSE"}` |

### Info

Informational messages about configuration patterns.

| Code | Description |
|:-----|:------------|
| I001 | Mountebank slice notation detected (`[:0]`) |
| I002 | Proxy targets localhost |
| I003 | Response uses the Rift `_rift` extension (not Mountebank-compatible) |

---

## Auto-Fix

The `--fix` flag automatically corrects certain issues:

- Header arrays → comma-separated strings
- Header numbers → strings
- Header booleans → strings

```bash
rift-lint ./imposters/ --fix
```

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
    docker run --rm -v ${{ github.workspace }}:/imposters \
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
