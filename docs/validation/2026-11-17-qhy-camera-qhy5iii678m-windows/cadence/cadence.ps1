# G1 cadence spike: back-to-back 2 s exposures through qhy-camera's production
# endpoint on loopback (HTTPS, Basic auth), timed from the client.
#
# Per frame: t_req just before PUT startexposure (Duration=2, Light=true),
# t_started after its answer; GET imageready every 25 ms until true (t_ready);
# GET imagearray as application/imagebytes, the whole body read (t_pixels).
# latency = t_pixels - (t_req + 2 s); the next frame's t_req follows at once.
#
# Runs, in order: A full frame binned 2 x 2 for 60 min (unbinned if the camera
# refuses 2 x 2, and then C is skipped); B a 512 x 512 subframe at the sensor's
# centre, unbinned, 10 min; C full frame unbinned, 10 min. One CSV row per
# frame, failures included; a summary per run; the host's CPU load once a
# minute. A file named STOP in the output directory ends the current run and
# the script cleanly, with the camera disconnected.
#
# The camera's credentials are read from rp.json's entry for this service
# (rp is the client that holds them) and never written anywhere.
param(
  [string]$Out = 'C:\Users\<rig-user>\Downloads\g1-cadence\cadence',
  [string]$Base = 'https://127.0.0.1:11121',
  [int]$Camera = 1,
  [int]$MinutesA = 60,
  [int]$MinutesB = 10,
  [int]$MinutesC = 10
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$inv = [Globalization.CultureInfo]::InvariantCulture
New-Item -ItemType Directory -Force $Out | Out-Null
$status = Join-Path $Out 'status.txt'
function Say([string]$m) { "$([DateTime]::UtcNow.ToString('o')) $m" | Add-Content $status }

Add-Type -AssemblyName System.Net.Http
Add-Type -ReferencedAssemblies System.Net.Http, System -TypeDefinition @'
using System;
using System.Net.Http;
using System.Net.Security;
using System.Runtime.InteropServices;
using System.Security.Cryptography.X509Certificates;
public static class G1Net {
    static X509Certificate2 ca;
    public static HttpClientHandler Handler(byte[] caDer) {
        ca = new X509Certificate2(caDer);
        HttpClientHandler h = new HttpClientHandler();
        h.ServerCertificateCustomValidationCallback = Validate;
        return h;
    }
    // Accept the server's certificate only if it chains to rusty-photon's CA
    // and carries no error but the CA being unknown to Windows.
    static bool Validate(HttpRequestMessage m, X509Certificate2 cert, X509Chain chain, SslPolicyErrors errors) {
        if (errors == SslPolicyErrors.None) return true;
        if ((errors & ~SslPolicyErrors.RemoteCertificateChainErrors) != SslPolicyErrors.None) return false;
        X509Chain c = new X509Chain();
        c.ChainPolicy.RevocationMode = X509RevocationMode.NoCheck;
        c.ChainPolicy.VerificationFlags = X509VerificationFlags.AllowUnknownCertificateAuthority;
        c.ChainPolicy.ExtraStore.Add(ca);
        if (!c.Build(cert)) return false;
        X509Certificate2 root = c.ChainElements[c.ChainElements.Count - 1].Certificate;
        return root.Thumbprint == ca.Thumbprint;
    }
    [DllImport("winmm.dll")] public static extern uint timeBeginPeriod(uint p);
    [DllImport("winmm.dll")] public static extern uint timeEndPeriod(uint p);
}
'@

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13
[Net.ServicePointManager]::Expect100Continue = $false
[Net.ServicePointManager]::UseNagleAlgorithm = $false
$null = [G1Net]::timeBeginPeriod(1)

$pem = Get-Content "$env:ProgramData\rusty-photon\pki\ca.pem" -Raw
$der = [Convert]::FromBase64String(($pem -replace '-----[A-Z ]+-----', '' -replace '\s', ''))
$client = New-Object System.Net.Http.HttpClient([G1Net]::Handler($der))
$client.Timeout = [TimeSpan]::FromSeconds(120)
$client.DefaultRequestHeaders.ExpectContinue = $false
$rp = Get-Content "$env:ProgramData\rusty-photon\rp.json" -Raw | ConvertFrom-Json
$cred = $rp.equipment.cameras | Where-Object { $_.alpaca_url -eq $Base } | Select-Object -First 1
if (-not $cred) { throw "rp.json has no camera entry for $Base" }
$basic = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes("$($cred.auth.username):$($cred.auth.password)"))
$client.DefaultRequestHeaders.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Basic', $basic)
$rp = $null; $cred = $null; $basic = $null

