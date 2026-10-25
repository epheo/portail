#!/usr/bin/env bash
# Set every in-tree version to X.Y.Z for a release.
#
#   scripts/release.sh 0.2.0
#
# Then: commit "Release v0.2.0", merge it like any change, and tag that
# commit on main: git tag v0.2.0 && git push origin v0.2.0
# The tag publishes the image and the GitHub release; publish.yml refuses a
# tag whose versions do not match.
set -euo pipefail

cd "$(dirname "$0")/.."

version="${1:-}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "usage: $0 X.Y.Z" >&2
    exit 1
fi

sed -i "s/^version = \".*\"/version = \"${version}\"/" Cargo.toml
sed -i "/^name = \"portail\"$/{n;s/^version = \".*\"/version = \"${version}\"/}" Cargo.lock
sed -i "s/\(portailVersion *= *\)\"v[^\"]*\"/\1\"v${version}\"/" conformance/conformance_test.go
sed -i "s/^\( *newTag: *\)\"[^\"]*\"$/\1\"${version}\"/" deploy/install/kustomization.yaml
sed -i "s|^\( *image: ghcr.io/epheo/portail:\)[0-9.]*$|\1${version}|" examples/kubernetes/unmanaged/deployment.yaml

git diff --stat
