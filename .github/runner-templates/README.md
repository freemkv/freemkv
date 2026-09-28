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

## Run-scoped labels, and user-data from the commit under test

qa.yml's `launch (<os>)` job passes **this directory's user-data at the commit
being tested** to `run-instances --user-data`, overriding the launch template's
copy, and pins the template version `plan-media` fingerprinted. It tags the
instance `runner-labels=freemkv-media,<os>,run-<run_id>`; the user-data reads
that tag from IMDS and registers with exactly those labels, as
`ephemeral-<os>-<instance-id>`. The leg's `runs-on` includes `run-<run_id>`, so
only the instance its own run launched can take the job — a stray runner (the
1.6.5 incident) or another run's instance cannot. A user-data change therefore
takes effect on the next qa run without touching AWS; applying it to the
templates as well only matters for `ci-runner-launch.yml`.

## qa launches: nothing to set up in AWS

Everything qa.yml's `launch (<leg>)` job passes to `run-instances` comes from
`tests/media-gate-policy.json` through `media_gate.py launch-spec`, so no
template or IAM change is ever needed for it:

- **Perf legs** (`linux-perf`, `windows-perf`, once `perf.enabled`) launch from
  the same two functional templates. Their differences are run-instances
  overrides: `--instance-type` `perf.instance_type` (no type fallback),
  `--block-device-mappings` with the root volume resized to
  `perf.root_volume_gib` (every other volume setting copied from the pinned
  template version), and the `freemkv-media-perf` label, with which the
  user-data registers as `ephemeral-<os>-perf-<instance-id>`.
- **The Spot price cap** is `launch.<os>.spot_max_price`, passed as
  `--instance-market-options` on every Spot attempt, so it no longer depends
  on (and overrides) whatever the template says.
- **The registration token** goes to one SecureString per branch and leg,
  `/freemkv-ci/runner-reg/<ref>-<leg>`, `--overwrite` on every launch. The
  launcher never deletes it (its role has no `ssm:DeleteParameter`): a token
  the instance never read expires 60 minutes after it was minted and the next
  launch of that leg overwrites it, so the set of parameters is fixed and
  nothing piles up. The instance still deletes it right after reading, which
  its own role already allows. Two launches of the same leg on the same
  branch within one boot could hand the second instance an already-deleted
  parameter; on qa the superseded check lets only the tip candidate launch,
  and if it ever happens the leg misses its pickup deadline and the run
  cancels itself (a red, re-runnable run, never a stray runner).
  `ci-runner-launch.yml` keeps its own per-run names.
