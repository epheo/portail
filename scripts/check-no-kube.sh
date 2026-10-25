#!/usr/bin/env bash
# A data plane runs without a Kubernetes client, permission or token, so no
# library module outside src/kubernetes may name kube, k8s_openapi,
# gateway_api or the kubernetes module: not src/sync or src/dataplane, and
# not the modules they import, which could re-export them. Grouped and
# multi-line imports, aliases and extern crate all name one on some line;
# src/sync and src/dataplane may not use #[path] either. The binary
# (main.rs) and lib.rs declaring the module are exempt. Comment lines are
# skipped; a false positive beats a miss.
#
#   scripts/check-no-kube.sh [SRC_DIR]
set -euo pipefail
src=${1:-"$(dirname "$0")/../src"}

for dir in "$src/sync" "$src/dataplane"; do
    if [ ! -d "$dir" ]; then
        echo "error: $dir does not exist" >&2
        exit 1
    fi
done

# A name, unless inside a string or a flag: "kubernetes.io/tls", --kubernetes.
names='(^|[^-"[:alnum:]_])(kube|k8s_openapi|gateway_api|kubernetes)([^[:alnum:]_]|$)'
comment='^[^:]+:[0-9]+:[[:space:]]*//'

hits=$(
    find "$src" -name '*.rs' -not -path "$src/kubernetes/*" -not -path "$src/main.rs" -print0 |
        xargs -0 grep -nE "$names" /dev/null |
        grep -vE "$comment" |
        grep -vE "^$src/lib.rs:[0-9]+:pub mod kubernetes;$" || true
    grep -rnE '#\[path' "$src/sync" "$src/dataplane" | grep -vE "$comment" || true
)
if [ -n "$hits" ]; then
    echo "$hits"
    echo "error: library code outside src/kubernetes names kube, k8s_openapi, gateway_api or the kubernetes module" >&2
    exit 1
fi
