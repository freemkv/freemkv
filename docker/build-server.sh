#!/bin/sh
# Build `freemkv --features server` inside docker/Dockerfile's builder stages and
# leave it at /build/out/freemkv.
#
#   build-server.sh [rust-target]      (none = the stage's own CPU)
#
# SIBLING_REF=dev (or qa) builds against the sibling crates' branch tips instead
# of the release tags (the same redirect CI uses); the lock then re-resolves, so
# --locked applies only to a tag build.
set -eu
target=${1:-}
set -- --release --features server --bin freemkv
if [ -n "$target" ]; then set -- "$@" --target "$target"; fi
if [ -n "${SIBLING_REF:-}" ]; then
  mkdir -p /siblings .cargo
  for r in libfreemkv freemkv-engine freemkv-keysources freemkv-i18n freemkv-unlock; do
    git clone -q --depth 1 --branch "$SIBLING_REF" "https://github.com/freemkv/$r" "/siblings/$r"
  done
  {
    echo '[patch.crates-io]'
    for r in libfreemkv freemkv-engine freemkv-keysources freemkv-i18n freemkv-unlock; do
      echo "$r = { path = \"/siblings/$r\" }"
    done
    echo '[patch."https://github.com/freemkv/freemkv-unlock"]'
    echo 'freemkv-unlock = { path = "/siblings/freemkv-unlock" }'
  } > .cargo/config.toml
  cargo build "$@"
else
  cargo build --locked "$@"
fi
mkdir -p out
cp "target/${target:+$target/}release/freemkv" out/freemkv
