# 降低接手成本——最终验证报告

- 分支：`simplify-project-handoff`（隔离 worktree）
- 基线：`0daefd9d5cc218321a4ee7d36d1b6d9148221bd3`
- 交付：T0–T14 分阶段提交，未 push、未打 tag、未发 Release、未换装、未升版
- 验收入口：`pwsh -File tools/verify-cognitive-contracts.ps1`（A01–A12 固定映射，缺目标/缺用例/空扫描/出现 SKIP/非零退出都失败）

## 一、A01–A12 实际业务组合测试

| 验收 | 场景 | 命令 | 结果 | 红→绿证据 |
|---|---|---|---|---|
| A01 | WorkBuddy 流头按协议完成度判定，不把块边界当帧边界 | `cargo test -p rs-codebuddy --test stream_head_contract` | 5/5 | 首版以块边界误判，HTTP split `dat`/`a:` 用例转红后修绿 |
| A02 | WorkBuddy 初选/换号都按最终 `sent_model` 资格 | `cargo test -p rs-codebuddy --test model_routing_contract` | 5/5 | 黑名单模型被换号绕过用例先红 |
| A03 | 码道稳定账号 ID、别名与 AK 轮换 | `cargo test -p rs-codearts --test account_lifecycle` | 14/14 | 无 domainId 同名合并用例先红；同 tenant 不同用户合并为复核新增回归 |
| A04 | 码道单次续期共享（并发入口 + 两个真实子进程） | 同上 | 14/14 | 首个并发入口各换一次 STS 先红；两进程单 STS 用真实子进程验证 |
| A05 | 码道瞬态/确定性失败与 health revision 恢复 | 同上 | 14/14 | 旧 health 绑定旧 revision 用例先红 |
| A06 | 码道提交前业务错误交回账号策略、提交后不重放 | `cargo test -p rs-codearts --test chat_recovery` | 4/4 | 首 SSE 已交付仍换号重放用例先红 |
| A07 | UI 区域上下文隔离旧响应（WorkBuddy/Qoder） | `node --test app/tests/ui-state.test.cjs` | 7/7 | 旧地区晚到响应污染新区先红 |
| A08 | hostApi 单一入口 + 键位持久化/失败回读 | 同上 + `cargo test -p rs-gui --lib keymap::` | 7/7 + 4/4 | 保存失败被谎报成功先红；恢复默认只写快路为复核新增回归 |
| A09 | 插件外部接管/自有子进程/generation | `cargo test -p rs-gui --lib plugin_hosting_tests::` | 9/9 | 旧 watcher 晚回调清掉新实例先红 |
| A10 | 换血计划：成功标记 vs 已识别旧镜像 vs 无操作 | `cargo test -p rs-gui --lib rollover::tests::` | 12/12 | 无标记时猜端口/未知镜像先红 |
| A11 | 团结请求生命周期：路由、load、registry、pacing | `cargo test -p rs-tuanjie --test p8b4_chat_behavior` | 28/28 | 流取消不转移 finish 所有权先红（结果 415ms/残留条目） |
| A12 | COMMAND 三协议真实增量交付 | `cargo test -p rs-proxy --test live_stream_contract` | 6/6 | 改回缓冲路径首帧 415ms 先红；取消后仍抽干上游先红 |

合计 A 场景实际执行 **0 跳过**；上表每条都跑过至少一次“真实缺陷红 → 修复绿”。

## 二、全量回归与基线对照

| 项 | 基线（恢复 CRLF 夹具后） | 本分支最终 |
|---|---|---|
| `cargo test --workspace --locked --no-fail-fast` 通过 | 971 | **1028** |
| 失败 | 1（既有 DPAPI 夹具） | 1（同一题，未修，超出本计划范围） |
| ignored | 3 | 3（同三条） |
| SKIP 输出 | 有（qoder DPAPI 换机等） | A01–A12 路径 0；`rs-qoder` 仅在非 Windows/DPAPI 不可用时仍显式 SKIP |

- 唯一失败：`rs-gui::dpapi::tests::decrypts_a_go_written_blob`——样例不是 `PDAPI1` 格式，
  基线即失败；本次未改 `rs-gui/src/dpapi.rs`，按边界保留并记录，未静默跳过。
