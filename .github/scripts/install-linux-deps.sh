#!/usr/bin/env bash
# Use the main HTTPS archive: the hosted runner's Azure HTTP mirror can crawl.
set -euo pipefail

apt_config=${FMKV_APT_CONFIG_DIR:-/etc/apt}
for source in "$apt_config"/sources.list "$apt_config"/sources.list.d/*.sources \
              "$apt_config"/sources.list.d/*.list "$apt_config"/apt-mirrors*.txt; do
  [ -f "$source" ] || continue
  sudo sed -i.bak 's|http://azure.archive.ubuntu.com/ubuntu|https://archive.ubuntu.com/ubuntu|g' "$source"
done

options=(-o Acquire::Retries=2 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
sudo apt-get "${options[@]}" update
sudo env DEBIAN_FRONTEND=noninteractive apt-get "${options[@]}" install --no-install-recommends -y "$@"
