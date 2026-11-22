# G3 frame recorder: back-to-back exposures from one camera on a qhy-camera
# endpoint (HTTPS, Basic auth), each frame kept exactly as the server sent it,
# for guide-replay to measure stars on later.
#
# The camera is found by its UniqueID among the endpoint's configured devices,
# because Alpaca device numbers follow the SDK's scan order. The frame is the
# full sensor at -Bin x -Bin; gain, offset and readout mode stay as the camera
# has them. The script addresses that one camera and nothing else.
#
# -Out holds:
#   camera.json              the camera's properties, read before the first frame
#   frames.csv               one row per frame, failures included
#   frame-NNNNN.imagebytes   the imagearray body, byte for byte: the 44-byte
#                            ImageBytes header, then the pixels
#   status.txt               the start, a line a minute, each failure, the end
# A failed frame has a row with its error and no file. The script refuses an
# -Out that already holds a frames.csv.
#
# The camera's credential is read from rp.json's camera entry for -Base (rp is
# the client that holds it) and never written anywhere. A file named STOP in
# -Out ends the recording early. Either way the frame is reset to bin 1 at the
# full sensor and the camera disconnected.
param(
  [Parameter(Mandatory = $true)][string]$Out,
  [string]$Base = 'https://127.0.0.1:11121',
  [string]$UniqueId = 'QHY5III678M-4a94be45b27d3aa79',
  [int]$Minutes = 60,
  [double]$Exposure = 2,
  [int]$Bin = 2
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$inv = [Globalization.CultureInfo]::InvariantCulture
New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).ProviderPath
$status = Join-Path $Out 'status.txt'
$stopFile = Join-Path $Out 'STOP'
function Say([string]$m) { "$([DateTime]::UtcNow.ToString('o', $inv)) $m" | Add-Content $status }

Add-Type -AssemblyName System.Net.Http
Add-Type -ReferencedAssemblies System.Net.Http, System -TypeDefinition @'
using System;
using System.Net.Http;
using System.Net.Security;
using System.Runtime.InteropServices;
using System.Security.Cryptography.X509Certificates;
public static class G3Net {
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

$freq = [Diagnostics.Stopwatch]::Frequency
function Now { [Diagnostics.Stopwatch]::GetTimestamp() }
function Ms([long]$a, [long]$b) { ($b - $a) * 1000.0 / $freq }
function F([double]$v) { $v.ToString('F3', $inv) }
function Csv([string]$s) { '"' + ($s -replace '"', '""' -replace "[`r`n]+", ' ') + '"' }
$script:tx = 0
$script:client = $null
$script:cam = $null

function Get-Body([string]$url) {
  $script:tx++
  $r = $script:client.GetAsync("${url}?ClientID=93&ClientTransactionID=$($script:tx)").GetAwaiter().GetResult()
  $s = $r.Content.ReadAsStringAsync().GetAwaiter().GetResult()
  if (-not $r.IsSuccessStatusCode) { throw "GET $url HTTP $([int]$r.StatusCode): $s" }
  $s
}
function Alpaca-Get([string]$member) {
  $j = Get-Body "$($script:cam)/$member" | ConvertFrom-Json
  if ($j.ErrorNumber -ne 0) { throw "GET $member error $($j.ErrorNumber): $($j.ErrorMessage)" }
  $j.Value
}
function Alpaca-Put([string]$member, [string]$form) {
  $script:tx++
  $body = New-Object System.Net.Http.StringContent("$form&ClientID=93&ClientTransactionID=$($script:tx)", [Text.Encoding]::UTF8, 'application/x-www-form-urlencoded')
  $r = $script:client.PutAsync("$($script:cam)/$member", $body).GetAwaiter().GetResult()
  $s = $r.Content.ReadAsStringAsync().GetAwaiter().GetResult()
  if (-not $r.IsSuccessStatusCode) { throw "PUT $member HTTP $([int]$r.StatusCode): $s" }
  if ($s -notmatch '"ErrorNumber"\s*:\s*0\b') { $j = $s | ConvertFrom-Json; throw "PUT $member error $($j.ErrorNumber): $($j.ErrorMessage)" }
}
# imageready, answered quickly: $true/$false, or throws with the error text
function Image-Ready {
  $s = Get-Body "$($script:cam)/imageready"
  if ($s -match '"ErrorNumber"\s*:\s*0\b') { return ($s -match '"Value"\s*:\s*true') }
  throw "GET imageready: $s"
}
# An empty string if the imagearray answer is a whole ImageBytes frame of
# $nx x $ny, else what is wrong with it.
function ImageBytes-Error($r, [byte[]]$body, [int]$nx, [int]$ny) {
  $ct = "$($r.Content.Headers.ContentType)"
  $text = { [Text.Encoding]::UTF8.GetString($body, 0, [Math]::Min(300, $body.Length)) }
  if (-not $r.IsSuccessStatusCode) { return "imagearray HTTP $([int]$r.StatusCode): $(& $text)" }
  if ($ct -notmatch 'imagebytes') { return "imagearray answered ${ct}: $(& $text)" }
  if ($body.Length -lt 44) { return "imagebytes body of $($body.Length) bytes" }
  $e = [BitConverter]::ToInt32($body, 4); $start = [BitConverter]::ToInt32($body, 16)
  $tt = [BitConverter]::ToInt32($body, 24)
  $d1 = [BitConverter]::ToInt32($body, 32); $d2 = [BitConverter]::ToInt32($body, 36)
  if ($e -ne 0) {
    $msg = if ($start -ge 44 -and $start -lt $body.Length) { [Text.Encoding]::UTF8.GetString($body, $start, [Math]::Min(300, $body.Length - $start)) } else { '' }
    return "imagebytes error ${e}: $msg"
  }
  if ($d1 -ne $nx -or $d2 -ne $ny) { return "imagebytes dimensions ${d1}x${d2}, not ${nx}x${ny}" }
  $size = switch ($tt) { 1 { 2 } 2 { 4 } 6 { 1 } 8 { 2 } default { 0 } }
  if ($size -eq 0) { return "imagebytes transmission element type $tt unknown" }
  $want = [long]$start + [long]$nx * $ny * $size
  if ($body.Length -ne $want) { return "imagebytes body of $($body.Length) bytes, not $want" }
  ''
}

$W = 0; $H = 0; $connecting = $false; $timer = $false; $failed = $false
try {
  if (Test-Path (Join-Path $Out 'frames.csv')) { throw "$Out already holds a recording" }
  Say "recorder starts: $UniqueId at $Base, ${Exposure} s exposures at bin $Bin for $Minutes min"
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13
  [Net.ServicePointManager]::Expect100Continue = $false
  [Net.ServicePointManager]::UseNagleAlgorithm = $false
  $null = [G3Net]::timeBeginPeriod(1); $timer = $true

  $pem = Get-Content "$env:ProgramData\rusty-photon\pki\ca.pem" -Raw
  $der = [Convert]::FromBase64String(($pem -replace '-----[A-Z ]+-----', '' -replace '\s', ''))
  $script:client = New-Object System.Net.Http.HttpClient([G3Net]::Handler($der))
  $script:client.Timeout = [TimeSpan]::FromSeconds(120)
  $script:client.DefaultRequestHeaders.ExpectContinue = $false
  $rp = Get-Content "$env:ProgramData\rusty-photon\rp.json" -Raw | ConvertFrom-Json
  $cred = $rp.equipment.cameras | Where-Object { "$($_.alpaca_url)".TrimEnd('/') -eq $Base.TrimEnd('/') } | Select-Object -First 1
  if (-not $cred) { throw "rp.json has no camera entry for $Base" }
  $basic = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes("$($cred.auth.username):$($cred.auth.password)"))
  $script:client.DefaultRequestHeaders.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Basic', $basic)
  $rp = $null; $cred = $null; $basic = $null

