#!/usr/bin/env bash
# A data plane runs without a Kubernetes client, permission or token: no
# code under src/sync or src/dataplane may reach kube, k8s_openapi,
# gateway_api or crate::kubernetes. Comment lines may name them.
#
#   scripts/check-no-kube.sh [DIR...]
set -euo pipefail
cd "$(dirname "$0")/.."

dirs=("$@")
[ ${#dirs[@]} -gt 0 ] || dirs=(src/sync src/dataplane)

pattern='(^|[^[:alnum:]_])(kube|k8s_openapi|gateway_api)::|(crate|super)::kubernetes'
if hits=$(grep -rnE "$pattern" "${dirs[@]}" | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//'); then
    echo "$hits"
    echo "error: ${dirs[*]} must not use kube, k8s_openapi, gateway_api or crate::kubernetes" >&2
    exit 1
fi
