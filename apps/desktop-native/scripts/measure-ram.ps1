# Measure idle RAM of the native Dice client — the <100 MB headline goal.
#
# Unlike the Tauri client (a host process + a WebView2 browser-process tree that
# had to be summed), this is a SINGLE native process, so one Get-Process read is
# the whole story.
#
#   pwsh -File scripts\measure-ram.ps1            # idles on the login screen
#   pwsh -File scripts\measure-ram.ps1 -Screen chat -Settle 8
param(
    [string]$Screen = "login",   # login | chat | voice | home
    [int]$Settle = 6             # seconds to let it settle before sampling
)

$ErrorActionPreference = "Stop"
$exe = Join-Path $PSScriptRoot "..\target\release\dice-native.exe"
if (-not (Test-Path $exe)) {
    Write-Error "release binary not found — run: cargo build --release"
    exit 1
}

Write-Host "launching $exe --start $Screen ..."
$p = Start-Process -FilePath $exe -ArgumentList "--start", $Screen -PassThru
Start-Sleep -Seconds $Settle

$proc = Get-Process -Id $p.Id
$privMB = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
$wsMB   = [math]::Round($proc.WorkingSet64 / 1MB, 1)

Write-Host ""
Write-Host "  screen        : $Screen"
Write-Host "  PID           : $($p.Id)"
Write-Host "  Private bytes : $privMB MB   <-- the <100 MB target metric"
Write-Host "  Working set   : $wsMB MB"
Write-Host ""

Stop-Process -Id $p.Id -Force
