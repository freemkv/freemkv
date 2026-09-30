#!/usr/bin/env bash
# Build and push the server image (`freemkv --features server`, the rip daemon + web UI
# that replaced autorip at 1.8.0) as ghcr.io/freemkv/freemkv-library and, so 1.7.x
# installs keep pulling the same name, ghcr.io/freemkv/autorip.
#
#   server-image.sh <tag> [sibling-ref] [latest]
#
# sibling-ref (qa) builds against the sibling crates' branch tips, as qa.yml tests them;
# empty builds the release commit --locked against its pinned tags. `latest` also moves
# :latest (a real release only, never a candidate). Needs GH_TOKEN with packages: write.
set -euo pipefail
tag=$1
sibling=${2:-}
latest=${3:-}
: "${GH_TOKEN:?GH_TOKEN unset}"
echo "$GH_TOKEN" | docker login ghcr.io -u "${GITHUB_ACTOR:?}" --password-stdin
tags=()
for img in ghcr.io/freemkv/freemkv-library ghcr.io/freemkv/autorip; do
  tags+=(-t "$img:$tag")
  if [ "$latest" = latest ]; then tags+=(-t "$img:latest"); fi
done
label=""
if [ -n "$sibling" ]; then label="${tag#v}"; fi
docker buildx build --push -f docker/Dockerfile \
  --build-arg SIBLING_REF="$sibling" \
  --build-arg AUTORIP_BUILD_LABEL="$label" \
  "${tags[@]}" .
