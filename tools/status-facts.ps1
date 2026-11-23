# status-facts.ps1 -- print the CURRENT handoff facts (version / tag / processes / ports / caches).
#
# Why: the "current state" section of CLAUDE.md used to hand-copy volatile facts (PID, exe version, which
# backends were lit). Every copy went stale within days; 2026-11-24 a session quoted
# v4.9.10 while the disk exe was already v4.10.2. Copy the OUTPUT of this script, or
# point the doc at it -- do not hand-transcribe numbers again.
#
# ASCII only on purpose: an un-BOM'd UTF-8 .ps1 with Chinese comments is parsed as GBK
# by Windows PowerShell 5.1 and can break the syntax (CLAUDE.md pitfall #12).
#
# Usage:  pwsh -File tools/status-facts.ps1

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot

function Section([string]$name) { "`n== $name ==" }

Section 'repo'
& git -C $root -c core.quotepath=false log -1 --format='HEAD      %h %ad %s' --date=short
$tags = & git -C $root tag --sort=-creatordate
"Tag(latest) $(if ($tags) { $tags | Select-Object -First 1 } else { '(none)' })"
$dirty = & git -C $root -c core.quotepath=false status --porcelain
"Worktree    $(if ($dirty) { "$($dirty.Count) dirty file(s)" } else { 'clean' })"

Section 'version anchors'
$lib = Join-Path $root 'rs-gui/src/lib.rs'
$m = Select-String -Path $lib -Pattern 'pub const APP_VERSION: &str = "(v[0-9.]+)"'
if ($m) { "APP_VERSION  $($m.Matches[0].Groups[1].Value)" }
$exe = Join-Path $root 'ProxyDeck.exe'
if (Test-Path $exe) {
    $fv = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($exe)
    "disk exe     $($fv.FileVersion)  ($exe)"
}
$ui = Join-Path $root 'app/ui.html'
$v = Select-String -Path $ui -Pattern 'id="verChip">([^<]+)<'
if ($v) { "ui verChip   $($v.Matches[0].Groups[1].Value)" }
$readme = Select-String -Path (Join-Path $root 'README.md') -Pattern 'Version-v([0-9.]+)-'
if ($readme) { "README badge v$($readme.Matches[0].Groups[1].Value)" }
$cl = Select-String -Path (Join-Path $root 'CLAUDE.md') -Pattern '## \u5F53\u524D\u72B6\u6001.*?(v[0-9.]+) \u5DF2\u53D1\u5E03'
if ($cl) { "CLAUDE.md    $($cl.Matches[0].Groups[1].Value)" } else { "CLAUDE.md    (anchor not found)" }
"(run `cargo test -p rs-gui --test release_sync` to verify they all agree)"

Section 'running ProxyDeck processes'
$procs = Get-CimInstance Win32_Process -Filter "Name like 'ProxyDeck%'" -ErrorAction SilentlyContinue
if (-not $procs) { 'none' }
foreach ($p in $procs) {
    $cmdline = $p.CommandLine -replace '-api-key \S+', '-api-key <redacted>'
    $mode = 'GUI'
    if ($cmdline -match '--plugin-([a-z-]+)') { $mode = "plugin:$($matches[1])" }
    elseif ($cmdline -match '-headless') { $mode = 'headless' }
    $port = ''
    if ($cmdline -match '--port\s+(\d+)') { $port = $matches[1] }
    "{0,-9} pid={1,-7} port={2,-6} {3}" -f $mode, $p.ProcessId, $port, (Split-Path $p.ExecutablePath -Leaf)
}

Section 'plugin ports'
$ports = @(
    @('opencode', 8784), @('qoder', 8785), @('comate', 8786), @('codebuddy', 8787),
    @('tuanjie', 8788), @('codebuddy-intl', 8789), @('vibex', 8790), @('qoder-intl', 8791),
    @('codearts', 8793), @('bai', 8891), @('command', 55990)
)
foreach ($pair in $ports) {
    $name = $pair[0]; $port = $pair[1]
    $tcp = New-Object System.Net.Sockets.TcpClient
    $up = $false
    try {
        $iar = $tcp.BeginConnect('127.0.0.1', $port, $null, $null)
        $up = $iar.AsyncWaitHandle.WaitOne(300) -and $tcp.Connected
    } catch { $up = $false } finally { $tcp.Close() }
    "{0,-14} {1,-6} {2}" -f $name, $port, $(if ($up) { 'UP' } else { 'down' })
}

Section 'caches (mtime)'
$files = @(
    (Join-Path $env:USERPROFILE '.qoder-plugin-models-cn.json'),
    (Join-Path $env:USERPROFILE '.qoder-plugin-models-intl.json'),
    (Join-Path $env:APPDATA 'ProxyDeck/codebuddy-pool.json'),
    (Join-Path $env:APPDATA 'ProxyDeck/codearts-refreshed.json')
)
foreach ($f in $files) {
    if (Test-Path $f) {
        $i = Get-Item $f
        "{0,-45} {1:yyyy-MM-dd HH:mm}  {2} B" -f $i.Name, $i.LastWriteTime, $i.Length
    } else {
        "{0,-45} (missing)" -f (Split-Path $f -Leaf)
    }
}
