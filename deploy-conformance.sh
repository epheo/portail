#!/usr/bin/env bash
# Build, push, deploy, and run Gateway API conformance tests.
# Usage: ./deploy-conformance.sh [--skip-build] [--skip-deploy] [--skip-tests]
set -euo pipefail

IMAGE="registry.desku.be/portail:conformance"
PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"

export KUBECONFIG="$PROJECT_DIR/.kubeconfig-local"

SKIP_BUILD=false
SKIP_DEPLOY=false
SKIP_TESTS=false

for arg in "$@"; do
  case "$arg" in
    --skip-build)  SKIP_BUILD=true ;;
    --skip-deploy) SKIP_DEPLOY=true ;;
    --skip-tests)  SKIP_TESTS=true ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

SUPPORTED_FEATURES="HTTPRoute,HTTPRouteSchemeRedirect,HTTPRouteDestinationPortMatching"
SUPPORTED_FEATURES+=",HTTPRouteHostRewrite,HTTPRoutePathRewrite,HTTPRouteRequestMirror"
SUPPORTED_FEATURES+=",ReferenceGrant,Gateway,HTTPRouteMethodMatching,HTTPRouteQueryParamMatching"
SUPPORTED_FEATURES+=",HTTPRouteRequestHeaderModification,GatewayPort8080"
SUPPORTED_FEATURES+=",HTTPRouteBackendRequestHeaderModification"
SUPPORTED_FEATURES+=",HTTPRoutePathRedirect,HTTPRoutePortRedirect"
SUPPORTED_FEATURES+=",HTTPRouteResponseHeaderModification"
SUPPORTED_FEATURES+=",TLSRoute,UDPRoute"

# ── 1. Build binary ──────────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
  echo "━━━ [1/5] Building release binary ━━━"
  cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"
  echo "✓ Binary built"
fi

# ── 2. Build container image ─────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
  echo "━━━ [2/5] Building container image ━━━"
  podman build -t "$IMAGE" -f "$PROJECT_DIR/Containerfile" "$PROJECT_DIR"
  echo "✓ Image built: $IMAGE"
fi

# ── 3. Push image ────────────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
  echo "━━━ [3/5] Pushing image ━━━"
  podman push "$IMAGE"
  echo "✓ Image pushed"
fi

# ── 4. Deploy to MicroShift ──────────────────────────────────────
if [ "$SKIP_DEPLOY" = false ]; then
  echo "━━━ [4/5] Deploying to MicroShift ━━━"
  kubectl apply -f "$PROJECT_DIR/deploy/conformance.yaml"
  kubectl rollout restart daemonset/portail -n portail-system
  echo "Waiting for rollout..."
  kubectl rollout status daemonset/portail -n portail-system --timeout=120s
  sleep 5
  echo "✓ Deployed and ready"
fi

# ── 5. Run conformance tests ─────────────────────────────────────
if [ "$SKIP_TESTS" = false ]; then
  echo "━━━ [5/5] Running conformance tests ━━━"
  cd "$PROJECT_DIR/gateway-api-conformance"
  go test -v -timeout 20m ./conformance -run TestConformance -args \
    -gateway-class=portail \
    -conformance-profiles=GATEWAY-HTTP,GATEWAY-TLS \
    -supported-features="$SUPPORTED_FEATURES" \
    2>&1 | tee "$PROJECT_DIR/conformance-results.log"
  echo "━━━ Done ━━━"
fi
