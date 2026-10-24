# Reads CoolBoost, turns it on, reads again, restores the original state. Run as Administrator.
$out = Join-Path $PSScriptRoot 'data\coolboost-test.txt'
$a = Get-CimInstance -Namespace root/wmi -ClassName APGeAction
function Get-CB { '0x{0:X16}' -f [uint64](Invoke-CimMethod -InputObject $a -MethodName GetFunction -Arguments @{ uiInput = [uint32]0x207 }).uiOutput }
function Set-CB($on) { $v = [uint64](7 -bor ([int]$on -shl 16)); '0x{0:X}' -f (Invoke-CimMethod -InputObject $a -MethodName SetFunction -Arguments @{ uiInput = $v }).uiOutput }
$lines = @()
$orig = Get-CB
$lines += "initial      : $orig"
$lines += "set ON  -> st: $(Set-CB 1)"
$lines += "after ON     : $(Get-CB)"
$wasOn = (([uint64]$orig -shr 8) -band 0xFF) -eq 1
$lines += "restore ($([int]$wasOn)) -> st: $(Set-CB $wasOn)"
$lines += "final        : $(Get-CB)"
$lines | Out-File $out -Encoding utf8