  $devices = @((Get-Body "$Base/management/v1/configureddevices" | ConvertFrom-Json).Value)
  $match = @($devices | Where-Object { $_.DeviceType -eq 'Camera' -and $_.UniqueID -eq $UniqueId })
  if ($match.Count -eq 0) {
    throw "no camera with UniqueID $UniqueId at $Base; configured devices: " + (($devices | ForEach-Object { "$($_.DeviceType) $($_.DeviceNumber) $($_.UniqueID)" }) -join '; ')
  }
  $device = [int]$match[0].DeviceNumber
  $script:cam = "$Base/api/v1/camera/$device"
  Say "camera $UniqueId is camera $device"

  $connecting = $true
  Alpaca-Put 'connected' 'Connected=true'
  $info = [ordered]@{ unique_id = $UniqueId; device_number = $device }
  foreach ($m in 'name', 'description', 'driverinfo', 'driverversion', 'interfaceversion', 'sensorname', 'sensortype', 'cameraxsize', 'cameraysize', 'maxbinx', 'maxbiny', 'pixelsizex', 'pixelsizey', 'gain', 'gainmin', 'gainmax', 'offset', 'offsetmin', 'offsetmax', 'readoutmode', 'readoutmodes', 'exposuremin', 'maxadu', 'canasymmetricbin', 'binx', 'biny', 'startx', 'starty', 'numx', 'numy') {
    try { $info[$m] = Alpaca-Get $m } catch { $info[$m] = "error: $_" }
  }
  $info['host_os'] = [Environment]::OSVersion.VersionString
  ($info | ConvertTo-Json -Depth 4) | Set-Content (Join-Path $Out 'camera.json')
  Say ('camera: ' + ($info | ConvertTo-Json -Compress -Depth 4))

