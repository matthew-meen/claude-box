#!/bin/bash
set -euo pipefail

# claude-box install script
# Builds the release binary and installs it to ~/.local/bin.

INSTALL_DIR="${CLAUDE_BOX_INSTALL_DIR:-$HOME/.local/bin}"

echo "==> Building claude-box (release)..."
cargo build --release

echo "==> Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
cp target/release/claude-box "$INSTALL_DIR/claude-box"
chmod +x "$INSTALL_DIR/claude-box"

# Check if install dir is on PATH.
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
    echo ""
    echo "    $INSTALL_DIR is not on your PATH."
    echo "    Add this to your shell profile:"
    echo ""
    echo "        export PATH=\"$INSTALL_DIR:\$PATH\""
    echo ""
fi

# Check for tart.
if ! command -v tart &>/dev/null; then
    echo "==> tart is not installed."
    echo "    claude-box will offer to install it on first run, or install manually:"
    echo ""
    echo "        brew install cirruslabs/cli/tart"
    echo ""
fi

# Check for claude.
if ! command -v claude &>/dev/null; then
    echo "==> claude is not installed."
    echo "    Install Claude Code: https://claude.ai/code"
    echo ""
fi

# Remind about base image.
if [ -z "${CLAUDE_BOX_IMAGE:-}" ]; then
    echo "==> Set your base image:"
    echo ""
    echo "    export CLAUDE_BOX_IMAGE=ghcr.io/cirruslabs/macos-sequoia-base:latest"
    echo ""
fi

echo "==> Done. Run 'claude-box --help' to get started."
