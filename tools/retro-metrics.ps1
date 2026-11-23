# retro-metrics.ps1 -- reproducible bug/rework numbers for the retrospective.
#
# Why: 2026-11-24 retrospective needed fix share, hotspots, fix storms, rework rate
# and release cadence. Recomputing by hand next time is how retro numbers become
# folklore (or wrong). This script is the single entry point; rerun with -Since.
#
# ASCII only on purpose (Windows PowerShell 5.1 parses un-BOM'd UTF-8 as GBK;
# Chinese text here would risk syntax breakage -- CLAUDE.md pitfall #12).
#
# Usage:  pwsh -File tools/retro-metrics.ps1 [-Since 2026-10-18] [-TopFiles 15]
#         pwsh -File tools/retro-metrics.ps1 -Since 2026-09-01
#
# All git calls pass `-c core.quotepath=false`: the default quotes non-ASCII paths
# (Chinese file names) and silently drops them from the sample.

param(
    [string]$Since = '2026-10-18',
    [int]$TopFiles = 15,
    [int]$StormThreshold = 3
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function GitOut([string[]]$GitArgs) {
    & git -C $root -c core.quotepath=false @GitArgs
}

function Section([string]$name) { "`n===== $name =====" }

$log = GitOut @('log', "--since=$Since", '--pretty=format:%h|%ad|%s', '--date=short', '--no-merges')
$rows = foreach ($line in $log) {
    $parts = $line -split '\|', 3
    [pscustomobject]@{ Hash = $parts[0]; Date = [datetime]$parts[1]; Subject = $parts[2] }
}
$rows = @($rows)
"window: since $Since    commits: $($rows.Count)"
if ($rows.Count -eq 0) { throw "no commits since $Since" }

Section 'commit types'
$types = $rows | Group-Object {
    if ($_.Subject -match '^([a-z]+)(\(|:|$)') { $matches[1] } else { 'other' }
}
$types | Sort-Object Count -Descending | ForEach-Object {
    "{0,-10} {1,4}  {2,5:P1}" -f $_.Name, $_.Count, ($_.Count / $rows.Count)
}

$fix = @($rows | Where-Object { $_.Subject -match '^fix' })
"fix commits: $($fix.Count) / $($rows.Count)  ({0:P1})" -f ($fix.Count / $rows.Count)

Section "fix hotspots (top $TopFiles files touched by fix commits)"
$filesByFix = @{}
foreach ($c in $fix) {
    $files = GitOut @('show', '--name-only', '--pretty=format:', $c.Hash) | Where-Object { $_ }
    foreach ($f in $files) {
        if (-not $filesByFix.ContainsKey($f)) { $filesByFix[$f] = 0 }
        $filesByFix[$f]++
    }
}
$filesByFix.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First $TopFiles |
    ForEach-Object { "{0,4}  {1}" -f $_.Value, $_.Key }

Section "fix storms (same day + same module, >= $StormThreshold)"
$fix | ForEach-Object {
    $mod = if ($_.Subject -match '^fix\(([^)]+)\)') { $matches[1] } else { 'none' }
    [pscustomobject]@{ Day = $_.Date.ToString('yyyy-MM-dd'); Module = $mod }
} | Group-Object Day, Module | Where-Object { $_.Count -ge $StormThreshold } |
    Sort-Object Count -Descending | ForEach-Object { "{0,3}  {1}" -f $_.Count, $_.Name }

Section 'rework rate (fix touching a file that another fix touched within 7 days)'
$lastFix = @{}
$reworkCommits = 0
$reworkPairs = @{}
foreach ($c in ($fix | Sort-Object Date)) {
    $files = @(GitOut @('show', '--name-only', '--pretty=format:', $c.Hash) | Where-Object { $_ })
    $hit = @()
    foreach ($f in $files) {
        if ($lastFix.ContainsKey($f) -and (($c.Date - $lastFix[$f]).TotalDays -le 7)) {
            $hit += $f
            if (-not $reworkPairs.ContainsKey($f)) { $reworkPairs[$f] = 0 }
            $reworkPairs[$f]++
        }
    }
    if ($hit.Count -gt 0) { $reworkCommits++ }
    foreach ($f in $files) { $lastFix[$f] = $c.Date }
}
if ($fix.Count -gt 0) {
    "rework commits: $reworkCommits / $($fix.Count)  ({0:P1})" -f ($reworkCommits / $fix.Count)
}
$reworkPairs.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 10 |
    ForEach-Object { "{0,4}  {1}" -f $_.Value, $_.Key }

Section 'release cadence'
$tagRows = @(GitOut @('for-each-ref', '--sort=creatordate', '--format=%(creatordate:short)', 'refs/tags') |
    Where-Object { $_ })
$tagRows | Group-Object | Sort-Object Name | ForEach-Object {
    $flag = if ($_.Count -ge 2) { '  <-- multi-release day' } else { '' }
    "{0}  {1}{2}" -f $_.Name, $_.Count, $flag
}

$tags = @(GitOut @('for-each-ref', '--sort=-creatordate', '--format=%(refname:short)', 'refs/tags') |
    Where-Object { $_ })
$singleFixReleases = 0
"`nlast releases (range -> commits / fix / feat):"
for ($i = 0; $i -lt [Math]::Min(12, [Math]::Max(0, $tags.Count - 1)); $i++) {
    $new = $tags[$i]; $old = $tags[$i + 1]
    $n = [int](GitOut @('rev-list', "$old..$new", '--count'))
    $f = @(GitOut @('log', "$old..$new", '--grep=^fix', '--oneline') | Where-Object { $_ }).Count
    $g = @(GitOut @('log', "$old..$new", '--grep=^feat', '--oneline') | Where-Object { $_ }).Count
    $mark = ''
    if ($n -le 2 -and $f -ge 1 -and $g -eq 0) { $mark = '  <-- one fix = one release'; $singleFixReleases++ }
    "{0,-8} <- {1,-8} commits={2,-3} fix={3,-3} feat={4}{5}" -f $new, $old, $n, $f, $g, $mark
}
"single-fix releases in the last 12: $singleFixReleases"

Section 'gate inventory (SKIP sites)'
$skips = @(Get-ChildItem -Path (Join-Path $root 'rs-*') -Recurse -Filter *.rs -ErrorAction SilentlyContinue |
    Select-String -Pattern 'SKIP\(')
"SKIP sites: $($skips.Count) in $(@($skips.Path | Sort-Object -Unique).Count) file(s)"
$skips | Group-Object Path | Sort-Object Count -Descending | ForEach-Object {
    "{0,3}  {1}" -f $_.Count, ($_.Name -replace [regex]::Escape($root + [IO.Path]::DirectorySeparatorChar), '')
}
"reminder: `cargo test` may be green while these paths never ran; run with --nocapture and read the SKIP lines."
"(version anchors: cargo test -p rs-gui --test release_sync)"

