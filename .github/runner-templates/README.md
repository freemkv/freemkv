# Ephemeral runner user-data

The scripts EC2 launch templates run at boot. Kept in the repo because they are
the part of the CI that is invisible from GitHub — when a runner never appears,
this is the only place that explains why, and every one of the failures below
was diagnosed from an instance console log rather than from Actions.

Apply a change with:

    aws ec2 create-launch-template-version --region us-west-2 \
      --launch-template-name freemkv-runner-<os> --source-version <n> \
      --launch-template-data "$(...UserData base64...)"
    aws ec2 modify-launch-template --launch-template-name freemkv-runner-<os> \
      --default-version <n+1>

## What each failure taught, so it is not rediscovered

**linux v1 → v2.** `aws: command not found`. The Ubuntu AMI does not ship the
AWS CLI, and the script called `aws ec2 describe-tags` to read its own
registration token BEFORE installing anything — with `set -e`, that took the
whole boot down. Now the tags come from IMDS, which needs no tooling at all and
removes an IAM call from the critical path. Requires
`InstanceMetadataTags=enabled`.

**windows v1 → v2.** Same IMDS change, plus chocolatey: it is not on the base
Windows Server AMI, so it has to bootstrap itself before it can install
anything.

**windows v2 → v3.** `bash: command not found`. The AMI has no Git for Windows,
so there is no bash — and every `shell: bash` step fails, and `actions/checkout`
has nothing to clone with. Hosted Windows runners ship it, which is exactly why
its absence was surprising.

**windows v3 → v4 → v5.** `aws: command not found`, twice. First the CLI was
genuinely absent; then it was installed but still not found, because chocolatey
writes the MACHINE PATH and that write is invisible to the already-running
user-data process. Appending to `$env:PATH` produced a PATH that still lacked
it, and the runner inherited that. The fix is to re-read PATH from the machine
environment after the installs, then assert every tool is present — failing at
boot rather than twenty minutes into a rip.

## Where the registration token lives

The token is **not** in an instance tag. A tag is readable by any principal with
`ec2:DescribeTags`/`DescribeInstances`, and a GitHub registration token is valid
for *repeated* registrations for its whole 60-minute life — long enough for an
account-read to register a rogue runner that then picks up a job carrying repo
secrets. So the launching workflow (`ci-runner-launch.yml`) stashes the token in
**SSM Parameter Store as a SecureString** and tags only its NAME
(`runner-token-param`, non-secret). The user-data reads the name from IMDS, then
`aws ssm get-parameter --with-decryption`, then `aws ssm delete-parameter` so the
token does not outlive the boot. If the box dies first, the token expires in
60 min and the sweeper terminates it.

IAM this needs (AWS-side, not in this repo):

- launcher role (`FMKV_RUNNER_ROLE`): `ssm:PutParameter` (+ `kms:Encrypt`).
- instance role (in the launch template): `ssm:GetParameter`,
  `ssm:DeleteParameter`, `kms:Decrypt` — scoped to `/freemkv-ci/runner-reg/*`.

Only the parameter *name* travels through IMDS tags, so
`InstanceMetadataTags=enabled` is still required.

## Teardown

Three independent mechanisms, because each fails alone:

1. `--ephemeral` — GitHub de-registers the runner after exactly one job.
2. `shutdown` + `InstanceInitiatedShutdownBehavior=terminate` — the instance
   deletes itself, and the EBS volume goes with it.
3. `ci-runner-sweeper.yml` — hourly, from OUTSIDE, kills anything tagged
   `freemkv-ci=runner` older than 5h. The only one that survives user-data
   dying before it arms the other two.

Both platforms were observed completing the full cycle: register, take one job,
`Removed .runner`, shut down, instance terminated.
