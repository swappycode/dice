<#
.SYNOPSIS
  Measure idle RAM of the release Dice client at the login screen.

.DESCRIPTION
  Launches the built release exe under an isolated `--profile` (so it sits on
  the login screen with no stored session and no network), idles, then sums
  memory across the host process + its entire WebView2 descendant tree.

  Two metrics, matching the M1 perf snapshot in WORKLOG.md:
    * PRIVATE  = sum of each process's private commit (PrivatePageCount).
                 Shared Chromium pages are NOT double-counted. This is the
                 "~170 MB private" number to compare against the <100 MB goal.
    * WORKING SET = sum of WorkingSetSize. Naive; overcounts shared pages
                 (M1's "373 MB"). Reported only for context.

  Set $env:DICE_WEBVIEW_ARGS before running to A/B a browser-arg experiment
  against the default (the launched exe inherits the env var).

  SAFETY: only descendants of the launched PID are ever stopped — never matches
  msedgewebview2.exe by name (VSCode and other apps share that binary).
#>
param(
  [string]$Exe = "apps/desktop-client/src-tauri/target/release/dice-desktop.exe",
  [string]$ProfileName = "bench",
  [int]$Idle = 30
)
$ErrorActionPreference = "Stop"

$resolved = (Resolve-Path $Exe).Path
$argsLabel = if ($env:DICE_WEBVIEW_ARGS) { $env:DICE_WEBVIEW_ARGS } else { "(default DEFAULT_WEBVIEW_ARGS)" }
Write-Host "exe:     $resolved"
Write-Host "profile: $ProfileName"
Write-Host "args:    $argsLabel"

$proc = Start-Process -FilePath $resolved -ArgumentList @("--profile", $ProfileName) -PassThru
Write-Host "launched host pid $($proc.Id); idling ${Idle}s for the WebView2 tree to settle..."
Start-Sleep -Seconds $Idle

# Snapshot AFTER idle so every WebView2 child already exists, then BFS the tree.
$all = Get-CimInstance Win32_Process
$want = [System.Collections.Generic.HashSet[int]]::new()
[void]$want.Add([int]$proc.Id)
$changed = $true
while ($changed) {
  $changed = $false
  foreach ($p in $all) {
    if ($want.Contains([int]$p.ParentProcessId) -and -not $want.Contains([int]$p.ProcessId)) {
      [void]$want.Add([int]$p.ProcessId); $changed = $true
    }
  }
}
$tree = $all | Where-Object { $want.Contains([int]$_.ProcessId) }

Write-Host ""
Write-Host "--- Dice process tree ($($tree.Count) processes) ---"
$tree | Sort-Object PrivatePageCount -Descending | ForEach-Object {
  "{0,-22} pid {1,-7} private {2,7:N1} MB   ws {3,7:N1} MB" -f `
    $_.Name, $_.ProcessId, ($_.PrivatePageCount / 1MB), ($_.WorkingSetSize / 1MB)
}
$privSum = ($tree | Measure-Object -Property PrivatePageCount -Sum).Sum
$wsSum = ($tree | Measure-Object -Property WorkingSetSize -Sum).Sum
Write-Host "----------------------------------------------------------"
"TOTAL private commit (compare to <100 MB goal): {0:N1} MB" -f ($privSum / 1MB)
"TOTAL working set (naive, overcounts shared):   {0:N1} MB" -f ($wsSum / 1MB)

# Tear down the whole tree (leaf-first not required; -Force is enough).
$want | ForEach-Object { Stop-Process -Id $_ -Force -ErrorAction SilentlyContinue }
