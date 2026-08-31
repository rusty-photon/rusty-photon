# verify-msi.ps1 - lifecycle-verify the built suite MSI on a Windows box
# (windows-latest CI or a dev VM; requires elevation, which both provide).
# The Windows analogue of scripts/verify-packages.sh: silent full install ->
# per-service class checks -> failure-actions proofs -> feature remove -> full
# uninstall (configs and logs survive, deb-`remove` parity).
#
# Service classes mirror verify-packages.sh:
#   - active:  reach RUNNING + HTTP probe (network-only services; the zwo
#     services serve with zero devices attached, phd2-guider answers 503
#     without PHD2, so all of them run on a hardware-less box)
#   - serial:  eager hardware validation exits when no device is present; the
#     contract is config self-created + "eager startup handshake" in the log +
#     SCM restarting the service (NOT "running")
#   - gated:   demand-start (no defaultable config): installed, Manual, stopped
#   - qhy-camera: without QHY's All-in-One pack the delay-load preflight must
#     log its distinctive pointer and exit cleanly (not a loader crash)
#
# Usage: scripts\verify-msi.ps1 [-Msi <path>] [-Keep] [-UpgradeFrom <path>]
#   -Msi          the MSI to verify (default: dist\<workspace version>\...)
#   -Keep         leave the product installed on exit (debugging)
#   -UpgradeFrom  a previously published MSI to install FIRST, so the main
#                 install runs as an in-place upgrade over it. The nightly
#                 channel's AllowSameVersionUpgrades path (every nightly
#                 authors the same compared ProductVersion) is exercised
#                 only this way - release-tag testing never sees it. After
#                 the upgrade, the SHIPPED doctor binary runs --fix - the
#                 documented operator migration path: a config key the
#                 upgraded services retired is deleted (this is what absorbs
#                 pre-1.0 schema churn with no hand-written test shims), and
#                 TLS plus the observatory credential are provisioned. The
#                 lifecycle asserts then run against that converged install:
#                 a service whose config gained server.tls/server.auth is
#                 probed over HTTPS with the observatory credential.
#                 Requires PowerShell 7 (the HTTPS probes use
#                 -SkipCertificateCheck); fresh-install mode stays Windows
#                 PowerShell 5.1-clean.

[CmdletBinding()]
param(
    [string]$Msi,
    [switch]$Keep,
    [string]$UpgradeFrom
)

$ErrorActionPreference = 'Stop'

function Die([string]$msg) {
    Write-Error "verify-msi: $msg"
    exit 1
}

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Die "must run elevated (msiexec /qn and service control need it)"
}
if (-not [Environment]::Is64BitProcess) {
    # Under 32-bit PowerShell, WOW64 redirection points the registry
    # provider (the ARP checks) and %ProgramFiles% (the install-dir
    # checks) at the 32-bit views - wrong for this x64-only product
    # (ADR-015).
    Die "must run from 64-bit PowerShell"
}
if ($UpgradeFrom -and $PSVersionTable.PSVersion.Major -lt 6) {
    # The post-fix HTTPS probes use -SkipCertificateCheck, which Windows
    # PowerShell 5.1 does not have; fresh-install mode keeps working there.
    Die "-UpgradeFrom requires PowerShell 7 (pwsh)"
}
if (-not (Test-Path "installer\Package.wxs")) { Die "run from the repo root" }

if (-not $Msi) {
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"$' |
        Select-Object -First 1).Matches[0].Groups[1].Value
    $Msi = "dist\$version\rusty-photon-$version-x64.msi"
}
if (-not (Test-Path $Msi)) { Die "$Msi not found - run scripts\build-msi.ps1 first" }
$Msi = (Resolve-Path $Msi).Path

