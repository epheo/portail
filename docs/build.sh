#!/bin/bash
# Build HTML documentation from Markdown files using pandoc.
# Output goes to docs/html/
#
# Usage: ./docs/build.sh
#
# Requires: pandoc (dnf install pandoc)

set -euo pipefail

DOCS_DIR="$(cd "$(dirname "$0")" && pwd)"
OUT_DIR="$DOCS_DIR/html"

mkdir -p "$OUT_DIR"

for f in "$DOCS_DIR"/*.md; do
  base="$(basename "${f%.md}")"
  out="$OUT_DIR/$base.html"
  pandoc "$f" -s --metadata title="Portail" -o "$out"
  echo "  $base.html"
done

echo "Done. Output in docs/html/"
