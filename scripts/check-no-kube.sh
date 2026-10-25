#!/usr/bin/env bash
# A data plane runs without a Kubernetes client, permission or token, so no
# library module outside src/kubernetes may name kube, k8s_openapi,
# gateway_api or the kubernetes module: not src/sync or src/dataplane, and
# not the modules they import, which could re-export them. Grouped and
# multi-line imports, aliases and extern crate all name one on some line.
# A module file can also come in by include! or a path attribute: include!
# is refused outside src/kubernetes, and so is a path attribute whose file
# names one; src/sync and src/dataplane take no path attribute at all.
#
# Exempt: lib.rs declaring the module, and main.rs, the binary: it is not
# the library a data plane links in but the dispatch to both modes, so it
# names the controller. Comment lines are skipped; a false positive beats
# a miss.
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

kube='(kube|k8s_openapi|gateway_api|kubernetes)'
# A name, unless inside a string or a flag: "kubernetes.io/tls", --kubernetes.
names="(^|[^-\"[:alnum:]_])$kube([^[:alnum:]_]|\$)"
# Where a string is a file to compile, its name counts.
files="include!|path[[:space:]]*=.*$kube"
# A path attribute: #[path], #[ path], a cfg_attr's, on its line or the next.
path_attr='#[[:space:]]*\[.*path[[:space:]]*=|^[[:space:]]*path[[:space:]]*='
comment='^[^:]+:[0-9]+:[[:space:]]*//'

hits=$(
    find "$src" -name '*.rs' -not -path "$src/kubernetes/*" -not -path "$src/main.rs" -print0 |
        xargs -0 grep -nE "$names|$files" /dev/null |
        grep -vE "$comment" |
        grep -vE "^$src/lib.rs:[0-9]+:pub mod kubernetes;$" || true
    grep -rnE "$path_attr" "$src/sync" "$src/dataplane" | grep -vE "$comment" || true
)
if [ -n "$hits" ]; then
    echo "$hits"
    echo "error: library code outside src/kubernetes names kube, k8s_openapi, gateway_api or the kubernetes module" >&2
    exit 1
fi
