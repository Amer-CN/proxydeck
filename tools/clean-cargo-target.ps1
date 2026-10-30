<#
Reclaim the shared Cargo target dir when it grows beyond a disk budget.

ASCII only: PowerShell 5.1 reads a .ps1 without BOM as GBK (repo CLAUDE.md pitfall 13).

Usage:
  pwsh -File tools/clean-cargo-target.ps1                  # clean if > 10 GiB
  pwsh -File tools/clean-cargo-target.ps1 -ThresholdGB 10
  pwsh -File tools/clean-cargo-target.ps1 -Force           # clean regardless of size
  pwsh -File tools/clean-cargo-target.ps1 -DryRun          # report only
  pwsh -File tools/clean-cargo-target.ps1 -TargetDir 'E:\cargo-target\workspace'

Safety:
  * refuses a drive root or the repo root
  * skips when cargo/rustc is running, unless -Force
  * only touches the target dir passed/derived; never other E: folders
#>
[CmdletBinding()]
param(
  [int]$ThresholdGB = 10,
  [switch]$Force,
  [switch]$DryRun,
  [string]$TargetDir = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($TargetDir)) {
  if ($env:CARGO_TARGET_DIR) { $TargetDir = $env:CARGO_TARGET_DIR }
  else { $TargetDir = 'E:\cargo-target\workspace' }
}
$full = [System.IO.Path]::GetFullPath($TargetDir).TrimEnd('\')
$rootFull = [System.IO.Path]::GetFullPath($repoRoot).TrimEnd('\')
if ($full -match '^[A-Za-z]:$' -or $full -ieq $rootFull) {
  throw "unsafe target dir: $full"
}
if (-not (Test-Path -LiteralPath $full)) {
  Write-Host "target dir not found: $full"
  exit 0
}

$busy = @(Get-Process cargo,rustc -ErrorAction SilentlyContinue)
if ($busy.Count -gt 0 -and -not $Force) {
  Write-Host ("skip: cargo/rustc running ({0} process(es)); not touching the cache" -f $busy.Count)
  exit 0
}

$sum = (Get-ChildItem -LiteralPath $full -Recurse -Force -File -ErrorAction SilentlyContinue |
        Measure-Object -Property Length -Sum).Sum
if ($null -eq $sum) { $sum = 0 }
$sizeGiB = [math]::Round($sum / 1GB, 2)
Write-Host ("target dir : {0}" -f $full)
Write-Host ("size       : {0} GiB (threshold {1} GiB)" -f $sizeGiB, $ThresholdGB)

if (-not $Force -and $sizeGiB -lt $ThresholdGB) {
  Write-Host "under threshold; nothing to do"
  exit 0
}
if ($DryRun) {
  Write-Host "dry run: would run cargo clean --target-dir"
  exit 0
}

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) { throw 'cargo not on PATH' }
Write-Host 'running cargo clean ...'
& cargo clean --target-dir $full
if ($LASTEXITCODE -ne 0) { throw ("cargo clean exit {0}" -f $LASTEXITCODE) }

if (Test-Path -LiteralPath $full) {
  $left = @(Get-ChildItem -LiteralPath $full -Force -ErrorAction SilentlyContinue)
  if ($left.Count -eq 0) {
    Remove-Item -LiteralPath $full -Force -ErrorAction SilentlyContinue
  } else {
    throw ("target dir still holds {0} entries: {1}" -f $left.Count, $full)
  }
}
Write-Host ("done: reclaimed {0} GiB" -f $sizeGiB)
exit 0
