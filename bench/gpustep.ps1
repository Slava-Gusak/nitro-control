# Constant GPU load (gpuload.exe), optionally plus full CPU load (-Cpu), with fan duty steps in
# Custom mode. Logs our sensors, nvidia-smi clock/power/throttle reasons and CPU package power /
# clock once per second to gpustep[-cpu].csv; restores the mode.
param([int[]]$Steps = @(50, 100, 35, 50), [int]$StepSecs = 120, [int]$Warmup = 60, [switch]$Cpu)
$out = Join-Path $PSScriptRoot ($(if ($Cpu) { 'data\gpustep-cpu.csv' } else { 'data\gpustep.csv' }))
$counters = '\Energy Meter(rapl_package0_pkg)\Power', '\Processor Information(_Total)\% Processor Performance'
$load = Join-Path $PSScriptRoot '..\target\release\gpuload.exe'
function Send($j) {
    $p = New-Object System.IO.Pipes.NamedPipeClientStream('.', 'nitro-control', [System.IO.Pipes.PipeDirection]::InOut)
    $p.Connect(2000)
    $w = New-Object System.IO.StreamWriter($p); $w.AutoFlush = $true
    $r = New-Object System.IO.StreamReader($p)
    $w.WriteLine($j); $s = $r.ReadLine() | ConvertFrom-Json; $p.Dispose(); $s
}
$orig = (Send '{"cmd":"status"}').config
'sec,duty,cpu_temp,gpu_temp,cpu_rpm,gpu_rpm,smi_temp,clock_mhz,power_w,util,throttle,safety,cpu_w,cpu_mhz' | Out-File $out -Encoding ascii
$g = $null
$workers = @()
try {
    Send ('{"cmd":"set_custom","cpu":' + $Steps[0] + ',"gpu":' + $Steps[0] + '}') | Out-Null
    Send '{"cmd":"set_mode","mode":"Custom"}' | Out-Null
    $g = Start-Process $load -PassThru -WindowStyle Hidden
    if ($Cpu) {
        # One core fewer than all, so gpuload still gets to feed the GPU.
        for ($c = 1; $c -lt [Environment]::ProcessorCount; $c++) {
            $workers += Start-Process powershell -ArgumentList '-NoProfile', '-Command', 'while($true){}' -WindowStyle Hidden -PassThru
        }
    }
    $plan = @(@{ duty = $Steps[0]; secs = $Warmup }) + ($Steps | ForEach-Object { @{ duty = $_; secs = $StepSecs } })
    $t = 0; $t0 = Get-Date
    foreach ($st in $plan) {
        Send ('{"cmd":"set_custom","cpu":' + $st.duty + ',"gpu":' + $st.duty + '}') | Out-Null
        for ($i = 0; $i -lt $st.secs; $i++) {
            $smi = (nvidia-smi --query-gpu=temperature.gpu,clocks.gr,power.draw,utilization.gpu,clocks_throttle_reasons.active --format=csv,noheader,nounits) -replace ' ', ''
            $s = Send '{"cmd":"status"}'
            $c = (Get-Counter $counters -SampleInterval 1 -MaxSamples 1 -ErrorAction SilentlyContinue).CounterSamples
            $cw = [math]::Round($c[0].CookedValue / 1000, 1); $cm = [math]::Round($c[1].CookedValue / 100 * 3000)
            "$t,$($st.duty),$($s.cpu_temp),$($s.gpu_temp),$($s.cpu_rpm),$($s.gpu_rpm),$smi,$($s.safety_active),$cw,$cm" | Out-File $out -Append -Encoding ascii
            $t++
            $d = ($t0.AddSeconds($t) - (Get-Date)).TotalMilliseconds
            if ($d -gt 0) { Start-Sleep -Milliseconds $d }
        }
    }
} finally {
    if ($g) { Stop-Process -Id $g.Id -Force -ErrorAction SilentlyContinue }
    $workers | Stop-Process -Force -ErrorAction SilentlyContinue
    Send ('{"cmd":"set_custom","cpu":' + $orig.cpu_pct + ',"gpu":' + $orig.gpu_pct + '}') | Out-Null
    Send ('{"cmd":"set_mode","mode":"' + $orig.mode + '"}') | Out-Null
}
