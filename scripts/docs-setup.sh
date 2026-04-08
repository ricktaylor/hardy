#!/bin/bash
# Install MkDocs and dependencies for local documentation development.
#
# Creates a Python venv at .venv/ and installs mkdocs-material into it.
#
# Usage:
#   ./scripts/docs-setup.sh
#   source .venv/bin/activate
#   mkdocs serve              # Preview at http://localhost:8000

set -e

VENV_DIR=".venv"

if [ ! -d "$VENV_DIR" ]; then
    echo "Creating Python venv at $VENV_DIR..."
    python3 -m venv "$VENV_DIR"
fi

echo "Installing MkDocs..."
"$VENV_DIR/bin/pip" install mkdocs-material mkdocs-minify-plugin

echo ""
echo "Setup complete. To use:"
echo "  source .venv/bin/activate"
echo "  mkdocs serve"
