# Git Hooks for Rift

This directory contains git hooks to enforce code quality standards.

## Installation

From the repository root, run:

```bash
./scripts/install-git-hooks.sh
```

Pass `--help` (or `-h`) to see what the script does without installing anything.

## Available Hooks

### pre-push

Runs before pushing to a remote repository. Performs the following checks:

1. **Code Formatting**: Verifies all code is properly formatted with `cargo fmt`
2. **Lint Checks**: Runs `cargo clippy` to catch common mistakes and enforce best practices
3. **Unit Tests**: Runs all workspace tests with `cargo test` (can be skipped)
4. **Integration Tests**: Runs Docker-based integration tests (enabled by default)
5. **Compatibility Tests**: Runs Mountebank compatibility tests - 72 BDD tests (enabled by default)

If any check fails, the push is aborted with helpful instructions on how to fix the issues.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SKIP_TESTS` | `0` | Set to `1` to skip unit tests (not recommended) |
| `SKIP_INTEGRATION` | `0` | Set to `1` to skip integration tests |
| `SKIP_COMPATIBILITY` | `0` | Set to `1` to skip compatibility tests |
| `SKIP_ALL_DOCKER_TESTS` | `0` | Set to `1` to skip both integration and compatibility tests |
| `QUICK_PUSH` | `0` | Set to `1` for quick push (format + clippy + unit tests only) |

### Examples

```bash
# Full push (format + clippy + unit + integration + compatibility tests)
git push

# Quick push (format + clippy + unit tests only - skip Docker tests)
QUICK_PUSH=1 git push

# Skip only integration tests
SKIP_INTEGRATION=1 git push

# Skip only compatibility tests
SKIP_COMPATIBILITY=1 git push

# Skip all Docker-based tests
SKIP_ALL_DOCKER_TESTS=1 git push

# Emergency push (skip unit tests - not recommended!)
SKIP_TESTS=1 git push
```

## Bypassing Hooks

In emergency situations, you can bypass all hooks with:

```bash
git push --no-verify
```

**Note**: Use this sparingly. It's better to fix the issues than bypass the checks.

## Fixing Issues

### Formatting Issues

```bash
cargo fmt --all
```

### Clippy Issues

Auto-fix (when possible):
```bash
cargo clippy --fix --workspace --all-targets --all-features --allow-dirty
```

Manual review:
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

### Unit Test Failures

Run tests to see failures:
```bash
cargo test --workspace --all-features
```

Run a specific test:
```bash
cargo test --workspace --all-features test_name
```

### Integration Test Failures

Run integration tests manually for detailed output:
```bash
cargo test --package rift-http-proxy --test integration --all-features -- --test-threads=1
```

### Compatibility Test Failures

Run compatibility tests manually:
```bash
cd tests/compatibility
docker compose up -d --build
cargo test --release -- --format pretty
docker compose down -v
```

## Uninstalling

To remove the hooks, simply delete them:

```bash
rm .git/hooks/pre-push
```