$freq = [Diagnostics.Stopwatch]::Frequency
function Now { [Diagnostics.Stopwatch]::GetTimestamp() }
function Ms([long]$a, [long]$b) { ($b - $a) * 1000.0 / $freq }
$script:tx = 0
$cam = "$Base/api/v1/camera/$Camera"

function Alpaca-Get([string]$member) {
  $script:tx++
  $r = $client.GetAsync("$cam/${member}?ClientID=92&ClientTransactionID=$($script:tx)").GetAwaiter().GetResult()
  $s = $r.Content.ReadAsStringAsync().GetAwaiter().GetResult()
  if (-not $r.IsSuccessStatusCode) { throw "GET $member HTTP $([int]$r.StatusCode): $s" }
  $j = $s | ConvertFrom-Json
  if ($j.ErrorNumber -ne 0) { throw "GET $member error $($j.ErrorNumber): $($j.ErrorMessage)" }
  $j.Value
}
function Alpaca-Put([string]$member, [string]$form) {
  $script:tx++
  $body = New-Object System.Net.Http.StringContent("$form&ClientID=92&ClientTransactionID=$($script:tx)", [Text.Encoding]::UTF8, 'application/x-www-form-urlencoded')
  $r = $client.PutAsync("$cam/$member", $body).GetAwaiter().GetResult()
  $s = $r.Content.ReadAsStringAsync().GetAwaiter().GetResult()
  if (-not $r.IsSuccessStatusCode) { throw "PUT $member HTTP $([int]$r.StatusCode): $s" }
  if ($s -notmatch '"ErrorNumber"\s*:\s*0\b') { $j = $s | ConvertFrom-Json; throw "PUT $member error $($j.ErrorNumber): $($j.ErrorMessage)" }
}
# imageready, answered quickly: $true/$false, or throws with the error text
function Image-Ready {
  $script:tx++
  $r = $client.GetAsync("$cam/imageready?ClientID=92&ClientTransactionID=$($script:tx)").GetAwaiter().GetResult()
  $s = $r.Content.ReadAsStringAsync().GetAwaiter().GetResult()
  if ($r.IsSuccessStatusCode -and $s -match '"ErrorNumber"\s*:\s*0\b') { return ($s -match '"Value"\s*:\s*true') }
  throw "GET imageready HTTP $([int]$r.StatusCode): $s"
}

$qhyProc = Get-Process -Name 'rusty-photon-qhy-camera' | Select-Object -First 1
$ncpu = [Environment]::ProcessorCount
$cpuTotal = New-Object System.Diagnostics.PerformanceCounter('Processor', '% Processor Time', '_Total')

function Csv([string]$s) { '"' + ($s -replace '"', '""' -replace "[`r`n]+", ' ') + '"' }
function F([double]$v) { $v.ToString('F3', $inv) }

