<#
Build script skeleton for the Rust rewrite (P7d-2). ASCII only:
a .ps1 without BOM is read as GBK by PowerShell 5.1 (see repo CLAUDE.md pitfall 13),
so every comment and string here stays ASCII on purpose.

What it does
------------
  1. cargo build --release --bin rs-gui   (workspace root; CARGO_TARGET_DIR is pinned
     to E:\cargo-target\rs-gui unless already set)
  2. hashes the release binary with SHA-256 and prints size + hash
  3. verifies the binary really runs: `<exe> --version` must exit 0 and print
     "ProxyDeck <version> (proxy core <version>)"
  4. reads the Windows version resource back out of the produced exe
     (FileVersion must be non-empty = the .rc landed in the binary)
  5. copies the binary to -OutName (default: a TEST NAME)
  6. optionally reclaims the target dir after the copy is verified:
     `cargo clean --target-dir <dir>` (-CleanTarget; implied by -Final unless
     -KeepTarget is given). Put the cleanup here, not in a doc line: the E:
     cache grew to 115.5 GB by 2026-10-18 and back to ~67 GB by 2026-10-30.

Hard rule (brief P7d-2): the DEFAULT output name is a test name and the script
NEVER overwrites a running ProxyDeck.exe. `-Final` changes the output name to
ProxyDeck.exe and additionally demands -ConfirmOverwrite; without -ConfirmOverwrite
the script refuses and exits 1. This round never runs with -Final.

Usage
-----
  # default: build + verify + emit ProxyDeck.rs-build.exe (test name)
  pwsh -File build-rs.ps1

  # custom test name / custom target dir
  pwsh -File build-rs.ps1 -OutName ProxyDeck.try2.exe

  #  # release packaging (needs both flags on purpose; auto-cleans the target dir)
  #  pwsh -File build-rs.ps1 -Final -ConfirmOverwrite -OutDir .
  #
  # delete the target dir after a successful build+copy (reclaims disk):
  pwsh -File build-rs.ps1 -CleanTarget
  #
  # -Final already implies -CleanTarget; -KeepTarget is a debug-only opt-out.
  # Cargo args default to --locked (the workspace Cargo.lock is committed, so a
  # silent lock rewrite is a smell: it means a dependency changed under us).
  # Pass -CargoArgs @() to allow cargo to update the lock.
