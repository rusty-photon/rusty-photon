# Provisions the Windows runner template with the MSI packaging toolchain, so
# a pool clone can run the msi job (`cargo ci build-msi` + `verify-msi`) as
# well as the bazel job. Run once during a Windows template rebuild, after the
# base image and the C:\ci toolchain are in place. Idempotent.
#
# Everything is installed machine-scoped under C:\ci and located by MACHINE
# environment variables, matching how the rest of the template is provisioned
# (the runner has no .env on Windows; see docs/skills/proxmox-runner-pool.md).
$ProgressPreference = 'SilentlyContinue'
# Fail fast: a broken step (failed download, dotnet install, or wix install)
# must abort the rebuild rather than print its summary lines and exit clean -
# a half-provisioned toolchain silently baked into a template image is the
# worst outcome, since every pool job then inherits it.
$ErrorActionPreference = 'Stop'

$WixVersion = '5.0.2'   # must match WIX_VERSION in tools/rusty-photon-ci/src/build_msi.rs
$dotnetDir  = 'C:\ci\dotnet'
$toolsDir   = 'C:\ci\dotnet-tools'

# 1. .NET SDK. The wix CLI is a dotnet global tool; `cargo ci build-msi`
#    refuses to start without it (`dotnet not found`). Hosted windows-latest
#    ships the SDK preinstalled, which is why this is only needed on the pool.
if (-not (Test-Path "$dotnetDir\dotnet.exe")) {
  Write-Output "installing .NET SDK 8 -> $dotnetDir"
  Invoke-WebRequest -UseBasicParsing 'https://dot.net/v1/dotnet-install.ps1' -OutFile 'C:\ci\dotnet-install.ps1'
  # ErrorActionPreference = 'Stop' does NOT throw on a native/script non-zero
  # exit -- it only catches cmdlet errors -- so every external invocation here
  # checks $LASTEXITCODE explicitly, or a failed install would be baked into
  # the template silently.
  & 'C:\ci\dotnet-install.ps1' -Channel 8.0 -InstallDir $dotnetDir -NoPath
  if ($LASTEXITCODE -ne 0) { throw "dotnet-install.ps1 failed (exit $LASTEXITCODE)" }
}

# 2. DOTNET_ROOT is REQUIRED, not a convenience. wix.exe is a
#    framework-dependent apphost: it resolves the runtime through this variable
#    and never through PATH, so `dotnet --version` can succeed while every
#    global tool dies with "You must install .NET to run this application /
#    .NET location: Not found".
[Environment]::SetEnvironmentVariable('DOTNET_ROOT', $dotnetDir, 'Machine')
$env:DOTNET_ROOT = $dotnetDir

# 3. PATH: the SDK and a machine-wide tools dir.
$p = [Environment]::GetEnvironmentVariable('Path', 'Machine')
foreach ($add in $dotnetDir, $toolsDir) {
  if (($p -split ';') -notcontains $add) { $p = "$p;$add" }
}
[Environment]::SetEnvironmentVariable('Path', $p, 'Machine')
$env:Path = "$env:Path;$dotnetDir;$toolsDir"

# 4. wix at the pinned version, on a machine-wide tool path. --tool-path rather
#    than --global so the job's user sees it regardless of which profile runs
#    this. `cargo ci build-msi` skips its own wix install when a matching version is
#    already on PATH, keeping the SDK fetch off the job's critical path.
$have = ''
if (Get-Command wix -ErrorAction SilentlyContinue) { $have = "$(wix --version 2>$null)" }
if (-not $have.StartsWith($WixVersion)) {
  Write-Output "installing wix $WixVersion -> $toolsDir"
  & "$dotnetDir\dotnet.exe" tool install wix --version $WixVersion --tool-path $toolsDir
  if ($LASTEXITCODE -ne 0) { throw "dotnet tool install wix failed (exit $LASTEXITCODE)" }
}

Write-Output "dotnet     : $(& "$dotnetDir\dotnet.exe" --version 2>&1)"
Write-Output "DOTNET_ROOT: $([Environment]::GetEnvironmentVariable('DOTNET_ROOT','Machine'))"
Write-Output "wix        : $(& "$toolsDir\wix.exe" --version 2>&1)"
