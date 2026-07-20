# rift-lint

Configuration linter for Rift HTTP Proxy - validates imposter configuration files for Mountebank compatibility.

## Features

- **Port conflict detection** across multiple imposter files
- **Header validation** - ensures values are strings (not arrays, numbers, booleans)
- **Predicate validation** - JSONPath selectors, regex patterns, operators
- **JavaScript validation** - syntax checking for wait/decorate behaviors
- **Response validation** - status codes, proxy URLs, required fields
- **Auto-fix** capability for common issues

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

# Lint a single file
rift-lint ./imposters/my-service.json

# Show only errors (hide warnings)
rift-lint ./imposters/ --errors-only

# JSON output for CI/CD
rift-lint ./imposters/ --output json

# Strict mode - treat warnings as errors
rift-lint ./imposters/ --strict

# Auto-fix issues where possible
rift-lint ./imposters/ --fix
```

### Options

| Option | Short | Description | Default |
|--------|-------|-------------|---------|
| `<PATH>` | | Path to file or directory | (required) |
| `--fix` | `-f` | Auto-fix issues | `false` |
| `--output` | `-o` | Output format: `text`, `json` | `text` |
| `--errors-only` | `-e` | Hide warnings | `false` |
| `--verbose` | `-v` | Verbose output | `false` |
| `--strict` | `-s` | Warnings become errors | `false` |

## Library Usage

```rust
use rift_lint::{lint_file, lint_json, lint_value, LintOptions};
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

// Lint already-parsed JSON
let value: serde_json::Value = serde_json::from_str(json).unwrap();
let result = lint_value(&value, "inline", &LintOptions::default());
```

## Validation Rules

### Errors

| Code | Description |
|------|-------------|
| E001 | Invalid JSON / file read error |
| E002 | Port conflict |
| E003 | Missing required field |
| E004 | Invalid protocol |
| E005 | Port out of range |
| E006-E033 | Various structural errors |
| E034 | Multiple predicate operations in one predicate |

### Warnings

| Code | Description |
|------|-------------|
| W001 | Privileged port |
| W002-W009 | Various potential issues |

### Info

| Code | Description |
|------|-------------|
| I001 | Mountebank slice notation in JSONPath |
| I002 | Proxy targets localhost |

## Feature Flags

- `cli` (default) - Enables CLI binary with clap
- `javascript` - Enables JavaScript syntax validation with boa_engine

```toml
# Library only (no CLI dependencies)
[dependencies]
rift-lint = { path = "../rift-lint", default-features = false }

# With JavaScript validation
[dependencies]
rift-lint = { path = "../rift-lint", features = ["javascript"] }
```

## License

Apache-2.0