# ---- service classification (mirrors verify-packages.sh) ------------------
$ports = @{
    'filemonitor' = 11111; 'ppba-driver' = 11112; 'qhy-focuser' = 11113
    'sentinel' = 11114; 'rp' = 11115; 'sky-survey-camera' = 11116
    'star-adventurer-gti' = 11117; 'pa-falcon-rotator' = 11118
    'dsd-fp2' = 11119; 'ui-htmx' = 11120; 'qhy-camera' = 11121
    'zwo-camera' = 11122; 'pa-scops-oag' = 11123; 'zwo-focuser' = 11124
    'planetarium-bridge' = 11126
    'phd2-guider' = 11130; 'plate-solver' = 11131; 'calibrator-flats' = 11170
    'session-runner' = 11171; 'polar-align' = 11172
}
$allServices = $ports.Keys | Sort-Object
# session-runner is gated like the Linux-gated three: its workflows_dir/
# state_dir are required config fields with no usable defaults.
$gated = @('sky-survey-camera', 'plate-solver', 'calibrator-flats', 'session-runner',
    'polar-align')
$serial = @('ppba-driver', 'qhy-focuser', 'pa-falcon-rotator', 'pa-scops-oag',
    'dsd-fp2', 'star-adventurer-gti')
$active = @('sentinel', 'ui-htmx', 'filemonitor', 'rp',
    'phd2-guider', 'zwo-camera', 'zwo-focuser', 'planetarium-bridge')
# Plain-HTTP services expose /health; Alpaca services answer the management
# API. The cameras, zwo-focuser, phd2-guider and session-runner never
# self-create a config (SDK-derived identity / built-in defaults); ui-htmx
# self-creates its default (the required rp target - no drivers map, #569).
$healthProbe = @('sentinel', 'rp', 'ui-htmx', 'phd2-guider')
$selfCreatesConfig = @('sentinel', 'rp', 'filemonitor', 'ui-htmx', 'planetarium-bridge') + $serial

$dataDir = Join-Path $env:ProgramData 'rusty-photon'
$logsDir = Join-Path $dataDir 'logs'
$installDir = Join-Path ${env:ProgramFiles} 'rusty-photon'
$installLog = Join-Path $env:TEMP 'rusty-photon-msi-install.log'
$uninstallLog = Join-Path $env:TEMP 'rusty-photon-msi-uninstall.log'

# Fresh-box preflight: the run asserts fresh-install invariants (gated
# services have no config, ui-htmx self-creates its rp-target default,
# configs self-create), which leftovers from a prior install would corrupt -
# fail fast with a pointer instead of failing (or passing) for the wrong
# reason mid-run. CI runners are always fresh; on a dev box, uninstall and
# delete %ProgramData%\rusty-photon (the documented manual purge) first.
if (Get-Service -Name 'rusty-photon-*' -ErrorAction SilentlyContinue) {
    Die "rusty-photon-* services already installed - msiexec /x the previous install first"
}
if (Test-Path $dataDir) {
    Die "$dataDir already exists - delete it (manual purge) so fresh-install checks are meaningful"
}

function Fail([string]$svc, [string]$msg) {
    Write-Host "verify-msi: FAIL [$svc]: $msg" -ForegroundColor Red
    $svcLog = Get-ChildItem -Path $logsDir -Filter "$svc.*" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if ($svcLog) {
        Write-Host "--- last 40 lines of $($svcLog.Name) ---"
        Get-Content $svcLog.FullName -Tail 40
    }
    foreach ($msiLog in @($uninstallLog, $installLog)) {
        if (-not (Test-Path $msiLog)) { continue }
        # The verbose log is huge; the failure signal is the action(s) that
        # ended with "Return value 3" plus any Error-coded lines.
        Write-Host "--- msiexec log ($(Split-Path $msiLog -Leaf)): failed actions + error lines ---"
        Select-String -Path $msiLog -Pattern 'Return value 3|^Error \d+|error 1\d{3}|CustomAction .+ returned actual error|failed to start|could not be|MainEngineThread is returning' |
            Select-Object -Last 30 | ForEach-Object { Write-Host $_.Line }
    }
    exit 1
}

