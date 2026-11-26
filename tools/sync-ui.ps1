# sync-ui.ps1 —— UI 唯一源 → 内嵌副本 + 哈希/长度常量同步。
#
# -Check : 只校验（副本逐字节、常量哈希/长度与原件一致）；不一致 exit 1。
# -Write : 复制 app/ui.html → rs-gui/ui.html，并只改 uihtml.rs 的 UI_HTML_SHA256 / UI_HTML_LEN。
#
# 范围红线：不升版本、不改 CHANGELOG、不改其它常量；文件缺失直接失败。
param(
    [string]$Root = (Split-Path -Parent $PSScriptRoot),
    [switch]$Check,
    [switch]$Write
)

$ErrorActionPreference = 'Stop'
if (-not $Check -and -not $Write) { throw '必须指定 -Check 或 -Write' }
if ($Check -and $Write) { throw '-Check 与 -Write 不能同时使用' }

$source = Join-Path $Root 'app/ui.html'
$copy = Join-Path $Root 'rs-gui/ui.html'
$meta = Join-Path $Root 'rs-gui/src/uihtml.rs'
foreach ($p in @($source, $copy, $meta)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "缺少文件: $p" }
}

function Get-Sha256Hex([byte[]]$Bytes) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    ($sha.ComputeHash($Bytes) | ForEach-Object { $_.ToString('x2') }) -join ''
}

$srcBytes = [System.IO.File]::ReadAllBytes($source)
$hash = Get-Sha256Hex $srcBytes
$len = $srcBytes.Length

if ($Write) {
    [System.IO.File]::WriteAllBytes($copy, $srcBytes)
    $text = [System.IO.File]::ReadAllText($meta)
    $new = [regex]::Replace($text, 'pub const UI_HTML_SHA256: &str = "[0-9a-f]+";',
        "pub const UI_HTML_SHA256: &str = `"$hash`";")
    $new = [regex]::Replace($new, 'pub const UI_HTML_LEN: usize = \d+;',
        "pub const UI_HTML_LEN: usize = $len;")
    if ($new -notmatch [regex]::Escape($hash) -or $new -notmatch "UI_HTML_LEN: usize = $len;") {
        throw '更新 uihtml.rs 常量失败（锚点形态变了？）'
    }
    [System.IO.File]::WriteAllText($meta, $new, [System.Text.UTF8Encoding]::new($false))
}

# 校验：副本逐字节 + 常量与原件一致（Write 之后同一套检查必须通过）
$copyBytes = [System.IO.File]::ReadAllBytes($copy)
$same = $copyBytes.Length -eq $srcBytes.Length
if ($same) {
    for ($i = 0; $i -lt $srcBytes.Length; $i++) {
        if ($copyBytes[$i] -ne $srcBytes[$i]) { $same = $false; break }
    }
}
$metaText = [System.IO.File]::ReadAllText($meta)
$hashMatch = [regex]::Match($metaText, 'pub const UI_HTML_SHA256: &str = "([0-9a-f]+)";')
$lenMatch = [regex]::Match($metaText, 'pub const UI_HTML_LEN: usize = (\d+);')
if (-not $hashMatch.Success -or -not $lenMatch.Success) {
    throw 'uihtml.rs 缺少 UI_HTML_SHA256 / UI_HTML_LEN 锚点'
}
$ok = $same -and ($hashMatch.Groups[1].Value -eq $hash) -and ([int]$lenMatch.Groups[1].Value -eq $len)
if (-not $ok) {
    Write-Output "MISMATCH copy_same=$same const_sha=$($hashMatch.Groups[1].Value) want_sha=$hash const_len=$($lenMatch.Groups[1].Value) want_len=$len"
    exit 1
}
Write-Output "OK sha256=$hash len=$len"
exit 0
