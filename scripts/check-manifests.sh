#!/usr/bin/env bash
# Build every kustomization under deploy/ and examples/kubernetes/, so a
# broken overlay fails CI instead of an install. A Component builds only
# inside an overlay that uses it: each must be used by one built here.
#
#   scripts/check-manifests.sh
set -euo pipefail
cd "$(dirname "$0")/.."

kubectl=${KUBECTL:-kubectl}
built=0
components=()
while IFS= read -r file; do
    dir=$(dirname "$file")
    if grep -q '^kind: Component$' "$file"; then
        components+=("$dir")
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
for dir in "${components[@]}"; do
    if ! grep -rqsx --include=kustomization.yaml -- "- .*/$dir" deploy examples/kubernetes; then
        echo "error: no kustomization uses component $dir, so nothing builds it" >&2
        exit 1
    fi
done
echo "$built kustomizations build; components: ${#components[@]}, each used"
