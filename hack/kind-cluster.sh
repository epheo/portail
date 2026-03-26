#!/usr/bin/env bash
# Create or delete a Kind cluster for Portail conformance testing.
# Usage: hack/kind-cluster.sh create | delete
set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-portail}"
KIND_NODE_TAG="${KIND_NODE_TAG:-v1.32.0}"
KUBECONFIG_PATH="${KUBECONFIG_PATH:-.kubeconfig-kind}"

usage() {
  echo "Usage: $0 {create|delete}"
  exit 1
}

create_cluster() {
  if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
    echo "Kind cluster '${CLUSTER_NAME}' already exists, reusing it."
  else
    echo "Creating Kind cluster '${CLUSTER_NAME}' (node: ${KIND_NODE_TAG})..."
    cat <<EOF | kind create cluster --name "${CLUSTER_NAME}" --image "kindest/node:${KIND_NODE_TAG}" --config -
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  extraPortMappings:
  - containerPort: 8080
    hostPort: 8080
    protocol: TCP
  - containerPort: 8443
    hostPort: 8443
    protocol: TCP
EOF
  fi

  kind get kubeconfig --name "${CLUSTER_NAME}" > "${KUBECONFIG_PATH}"
  echo "Kubeconfig written to ${KUBECONFIG_PATH}"
}

delete_cluster() {
  if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
    kind delete cluster --name "${CLUSTER_NAME}"
    rm -f "${KUBECONFIG_PATH}"
    echo "Kind cluster '${CLUSTER_NAME}' deleted."
  else
    echo "Kind cluster '${CLUSTER_NAME}' does not exist, nothing to delete."
  fi
}

case "${1:-}" in
  create) create_cluster ;;
  delete) delete_cluster ;;
  *)      usage ;;
esac
