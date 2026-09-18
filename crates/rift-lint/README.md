# rift-lint

Configuration linter for [Rift](https://github.com/achird-labs/rift) — validates imposter
configuration files (JSON or YAML) before the server loads them.

## Features

- **Port conflict detection** within a file and across multiple imposter files
- **EJS rendering** with the same preprocessor the server uses, so a templated config is linted as it will load
- **Header validation** - values must be strings; duplicate header names are reported
- **Predicate validation** - JSONPath selectors, regex patterns, operators
- **JavaScript validation** - syntax checking for wait/decorate behaviors
- **Response validation** - status codes, proxy URLs, required fields, behaviors as the engine merges them
- **Number fidelity** - warns when a JSON number literal would not be served as written
- **Auto-fix** capability for common issues (refused when the rewrite would lose information)

## Installation

### Via crates.io

```bash
cargo install rift-lint
```

### Via Homebrew (macOS/Linux)

```bash
brew tap achird-labs/rift
brew install rift
# rift-lint is included
```

### Via Docker (for CI/CD)

```bash
docker pull zainalpour/rift-lint:latest
docker run --rm -v $(pwd):/imposters zainalpour/rift-lint .
```

### Build from source

```bash
cargo build --release -p rift-lint
./target/release/rift-lint --help
```

### As a library

Add to your `Cargo.toml`:

```toml
[dependencies]
rift-lint = { path = "../rift-lint", default-features = false }
```

## CLI Usage

```bash
# Lint a directory of imposters
rift-lint ./imposters/

# Lint a single file (JSON or YAML)
rift-lint ./imposters/my-service.json

# Show only errors (hide warnings)
rift-lint ./imposters/ --errors-only

# JSON output for CI/CD
rift-lint ./imposters/ --output json

# Strict mode - treat warnings as errors
rift-lint ./imposters/ --strict

# Auto-fix issues where possible
rift-lint ./imposters/ --fix

# Lint verbatim, without rendering EJS tags (matches `rift --no-parse`)
rift-lint ./imposters/ --no-parse
```

### Options

| Option | Short | Description | Default |
|--------|-------|-------------|---------|
| `<PATH>` | | Path to file or directory | (required) |
| `--fix` | `-f` | Auto-fix issues | `false` |
| `--output` | `-o` | Output format: `text`, `json` | `text` |
| `--errors-only` | `-e` | Hide warnings | `false` |
| `--strict` | `-s` | Warnings become errors | `false` |
| `--no-parse` | | Skip EJS rendering (alias `--noParse`) | `false` |

## Library Usage

```rust
use rift_lint::{lint_directory, lint_file, lint_json, lint_value, lint_yaml, LintOptions};
use std::path::Path;

// Lint a file
let result = lint_file(Path::new("imposter.json"), &LintOptions::default());
if result.has_errors() {
    for issue in &result.issues {
        eprintln!("{}: {}", issue.code, issue.message);
    }
}

// Lint a JSON string (useful for in-memory validation)
let json = r#"{"port": 4545, "protocol": "http", "stubs": []}"#;
let result = lint_json(json, "inline", &LintOptions::default());

// Lint a YAML string, or every imposter file in a directory
let result = lint_yaml("port: 4545\nprotocol: http\n", "inline.yaml", &LintOptions::default());
let result = lint_directory(Path::new("imposters/"), &LintOptions::default());

// Lint already-parsed JSON
let value: serde_json::Value = serde_json::from_str(json).unwrap();
let result = lint_value(&value, "inline", &LintOptions::default());
```

## Validation Rules

### Errors

| Code | Description |
|------|-------------|
| E001 | Invalid JSON or YAML / file read error |
| E002 | Port conflict |
| E003 | Missing required field |
| E004 | Invalid protocol |
| E005 | Port out of range, or `0` (auto-assigned by the engine; a config file must pin its ports) |
| E006-E048 | Structural errors in predicates, responses, behaviors, scripts and headers |
| E049 | The engine would refuse to preprocess the file (unsupported EJS tag, unreadable include) |

### Warnings

| Code | Description |
|------|-------------|
| W001 | Privileged port |
| W002-W013 | Potential issues, including lossy number literals (W012) and unset EJS env vars (W013) |

### Info

| Code | Description |
|------|-------------|
| I001 | Mountebank slice notation in JSONPath |
| I002 | Proxy targets localhost |
| I003 | Response uses the Rift `_rift` extension |

The full table, with an example for every code, is in
[Configuration Linting](https://achird-labs.github.io/rift/features/linting/).

## Feature Flags

- `cli` (default) - Enables CLI binary with clap
- `javascript` (default) - JavaScript syntax validation with boa_engine (E028, E040). On by default
  since #1156: without it E040 has no fallback at all. A build that opts out reports `I004` once per
  run, so a clean result is never mistaken for a checked one.

```toml
# Everything (the default: CLI + JavaScript validation)
[dependencies]
rift-lint = { path = "../rift-lint" }

# Library only, with JavaScript validation
[dependencies]
rift-lint = { path = "../rift-lint", default-features = false, features = ["javascript"] }

# Library only, no JavaScript engine — JS sources are not syntax-checked, and I004 says so
[dependencies]
rift-lint = { path = "../rift-lint", default-features = false }
```

## License

Apache-2.0
