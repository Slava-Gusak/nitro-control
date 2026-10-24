# Full CPU load with fan duty steps (Custom mode). Logs temp, RPM, CPU package power and
# effective clock once per second to fanstep.csv, then restores the previous mode.
param([int[]]$Steps = @(50, 100, 35, 50), [int]$StepSecs = 90, [int]$Warmup = 30)
$out = Join-Path $PSScriptRoot 'data\fanstep.csv'
function Send($j) {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'nitro-control', [System.IO.Pipes.PipeDirection]::InOut)
    $p.Connect(2000)
    $w = New-Object System.IO.StreamWriter($p); $w.AutoFlush = $true
    $r = New-Object System.IO.StreamReader($p)
    $w.WriteLine($j); $s = $r.ReadLine() | ConvertFrom-Json; $p.Dispose(); $s
}
$counters = '\Energy Meter(rapl_package0_pkg)\Power', '\Processor Information(_Total)\% Processor Performance'
$orig = (Send '{"cmd":"status"}').config
$workers = @()
'sec,duty,cpu_temp,gpu_temp,cpu_rpm,gpu_rpm,pkg_w,clock_mhz,safety' | Out-File $out -Encoding ascii
try {
    Send ('{"cmd":"set_custom","cpu":' + $Steps[0] + ',"gpu":' + $Steps[0] + '}') | Out-Null
    Send '{"cmd":"set_mode","mode":"Custom"}' | Out-Null
    for ($c = 0; $c -lt [Environment]::ProcessorCount; $c++) {
        $workers += Start-Process powershell -ArgumentList '-NoProfile', '-Command', 'while($true){}' -WindowStyle Hidden -PassThru
    }
    $plan = @(@{ duty = $Steps[0]; secs = $Warmup }) + ($Steps | ForEach-Object { @{ duty = $_; secs = $StepSecs } })
    $t = 0
    foreach ($st in $plan) {
        Send ('{"cmd":"set_custom","cpu":' + $st.duty + ',"gpu":' + $st.duty + '}') | Out-Null
        for ($i = 0; $i -lt $st.secs; $i++) {
            # Get-Counter itself waits ~1 s per sample, which paces the loop.
            $c = (Get-Counter $counters -ErrorAction SilentlyContinue).CounterSamples
            $s = Send '{"cmd":"status"}'
            $w = [math]::Round($c[0].CookedValue / 1000, 1)
            $mhz = [math]::Round($c[1].CookedValue / 100 * 3000)
            "$t,$($st.duty),$($s.cpu_temp),$($s.gpu_temp),$($s.cpu_rpm),$($s.gpu_rpm),$w,$mhz,$($s.safety_active)" | Out-File $out -Append -Encoding ascii
            $t++
        }
    }
} finally {
    $workers | Stop-Process -Force -ErrorAction SilentlyContinue
    Send ('{"cmd":"set_custom","cpu":' + $orig.cpu_pct + ',"gpu":' + $orig.gpu_pct + '}') | Out-Null
    Send ('{"cmd":"set_mode","mode":"' + $orig.mode + '"}') | Out-Null
}
