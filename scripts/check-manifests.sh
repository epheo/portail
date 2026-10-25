#!/usr/bin/env bash
# Build every kustomization under deploy/ and examples/kubernetes/, so a
# broken overlay fails CI instead of an install. A Component builds only
# inside an overlay that uses it, so it is skipped.
#
#   scripts/check-manifests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

kubectl=${KUBECTL:-kubectl}
built=0
while IFS= read -r file; do
    dir=$(dirname "$file")
    if grep -q '^kind: Component$' "$file"; then
        continue
    fi
    if ! out=$("$kubectl" kustomize "$dir" 2>&1); then
        echo "error: $dir does not build:" >&2
        echo "$out" >&2
        exit 1
    fi
    built=$((built + 1))
done < <(find deploy examples/kubernetes -name kustomization.yaml | sort)

if [ "$built" -eq 0 ]; then
    echo "error: no kustomization found" >&2
    exit 1
fi
echo "$built kustomizations build"
