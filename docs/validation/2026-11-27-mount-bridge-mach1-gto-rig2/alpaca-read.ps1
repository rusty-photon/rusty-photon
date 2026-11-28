# Read-only Alpaca GETs against the mount bridge on loopback.
param([string]$Base = 'http://127.0.0.1:11190')
$mgmt = Invoke-RestMethod -TimeoutSec 10 -Uri "$Base/management/v1/configureddevices"
'configureddevices: ' + ($mgmt.Value | ConvertTo-Json -Compress)
$props = 'connected','name','description','driverinfo','driverversion','interfaceversion',
         'atpark','athome','tracking','slewing','ispulseguiding','canpulseguide','canpark','canunpark',
         'canslew','cansettracking','guideraterightascension','guideratedeclination','sideofpier',
         'rightascension','declination','altitude','azimuth','siderealtime','sitelatitude','sitelongitude',
         'siteelevation','equatorialsystem','trackingrate','alignmentmode'
$i = 0
foreach ($p in $props) {
    $i++
    try {
        $r = Invoke-RestMethod -TimeoutSec 10 -Uri "$Base/api/v1/telescope/0/$($p)?ClientID=7&ClientTransactionID=$i"
        $v = if ($r.ErrorNumber -ne 0) { "ERROR $($r.ErrorNumber): $($r.ErrorMessage)" } else { $r.Value }
    } catch { $v = "HTTP: $($_.Exception.Message)" }
    "{0,-24} {1}" -f $p, $v
}
