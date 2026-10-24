# NitroSense vs Nitro Control: startup time, memory, idle CPU, disk size.
# Run non-elevated with the Nitro Control daemon already running. Takes about 4 minutes.
param([int]$Runs = 5, [int]$IdleSeconds = 30)

$ErrorActionPreference = 'Stop'
$out = Join-Path $PSScriptRoot 'data\bench-result.txt'
$ours = Join-Path $PSScriptRoot '..\target\release\nitro-control.exe' | Resolve-Path
$nsAumid = 'shell:AppsFolder\AcerIncorporated.NitroSenseV31_48frkmn4z8aw4!App'
$cores = [Environment]::ProcessorCount
$log = [System.Collections.Generic.List[string]]::new()
function Say($s) { Write-Host $s; $log.Add($s) }

function Close-App($name) {
    Get-Process $name -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 1500
}

# Time until the main window exists, then until the process stops burning CPU
# (CPU time grows < 5 ms per 100 ms for 3 samples in a row) = "finished loading".
function Measure-Start($name, [scriptblock]$launch) {
    Close-App $name
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $launch
    $p = $null
    while ($sw.ElapsedMilliseconds -lt 20000) {
        $p = Get-Process $name -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
        if ($p) { break }
        Start-Sleep -Milliseconds 5
    }
    if (-not $p) { return $null }
    $window = $sw.ElapsedMilliseconds
    $ready = $null
    try {
        $prev = $p.TotalProcessorTime.TotalMilliseconds; $quiet = 0
        while ($sw.ElapsedMilliseconds -lt 20000) {
            Start-Sleep -Milliseconds 100
            $p.Refresh()
            $now = $p.TotalProcessorTime.TotalMilliseconds
            if ($now - $prev -lt 5) { $quiet++ } else { $quiet = 0 }
            $prev = $now
            if ($quiet -ge 3) { $ready = $sw.ElapsedMilliseconds - 300; break }
        }
    } catch { }
    Start-Sleep -Seconds 2
    $p.Refresh()
    [pscustomobject]@{ Window = $window; Ready = $ready; PrivateMB = [math]::Round($p.PrivateMemorySize64 / 1MB, 1) }
}

function Summary($label, $rows) {
    $w = $rows | Measure-Object Window -Average -Minimum -Maximum
    $r = $rows | Where-Object Ready | Measure-Object Ready -Average -Minimum -Maximum
    $m = $rows | Measure-Object PrivateMB -Average
    Say ("{0,-14} window: avg {1,6:N0} ms (min {2:N0}, max {3:N0}) | loaded: avg {4,6:N0} ms (min {5:N0}, max {6:N0}) | UI private mem {7:N1} MB" -f `
        $label, $w.Average, $w.Minimum, $w.Maximum, $r.Average, $r.Minimum, $r.Maximum, $m.Average)
    $i = 1; foreach ($x in $rows) { Say ("    run {0}: window {1} ms, loaded {2} ms, {3} MB" -f $i++, $x.Window, $x.Ready, $x.PrivateMB) }
}

# Average CPU (% of the whole machine) and private memory per process over a window of time.
function Measure-Idle($label, [string[]]$procs) {
    $paths = foreach ($n in $procs) { "\Process($n)\% Processor Time"; "\Process($n)\Working Set - Private" }
    $samples = Get-Counter -Counter $paths -SampleInterval 1 -MaxSamples $IdleSeconds -ErrorAction SilentlyContinue
    Say "  $label"
    $totCpu = 0; $totMem = 0
    $all = $samples.CounterSamples | Group-Object InstanceName
    foreach ($g in $all) {
        $cpu = ($g.Group | Where-Object Path -like '*processor time' | Measure-Object CookedValue -Average).Average / $cores
        $mem = ($g.Group | Where-Object Path -like '*working set - private' | Measure-Object CookedValue -Average).Average / 1MB
        $totCpu += $cpu; $totMem += $mem
        Say ("    {0,-16} CPU {1,6:N3} %   private WS {2,6:N1} MB" -f $g.Name, $cpu, $mem)
    }
    Say ("    {0,-16} CPU {1,6:N3} %   private WS {2,6:N1} MB" -f 'TOTAL', $totCpu, $totMem)
}

Say "Benchmark $(Get-Date -Format 'yyyy-MM-dd HH:mm')  |  $((Get-CimInstance Win32_Processor).Name.Trim()), $cores threads"
Say ''
Say "== Startup ($Runs runs each, warm cache) =="
$ns = foreach ($i in 1..$Runs) { Measure-Start 'NitroSense' { Start-Process $nsAumid } }
Close-App 'NitroSense'
$nc = foreach ($i in 1..$Runs) { Measure-Start 'nitro-control' { Start-Process $ours } }
Close-App 'nitro-control'
Summary 'NitroSense' $ns
Summary 'Nitro Control' $nc

$nsBg = 'PSSvc', 'PSAgent', 'PSAdminAgent', 'pservice'
Say ''
Say "== Idle, ${IdleSeconds} s average =="
Start-Process $nsAumid; Start-Sleep -Seconds 8
Measure-Idle 'NitroSense, window open' (@('NitroSense') + $nsBg)
Close-App 'NitroSense'
Start-Process $ours; Start-Sleep -Seconds 3
Measure-Idle 'Nitro Control, window open (daemon + window)' @('nitro-control*')
Close-App 'nitro-control'
Measure-Idle 'NitroSense, background only' $nsBg
Measure-Idle 'Nitro Control, background only (daemon)' @('nitro-control*')

Say ''
Say '== Disk =='
function DirMB($p) { if (Test-Path $p) { [math]::Round(((Get-ChildItem $p -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum) / 1MB, 1) } else { 'n/a' } }
$pkg = (Get-AppxPackage AcerIncorporated.NitroSenseV31).InstallLocation
Say ("  NitroSense app package       {0} MB" -f (DirMB $pkg))
Say ("  NitroSense Service folder    {0} MB" -f (DirMB 'C:\Program Files\Acer\NitroSense Service'))
Say ("  Nitro Control exe            {0} MB" -f [math]::Round((Get-Item $ours).Length / 1MB, 2))

$log | Out-File $out -Encoding utf8
Say "`nSaved to $out"
