# Reads every GetGamingSysInfo sensor id at idle and again under full CPU+GPU load (read-only).
# Run as Administrator. Output: sensor-probe.txt
$out = Join-Path $PSScriptRoot 'data\sensor-probe.txt'
$load = Join-Path $PSScriptRoot '..\target\release\gpuload.exe'
$g = Get-CimInstance -Namespace root/wmi -ClassName AcerGamingFunction

function Call([uint32]$in) { [uint64](Invoke-CimMethod -InputObject $g -MethodName GetGamingSysInfo -Arguments @{ gmInput = $in }).gmOutput }

function Probe {
    $res = @{}
    foreach ($id in 0..0x30) {
        try { $res[$id] = Call (0x01 -bor ($id -shl 8)) } catch { $res[$id] = $null }
    }
    $res
}

$lines = @()
$sup = Call 0x0000
$lines += 'supported-sensors query (input 0x0000): 0x{0:X16}' -f $sup
$idle = Probe

$workers = @()
$gp = Start-Process $load -ArgumentList 100 -PassThru -WindowStyle Hidden
for ($c = 1; $c -lt [Environment]::ProcessorCount; $c++) {
    $workers += Start-Process powershell -ArgumentList '-NoProfile', '-Command', 'while($true){}' -WindowStyle Hidden -PassThru
}
Start-Sleep -Seconds 90
$hot = Probe
$workers | Stop-Process -Force -ErrorAction SilentlyContinue
Stop-Process -Id $gp.Id -Force -ErrorAction SilentlyContinue

$lines += 'id   status  idle-value  load-value  (raw idle / raw load)'
foreach ($id in 0..0x30) {
    $a = $idle[$id]; $b = $hot[$id]
    if ($a -eq $null -or $b -eq $null) { $lines += ('0x{0:X2} call failed' -f $id); continue }
    $st = $a -band 0xFF
    $lines += ('0x{0:X2}  {1,3}  {2,10}  {3,10}  (0x{4:X} / 0x{5:X})' -f $id, $st, (($a -shr 8) -band 0xFFFF), (($b -shr 8) -band 0xFFFF), $a, $b)
}
$lines | Out-File $out -Encoding ascii
