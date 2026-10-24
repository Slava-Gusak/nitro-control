# Full CPU load under each power plan, fans at Max; logs CPU package power and clock once per
# second to powerplans.csv. Restores the original plan and fan mode afterwards.
param([int]$LoadSecs = 150, [int]$CoolSecs = 120, [string[]]$Only, [string]$Out = 'powerplans.csv')
$out = Join-Path $PSScriptRoot "data\$Out"
$plans = [ordered]@{
    'GameTurbo'    = 'ce9fa8b3-8e9e-42db-b4e1-e6297f6cdd7e'
    'Balanced'     = '381b4222-f694-41f0-9685-ff5bb260df2e'
    'Atlas'        = '11111111-1111-1111-1111-111111111111'
    'DynamicBoost' = '10728b17-d7bd-4ca1-990c-b4f7c030f8cd'
}
$counters = '\Energy Meter(rapl_package0_pkg)\Power', '\Processor Information(_Total)\% Processor Performance'
function Send($j) {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'nitro-control', [System.IO.Pipes.PipeDirection]::InOut)
    $p.Connect(2000)
    $w = New-Object System.IO.StreamWriter($p); $w.AutoFlush = $true
    $r = New-Object System.IO.StreamReader($p)
    $w.WriteLine($j); $s = $r.ReadLine() | ConvertFrom-Json; $p.Dispose(); $s
}
$origPlan = ((powercfg /getactivescheme) -replace '.*GUID: ([0-9a-f-]+).*', '$1').Trim()
$origMode = (Send '{"cmd":"status"}').config.mode
'plan,phase,sec,cpu_temp,cpu_rpm,pkg_w,clock_mhz' | Out-File $out -Encoding ascii
$workers = @()
try {
    Send '{"cmd":"set_mode","mode":"Max"}' | Out-Null
    foreach ($name in @($plans.Keys | Where-Object { -not $Only -or $Only -contains $_ })) {
        powercfg /setactive $plans[$name]
        foreach ($phase in 'load', 'cool') {
            if ($phase -eq 'load') {
                for ($c = 0; $c -lt [Environment]::ProcessorCount; $c++) {
                    $workers += Start-Process powershell -ArgumentList '-NoProfile', '-Command', 'while($true){}' -WindowStyle Hidden -PassThru
                }
                $secs = $LoadSecs
            } else {
                $workers | Stop-Process -Force -ErrorAction SilentlyContinue; $workers = @()
                $secs = $CoolSecs
            }
            for ($i = 0; $i -lt $secs; $i++) {
                # Get-Counter waits ~1 s per sample, which paces the loop.
                $c = (Get-Counter $counters -ErrorAction SilentlyContinue).CounterSamples
                $s = Send '{"cmd":"status"}'
                "$name,$phase,$i,$($s.cpu_temp),$($s.cpu_rpm),$([math]::Round($c[0].CookedValue / 1000, 1)),$([math]::Round($c[1].CookedValue / 100 * 3000))" | Out-File $out -Append -Encoding ascii
            }
        }
    }
} finally {
    $workers | Stop-Process -Force -ErrorAction SilentlyContinue
    powercfg /setactive $origPlan
    Send ('{"cmd":"set_mode","mode":"' + $origMode + '"}') | Out-Null
}