  $W = [int](Alpaca-Get 'cameraxsize'); $H = [int](Alpaca-Get 'cameraysize')
  $nx = [int][Math]::Floor($W / $Bin); $ny = [int][Math]::Floor($H / $Bin)
  Alpaca-Put 'binx' "BinX=$Bin"; Alpaca-Put 'biny' "BinY=$Bin"
  Alpaca-Put 'numx' "NumX=$nx"; Alpaca-Put 'numy' "NumY=$ny"
  Alpaca-Put 'startx' 'StartX=0'; Alpaca-Put 'starty' 'StartY=0'
  $rb = [ordered]@{ binx = $Bin; biny = $Bin; startx = 0; starty = 0; numx = $nx; numy = $ny }
  $got = foreach ($k in @($rb.Keys)) { '{0}={1}' -f $k, (Alpaca-Get $k) }
  Say ('frame reads back ' + ($got -join ' '))
  $want = foreach ($k in @($rb.Keys)) { '{0}={1}' -f $k, $rb[$k] }
  if (($got -join ' ') -ne ($want -join ' ')) { throw ('the frame reads back ' + ($got -join ' ') + ', not ' + ($want -join ' ')) }

  $csv = New-Object IO.StreamWriter((Join-Path $Out 'frames.csv'), $false, (New-Object Text.UTF8Encoding $false))
  $csv.AutoFlush = $true
  $csv.WriteLine('frame,file,start_utc,exposure_s,bin,numx,numy,bytes,download_ms,error')
  $expS = $Exposure.ToString('R', $inv)
  $poll = [long](0.025 * $freq)
  $readyLimit = [long](($Exposure + 60) * $freq)
  $t0 = Now
  $deadline = $t0 + [long]$Minutes * 60 * $freq
  $nextMinute = $t0 + 60 * $freq
  $frame = 0; $fails = 0
  Say 'recording begins'
  while ((Now) -lt $deadline) {
    if (Test-Path $stopFile) { Say 'STOP file seen'; break }
    $frame++
    $file = 'frame-{0:D5}.imagebytes' -f $frame
    $err = ''; $bytes = ''; $dl = ''
    $wall = [DateTime]::UtcNow.ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", $inv)
    $tReq = Now
    try {
      Alpaca-Put 'startexposure' "Duration=$expS&Light=true"
      while ($true) {
        $tp = Now
        if (Image-Ready) { break }
        if (($tp - $tReq) -gt $readyLimit) { throw "imageready not true $($Exposure + 60) s after the request" }
        $rest = ($tp + $poll - (Now)) * 1000 / $freq
        if ($rest -ge 1) { [Threading.Thread]::Sleep([int]$rest) }
      }
      $tDl = Now
      $script:tx++
      $req = New-Object System.Net.Http.HttpRequestMessage([System.Net.Http.HttpMethod]::Get, "$($script:cam)/imagearray?ClientID=93&ClientTransactionID=$($script:tx)")
      $req.Headers.Accept.Add((New-Object System.Net.Http.Headers.MediaTypeWithQualityHeaderValue('application/imagebytes')))
      $r = $script:client.SendAsync($req, [System.Net.Http.HttpCompletionOption]::ResponseContentRead).GetAwaiter().GetResult()
      $body = $r.Content.ReadAsByteArrayAsync().GetAwaiter().GetResult()
      $dl = F (Ms $tDl (Now))
      $bytes = $body.Length
      $err = ImageBytes-Error $r $body $nx $ny
      $r.Dispose(); $req.Dispose()
      if (-not $err) { [IO.File]::WriteAllBytes((Join-Path $Out $file), $body) }
      $body = $null
    } catch {
      $err = "$_"
    }
    if ($err) {
      $fails++
      $csv.WriteLine(('{0},,{1},{2},{3},{4},{5},{6},{7},{8}' -f $frame, $wall, $expS, $Bin, $nx, $ny, $bytes, $dl, (Csv $err)))
      Say "frame ${frame}: $err"
      [Threading.Thread]::Sleep(1000)
    } else {
      $csv.WriteLine(('{0},{1},{2},{3},{4},{5},{6},{7},{8},' -f $frame, $file, $wall, $expS, $Bin, $nx, $ny, $bytes, $dl))
    }
    if ((Now) -ge $nextMinute) { Say "$frame frames, $fails failed"; $nextMinute += 60 * $freq }
  }
  $csv.Close()
  Say "recording ends after $frame frames, $fails failed, $([Math]::Round((Ms $t0 (Now)) / 1000, 1)) s"
} catch {
  Say "FAILED: $_"
  $failed = $true
} finally {
  if ($csv) { $csv.Dispose() }
  if ($W -gt 0) {
    try {
      Alpaca-Put 'binx' 'BinX=1'; Alpaca-Put 'biny' 'BinY=1'
      Alpaca-Put 'startx' 'StartX=0'; Alpaca-Put 'starty' 'StartY=0'
      Alpaca-Put 'numx' "NumX=$W"; Alpaca-Put 'numy' "NumY=$H"
    } catch { Say "frame reset failed: $_" }
  }
  if ($connecting) {
    try { Alpaca-Put 'connected' 'Connected=false'; Say 'camera disconnected' } catch { Say "disconnect failed: $_" }
  }
  if ($timer) { $null = [G3Net]::timeEndPeriod(1) }
  Say 'DONE'
}
if ($failed) { exit 1 }
