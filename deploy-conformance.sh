#!/usr/bin/env bash
# Build, deploy, and run Gateway API conformance tests in an ephemeral Kind cluster.
# Usage: ./deploy-conformance.sh [--skip-build] [--skip-deploy] [--skip-tests] [--no-kind] [--no-cleanup]
set -euo pipefail

IMAGE="ghcr.io/epheo/portail:latest"
PROJECT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEPLOY_DIR="$PROJECT_DIR/deploy"
GATEWAY_API_VERSION="v1.4.1"
PORTAIL_VERSION="v0.1.0"
REPORT_OUTPUT="$PROJECT_DIR/conformance-report.yaml"

SKIP_BUILD=false
SKIP_DEPLOY=false
SKIP_TESTS=false
USE_KIND=true
CLEANUP=true

for arg in "$@"; do
  case "$arg" in
    --skip-build)  SKIP_BUILD=true ;;
    --skip-deploy) SKIP_DEPLOY=true ;;
    --skip-tests)  SKIP_TESTS=true ;;
    --no-kind)     USE_KIND=false ;;
    --no-cleanup)  CLEANUP=false ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

# ── 0. Set up cluster ────────────────────────────────────────────
if [ "$USE_KIND" = true ]; then
  KUBECONFIG_PATH="$PROJECT_DIR/.kubeconfig-kind"
  "$PROJECT_DIR/hack/kind-cluster.sh" create
  export KUBECONFIG="$KUBECONFIG_PATH"

  if [ "$CLEANUP" = true ]; then
    trap '"$PROJECT_DIR/hack/kind-cluster.sh" delete' EXIT
  fi
else
  export KUBECONFIG="${KUBECONFIG:-$PROJECT_DIR/.kubeconfig-local}"
fi

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

# ── 3. Push / load image ─────────────────────────────────────────
if [ "$SKIP_BUILD" = false ]; then
  if [ "$USE_KIND" = true ]; then
    echo "━━━ [3/6] Loading image into Kind cluster ━━━"
    rm -f /tmp/portail-image.tar
    podman save --format docker-archive "$IMAGE" -o /tmp/portail-image.tar
    kind load image-archive /tmp/portail-image.tar --name "${CLUSTER_NAME:-portail}"
    rm -f /tmp/portail-image.tar
    echo "✓ Image loaded into Kind"
  else
    echo "━━━ [3/6] Pushing image ━━━"
    podman push "$IMAGE"
    echo "✓ Image pushed"
  fi
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
  # Override image with the actual registry image
  kubectl set image daemonset/portail portail="$IMAGE" -n portail-system
  if [ "$USE_KIND" = true ]; then
    # Image is loaded locally — prevent pulling from registry
    kubectl patch daemonset/portail -n portail-system \
      -p '{"spec":{"template":{"spec":{"containers":[{"name":"portail","imagePullPolicy":"IfNotPresent"}]}}}}'
  fi
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

  if [ "$USE_KIND" = true ]; then
    # Build the test binary on the host, then run it inside the Kind container
    # where pod IPs are directly reachable (rootless Podman can't bind port 80).
    echo "Building conformance test binary..."
    CGO_ENABLED=0 go test -c -o /tmp/conformance.test ./conformance
    CONTAINER_NAME="${CLUSTER_NAME:-portail}-control-plane"
    podman cp /tmp/conformance.test "$CONTAINER_NAME":/conformance.test
    podman cp "$KUBECONFIG" "$CONTAINER_NAME":/kubeconfig
    rm -f /tmp/conformance.test

    echo "Running conformance tests inside Kind container..."
    podman exec -e KUBECONFIG=/etc/kubernetes/admin.conf "$CONTAINER_NAME" /conformance.test \
      -test.v -test.timeout 20m -test.run TestConformance \
      -gateway-class=portail \
      -conformance-profiles=GATEWAY-HTTP,GATEWAY-TLS \
      -supported-features="$SUPPORTED_FEATURES" \
      -report-output=/conformance-report.yaml \
      -organization=epheo \
      -project=portail \
      -url=https://github.com/epheo/portail \
      -version="$PORTAIL_VERSION" \
      -contact=@epheo \
      2>&1 | tee "$PROJECT_DIR/conformance-results.log"

    # Copy report back to host
    podman cp "$CONTAINER_NAME":/conformance-report.yaml "$REPORT_OUTPUT" 2>/dev/null || true
  else
    go test -v -timeout 20m ./conformance -run TestConformance -args \
      -gateway-class=portail \
      -conformance-profiles=GATEWAY-HTTP,GATEWAY-TLS \
      -supported-features="$SUPPORTED_FEATURES" \
      -report-output="$REPORT_OUTPUT" \
      -organization=epheo \
      -project=portail \
      -url=https://github.com/epheo/portail \
      -version="$PORTAIL_VERSION" \
      -contact=@epheo \
      2>&1 | tee "$PROJECT_DIR/conformance-results.log"
  fi

  echo "━━━ Done ━━━"
  echo "Conformance report written to: $REPORT_OUTPUT"
fi
