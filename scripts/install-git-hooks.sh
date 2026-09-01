#!/bin/bash
# Install git hooks for Rift development
# This script copies the pre-push hook to .git/hooks/

set -e

usage() {
    echo "Install git hooks for Rift development"
    echo "This script copies the pre-push hook to .git/hooks/"
    echo ""
    echo "Usage: install-git-hooks.sh [options]"
    echo ""
    echo "Options:"
    echo "  --help, -h    Show this help message"
}

# Parsed before anything touches .git/hooks, so --help explains the script instead of running it.
# An `if` rather than the usual option loop: the script takes no non-flag arguments, so every
# branch below is terminal and a loop could never reach a second iteration.
if [[ $# -gt 0 ]]; then
    case "$1" in
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1. Use --help for usage." >&2
            exit 1
            ;;
    esac
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOKS_DIR="$REPO_ROOT/.git/hooks"

echo "Installing git hooks for Rift..."

# Create hooks directory if it doesn't exist
mkdir -p "$HOOKS_DIR"

# Install pre-push hook
echo "📋 Installing pre-push hook..."
cp "$SCRIPT_DIR/git-hooks/pre-push" "$HOOKS_DIR/pre-push"
chmod +x "$HOOKS_DIR/pre-push"

echo "✅ Git hooks installed successfully!"
echo ""
echo "The following hooks are now active:"
echo "  - pre-push: Runs cargo fmt and clippy checks before pushing"
echo ""
echo "To bypass the hook in an emergency, use: git push --no-verify"
echo ""
