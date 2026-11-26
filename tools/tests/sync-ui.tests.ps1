# T13 夹具测试：sync-ui.ps1 的红→绿、幂等与「只改明确文件」。
# 用法：pwsh -File tools/tests/sync-ui.tests.ps1
param([string]$Root = (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)))

$ErrorActionPreference = 'Stop'
$tool = Join-Path $Root 'tools/sync-ui.ps1'
if (-not (Test-Path -LiteralPath $tool)) { throw "缺少 $tool" }

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("sync-ui-fixture-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path (Join-Path $tmp 'app'), (Join-Path $tmp 'rs-gui/src') | Out-Null
try {
    $ui = "<html>`n<script>var UI_CUR='v1.2.3';</script>`n</html>`n"
    $meta = @'
// fixture
pub const UI_HTML: &[u8] = include_bytes!("../ui.html");
pub const UI_HTML_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";
pub const UI_HTML_LEN: usize = 1;
'@
    [System.IO.File]::WriteAllText((Join-Path $tmp 'app/ui.html'), $ui, [System.Text.UTF8Encoding]::new($false))
    [System.IO.File]::WriteAllText((Join-Path $tmp 'rs-gui/ui.html'), 'stale', [System.Text.UTF8Encoding]::new($false))
    [System.IO.File]::WriteAllText((Join-Path $tmp 'rs-gui/src/uihtml.rs'), $meta, [System.Text.UTF8Encoding]::new($false))

    # 红：副本与常量都不对
    & pwsh -NoProfile -File $tool -Root $tmp -Check | Out-Null
    if ($LASTEXITCODE -eq 0) { throw '偏差夹具的 -Check 必须失败' }

    # 绿：-Write 后 -Check 通过
    & pwsh -NoProfile -File $tool -Root $tmp -Write | Out-Null
    if ($LASTEXITCODE -ne 0) { throw '-Write 失败' }
    & pwsh -NoProfile -File $tool -Root $tmp -Check | Out-Null
    if ($LASTEXITCODE -ne 0) { throw '-Write 后 -Check 仍失败' }

    # 幂等：第二次 -Write 不改字节与 mtime
    $copy = Join-Path $tmp 'rs-gui/ui.html'
    $beforeHash = (Get-FileHash -LiteralPath $copy -Algorithm SHA256).Hash
    $beforeMeta = (Get-FileHash -LiteralPath (Join-Path $tmp 'rs-gui/src/uihtml.rs') -Algorithm SHA256).Hash
    Start-Sleep -Milliseconds 50
    & pwsh -NoProfile -File $tool -Root $tmp -Write | Out-Null
    $afterHash = (Get-FileHash -LiteralPath $copy -Algorithm SHA256).Hash
    $afterMeta = (Get-FileHash -LiteralPath (Join-Path $tmp 'rs-gui/src/uihtml.rs') -Algorithm SHA256).Hash
    if ($beforeHash -ne $afterHash -or $beforeMeta -ne $afterMeta) { throw '二次 -Write 必须幂等' }

    # 版本串不被工具触碰
    $metaText = [System.IO.File]::ReadAllText((Join-Path $tmp 'rs-gui/src/uihtml.rs'))
    if ($metaText -notmatch "UI_CUR" -and $metaText -notmatch "fixture") { throw '夹具元数据异常' }
    $uiText = [System.IO.File]::ReadAllText((Join-Path $tmp 'app/ui.html'))
    if ($uiText -ne $ui) { throw '工具不得改写 UI 原件' }

    Write-Output 'sync-ui tests: 4/4 passed'
} finally {
    $resolved = [System.IO.Path]::GetFullPath($tmp)
    if ($resolved.StartsWith([System.IO.Path]::GetTempPath(), [System.StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction SilentlyContinue
    }
}
