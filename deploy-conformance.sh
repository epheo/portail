#!/usr/bin/env bash
# Build, push, deploy, and run Gateway API conformance tests.
# Usage: ./deploy-conformance.sh [--skip-build] [--skip-deploy] [--skip-tests]
set -euo pipefail

IMAGE="registry.desku.be/portail:latest"
PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEPLOY_DIR="$PROJECT_DIR/deploy"
GATEWAY_API_VERSION="v1.4.1"

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

# Get supported features from the binary (single source of truth)
SUPPORTED_FEATURES="$("$PROJECT_DIR/target/release/portail" --supported-features)"

# ── 1. Build binary ──────────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
  echo "━━━ [1/6] Building release binary ━━━"
  cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"
  echo "✓ Binary built"
fi

# ── 2. Build container image ─────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
  echo "━━━ [2/6] Building container image ━━━"
  podman build -t "$IMAGE" -f "$PROJECT_DIR/Containerfile" "$PROJECT_DIR"
  echo "✓ Image built: $IMAGE"
fi

# ── 3. Push image ────────────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
  echo "━━━ [3/6] Pushing image ━━━"
  podman push "$IMAGE"
  echo "✓ Image pushed"
fi

# ── 4. Install Gateway API experimental CRDs ────────────────────
if [ "$SKIP_DEPLOY" = false ]; then
  echo "━━━ [4/6] Installing Gateway API experimental CRDs ($GATEWAY_API_VERSION) ━━━"
  # Portail requires experimental-channel CRDs for TCPRoute/TLSRoute/UDPRoute.
  # --server-side --force-conflicts avoids "annotations too long" on HTTPRoute CRD.
  kubectl apply --server-side --force-conflicts \
    -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/${GATEWAY_API_VERSION}/experimental-install.yaml"
  echo "✓ Experimental CRDs installed"
fi

# ── 5. Deploy portail using deploy/*.yaml ────────────────────────
if [ "$SKIP_DEPLOY" = false ]; then
  echo "━━━ [5/6] Deploying portail to cluster ━━━"
  kubectl apply -f "$DEPLOY_DIR/namespace.yaml"
  kubectl apply -f "$DEPLOY_DIR/rbac.yaml"
  kubectl apply -f "$DEPLOY_DIR/scc.yaml" 2>/dev/null || true  # SCC is OpenShift-only
  kubectl apply -f "$DEPLOY_DIR/gatewayclass.yaml"
  kubectl apply -f "$DEPLOY_DIR/daemonset.yaml"
  kubectl rollout restart daemonset/portail -n portail-system
  echo "Waiting for rollout..."
  kubectl rollout status daemonset/portail -n portail-system --timeout=120s
  sleep 5
  echo "✓ Deployed and ready"
fi

# ── 6. Run conformance tests ─────────────────────────────────────
if [ "$SKIP_TESTS" = false ]; then
  echo "━━━ [6/6] Running conformance tests ━━━"
  cd "$PROJECT_DIR/gateway-api-conformance"
  go test -v -timeout 20m ./conformance -run TestConformance -args \
    -gateway-class=portail \
    -conformance-profiles=GATEWAY-HTTP,GATEWAY-TLS \
    -supported-features="$SUPPORTED_FEATURES" \
    2>&1 | tee "$PROJECT_DIR/conformance-results.log"
  echo "━━━ Done ━━━"
fi
