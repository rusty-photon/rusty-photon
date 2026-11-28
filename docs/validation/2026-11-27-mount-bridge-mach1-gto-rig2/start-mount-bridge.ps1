# Starts rig2's mount bridge in the owner's session: APCC Pro first, then ASCOM
# Remote Server once APCC's REST API accepts connections on 127.0.0.1:60001.
# Remote Server connects the AP V2 driver as it starts, and the driver gives up
# on an APCC that has not answered within its connect timeout, so Remote Server
# must not start before APCC serves.
$log = Join-Path $env:LOCALAPPDATA 'ASCOM Remote\Logs\mount-bridge-start.log'
function Write-Log([string]$Message) {
    Add-Content -Path $log -Value ('{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $Message)
}
$apcc = 'C:\Program Files (x86)\Astro-Physics\APCC Pro\AstroPhysicsCommandCenter.exe'
$remote = 'C:\Program Files\ASCOM\RemoteServer\32bit\RemoteServer.exe'

if (Get-Process AstroPhysicsCommandCenter -ErrorAction SilentlyContinue) {
    Write-Log 'APCC Pro is already running'
} else {
    Start-Process -FilePath $apcc -WorkingDirectory (Split-Path $apcc)
    Write-Log 'started APCC Pro'
}

$deadline = (Get-Date).AddMinutes(10)
$serving = $false
while (-not $serving -and (Get-Date) -lt $deadline) {
    $client = New-Object System.Net.Sockets.TcpClient
    try { $client.Connect('127.0.0.1', 60001); $serving = $true }
    catch { Start-Sleep -Seconds 2 }
    finally { $client.Close() }
}
if (-not $serving) {
    Write-Log 'APCC REST API did not accept a connection on 127.0.0.1:60001 within 10 minutes; Remote Server not started'
    exit 1
}
Write-Log 'APCC REST API accepts connections'

if (Get-Process RemoteServer -ErrorAction SilentlyContinue) {
    Write-Log 'Remote Server is already running'
} else {
    Start-Process -FilePath $remote -WorkingDirectory (Split-Path $remote)
    Write-Log 'started Remote Server'
}
