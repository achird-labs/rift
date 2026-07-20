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
| E001 | Invalid JSON syntax | Missing comma, unquoted string |
| E002 | Port conflict | Two imposters on port 4545 |
| E003 | Missing required field | No `port` or `stubs` field |
| E004 | Invalid protocol | Protocol is "ftp" instead of "http" |
| E005 | Port out of range | Port 70000 (max is 65535) |
| E010 | Unbalanced brackets in JSONPath | `$.user[0` missing `]` |
| E013 | Invalid regex | `[invalid(` |
| E018 | Header is array | `"Accept": ["text/html", "application/json"]` |
| E019 | Header is number | `"Content-Length": 256` |

### Warnings

Warnings indicate potential issues that may cause unexpected behavior.

| Code | Description | Example |
|:-----|:------------|:--------|
| W001 | Privileged port | Port 80 requires root access |
| W004 | Invalid JSON body | Body isn't JSON but Content-Type is application/json |
| W006 | Small Content-Length | `"Content-Length": "5"` with large body |
| W009 | Non-function behavior | `"wait": "return 100"` without function wrapper |

### Info

Informational messages about configuration patterns.

| Code | Description |
|:-----|:------------|
| I001 | Mountebank slice notation detected (`[:0]`) |
| I002 | Proxy targets localhost |

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
