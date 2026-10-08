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

Runs before pushing to a remote repository:

1. **Code Formatting**: `cargo fmt --all -- --check`
2. **Lint Checks**: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

If either fails, the push is aborted. Tests are left to CI, including the Mountebank differential
harness (`conformance/differential/`, run it locally with `cargo test -p rift-differential`).

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

## Uninstalling

To remove the hooks, simply delete them:

```bash
rm .git/hooks/pre-push
```
