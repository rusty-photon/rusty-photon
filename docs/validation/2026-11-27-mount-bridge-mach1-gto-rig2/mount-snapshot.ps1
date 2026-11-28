# Read-only LX200 get commands sent to the mount's own web interface.
param([string]$MountHost)
$cmds = ':GR#', ':GD#', ':GA#', ':GZ#', ':pS#', ':GOS#', ':GL#', ':GC#'
foreach ($c in $cmds) {
    $u = "http://$MountHost/cmd.cgi?CMD=" + [uri]::EscapeDataString($c)
    try { $r = (Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 -Uri $u).Content } catch { $r = "ERR: $($_.Exception.Message)" }
    "{0,-6} {1}" -f $c, $r
}