#>
[CmdletBinding()]
param(
  # Where the produced exe is copied to (drops the "test name" default).
  [string]$OutName = 'ProxyDeck.rs-build.exe',

  # Directory the output name is resolved against (default: repo root).
  [string]$OutDir = '',

  # Switch the output name to ProxyDeck.exe (release packaging). Requires -ConfirmOverwrite.
  [switch]$Final,

  # Explicit acknowledgement that ProxyDeck.exe may be overwritten.
  [switch]$ConfirmOverwrite,

  # Extra args passed to cargo build, e.g. -CargoArgs @('--locked')
  [string[]]$CargoArgs = @('--locked'),

  # Data directory for the --version run (kept out of the real one).
  [string]$DataDir = '',

  # After a successful copy+hash check, run `cargo clean --target-dir $targetDir`.
  [switch]$CleanTarget,

  # Debug-only opt-out: keep the target dir even for -Final.
  [switch]$KeepTarget
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = $PSScriptRoot

# -- target dir pinned per repo discipline (F: is nearly full, builds go to E:) --
if (-not $env:CARGO_TARGET_DIR) {
  $env:CARGO_TARGET_DIR = 'E:\cargo-target\rs-gui'
}
# Normalize once: cargo build runs from the repo root while cargo clean runs
# from the caller's cwd, so a relative target dir must not be resolved twice.
$targetDir = [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
$env:CARGO_TARGET_DIR = $targetDir
$releaseDir = Join-Path $targetDir 'release'

# -Final is release packaging: after the verified copy, reclaim the cache by
# default. -KeepTarget exists only for debugging a failed final build.
if ($CleanTarget -and $KeepTarget) {
  Write-Host "[refused] -CleanTarget and -KeepTarget are mutually exclusive." -ForegroundColor Red
  exit 1
}
$cleanAfter = ($CleanTarget -or ($Final -and -not $KeepTarget))

# Never hand an obviously wrong path to cargo clean.
if ($cleanAfter) {
  if ([string]::IsNullOrWhiteSpace($targetDir)) {
    Write-Host "[refused] empty target dir for cleanup." -ForegroundColor Red
    exit 1
  }
  $targetFull = [System.IO.Path]::GetFullPath($targetDir).TrimEnd('\')
  $rootFull = [System.IO.Path]::GetFullPath($root).TrimEnd('\')
  if ($targetFull -match '^[A-Za-z]:$' -or $targetFull -ieq $rootFull) {
    Write-Host ("[refused] unsafe target dir for cleanup: '{0}'" -f $targetDir) -ForegroundColor Red
    exit 1
  }
}

# -- output path + the "never overwrite the live exe" guard -------------------
if ($Final) {
  if (-not $ConfirmOverwrite) {
    Write-Host "[refused] -Final would emit ProxyDeck.exe (the release name)." -ForegroundColor Red
    Write-Host "          Pass -ConfirmOverwrite as well, and only when you really mean to" -ForegroundColor Red
    Write-Host "          replace the executable that is in use. Default mode never does this." -ForegroundColor Red
    exit 1
  }
  $outName = 'ProxyDeck.exe'
} else {
  $outName = $OutName
}

if ($outName -ieq 'ProxyDeck.exe' -and -not $ConfirmOverwrite) {
  Write-Host "[refused] output name resolves to ProxyDeck.exe without -ConfirmOverwrite." -ForegroundColor Red
  exit 1
}

if (-not $OutDir) { $OutDir = $root }
$outPath = Join-Path $OutDir $outName
$outDirFull = [System.IO.Path]::GetFullPath($OutDir)
$outFull = [System.IO.Path]::GetFullPath($outPath)

# Resolve the intended target before any write/delete and print it.
Write-Host "== build-rs.ps1 =="
Write-Host ("root        : {0}" -f $root)
Write-Host ("target dir  : {0}" -f $targetDir)
Write-Host ("output      : {0}" -f $outFull)
Write-Host ("mode        : {0}" -f $(if ($Final) { 'FINAL (release name)' } else { 'test name (default)' }))
Write-Host ("cleanup     : {0}" -f $(if ($cleanAfter) { 'cargo clean after verified copy' } else { 'keep target dir' }))
Write-Host ""

# -- 1. cargo build -----------------------------------------------------------
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) { Write-Host "[failed] cargo not on PATH" -ForegroundColor Red; exit 1 }

$cargoArgList = @('build', '--release', '--bin', 'rs-gui') + $CargoArgs
Write-Host ("[1/5] cargo {0}" -f ($cargoArgList -join ' '))
Push-Location $root
try {
  & cargo @cargoArgList
  if ($LASTEXITCODE -ne 0) {
    Write-Host ("[failed] cargo build exit {0}" -f $LASTEXITCODE) -ForegroundColor Red
    exit 1
  }
} finally {
  Pop-Location
}

$exe = Join-Path $releaseDir 'rs-gui.exe'
if (-not (Test-Path -LiteralPath $exe)) {
  Write-Host ("[failed] expected release binary not found: {0}" -f $exe) -ForegroundColor Red
  exit 1
}

# -- 2. SHA-256 ---------------------------------------------------------------
Write-Host ""
$item = Get-Item -LiteralPath $exe
$hash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
Write-Host ("[2/5] artifact : {0}" -f $item.FullName)
Write-Host ("      size     : {0} bytes" -f $item.Length)
Write-Host ("      sha256   : {0}" -f $hash)

# -- 3. really runs: --version -------------------------------------------------
Write-Host ""
Write-Host "[3/5] run: --version"
$oldDataDir = $env:PROXYDECK_DATA_DIR
if ($DataDir) { $env:PROXYDECK_DATA_DIR = $DataDir }
try {
  $verOut = & $exe --version 2>&1
  $verCode = $LASTEXITCODE
} finally {
  if ($DataDir) {
    if ($null -eq $oldDataDir) { Remove-Item Env:\PROXYDECK_DATA_DIR -ErrorAction SilentlyContinue }
    else { $env:PROXYDECK_DATA_DIR = $oldDataDir }
  }
}
Write-Host ("      exit {0} : {1}" -f $verCode, ($verOut -join ' '))
if ($verCode -ne 0) {
  Write-Host ("[failed] --version exit {0}" -f $verCode) -ForegroundColor Red
  exit 1
}
if (($verOut -join ' ') -notmatch '^ProxyDeck v\d+\.\d+\.\d+ \(proxy core v\d+\.\d+\.\d+\)$') {
  Write-Host "[failed] --version output does not match the Go-compatible line" -ForegroundColor Red
  exit 1
}

# -- 4. version resource inside the exe --------------------------------------
Write-Host ""
Write-Host "[4/5] version resource (Get-Item ... .VersionInfo)"
$vi = (Get-Item -LiteralPath $exe).VersionInfo
Write-Host ("      FileVersion    : {0}" -f $vi.FileVersion)
Write-Host ("      ProductVersion : {0}" -f $vi.ProductVersion)
Write-Host ("      ProductName    : {0}" -f $vi.ProductName)
Write-Host ("      FileDescription: {0}" -f $vi.FileDescription)
if ([string]::IsNullOrWhiteSpace($vi.FileVersion)) {
  Write-Host "[failed] exe has no FileVersion: the .rc did not reach the binary" -ForegroundColor Red
  exit 1
}

# -- 5. copy to the output name ----------------------------------------------
Write-Host ""
Write-Host ("[5/5] copy -> {0}" -f $outFull)
if (Test-Path -LiteralPath $outFull) {
  $dstHash = (Get-FileHash -LiteralPath $outFull -Algorithm SHA256).Hash
  if ($dstHash -eq $hash) {
    Write-Host "      already up to date (same sha256), left untouched"
  } else {
    Copy-Item -LiteralPath $exe -Destination $outFull -Force
    Write-Host "      replaced (previous copy had a different sha256)"
  }
} else {
  Copy-Item -LiteralPath $exe -Destination $outFull -Force
  Write-Host "      created"
}

# Re-read what actually landed at the output path (byte-for-byte check).
$landedHash = (Get-FileHash -LiteralPath $outFull -Algorithm SHA256).Hash
if ($landedHash -ne $hash) {
  Write-Host ("[failed] hash mismatch after copy: {0} != {1}" -f $landedHash, $hash) -ForegroundColor Red
  exit 1
}
# -- 6. reclaim the target dir (only after the artifact is safely copied) -----
if ($cleanAfter) {
  Write-Host ""
  Write-Host ("[clean] cargo clean --target-dir {0}" -f $targetDir)
  & cargo clean --target-dir $targetDir
  if ($LASTEXITCODE -ne 0) {
    Write-Host ("[failed] cargo clean exit {0}; artifact is valid but the cache was NOT reclaimed" -f $LASTEXITCODE) -ForegroundColor Red
    exit 1
  }
  if (Test-Path -LiteralPath $targetDir) {
    $left = @(Get-ChildItem -LiteralPath $targetDir -Force -ErrorAction SilentlyContinue)
    if ($left.Count -eq 0) {
      Remove-Item -LiteralPath $targetDir -Force -ErrorAction SilentlyContinue
    } else {
      Write-Host ("[failed] target dir still holds {0} entries after clean: {1}" -f $left.Count, $targetDir) -ForegroundColor Red
      exit 1
    }
  }
  Write-Host "        target dir reclaimed"
}
Write-Host ""
Write-Host ("[ok] {0}" -f $outFull)
Write-Host ("     sha256 {0}" -f $hash)
Write-Host ("     target dir {0} (out dir {1}){2}" -f $targetDir, $outDirFull, $(if ($cleanAfter) { ' [cleaned]' } else { '' }))
exit 0