- 新增 57 条测试全部在 A01–A12 与工具自检内；无删除、无放宽、无新增 ignore/skip。
- 复现：`$env:CARGO_TARGET_DIR='E:\cargo-target\workspace'; cargo test --workspace --locked --no-fail-fast`
  （日志 `.work/cognitive-simplification/t14c-workspace.log`）。

## 三、五条职责链复核

首次只读复核（reviewer 子代理，面对真实 diff）结论与处置：

1. **COMMAND 增量交付**：唯一入口 `handle_*`，状态在每次请求的翻译任务内，提交点=首帧发出；
   复核指出缓冲副本已无调用方 → 已删除（`9aa04fa`），门禁/黄金与 HTTP 同走 live。
   取消时 `ChannelSink::is_closed()`（含 `tx.is_closed()`）中断三条翻译循环（`bc4d253`）。
2. **WorkBuddy 流头/模型路由**：无 MUST-FIX；初选与换号统一按 `sent_model`。
3. **码道账号/恢复**：稳定 ID、revision、OS 锁单飞续期成立；复核发现同 tenant 不同用户
   会按 domain 合并（数据丢失）→ 已改为 domain+显示名双条件并加回归（`77f0ac9`）。
4. **团结生命周期**：无手工配对残留（复核 grep 确认），load 只绑初选账号，
   流式 finish 所有权随流；凭证链文件零 diff。
5. **GUI/UI**：区域 generation/旧响应丢弃、插件 generation/停止回滚成立；复核发现
   “恢复默认”只写快路并谎报已持久化 → 改走 `kmPersist`（失败回读文件），并补 Node 合同（`086b95a`），
   保存成功后重建键区（`bc4d253`）。

二次只读复核：F1–F4 均判定 FIXED，无剩余 MUST-FIX；列出的文档/死代码残留已在
`bc4d253` 清掉。保留为 LEAD（未在本计划内改行为）：
- 码道 pre-commit 5xx 会冷却并换号（提交前无字节，属可辩护策略，缺专门裁决测试）；
- 每请求仍会取账号锁+配置锁做续期协调（正确性已由 A04 覆盖，热路径开销未优化）；
- 配置锁在 tokio 线程同步等待（持锁短，极端争用有阻塞风险）；
- `ui_html` 门禁只锁定 `PTApp.hostApi.call` 形态，不穷举历史 `window.cc*` 调用。

## 四、保护项核对

- `**/tests/golden/**` 冻结数据无改动；`CHANGELOG*.md`、`version.rc`、版本锚点未动
  （`release_sync` / `ui_html` 门禁通过）。
- 团结凭证链文件（`client.rs`/`reqshape.rs`/`headers.rs`/`session.rs`/`crypto.rs`）零 diff；
  仅 `gojson.rs` 补 `\b`/`\f` 转义与冻结黄金对齐（非签名/会话/UA 字节）。
- 未启动/停止/换装任何生产服务；未读写真实账号/凭据/配额；测试只用临时目录、随机回环端口、
  合成账号与假上游/STS/worker。
- UI 副本由 `tools/sync-ui.ps1 -Write` 生成并 `-Check` 通过（sha256/len 与源一致）。

## 五、可复现命令

```powershell
$env:CARGO_TARGET_DIR='E:\cargo-target\workspace'
cargo test --workspace --locked --no-fail-fast
pwsh -File tools/verify-cognitive-contracts.ps1     # 19/19，A01–A12 + 工具门禁
node --test app/tests/ui-state.test.cjs             # 7/7
node tools/check-ui-js.cjs
pwsh -File tools/sync-ui.ps1 -Check
pwsh -File tools/tests/sync-ui.tests.ps1            # 4/4
cargo run --manifest-path tools/compat-vectors/Cargo.toml  # 6 实现 × 8 输入逐字节一致
```

## 六、边界与后续

- 本分支未 push、未合并、未打 tag、未发 Release、未替换运行中的 `ProxyDeck.exe`；worktree 保留供验收。
- 版本号与 CHANGELOG 按“攒版发布”纪律未动；本批属“下一版待办”。
- 唯一已知失败为基线既有 DPAPI 夹具题，修复它需要新样例或改动测试夹具，超出本次授权边界，
  已在 `PROGRESS.md` 与本文记录，未掩盖。
