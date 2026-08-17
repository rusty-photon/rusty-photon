# Runs exactly ONE GitHub Actions job on an ephemeral Windows pool clone, then
# powers the VM off. The Windows counterpart of one-job.sh in this directory.
#
# Installed in the Windows runner template at C:\actions-runner\one-job.ps1 and
# started by the `gha-runner` scheduled task, which MUST run as an interactive
# desktop session rather than as SYSTEM - see docs/skills/proxmox-runner-pool.md.
# The pool orchestrator on the hypervisor (tools/ci/rp-runner-pool.sh) injects
# .jitconfig through the QEMU guest agent.
#
# This file is the source of truth for what the template contains: rebuilding a
# template means copying this in, not editing the guest's copy in place.
$ErrorActionPreference = 'Continue'
$dir = 'C:\actions-runner'
$cfg = Join-Path $dir '.jitconfig'
Start-Transcript -Path 'C:\actions-runner\one-job.log' -Append

# A linked clone inherits the template's RTC, so the clock can be badly wrong
# at boot - wrong enough to break TLS to GitHub. Sync before anything talks to
# the network, and log the correction so a bad clock is visible in triage.
Write-Output "clock before resync: $(Get-Date -Format o)"
Start-Service w32time -ErrorAction SilentlyContinue
& w32tm /resync /force 2>&1 | ForEach-Object { Write-Output "  w32tm: $_" }
Write-Output "clock after resync:  $(Get-Date -Format o)"

# Every job runs as this account, and the account's %TEMP% is whatever the
# template captured - the debris of the BDD run that warmed the template
# included. Windows never empties a user temp on its own (Linux clones get the
# equivalent from tmpfiles' `D /tmp` rule at every boot), so a leftover rp
# session registry there, still marked active, sits at a path a job's test
# process can be handed again - same PID, same sequence number - and rp then
# restores a session the scenario never started. Empty the directory here, in
# the job account's own session and before the runner starts. Anything held
# open (Windows keeps a few handles in it at logon) is skipped, which is fine:
# no job depends on the contents, and Bazel re-extracts its own JNI/perf files
# on the next server start.
foreach ($t in @($env:TEMP, $env:TMP) | Select-Object -Unique) {
  if (-not $t -or -not (Test-Path $t)) { continue }
  $before = (Get-ChildItem $t -Force -ErrorAction SilentlyContinue | Measure-Object).Count
  Get-ChildItem $t -Force -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
  $after = (Get-ChildItem $t -Force -ErrorAction SilentlyContinue | Measure-Object).Count
  Write-Output "emptied ${t}: $before entries before, $after left"
}

# Monotonic, NOT a wall-clock deadline: the resync above (or any later
# correction) would otherwise jump straight past a computed end time.
#
# This is the guest's own backstop, not the primary defence: the orchestrator
# reclaims a slot whose runner never connects, and destroys a clone that has no
# injection marker. Both need the orchestrator to be running, which is exactly
# what this covers.
$sw = [Diagnostics.Stopwatch]::StartNew()
$waitMinutes = 30
while ($sw.Elapsed.TotalMinutes -lt $waitMinutes) {
  if ((Test-Path $cfg) -and (Get-Item $cfg).Length -gt 0) { break }
  Start-Sleep -Seconds 2
}
if (-not ((Test-Path $cfg) -and (Get-Item $cfg).Length -gt 0)) {
  Write-Output "no jitconfig within $waitMinutes minutes; powering off"
  Stop-Transcript
  Stop-Computer -Force
  return
}

$jit = (Get-Content $cfg -Raw).Trim()
# Single use: remove it before running, so a crash-restart of this task can
# never re-register with a spent config.
Remove-Item $cfg -Force
Set-Location $dir
Write-Output "starting one job at $(Get-Date -Format o)"
& "$dir\run.cmd" --jitconfig $jit
Write-Output "job finished (exit $LASTEXITCODE) at $(Get-Date -Format o); powering off"
Stop-Transcript
Stop-Computer -Force
