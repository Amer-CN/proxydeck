# verify-cognitive-contracts.ps1 —— A01–A12 唯一验收入口。
#
# 固定映射（缺目标、缺用例、零扫描、出现 SKIP 或非零退出码 = 失败）：
#   A01 rs-codebuddy stream_head_contract      A07 node ui-state（区域/防过期）
#   A02 rs-codebuddy model_routing_contract    A08 node ui-state + rs-gui keymap 单测
#   A03 rs-codearts account_lifecycle          A09 rs-gui 插件托管行为
#   A04/A05 rs-codearts account_lifecycle      A10 rs-gui 换血计划单测
#   A06 rs-codearts chat_recovery              A11 rs-tuanjie p8b4_chat_behavior
#   A12 rs-proxy live_stream_contract
# 外加：UI 同步 -Check、内联 JS 语法、兼容原语向量、ui_html、release_sync。
param(
    [string]$Root = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'
$logDir = Join-Path $Root '.work/cognitive-simplification'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$log = Join-Path $logDir "verify-$stamp.log"
$failures = New-Object System.Collections.Generic.List[string]
$rows = New-Object System.Collections.Generic.List[object]

function Invoke-Step {
    param(
        [string]$Id,
        [string]$File,
        [string[]]$ArgList,
        [string[]]$Expect,
        [int]$MinPass = 1,
        [string[]]$MustContain = @()
    )
    $display = "$File $($ArgList -join ' ')"
    $out = & $File @ArgList 2>&1 | Out-String
    $code = $LASTEXITCODE
    Add-Content -LiteralPath $log -Value "`n===== $Id : $display (exit $code) =====`n$out"
    $problems = New-Object System.Collections.Generic.List[string]
    if ($code -ne 0) { $problems.Add("exit=$code") }
    if ($out -match 'SKIP\(') { $problems.Add('出现 SKIP(') }
    if ($out -match 'test result: FAILED') { $problems.Add('test result FAILED') }
    foreach ($e in $Expect) {
        $caseOk = [regex]::IsMatch($out, "test\s+\S*" + [regex]::Escape($e) + "\s+\.\.\.\s+ok")
        if (-not $caseOk) {
            if ($out -notmatch [regex]::Escape($e)) { $problems.Add("缺少用例 $e") }
            else { $problems.Add("用例 $e 未显示 ok") }
        }
    }
    if ($out -match 'ℹ fail (\d+)' -and [int]$Matches[1] -ne 0) {
        $problems.Add("node fail=$($Matches[1])")
    }
    foreach ($m in $MustContain) {
        if (-not $out.Contains($m)) { $problems.Add("缺少标记 '$m'") }
    }
    if ($MinPass -gt 0) {
        $sum = 0
        foreach ($m in [regex]::Matches($out, '(\d+) passed')) { $sum += [int]$m.Groups[1].Value }
        foreach ($m in [regex]::Matches($out, 'pass (\d+)')) { $sum += [int]$m.Groups[1].Value }
        if ($sum -lt $MinPass) { $problems.Add("通过数 $sum < $MinPass") }
    }
    $ok = $problems.Count -eq 0
    $rows.Add([pscustomobject]@{ Id = $Id; Command = $display; Ok = $ok; Detail = ($problems -join '; ') })
    if (-not $ok) { $failures.Add("$Id ($display): $($problems -join '; ')") }
    Write-Output ("{0} {1} {2}" -f ($(if ($ok) { 'PASS' } else { 'FAIL' })), $Id, $display)
}

$cargo = 'cargo'
$node = 'node'

# ---- A01–A06：平台行为 ----
Invoke-Step 'A01' $cargo @('test','-p','rs-codebuddy','--locked','--test','stream_head_contract','--','--nocapture') `
    @('split_data_prefix_does_not_resend_the_account') 5
Invoke-Step 'A02' $cargo @('test','-p','rs-codebuddy','--locked','--test','model_routing_contract','--','--nocapture') `
    @('model_block_is_remembered_on_next_http_request_without_poisoning_other_models','fallback_rewrite_selects_account_for_the_sent_model') 5
Invoke-Step 'A03' $cargo @('test','-p','rs-codearts','--locked','--test','account_lifecycle','--','--nocapture') `
    @('ak_rotation_and_reload_preserve_id_aliases_and_other_accounts','no_domain_same_name_accounts_keep_distinct_stable_ids') 13
Invoke-Step 'A04' $cargo @('test','-p','rs-codearts','--locked','--test','account_lifecycle','--','--nocapture') `
    @('concurrent_entries_share_one_sts_rotation_and_latest_credentials','two_processes_share_one_sts_rotation') 13
Invoke-Step 'A05' $cargo @('test','-p','rs-codearts','--locked','--test','account_lifecycle','--','--nocapture') `
    @('refresh_errors_distinguish_transient_definitive_and_success','server_health_is_bound_to_credential_revision_and_recovers_on_new_success') 13
Invoke-Step 'A06' $cargo @('test','-p','rs-codearts','--locked','--test','chat_recovery','--','--nocapture') `
    @('http_json_and_sse_errors_share_account_recovery_policy','nonstream_late_error_recovers_before_commit_but_stream_never_replays_after_head') 4

# ---- A07–A08：UI / 键位 ----
Invoke-Step 'A07' $node @('--test','app/tests/ui-state.test.cjs') @() 6 @('production WorkBuddy account script discards late CN response after INTL switch')
Invoke-Step 'A08' $node @('--test','app/tests/ui-state.test.cjs') @() 6 @('A08 file value wins over fast path and save failure is not reported as saved')
Invoke-Step 'A08-keymap' $cargo @('test','-p','rs-gui','--locked','--lib','keymap::','--','--nocapture') `
    @('save_failure_is_reported_not_swallowed','migration_fills_opencode_once_and_respects_later_removal') 4

# ---- A09–A10：宿主生命周期 / 换血计划 ----
Invoke-Step 'A09' $cargo @('test','-p','rs-gui','--locked','--lib','plugin_hosting_tests::','--','--nocapture') `
    @('adopted_endpoint_exit_allows_a_fresh_start','spawned_child_without_listener_is_cleaned_up_by_pid','stop_failure_restores_pid_and_token') 6
Invoke-Step 'A10' $cargo @('test','-p','rs-gui','--locked','--lib','rollover::tests::','--','--nocapture') `
    @('update_plan_source_is_explicit_and_never_guesses','core_port_and_unknown_images_are_never_targets') 6

# ---- A11 / A12：团结生命周期 / COMMAND 增量 ----
Invoke-Step 'A11' $cargo @('test','-p','rs-tuanjie','--locked','--test','p8b4_chat_behavior','--','--nocapture') `
    @('route_comes_from_selection_not_account_count','stream_cancel_releases_registry_entry_without_replay') 28
Invoke-Step 'A12' $cargo @('test','-p','rs-proxy','--locked','--test','live_stream_contract','--','--nocapture') `
    @('chat_stream_delivers_first_frame_before_upstream_finishes','anthropic_stream_delivers_message_start_before_upstream_finishes') 5

# ---- 工具与合同门禁 ----
Invoke-Step 'UI-SYNC' 'pwsh' @('-NoProfile','-File','tools/sync-ui.ps1','-Root',$Root,'-Check') @() 0 @('OK sha256=')
Invoke-Step 'SYNC-SELFTEST' 'pwsh' @('-NoProfile','-File','tools/tests/sync-ui.tests.ps1','-Root',$Root) @() 0 @('sync-ui tests: 4/4 passed')
Invoke-Step 'UI-JS' $node @('tools/check-ui-js.cjs') @() 0 @('个内联脚本语法通过')
Invoke-Step 'COMPAT-VECTORS' $cargo @('run','--manifest-path','tools/compat-vectors/Cargo.toml') @() 0 @('逐字节一致')
Invoke-Step 'UI-HTML' $cargo @('test','-p','rs-gui','--locked','--test','ui_html','--','--nocapture') `
    @('embedded_ui_html_is_byte_identical_to_app_ui_html') 4
Invoke-Step 'RELEASE-SYNC' $cargo @('test','-p','rs-gui','--locked','--test','release_sync','--','--nocapture') `
    @('every_version_anchor_matches_app_version') 1

Write-Output ''
Write-Output ('验收日志: ' + $log)
if ($failures.Count -gt 0) {
    Write-Output ''
    Write-Output "FAILED $($failures.Count) 项："
    foreach ($f in $failures) { Write-Output (" - " + $f) }
    exit 1
}
Write-Output "全部 $($rows.Count) 项认知成本合同通过（A01–A12 实际执行为 0 跳过）"
exit 0
