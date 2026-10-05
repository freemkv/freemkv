#!/usr/bin/env bash
# Build the server image (`freemkv --features server`, the rip daemon + web UI that
# replaced autorip at 1.8.0) one platform at a time, then publish them as ONE
# multi-arch manifest under ghcr.io/freemkv/freemkv-library and, so 1.7.x installs
# keep pulling the same name, ghcr.io/freemkv/autorip. Driven by server-image.yml.
#
#   server-image.sh build <platform> <tag> [sibling-ref] [push]
#       Build docker/Dockerfile for <platform> (linux/amd64, linux/arm64,
#       linux/arm/v7). With `push`, push it by digest (untagged) to
#       freemkv-library and record the digest as an empty file in $DIGEST_DIR.
#   server-image.sh merge <tag> <latest|''> <digest-dir>
#       Tag the pushed digests as one manifest list: <tag> (and :latest with
#       `latest`) on both names, so the two names always resolve identically.
#
# sibling-ref (dev/qa) builds against the sibling crates' branch tips, as CI tests
# them; empty builds the commit --locked against its pinned tags. Pushing needs
# GH_TOKEN with packages: write.
set -euo pipefail
lib=ghcr.io/freemkv/freemkv-library
names=("$lib" ghcr.io/freemkv/autorip)

login() {
  : "${GH_TOKEN:?GH_TOKEN unset}"
  echo "$GH_TOKEN" | docker login ghcr.io -u "${GITHUB_ACTOR:?}" --password-stdin
}

build() {
  local platform=$1 tag=$2 sibling=${3:-} push=${4:-} label="" meta out digest
  if [ -n "$sibling" ]; then label="${tag#v}"; fi
  out=(--output type=cacheonly)
  if [ "$push" = push ]; then
    login
    out=(--output "type=image,name=$lib,push-by-digest=true,name-canonical=true,push=true")
  fi
  meta=$(mktemp)
  # No provenance: plain per-platform manifests, so the merged list is just the images.
  docker buildx build --platform "$platform" --provenance=false -f docker/Dockerfile \
    --build-arg SIBLING_REF="$sibling" \
    --build-arg AUTORIP_BUILD_LABEL="$label" \
    --metadata-file "$meta" "${out[@]}" .
  if [ "$push" = push ]; then
    digest=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["containerimage.digest"])' "$meta")
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "::error::no image digest for $platform" >&2; exit 1; }
    mkdir -p "${DIGEST_DIR:?DIGEST_DIR unset}"
    touch "$DIGEST_DIR/${digest#sha256:}"
    echo "$platform -> $lib@$digest"
  fi
}

merge() {
  local tag=$1 latest=${2:-} dir=$3 img d want got
  login
  local tags=() srcs=()
  for img in "${names[@]}"; do
    tags+=(-t "$img:$tag")
    if [ "$latest" = latest ]; then tags+=(-t "$img:latest"); fi
  done
  for d in "$dir"/*; do srcs+=("$lib@sha256:$(basename "$d")"); done
  [ "${#srcs[@]}" -ge 1 ] || { echo "::error::no platform digests in $dir" >&2; exit 1; }
  docker buildx imagetools create "${tags[@]}" "${srcs[@]}"
  # Every name and tag must be the same manifest list, holding every platform.
  want=$(docker buildx imagetools inspect "$lib:$tag" --format '{{json .Manifest.Digest}}')
  docker buildx imagetools inspect "$lib:$tag"
  for img in "${names[@]}"; do
    for t in "$tag" ${latest:+latest}; do
      got=$(docker buildx imagetools inspect "$img:$t" --format '{{json .Manifest.Digest}}')
      [ "$got" = "$want" ] || { echo "::error::$img:$t is $got, $lib:$tag is $want" >&2; exit 1; }
    done
  done
  got=$(docker buildx imagetools inspect "$lib:$tag" --raw \
        | python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("manifests", [])))')
  [ "$got" = "${#srcs[@]}" ] || { echo "::error::$lib:$tag lists $got platforms, built ${#srcs[@]}" >&2; exit 1; }
}

cmd=${1:-}
shift || true
case "$cmd" in
  build) [ $# -ge 2 ] || { echo "usage: $0 build <platform> <tag> [sibling-ref] [push]" >&2; exit 2; }; build "$@" ;;
  merge) [ $# -eq 3 ] || { echo "usage: $0 merge <tag> <latest|''> <digest-dir>" >&2; exit 2; }; merge "$@" ;;
  *) echo "usage: $0 build|merge ..." >&2; exit 2 ;;
esac
