#!/usr/bin/env bash
# Build the server image from this repo's pushed HEAD against the sibling
# crates' dev tips, on a Latchkey runner, and push it to ghcr as :dev and
# :dev-<sha>. Needs `latchkey login` and a gh token with write:packages.
set -euo pipefail
repo=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
sha=$(git -C "$repo" rev-parse --short=7 HEAD)
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)
image=ghcr.io/freemkv/freemkv-library
full=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" fetch -q origin
git -C "$repo" branch -r --contains "$full" | grep -q . \
  || { echo "push $sha first: the runner clones it from GitHub" >&2; exit 1; }
latchkey run --no-context --size xlarge --timeout 3600 \
  --env GHCR_USER="$(gh api user --jq .login)" --env GHCR_TOKEN="$(gh auth token)" -- \
  "git clone -q https://github.com/freemkv/freemkv src && cd src && git checkout -q $full \
   && echo \"\$GHCR_TOKEN\" | docker login ghcr.io -u \"\$GHCR_USER\" --password-stdin >/dev/null \
   && docker buildx build --push --build-arg SIBLING_REF=dev \
        --build-arg AUTORIP_BUILD_LABEL=$ver-dev.$sha \
        -f docker/Dockerfile -t $image:dev -t $image:dev-$sha . 2>&1 | grep -E 'ERROR|error\\[|Finished|pushing manifest' ; exit \${PIPESTATUS[0]}"
echo "pushed $image:dev-$sha"
