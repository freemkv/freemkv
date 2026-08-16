#!/bin/bash
# Ephemeral freemkv CI runner. Registers, runs exactly one job, self-destructs.
#
# Two independent teardown mechanisms, because either alone leaks money:
#   1. --ephemeral      GitHub de-registers the runner after ONE job.
#   2. shutdown -h now  + InstanceInitiatedShutdownBehavior=terminate on the
#                       launch template, so the instance DELETES itself.
# A third, the scheduled sweeper, catches the case where this script dies
# before reaching the shutdown.
set -euxo pipefail
exec > >(tee /var/log/freemkv-runner.log) 2>&1

# Hard deadline: terminate no matter what, even if the job hangs and the
# runner never returns. 4h is well past the longest real-media rip (~90m).
( sleep 14400 ; shutdown -h now ) &

export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y curl jq ffmpeg unzip build-essential
# Needed by the JOB (S3 fixture pulls), not by this script — see the IMDS note.
apt-get install -y awscli || snap install aws-cli --classic

# Toolchain the rip suite needs.
su - ubuntu -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.97'

TOKEN=$(curl -sS -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 300")
md() { curl -sS -H "X-aws-ec2-metadata-token: $TOKEN" "http://169.254.169.254/latest/meta-data/$1"; }
IID=$(md instance-id)

# The registration token is passed in as a TAG by the launching workflow (it is
# single-use and expires in 60 min, so it never sits in an AMI or in SSM) and
# read back through IMDS rather than `aws ec2 describe-tags`.
#
# That is not a style choice: this AMI does not ship the AWS CLI, so the first
# version of this script died on `aws: command not found` BEFORE it could
# install anything, and `set -e` took the whole boot down with it. IMDS needs
# no tooling at all, which removes both the ordering trap and an IAM call from
# the critical path. It requires InstanceMetadataTags=enabled on the launch
# template.
REG=$(md tags/instance/runner-token)
REPO=$(md tags/instance/runner-repo)
[ -n "$REG" ] && [ -n "$REPO" ] || { echo "FATAL: tags not visible via IMDS — is InstanceMetadataTags enabled?"; shutdown -h now; }

mkdir -p /home/ubuntu/actions-runner && cd /home/ubuntu/actions-runner
RUNNER_VER=$(curl -sS https://api.github.com/repos/actions/runner/releases/latest | jq -r .tag_name | tr -d v)
curl -sSL -o r.tar.gz "https://github.com/actions/runner/releases/download/v${RUNNER_VER}/actions-runner-linux-x64-${RUNNER_VER}.tar.gz"
tar xzf r.tar.gz && rm r.tar.gz
chown -R ubuntu:ubuntu /home/ubuntu/actions-runner

su - ubuntu -c "cd /home/ubuntu/actions-runner && ./config.sh \
  --url https://github.com/$REPO --token $REG \
  --name ephemeral-linux-$IID --labels freemkv-media,linux --unattended --ephemeral"

# `run.sh` returns as soon as the single job finishes, because of --ephemeral.
su - ubuntu -c "cd /home/ubuntu/actions-runner && ./run.sh" || true

shutdown -h now
