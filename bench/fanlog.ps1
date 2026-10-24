# Logs daemon status to CSV once per second. Optionally loads all CPU cores.
# Usage: fanlog.ps1 -Label auto -LoadSecs 180 -CoolSecs 120   (LoadSecs 0 = log only, e.g. while gaming)
param([string]$Label = 'run', [int]$LoadSecs = 180, [int]$CoolSecs = 120, [int]$IdleSecs = 20)

$out = Join-Path $PSScriptRoot "data\fanlog-$Label.csv"

function Get-Status {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'nitro-control', [System.IO.Pipes.PipeDirection]::InOut)
    try {
        $p.Connect(1000)
        $w = New-Object System.IO.StreamWriter($p); $w.AutoFlush = $true
        $r = New-Object System.IO.StreamReader($p)
        $w.WriteLine('{"cmd":"status"}')
        return ($r.ReadLine() | ConvertFrom-Json)
    } finally { $p.Dispose() }
}

$workers = @()
$t0 = Get-Date
$total = $IdleSecs + $LoadSecs + $CoolSecs
'sec,phase,cpu_temp,gpu_temp,cpu_rpm,gpu_rpm,cpu_load,gpu_load' | Out-File $out -Encoding ascii
try {
    for ($i = 0; $i -lt $total; $i++) {
        $phase = if ($i -lt $IdleSecs) { 'idle' } elseif ($i -lt $IdleSecs + $LoadSecs) { 'load' } else { 'cool' }
        if ($phase -eq 'load' -and $workers.Count -eq 0 -and $LoadSecs -gt 0) {
            for ($c = 0; $c -lt [Environment]::ProcessorCount; $c++) {
                $workers += Start-Process powershell -ArgumentList '-NoProfile', '-Command', 'while($true){}' -WindowStyle Hidden -PassThru
            }
        }
        if ($phase -eq 'cool' -and $workers.Count -gt 0) {
            $workers | Stop-Process -Force -ErrorAction SilentlyContinue; $workers = @()
        }
        try {
            $s = Get-Status
            "$i,$phase,$($s.cpu_temp),$($s.gpu_temp),$($s.cpu_rpm),$($s.gpu_rpm),$([math]::Round([double]$s.cpu_load)),$([math]::Round([double]$s.gpu_load))" | Out-File $out -Append -Encoding ascii
        } catch { "$i,$phase,,,,,," | Out-File $out -Append -Encoding ascii }
        $next = $t0.AddSeconds($i + 1)
        $d = ($next - (Get-Date)).TotalMilliseconds
        if ($d -gt 0) { Start-Sleep -Milliseconds $d }
    }
} finally {
    $workers | Stop-Process -Force -ErrorAction SilentlyContinue
}
