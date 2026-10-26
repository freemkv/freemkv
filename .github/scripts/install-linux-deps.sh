#!/usr/bin/env bash
# Use the main HTTPS archive: the hosted runner's Azure HTTP mirror can crawl.
set -euo pipefail

for source in /etc/apt/sources.list /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list; do
  [ -f "$source" ] || continue
  sudo sed -i 's|http://azure.archive.ubuntu.com/ubuntu|https://archive.ubuntu.com/ubuntu|g' "$source"
done

options=(-o Acquire::Retries=2 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
sudo apt-get "${options[@]}" update
sudo env DEBIAN_FRONTEND=noninteractive apt-get "${options[@]}" install --no-install-recommends -y "$@"