# Poll $probe every second until it returns truthy or $timeoutSec elapses.
function WaitFor([string]$svc, [string]$what, [scriptblock]$probe, [int]$timeoutSec = 60) {
    for ($i = 0; $i -lt $timeoutSec; $i++) {
        if (& $probe) { return }
        Start-Sleep -Seconds 1
    }
    Fail $svc "timed out after ${timeoutSec}s waiting for: $what"
}

function ServiceLogContent([string]$svc) {
    # Newest daily file only, tail-bounded: crash-looping services append
    # every 5 s while WaitFor polls every second, so unbounded -Raw reads of
    # every file would grow quadratically over a verification run.
    $f = Get-ChildItem -Path $logsDir -Filter "$svc.*" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $f) { return "" }
    (Get-Content $f.FullName -Tail 500) -join "`n"
}

function Msiexec([string[]]$msiArgs) {
    $p = Start-Process -FilePath msiexec.exe -ArgumentList $msiArgs -Wait -PassThru
    return $p.ExitCode
}

# The product's Programs & Features registrations (x64 MSI -> native hive).
function ArpEntries {
    Get-ChildItem 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall' |
        ForEach-Object { Get-ItemProperty $_.PSPath } |
        Where-Object { $_.DisplayName -eq 'Rusty Photon' }
}

