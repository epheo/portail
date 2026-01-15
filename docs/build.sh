#!/bin/bash
# Build HTML documentation from Markdown files using pandoc.
# Output goes to docs/*.html alongside the source .md files.
#
# Usage: ./docs/build.sh
#
# Requires: pandoc (dnf install pandoc)

set -euo pipefail

DOCS_DIR="$(cd "$(dirname "$0")" && pwd)"

for f in "$DOCS_DIR"/*.md; do
  out="${f%.md}.html"
  pandoc "$f" -s --metadata title="Portail" -o "$out"
  echo "  $(basename "$out")"
done

echo "Done."
