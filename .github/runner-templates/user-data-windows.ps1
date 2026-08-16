<powershell>
# Ephemeral freemkv CI runner (Windows). One job, then self-destruct.
# Same two mechanisms as Linux: --ephemeral de-registers, and
# `Stop-Computer` + InstanceInitiatedShutdownBehavior=terminate deletes.
Start-Transcript -Path C:\freemkv-runner.log -Append

# Hard deadline so a hung job cannot bill indefinitely.
Start-Job { Start-Sleep -Seconds 14400; Stop-Computer -Force }

$ErrorActionPreference = "Stop"
$tok  = Invoke-RestMethod -Method PUT -Uri "http://169.254.169.254/latest/api/token" -Headers @{"X-aws-ec2-metadata-token-ttl-seconds"="300"}
$iid  = Invoke-RestMethod -Uri "http://169.254.169.254/latest/meta-data/instance-id" -Headers @{"X-aws-ec2-metadata-token"=$tok}

# Read the tags through IMDS, not `aws ec2 describe-tags`. The Linux sibling
# died on `aws: command not found` before it could install anything, and while
# the Windows AMI does ship the CLI, going through IMDS removes the dependency,
# an IAM call, and the ordering trap in one move. Requires
# InstanceMetadataTags=enabled on the launch template.
$hdr  = @{"X-aws-ec2-metadata-token"=$tok}
$reg  = Invoke-RestMethod -Uri "http://169.254.169.254/latest/meta-data/tags/instance/runner-token" -Headers $hdr
$repo = Invoke-RestMethod -Uri "http://169.254.169.254/latest/meta-data/tags/instance/runner-repo"  -Headers $hdr
if (-not $reg -or -not $repo) { Write-Error "tags not visible via IMDS"; Stop-Computer -Force }

# Toolchain: rustup + ffmpeg (the suite shells out to ffprobe).
Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile C:\rustup-init.exe
C:\rustup-init.exe -y --default-toolchain 1.97 | Out-Null
# choco is NOT on the base Windows Server AMI, so install it first.
if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
  Set-ExecutionPolicy Bypass -Scope Process -Force
  [System.Net.ServicePointManager]::SecurityProtocol = 3072
  iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
  $env:PATH += ";$env:ProgramData\chocolatey\bin"
}
# git AND ffmpeg. `git` is not optional: without it there is no bash, so every
# `shell: bash` step fails with `bash: command not found`, and actions/checkout
# has nothing to clone with. Hosted windows runners ship Git for Windows, so
# omitting it here made this runner quietly unlike the one the workflows were
# written against — which is exactly how the first Windows smoke run failed.
# awscli too: the job pulls fixtures from S3. The base AMI ships a CLI, but not
# on a PATH Git Bash can see, and every `shell: bash` step in the media job
# calls `aws s3 cp` — which failed with `aws: command not found` even though
# the binary existed on the box. Installing through choco puts it somewhere
# both shells agree on.
choco install -y git ffmpeg awscli --no-progress | Out-Null
# Re-read PATH from the MACHINE environment after the installs, rather than
# appending to the copy this process started with. choco writes the machine
# PATH itself, and that write is invisible to an already-running process — so
# appending to $env:PATH here produced a PATH that still lacked awscli, the
# runner inherited it, and every `aws` call died with `command not found` even
# though the binary was installed. Refreshing, then re-persisting, is what
# makes the runner see what choco actually did.
$machine = [Environment]::GetEnvironmentVariable("PATH","Machine")
$user    = [Environment]::GetEnvironmentVariable("PATH","User")
$env:PATH = "$machine;$user;$env:ProgramFiles\Git\bin;$env:ProgramFiles\Git\usr\bin;$env:ProgramFiles\Amazon\AWSCLIV2"
[Environment]::SetEnvironmentVariable("PATH", $env:PATH, "Machine")
# Fail loudly here rather than 20 minutes later inside a rip.
foreach ($t in @("git","ffprobe","aws","cargo")) {
  if (-not (Get-Command $t -ErrorAction SilentlyContinue)) {
    Write-Error "FATAL: $t missing from PATH after install"; Stop-Computer -Force
  }
}

$rv = (Invoke-RestMethod -Uri "https://api.github.com/repos/actions/runner/releases/latest").tag_name.TrimStart("v")
New-Item -ItemType Directory -Force -Path C:\actions-runner | Out-Null
Set-Location C:\actions-runner
Invoke-WebRequest -Uri "https://github.com/actions/runner/releases/download/v$rv/actions-runner-win-x64-$rv.zip" -OutFile r.zip
Expand-Archive -Path r.zip -DestinationPath . -Force; Remove-Item r.zip

.\config.cmd --url "https://github.com/$repo" --token $reg --name "ephemeral-win-$iid" --labels freemkv-media,windows --unattended --ephemeral
.\run.cmd

Stop-Computer -Force
</powershell>