# ---- optional upgrade seed (nightly-over-nightly proof) --------------------
if ($UpgradeFrom) {
    if (-not (Test-Path $UpgradeFrom)) { Die "-UpgradeFrom $UpgradeFrom not found" }
    $UpgradeFrom = (Resolve-Path $UpgradeFrom).Path
    $priorLog = Join-Path $env:TEMP 'rusty-photon-msi-prior-install.log'
    Write-Host "== upgrade seed: installing the prior MSI ($(Split-Path -Leaf $UpgradeFrom))"
    $code = Msiexec @('/i', "`"$UpgradeFrom`"", '/qn', '/norestart', "/l*v", "`"$priorLog`"", 'ADDLOCAL=ALL')
    if ($code -ne 0) {
        # Fail's msiexec-log excerpt must come from the install that
        # actually failed (the script exits inside Fail; the main install
        # never runs, so repointing is safe).
        $installLog = $priorLog
        Fail 'msiexec' "prior-MSI install exited $code (log: $priorLog)"
    }
    if (-not (Get-Service -Name 'rusty-photon-sentinel' -ErrorAction SilentlyContinue)) {
        $installLog = $priorLog
        Fail 'msiexec' "prior MSI installed no services - the upgrade proof would be vacuous"
    }
    # The seed must leave configs behind in the PRIOR schema - that is the
    # half of the proof doctor absorbs - so wait for the anchor service's
    # self-created config before tearing the seeded install down.
    WaitFor 'sentinel' "the prior install's self-created sentinel.json" {
        Test-Path (Join-Path $dataDir 'sentinel.json')
    } 30
    # The seeded services have been running and logging, and the lifecycle
    # asserts below grep those same daily log files - pre-upgrade output
    # could satisfy them vacuously. Disable first (a failure-actions
    # restart already scheduled by a crash-looping serial driver cannot
    # start a disabled service), stop everything, and clear the logs, so
    # every log-based check reflects the upgraded install only. Configs
    # stay - surviving the upgrade is part of the contract - and the
    # upgrade reinstalls the services fresh, so the disabling cannot leak
    # into the new product. Deliberately NO config migration here: schema
    # churn between the seeded and the new install is the shipped doctor's
    # job to absorb, below.
    foreach ($svc in (Get-Service -Name 'rusty-photon-*')) {
        sc.exe config $svc.Name start= disabled | Out-Null
    }
    Get-Service -Name 'rusty-photon-*' | Where-Object { $_.Status -ne 'Stopped' } |
        Stop-Service -Force -ErrorAction SilentlyContinue
    WaitFor 'msiexec' "all seeded services stopped" {
        -not (Get-Service -Name 'rusty-photon-*' | Where-Object { $_.Status -ne 'Stopped' })
    } 60
    Remove-Item -Recurse -Force $logsDir -ErrorAction SilentlyContinue
    Write-Host "== upgrade seed: prior services stopped + logs cleared (asserts now reflect the upgraded install)"
}

# ---- install (all features) ----------------------------------------------
Write-Host "== install: msiexec /qn ADDLOCAL=ALL"
$code = Msiexec @('/i', "`"$Msi`"", '/qn', '/norestart', "/l*v", "`"$installLog`"", 'ADDLOCAL=ALL')
if ($code -ne 0) { Fail 'msiexec' "silent install exited $code (log: $installLog)" }

if ($UpgradeFrom) {
    # The install above ran over the seeded product: prove it upgraded in
    # place (RemoveExistingProducts consumed the old registration) rather
    # than installing side by side - the failure mode
    # AllowSameVersionUpgrades exists to prevent.
    $entries = @(ArpEntries)
    if ($entries.Count -ne 1) {
        Fail 'msiexec' "expected exactly one Rusty Photon ARP entry after the upgrade, found $($entries.Count) (side-by-side install?)"
    }
    # ARPCOMMENTS carries the full version string, and the MSI under test
    # always authors it; the filename is rusty-photon-<fullversion>-x64.msi,
    # so this pins the surviving entry to the MSI just installed. A filename
    # that cannot be parsed fails outright - silently skipping the pin would
    # leave "the surviving entry is the OLD product" undetected.
    if ((Split-Path -Leaf $Msi) -notmatch '^rusty-photon-(.+)-x64\.msi$') {
        Fail 'msiexec' "cannot pin the surviving ARP entry: '$(Split-Path -Leaf $Msi)' is not named rusty-photon-<version>-x64.msi"
    }
    $expected = "rusty-photon $($Matches[1])"
    if ($entries[0].Comments -ne $expected) {
        Fail 'msiexec' "surviving ARP entry comments '$($entries[0].Comments)' != '$expected' (old product survived the upgrade?)"
    }
    Write-Host "== upgrade: OK (single ARP entry after installing over the prior MSI)"

    # The operator's documented post-upgrade step (docs/packaging-windows.md
    # section First wiring), run with the SHIPPED binary: --fix deletes the
    # keys the upgraded services retired out of the seeded configs and
    # provisions TLS + the observatory credential into them.
    $doctorExe = Join-Path $installDir 'rusty-photon-doctor.exe'
    if (-not (Test-Path $doctorExe)) { Fail 'doctor' "rusty-photon-doctor.exe not installed" }
    Write-Host "== upgrade: running doctor --fix (the shipped binary - the operator migration path)"
    $fixJson = & $doctorExe --fix --json
    if ($LASTEXITCODE -gt 1) { Fail 'doctor' "doctor --fix exited $LASTEXITCODE" }
    try {
        $fixReport = ($fixJson -join "`n") | ConvertFrom-Json
    } catch {
        Fail 'doctor' "doctor --fix --json emitted unparseable output:`n$($fixJson -join "`n")"
    }
    # fixes_applied is omitted from the JSON when empty, and @($null) has
    # Count 1 - filter the null so a no-fix run reports 0.
    $appliedCount = @($fixReport.fixes_applied | Where-Object { $null -ne $_ }).Count
    Write-Host "== upgrade: doctor --fix applied $appliedCount fix(es)"
    # The sharp churn assertion: a config key the upgraded services retired
    # must be GONE from the post-fix diagnosis. A failure here means a
    # breaking config change shipped without its doctor remedy - a product
    # gap, exactly what this proof exists to redden on.
    $retired = @($fixReport.checks | Where-Object {
            $_.name -eq 'config.retired-keys' -and $_.status -eq 'fail'
        })
    if ($retired.Count -gt 0) {
        $details = ($retired | ForEach-Object { "$($_.service): $($_.detail)" }) -join "`n"
        Fail 'doctor' "config.retired-keys still failing after --fix (breaking config change without a doctor remedy?):`n$details"
    }
    # The provisioning half must have landed before the probes rely on it.
    foreach ($pkiFile in @('pki\ca.pem', 'pki\credential')) {
        if (-not (Test-Path (Join-Path $dataDir $pkiFile))) {
            Fail 'doctor' "--fix provisioning did not create $pkiFile"
        }
    }
    $observatoryCredential = (Get-Content (Join-Path $dataDir 'pki\credential') -Raw).Trim()
    # Fixes take effect on each service's next restart (the documented
    # operator step) - restart the probed services so the lifecycle asserts
    # below run against the converged TLS-on, auth-on install. A restart
    # failure is not diagnosed here: the RUNNING wait below fails with the
    # service's own log excerpt, which is the better forensics.
    foreach ($svc in $active) {
        try { Restart-Service -Name "rusty-photon-$svc" -Force -ErrorAction Stop }
        catch { Start-Service -Name "rusty-photon-$svc" -ErrorAction SilentlyContinue }
    }
    Write-Host "== upgrade: OK (doctor --fix converged the install; active services restarted)"
}

# ---- static asserts: services, start types, failure actions ---------------
foreach ($svc in $allServices) {
    $name = "rusty-photon-$svc"
    $s = Get-Service -Name $name -ErrorAction SilentlyContinue
    if (-not $s) { Fail $svc "service $name not installed" }

    $expectedStart = if ($gated -contains $svc) { 'Manual' } else { 'Automatic' }
    if ($s.StartType -ne $expectedStart) {
        Fail $svc "StartType is $($s.StartType), expected $expectedStart"
    }

    # Failure actions: restart after 5000 ms, and the failure-actions-on-
    # non-crash-failures flag MUST be set or the ServiceSpecific(1) exits of
    # the serial drivers would never trigger a restart (ADR-015 / W1).
    $qf = sc.exe qfailure $name
    if ($LASTEXITCODE -ne 0) { Fail $svc "sc qfailure failed" }
    if (-not (($qf | Out-String) -match 'RESTART -- Delay = 5000')) {
        Fail $svc "failure actions do not restart after 5000 ms:`n$($qf | Out-String)"
    }
    $qff = sc.exe qfailureflag $name
    if ($LASTEXITCODE -ne 0) { Fail $svc "sc qfailureflag failed" }
    if (-not (($qff | Out-String) -match 'TRUE')) {
        Fail $svc "SERVICE_CONFIG_FAILURE_ACTIONS_FLAG not set:`n$($qff | Out-String)"
    }
}
Write-Host "== static: $($allServices.Count) services installed, start types + failure actions OK"

# ---- gated trio: installed but never started, no config -------------------
foreach ($svc in $gated) {
    $s = Get-Service -Name "rusty-photon-$svc"
    if ($s.Status -ne 'Stopped') { Fail $svc "gated service is $($s.Status), expected Stopped" }
    if (Test-Path (Join-Path $dataDir "$svc.json")) {
        Fail $svc "config exists on a fresh install of a gated service"
    }
    Write-Host "== ${svc}: OK (gated: Manual + stopped, no config)"
}

# ---- ui-htmx self-created config (no seed action since #569) ---------------
# The config self-creates on first service start (it is in $selfCreatesConfig,
# so the active-class loop below waits for it); here assert its shape: the
# required rp target, and no retired drivers key.
$uiCfgPath = Join-Path $dataDir 'ui-htmx.json'

# ---- active class: RUNNING + config + probe --------------------------------
foreach ($svc in $active) {
    $name = "rusty-photon-$svc"
    WaitFor $svc "service RUNNING" { (Get-Service -Name $name).Status -eq 'Running' }

    if ($selfCreatesConfig -contains $svc) {
        $cfg = Join-Path $dataDir "$svc.json"
        WaitFor $svc "config self-created at $cfg" { Test-Path $cfg } 30
    }

    # After the self-create wait, so a fresh install can never race past the
    # shape assertions before ui-htmx has written its file.
    if ($svc -eq 'ui-htmx') {
        $uiCfg = Get-Content $uiCfgPath -Raw | ConvertFrom-Json
        if ($uiCfg.PSObject.Properties['drivers']) {
            Fail 'ui-htmx' "self-created config carries the retired drivers key"
        }
        if (-not $uiCfg.PSObject.Properties['rp']) {
            Fail 'ui-htmx' "self-created config has no rp target"
        }
    }

    $port = $ports[$svc]
    $path = if ($healthProbe -contains $svc) { '/health' } else { '/management/apiversions' }
    # After an upgrade + doctor --fix, a service with a config file serves
    # what --fix provisioned into it. Read the scheme and auth posture off
    # that config - the same source the service reads - rather than
    # predicting from the environment: a service with no config file
    # (cameras, phd2-guider) stays plain HTTP, absent-means-off. The null
    # checks cover every unprovisioned shape: no server block, or tls/auth
    # absent or explicitly null.
    $scheme = 'http'
    $probeArgs = @{ UseBasicParsing = $true; TimeoutSec = 5 }
    if ($UpgradeFrom -and (Test-Path (Join-Path $dataDir "$svc.json"))) {
        $svcCfg = Get-Content (Join-Path $dataDir "$svc.json") -Raw | ConvertFrom-Json
        if ($null -ne $svcCfg.server.tls) {
            # Self-signed material from the just-created CA: the handshake
            # completing at all is the proof the issued pair loads and
            # serves, so no trust-store import is needed.
            $scheme = 'https'
            $probeArgs.SkipCertificateCheck = $true
        }
        if ($null -ne $svcCfg.server.auth) {
            $basic = [Convert]::ToBase64String(
                [Text.Encoding]::UTF8.GetBytes("observatory:$observatoryCredential"))
            $probeArgs.Headers = @{ Authorization = "Basic $basic" }
        }
    }
    WaitFor $svc "HTTP response on ${scheme}://127.0.0.1:$port$path" {
        try {
            Invoke-WebRequest @probeArgs -Uri "${scheme}://127.0.0.1:$port$path" | Out-Null
            $true
        } catch {
            # No PHD2 on a verify box: phd2-guider's /health legitimately
            # answers 503 (listener up, guider not connected). Non-HTTP
            # failures (connection refused while the service is coming up)
            # carry no Response - treat those as "not yet" and keep polling.
            $resp = $_.Exception.PSObject.Properties['Response']
            $status = if ($resp -and $resp.Value) { [int]$resp.Value.StatusCode } else { 0 }
            $svc -eq 'phd2-guider' -and $status -eq 503
        }
    }
    Write-Host "== ${svc}: OK (running, ${scheme} port $port$path)"
}

# ---- serial class: config + handshake attempts + SCM restart proof ---------
foreach ($svc in $serial) {
    $cfg = Join-Path $dataDir "$svc.json"
    WaitFor $svc "config self-created at $cfg" { Test-Path $cfg } 30
    WaitFor $svc "'eager startup handshake' in the service log" {
        (ServiceLogContent $svc) -match 'eager startup handshake'
    } 30
    Write-Host "== ${svc}: OK (config self-created; retrying on absent serial device)"
}

# Behavioral proof of the failure-actions flag: an eager-exit stop is a
# ServiceSpecific(1) NON-CRASH failure, so a second handshake attempt can only
# happen if SCM counted the first exit as a failure and restarted the service.
$flagProbe = $serial[0]
WaitFor $flagProbe "a second handshake attempt (SCM restart-on-error proof)" {
    ([regex]::Matches((ServiceLogContent $flagProbe), 'eager startup handshake')).Count -ge 2
} 90
Write-Host "== ${flagProbe}: OK (restarted after a clean error exit - failure-actions flag works)"

# ---- qhy-camera: delay-load preflight ---------------------------------------
# The preflight has exactly two correct outcomes, and this check's job is to
# prove the loader did NOT crash - not to predict which outcome this box should
# produce:
#
#   * Where qhyccd.dll cannot be resolved (a plain verify box, no All-in-One
#     pack), the delay-load must REPORT the missing DLL rather than dying in
#     the loader.
#   * Where it resolves (e.g. the Proxmox pool template stages the SDK so the
#     Bazel suites can link and run against it), the service enumerates zero
#     cameras and starts cleanly, never emitting the missing-DLL line.
#
# Accept either; fail only if NEITHER line appears within the timeout, which is
# the loader crash this check exists to catch. Deliberately not decided from
# the environment: the service resolves the DLL through the full Windows search
# order (executable dir, system dirs, then PATH), so a check that inspected any
# single one of those - e.g. PATH alone - could disagree with the very service
# it is verifying.
# Capture the log content that satisfies the wait and branch on THAT snapshot,
# rather than re-reading afterwards: a second read could see later writes or a
# rotated tail and report a different outcome than the one that actually passed.
# The probe runs in a child scope (WaitFor's `& $probe`), so the capture must
# be script-scoped to survive back to the branch below; every reference here is
# spelled `$script:qhyLog` so the single-variable intent is unambiguous.
$script:qhyLog = ''
WaitFor 'qhy-camera' "the delay-load to resolve either way (started, or reported the missing DLL - not a loader crash)" {
    $script:qhyLog = ServiceLogContent 'qhy-camera'
    $script:qhyLog -match 'Service started successfully|qhyccd\.dll not found'
} 30
if ($script:qhyLog -match 'qhyccd\.dll not found') {
    Write-Host "== qhy-camera: OK (preflight reported the missing DLL - no loader crash)"
} else {
    Write-Host "== qhy-camera: OK (delay-load resolved - service started)"
}

# ---- log files for everything that ran -------------------------------------
foreach ($svc in ($active + $serial + @('qhy-camera'))) {
    if (-not (Get-ChildItem -Path $logsDir -Filter "$svc.*" -ErrorAction SilentlyContinue)) {
        Fail $svc "no rolling log file under $logsDir"
    }
}
Write-Host "== logs: OK (rolling files present for every started service)"

# ---- kill-and-observe: crash restart (sentinel) -----------------------------
$sentinelPid = (Get-CimInstance Win32_Service -Filter "Name='rusty-photon-sentinel'").ProcessId
if (-not $sentinelPid) { Fail 'sentinel' "no PID for the running service" }
Write-Host "== sentinel: killing PID $sentinelPid to observe the SCM restart"
Stop-Process -Id $sentinelPid -Force
WaitFor 'sentinel' "SCM restart after kill (new PID, RUNNING)" {
    $s = Get-CimInstance Win32_Service -Filter "Name='rusty-photon-sentinel'"
    $s.State -eq 'Running' -and $s.ProcessId -ne $sentinelPid -and $s.ProcessId -ne 0
} 60
Write-Host "== sentinel: OK (SCM restarted it after a hard kill)"

# ---- reload smoke: SCM ParamChange -> ReloadSignal --------------------------
sc.exe control rusty-photon-filemonitor paramchange | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 'filemonitor' "sc control paramchange failed ($LASTEXITCODE)" }
Write-Host "== filemonitor: OK (accepted ParamChange)"

# ---- doctor: shipped binary + renewal scheduled task ------------------------
# Doctor rides in Core with sentinel (no rusty-photon-doctor package). A
# diagnosis may legitimately find problems on the verify box (exit 1);
# exit 2 would mean the binary crashed. `tls renew` with nothing staged is
# the scheduled task's steady state and must exit 0.
$doctorExe = Join-Path $installDir 'rusty-photon-doctor.exe'
if (-not (Test-Path $doctorExe)) { Fail 'doctor' "rusty-photon-doctor.exe not installed" }
& $doctorExe --json | Out-Null
if ($LASTEXITCODE -gt 1) { Fail 'doctor' "rusty-photon-doctor --json exited $LASTEXITCODE" }
& $doctorExe tls renew | Out-Null
if ($LASTEXITCODE -ne 0) { Fail 'doctor' "tls renew (nothing due) exited $LASTEXITCODE" }
$null = cmd /c "schtasks /Query /TN rusty-photon-renew >nul 2>&1"
if ($LASTEXITCODE -ne 0) { Fail 'doctor' "scheduled task rusty-photon-renew not registered" }
Write-Host "== doctor: OK (binary runs; renewal task registered)"

# ---- feature remove: per-device split stays honest --------------------------
Write-Host "== modify: REMOVE=ZwoCamera"
$code = Msiexec @('/i', "`"$Msi`"", '/qn', '/norestart', 'REMOVE=ZwoCamera')
if ($code -ne 0) { Fail 'zwo-camera' "feature remove exited $code" }
if (Get-Service -Name 'rusty-photon-zwo-camera' -ErrorAction SilentlyContinue) {
    Fail 'zwo-camera' "service still installed after feature remove"
}
if (Test-Path (Join-Path $installDir 'rusty-photon-zwo-camera.exe')) {
    Fail 'zwo-camera' "exe still present after feature remove"
}
if (Test-Path (Join-Path $installDir 'ASICamera2.dll')) {
    Fail 'zwo-camera' "ASICamera2.dll still present after feature remove"
}
# The focuser must be untouched: its own DLL and the shared license stay.
if ((Get-Service -Name 'rusty-photon-zwo-focuser').Status -ne 'Running') {
    Fail 'zwo-focuser' "not running after removing the zwo-camera feature"
}
if (-not (Test-Path (Join-Path $installDir 'EAF_focuser.dll'))) {
    Fail 'zwo-focuser' "EAF_focuser.dll disappeared with the zwo-camera feature"
}
if (-not (Test-Path (Join-Path $installDir 'ZWO-SDK-LICENSE.txt'))) {
    Fail 'zwo-focuser' "shared ZWO license disappeared while a zwo feature is installed"
}
Write-Host "== modify: OK (zwo-camera gone; zwo-focuser + its DLL + shared license intact)"

if ($Keep) {
    Write-Host "verify-msi: -Keep set; leaving the product installed"
    exit 0
}

# ---- full uninstall ---------------------------------------------------------
Write-Host "== uninstall: msiexec /qn /x"
$code = Msiexec @('/x', "`"$Msi`"", '/qn', '/norestart', "/l*v", "`"$uninstallLog`"")
if ($code -ne 0) { Fail 'msiexec' "uninstall exited $code (log: $uninstallLog)" }
foreach ($svc in $allServices) {
    if (Get-Service -Name "rusty-photon-$svc" -ErrorAction SilentlyContinue) {
        Fail $svc "service still installed after uninstall"
    }
}
if (Get-ChildItem -Path $installDir -Filter '*.exe' -ErrorAction SilentlyContinue) {
    Fail 'msiexec' "exes left under $installDir after uninstall"
}
$null = cmd /c "schtasks /Query /TN rusty-photon-renew >nul 2>&1"
if ($LASTEXITCODE -eq 0) {
    Fail 'doctor' "scheduled task rusty-photon-renew survived uninstall"
}
# deb `remove` parity: self-created configs and logs are untracked by the MSI
# and survive uninstall; purge is a documented manual step.
if (-not (Test-Path (Join-Path $dataDir 'sentinel.json'))) {
    Fail 'sentinel' "self-created config did not survive uninstall (must only go on manual purge)"
}
# Same contract for doctor-provisioned material: losing the CA or the
# observatory credential on uninstall would strand every distributed trust
# anchor and client credential.
if ($UpgradeFrom -and -not (Test-Path (Join-Path $dataDir 'pki\credential'))) {
    Fail 'doctor' "provisioned pki material did not survive uninstall (must only go on manual purge)"
}
if (-not (Get-ChildItem -Path $logsDir -ErrorAction SilentlyContinue)) {
    Fail 'msiexec' "log files did not survive uninstall"
}
Write-Host "== uninstall: OK (services + exes gone; configs and logs survive)"

Write-Host ""
Write-Host "verify-msi: OK ($($allServices.Count) services)" -ForegroundColor Green
# The runner shell appends `exit $LASTEXITCODE`; the last native command
# above is the uninstall-phase schtasks query, whose EXPECTED failure
# (the task is gone) must not become the step's exit code.
exit 0
