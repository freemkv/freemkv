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
RUNNER_USER=ubuntu
RUNNER_HOME=$(getent passwd "$RUNNER_USER" | cut -d: -f6)
su - "$RUNNER_USER" -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.97'

TOKEN=$(curl -sS -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 300")
md() { curl -sS -H "X-aws-ec2-metadata-token: $TOKEN" "http://169.254.169.254/latest/meta-data/$1"; }
IID=$(md instance-id)

# The registration token is NOT passed in a tag: a tag is readable by any
# principal with ec2:DescribeTags/DescribeInstances, and a GitHub registration
# token is valid for REPEATED registrations for its whole 60-minute life — long
# enough for an account-read to register a rogue runner. So the launcher stashes
# the token in SSM Parameter Store as a SecureString and tags only its NAME
# (non-secret). We read the NAME from IMDS (no tooling), then fetch+decrypt+
# DELETE the token via awscli, which is installed above (so `aws` is on PATH and
# there is no ordering trap). IMDS tags still require InstanceMetadataTags=enabled.
PARAM=$(md tags/instance/runner-token-param)
REPO=$(md tags/instance/runner-repo)
[ -n "$PARAM" ] && [ -n "$REPO" ] || { echo "FATAL: tags not visible via IMDS — is InstanceMetadataTags enabled?"; shutdown -h now; }
REGION=$(md placement/region)
REG=$(aws ssm get-parameter --region "$REGION" --name "$PARAM" --with-decryption --query Parameter.Value --output text)
# Delete immediately so the token never outlives this boot, whatever happens next.
aws ssm delete-parameter --region "$REGION" --name "$PARAM" || true
[ -n "$REG" ] || { echo "FATAL: registration token not found in SSM ($PARAM)"; shutdown -h now; }

mkdir -p "$RUNNER_HOME/actions-runner" && cd "$RUNNER_HOME/actions-runner"
RUNNER_VER=$(curl -sS https://api.github.com/repos/actions/runner/releases/latest | jq -r .tag_name | tr -d v)
curl -sSL -o r.tar.gz "https://github.com/actions/runner/releases/download/v${RUNNER_VER}/actions-runner-linux-x64-${RUNNER_VER}.tar.gz"
tar xzf r.tar.gz && rm r.tar.gz
chown -R "$RUNNER_USER:$RUNNER_USER" "$RUNNER_HOME/actions-runner"

su - "$RUNNER_USER" -c "cd $RUNNER_HOME/actions-runner && ./config.sh \
  --url https://github.com/$REPO --token $REG \
  --name ephemeral-linux-$IID --labels freemkv-media,linux --unattended --ephemeral"

# `run.sh` returns as soon as the single job finishes, because of --ephemeral.
su - "$RUNNER_USER" -c "cd $RUNNER_HOME/actions-runner && ./run.sh" || true

shutdown -h now
