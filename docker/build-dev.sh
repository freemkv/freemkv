#!/usr/bin/env bash
# Build the server image from this repo's committed HEAD against the sibling
# crates' dev tips, on a Latchkey runner, and push it to ghcr as :dev and
# :dev-<sha>. Needs `latchkey login` and a gh token with write:packages.
set -euo pipefail
repo=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
sha=$(git -C "$repo" rev-parse --short=7 HEAD)
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)
image=ghcr.io/freemkv/freemkv-library
ctx=$(mktemp -d)
trap 'rm -rf "$ctx"' EXIT
git -C "$repo" archive HEAD | tar -x -C "$ctx"
cd "$ctx"
latchkey run --size xlarge --timeout 3600 \
  --env GHCR_USER="$(gh api user --jq .login)" --env GHCR_TOKEN="$(gh auth token)" -- \
  "echo \"\$GHCR_TOKEN\" | docker login ghcr.io -u \"\$GHCR_USER\" --password-stdin >/dev/null \
   && docker buildx build --push --build-arg SIBLING_REF=dev \
        --build-arg AUTORIP_BUILD_LABEL=$ver-dev.$sha \
        -f docker/Dockerfile -t $image:dev -t $image:dev-$sha . 2>&1 | grep -E 'ERROR|error\\[|Finished|pushing manifest' ; exit \${PIPESTATUS[0]}"
echo "pushed $image:dev-$sha"