function Run-Cadence([string]$name, [int]$bin, [int]$sx, [int]$sy, [int]$nx, [int]$ny, [int]$minutes) {
  Alpaca-Put 'binx' "BinX=$bin"; Alpaca-Put 'biny' "BinY=$bin"
  Alpaca-Put 'numx' "NumX=$nx"; Alpaca-Put 'numy' "NumY=$ny"
  Alpaca-Put 'startx' "StartX=$sx"; Alpaca-Put 'starty' "StartY=$sy"
  $rb = '{0}x{1} start {2},{3} size {4}x{5}' -f (Alpaca-Get 'binx'), (Alpaca-Get 'biny'), (Alpaca-Get 'startx'), (Alpaca-Get 'starty'), (Alpaca-Get 'numx'), (Alpaca-Get 'numy')
  Say "run $name begins: bin $rb, $minutes min"
  $csvPath = Join-Path $Out "run-$name.csv"
  $w = New-Object IO.StreamWriter($csvPath, $false, (New-Object Text.UTF8Encoding $false))
  $w.AutoFlush = $true
  $w.WriteLine('frame,wall_utc,bin,startx,starty,numx,numy,bytes,start_call_ms,ready_after_end_ms,download_ms,latency_ms,cycle_ms,error')
  $cw = New-Object IO.StreamWriter((Join-Path $Out "cpu-$name.csv"), $false, (New-Object Text.UTF8Encoding $false))
  $cw.AutoFlush = $true
  $cw.WriteLine('wall_utc,host_cpu_pct,qhy_camera_cpu_pct_of_host,qhy_camera_working_set_mb')
  $null = $cpuTotal.NextValue()
  $qhyProc.Refresh(); $lastCpu = $qhyProc.TotalProcessorTime.TotalMilliseconds; $lastCpuT = Now
  $runStart = Now
  $deadline = $runStart + [long]$minutes * 60 * $freq
  $exp = [long](2 * $freq)
  $poll = [long](0.025 * $freq)
  $frame = 0
  while ((Now) -lt $deadline) {
    if (Test-Path (Join-Path $Out 'STOP')) { Say "run ${name}: STOP file seen"; break }
    $frame++
    $err = ''; $bytes = 0; $tReady = 0; $tPix = 0; $tStarted = 0
    $wall = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ss.fffZ')
    $tReq = Now
    try {
      Alpaca-Put 'startexposure' 'Duration=2&Light=true'
      $tStarted = Now
      while ($true) {
        $tp = Now
        if (Image-Ready) { $tReady = Now; break }
        if (($tp - $tReq) -gt 60 * $freq) { throw 'imageready not true 60 s after the request' }
        $rest = ($tp + $poll - (Now)) * 1000 / $freq
        if ($rest -ge 1) { [Threading.Thread]::Sleep([int]$rest) }
      }
      $script:tx++
      $req = New-Object System.Net.Http.HttpRequestMessage([System.Net.Http.HttpMethod]::Get, "$cam/imagearray?ClientID=92&ClientTransactionID=$($script:tx)")
      $req.Headers.Accept.Add((New-Object System.Net.Http.Headers.MediaTypeWithQualityHeaderValue('application/imagebytes')))
      $r = $client.SendAsync($req, [System.Net.Http.HttpCompletionOption]::ResponseContentRead).GetAwaiter().GetResult()
      $body = $r.Content.ReadAsByteArrayAsync().GetAwaiter().GetResult()
      $tPix = Now
      $bytes = $body.Length
      $ct = "$($r.Content.Headers.ContentType)"
      if (-not $r.IsSuccessStatusCode) { $err = "imagearray HTTP $([int]$r.StatusCode)" }
      elseif ($ct -notmatch 'imagebytes') { $err = 'imagearray answered ' + $ct + ': ' + [Text.Encoding]::UTF8.GetString($body, 0, [Math]::Min(300, $body.Length)) }
      else {
        $e = [BitConverter]::ToInt32($body, 4)
        $d1 = [BitConverter]::ToInt32($body, 32); $d2 = [BitConverter]::ToInt32($body, 36)
        if ($e -ne 0) { $err = "imagebytes error $e" }
        elseif ($d1 -ne $nx -or $d2 -ne $ny) { $err = "imagebytes dimensions ${d1}x${d2}, not ${nx}x${ny}" }
      }
      $r.Dispose(); $req.Dispose(); $body = $null
    } catch {
      $err = "$_"
    }
    if ($err) {
      $w.WriteLine(('{0},{1},{2},{3},{4},{5},{6},{7},,,,,,{8}' -f $frame, $wall, $bin, $sx, $sy, $nx, $ny, $bytes, (Csv $err)))
      Say "run $name frame ${frame}: $err"
      [Threading.Thread]::Sleep(1000)
    } else {
      $end = $tReq + $exp
      $w.WriteLine(('{0},{1},{2},{3},{4},{5},{6},{7},{8},{9},{10},{11},{12},' -f $frame, $wall, $bin, $sx, $sy, $nx, $ny, $bytes,
        (F (Ms $tReq $tStarted)), (F (Ms $end $tReady)), (F (Ms $tReady $tPix)), (F (Ms $end $tPix)), (F (Ms $tReq $tPix))))
    }
    $tStarted = 0
    if (((Now) - $lastCpuT) -ge 60 * $freq) {
      $t = Now; $qhyProc.Refresh(); $c = $qhyProc.TotalProcessorTime.TotalMilliseconds
      $pct = ($c - $lastCpu) / (Ms $lastCpuT $t) / $ncpu * 100
      $cw.WriteLine(('{0},{1},{2},{3}' -f [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ'), $cpuTotal.NextValue().ToString('F1', $inv), $pct.ToString('F2', $inv), ($qhyProc.WorkingSet64 / 1MB).ToString('F0', $inv)))
      $lastCpu = $c; $lastCpuT = $t
    }
  }
  $elapsed = (Ms $runStart (Now)) / 1000
  $w.Close(); $cw.Close()
  Say "run $name ends after $frame frames, $([Math]::Round($elapsed, 1)) s"
  Summarize $name $csvPath $elapsed $rb
}

function Stats([double[]]$v) {
  $s = [double[]]($v | Sort-Object)
  $n = $s.Count
  if ($n -eq 0) { return $null }
  $med = if ($n % 2) { $s[($n - 1) / 2] } else { ($s[$n / 2 - 1] + $s[$n / 2]) / 2 }
  $p95 = $s[[int][Math]::Ceiling(0.95 * $n) - 1]
  [ordered]@{ median = [Math]::Round($med, 1); p95 = [Math]::Round($p95, 1); max = [Math]::Round($s[$n - 1], 1); min = [Math]::Round($s[0], 1) }
}
function Summarize([string]$name, [string]$csvPath, [double]$elapsed, [string]$frameDesc) {
  $rows = @(Import-Csv $csvPath)
  $ok = @($rows | Where-Object { -not $_.error })
  $sum = [ordered]@{
    run = $name; frame = $frameDesc; exposure_s = 2
    first_frame_utc = if ($rows) { $rows[0].wall_utc } else { $null }
    elapsed_s = [Math]::Round($elapsed, 1)
    frames = $rows.Count; failures = $rows.Count - $ok.Count
    frames_per_hour = [Math]::Round($ok.Count * 3600 / $elapsed, 1)
    bytes = (@($ok | ForEach-Object { $_.bytes } | Sort-Object -Unique) -join ' ')
  }
  foreach ($m in 'latency_ms', 'ready_after_end_ms', 'download_ms', 'cycle_ms', 'start_call_ms') {
    $sum[$m] = Stats ([double[]]@($ok | ForEach-Object { [double]::Parse($_.$m, $inv) }))
  }
  $lat = [double[]]@($ok | ForEach-Object { [double]::Parse($_.latency_ms, $inv) })
  $o5 = @($lat | Where-Object { $_ -gt 500 }).Count; $o10 = @($lat | Where-Object { $_ -gt 1000 }).Count
  $sum['latency_over_500ms'] = "$o5 of $($ok.Count) ($([Math]::Round(100.0 * $o5 / [Math]::Max(1, $ok.Count), 2)) %)"
  $sum['latency_over_1000ms'] = "$o10 of $($ok.Count) ($([Math]::Round(100.0 * $o10 / [Math]::Max(1, $ok.Count), 2)) %)"
  ($sum | ConvertTo-Json -Depth 4) | Set-Content (Join-Path $Out "summary-$name.json")
  Say ("summary ${name}: " + ($sum | ConvertTo-Json -Compress -Depth 4))
}

try {
  Say 'cadence script starts'
  Alpaca-Put 'connected' 'Connected=true'
  $info = [ordered]@{}
  foreach ($m in 'name', 'description', 'driverinfo', 'driverversion', 'interfaceversion', 'sensorname', 'sensortype', 'cameraxsize', 'cameraysize', 'maxbinx', 'maxbiny', 'pixelsizex', 'pixelsizey', 'gain', 'gainmin', 'gainmax', 'offset', 'offsetmin', 'offsetmax', 'readoutmode', 'readoutmodes', 'exposuremin', 'maxadu', 'canasymmetricbin', 'binx', 'biny', 'startx', 'starty', 'numx', 'numy') {
    try { $info[$m] = Alpaca-Get $m } catch { $info[$m] = "error: $_" }
  }
  $info['ncpu'] = $ncpu
  $info['host_os'] = [Environment]::OSVersion.VersionString
  ($info | ConvertTo-Json -Depth 4) | Set-Content (Join-Path $Out 'camera.json')
  Say ("camera: " + ($info | ConvertTo-Json -Compress -Depth 4))
  $W = [int]$info['cameraxsize']; $H = [int]$info['cameraysize']
  $binA = 2
  try { Alpaca-Put 'binx' 'BinX=2'; Alpaca-Put 'biny' 'BinY=2' } catch { Say "the camera refuses 2 x 2 binning: $_"; $binA = 1 }
  Run-Cadence 'A' $binA 0 0 ([int][Math]::Floor($W / $binA)) ([int][Math]::Floor($H / $binA)) $MinutesA
  if (-not (Test-Path (Join-Path $Out 'STOP'))) {
    Run-Cadence 'B' 1 ([int][Math]::Floor(($W - 512) / 2)) ([int][Math]::Floor(($H - 512) / 2)) 512 512 $MinutesB
  }
  if ($binA -ne 1 -and -not (Test-Path (Join-Path $Out 'STOP'))) {
    Run-Cadence 'C' 1 0 0 $W $H $MinutesC
  }
} catch {
  Say "FAILED: $_"
} finally {
  try {
    Alpaca-Put 'binx' 'BinX=1'; Alpaca-Put 'biny' 'BinY=1'
    Alpaca-Put 'startx' 'StartX=0'; Alpaca-Put 'starty' 'StartY=0'
    Alpaca-Put 'numx' "NumX=$W"; Alpaca-Put 'numy' "NumY=$H"
  } catch { Say "frame reset failed: $_" }
  try { Alpaca-Put 'connected' 'Connected=false'; Say 'camera disconnected' } catch { Say "disconnect failed: $_" }
  $null = [G1Net]::timeEndPeriod(1)
  Say 'DONE'
}
