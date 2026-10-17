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

  #  # release packaging (NOT this round; needs both flags on purpose)
  #  pwsh -File build-rs.ps1 -Final -ConfirmOverwrite -OutDir .
  #
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
  [string]$DataDir = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = $PSScriptRoot

# -- target dir pinned per repo discipline (F: is nearly full, builds go to E:) --
if (-not $env:CARGO_TARGET_DIR) {
  $env:CARGO_TARGET_DIR = 'E:\cargo-target\rs-gui'
}
$targetDir = $env:CARGO_TARGET_DIR
$releaseDir = Join-Path $targetDir 'release'

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
Write-Host ""
Write-Host ("[ok] {0}" -f $outFull)
Write-Host ("     sha256 {0}" -f $hash)
Write-Host ("     target dir {0} (out dir {1})" -f $targetDir, $outDirFull)
exit 0
