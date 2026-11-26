# ProxyDeck 降低接手成本 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `executing-plans` to implement this plan task-by-task. 若用户另行选择 `subagent-driven-development`，只读评审可以委派，写入仍由单个执行窗口串行整合。Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让关键行为从一个职责入口可追踪，消除已确认的非局部状态合同冲突，同时保持既有客户端、配置和受保护协议可用。

**Architecture:** 先用组合测试确认审查线索，再依次收拢账号、UI 与进程生命周期状态，分离响应提交和恢复决策。只共享经证明一致的纯兼容原语，保留平台策略；每个任务独立验证、提交。

**Tech Stack:** Rust workspace、Tokio、Axum、现有 reqwest/serde 依赖、WebView2/wry/tao、内联 JavaScript、Node 内置测试设施、PowerShell。

**Spec:** [设计与范围](../specs/2026-11-26-cognitive-cost-reduction-design.md)。先读设计，再执行本计划；用户粘贴配套提示词即授权按此计划实施。

## Global Constraints

- 审查基准 `0daefd9`；真实实现以执行前 HEAD 为准。行号只作定位线索，先读再改。
- 一个实施者串行写入。先用 `using-git-worktrees` 建隔离分支；不混入其他窗口未提交改动；不自动合并、push 或发版。
- 先完整读[项目纪律](../../../CLAUDE.md)。本任务不授权停服、换装、覆盖/改名成品 exe、真实更新换血、真实登录/续期/额度请求、权限或 DSH 配置修改。
- 测试只用合成账号、随机回环端口、临时目录、假 STS/上游/worker/后端进程；运行测试前检查它不会读取真人凭据或操作生产端口。
- 不更改任何版本值、历史变更日志、冻结 `tests/golden/**`、已有比较豁免或 CI 判定标准。UI 改动同步原件、副本、哈希及长度；不手改副本。
- 不能删测试、增加跳过、降低断言、mock 被测策略或忽略非零退出码。修复本文目标缺陷若需改旧断言，必须证明旧断言固定了错误语义，保留其余字节、时序预算和行为检查，并在报告列出精确差异。
- 团结签名/会话/UA/请求形态红线完全保留；本轮不改模型目录、上游账号政策、监听鉴权、GUI 视觉或动画。
- 不新增外部依赖或运行环境；标准库及已锁定依赖足够。共享纯 Rust crate 仅在 T12 的证据条件成立时允许。
- Cargo 缓存沿用现有共享配置；隔离树无本机配置时使用 `E:\cargo-target\workspace`，不另建逐 crate target、不清缓存。PowerShell 变量用任务专属名字，不能覆盖 `$HOME` 等自动变量。
- 执行进度写隔离树根的 `PROGRESS.md`，问题写 `BLOCKED.md`；原始无密钥日志放 `.work/cognitive-simplification/`，可持久复核的结论写最终报告。

## Review Focus

1. 上游网络块恰好截在 `dat`、UTF-8 或 JSON 中间：不能把块边界当成 SSE 帧/完整响应边界。归 T2、T11。
2. 单次轮转 refresh token 被 chat、quota、后台同时使用：应共享一次续期且复用最新凭据。归 T4、T5。
3. 区域切换、异步文件读取或保存失败晚到：不能污染新区缓存或伪报已保存。归 T7、T8。
4. 外部接管实例退出、停止失败、旧 watcher 晚回调：不能卡死启动或清掉新实例状态。归 T9。
5. 流头提交后取消/读错/工具帧：不能重放已经输出的请求；计数和凭据租借必须结束。归 T6、T10、T11。

---

## 工作包和文件归属

不为达到文件行数而拆分；新模块必须实际接入生产路径，不能保留两套同样职责的实现。以下新文件是职责落点；若当前代码演进导致更合适名称，可以调整并说明，不重新扩大设计。

| 工作包 | 现有修改范围 | 建议新职责与测试文件 |
|---|---|---|
| T1 低风险语义清理 | [团结 pacing](../../../rs-tuanjie/src/pacing.rs)、[COMMAND 状态](../../../rs-proxy/src/lib.rs)、[Comate 构造](../../../rs-comate/src/lib.rs)、相关模块头与调用点 | 不新增框架 |
| T2–T3 WorkBuddy | [chat](../../../rs-codebuddy/src/chat.rs)、[failover](../../../rs-codebuddy/src/failover.rs)、[pool](../../../rs-codebuddy/src/pool.rs)、现有 HTTP 测试 | `rs-codebuddy/tests/stream_head_contract.rs`、`model_routing_contract.rs` |
| T4–T6 码道 | [config](../../../rs-codearts/src/config.rs)、[store](../../../rs-codearts/src/store.rs)、[refresh](../../../rs-codearts/src/refresh.rs)、[lib](../../../rs-codearts/src/lib.rs)、[sse](../../../rs-codearts/src/sse.rs) | `src/accounts.rs`、`src/account_lock.rs`、`src/chat.rs`、`src/response.rs`；`tests/account_lifecycle.rs`、`chat_recovery.rs` |
| T7–T8 UI/桥 | [UI 原件](../../../app/ui.html)、[桥](../../../rs-gui/src/bridge.rs)、[IPC](../../../rs-gui/src/webview.rs)、对应合同测试 | `rs-gui/src/keymap.rs`、`app/tests/ui-state.test.cjs`、`tools/check-ui-js.cjs`；UI 模块仍嵌在原件中 |
| T9 宿主生命周期 | [plugins](../../../rs-gui/src/plugins.rs)、[update](../../../rs-gui/src/update.rs)、[rollover](../../../rs-gui/src/rollover.rs)、[托管测试](../../../rs-gui/src/plugin_hosting_tests.rs) | 按需 `src/plugin_runtime.rs`，只保存实际状态机 |
| T10 团结执行 | [chat](../../../rs-tuanjie/src/chat.rs)、[server](../../../rs-tuanjie/src/server.rs)、[inflight](../../../rs-tuanjie/src/inflight.rs)、现有行为测试 | `src/request_lifecycle.rs` |
| T11 COMMAND 流交付 | [HTTP](../../../rs-proxy/src/http.rs)、[chat](../../../rs-proxy/src/chat.rs)、[responses](../../../rs-proxy/src/responses.rs)、[anthropic](../../../rs-proxy/src/anthropic.rs)、[stream](../../../rs-proxy/src/stream.rs)、现有测试 | `src/output.rs`、`tests/live_stream_contract.rs` |
| T12 兼容/可移植测试 | 已证明同义的编码模块；[Qoder auth](../../../rs-qoder/src/auth.rs)、[测试辅助](../../../rs-qoder/tests/common/mod.rs)、非冻结测试源码 | 纯算法测试、运行时合成夹具；共享 crate 需先满足证据条件 |
| T13 自动同步/交接 | [嵌入元数据](../../../rs-gui/src/uihtml.rs)、模块 README、[项目说明](../../../CLAUDE.md)的非历史部分 | `tools/sync-ui.ps1`、`tools/verify-cognitive-contracts.ps1`、`docs/development-map.md`、`docs/cognitive-simplification-verification.md` |

## 明确的行为验收清单

| ID | 必须实际触发的行为 | 归属 |
|---|---|---|
| A01 | 合法 SSE 被切成 `dat` / `a:` / JSON 分片后，不误换号，字节完整；EOF/预算耗尽区别明确 | T2 |
| A02 | 首次请求记住账号 A 不支持模型 M 后，下一次 M 请求从 HTTP 入口不再先打 A；其他模型仍可用 A | T3 |
| A03 | 仍有记录的无 domainId 账号在 AK 轮换、重新加载后保持稳定归属；遗留删除字符串按 T4 的精确匹配边界保留，不猜测跨轮换身份 | T4 |
| A04 | 同一过期账号在单 Server 多入口和两个共用临时数据目录的测试进程中，都只发一次实际 STS 轮换并复用新凭据 | T5 |
| A05 | 续期瞬时失败不判死；旧失败不得覆盖新凭据成功；明确的成功恢复可重新入选 | T5 |
| A06 | HTTP 错误与 HTTP200 内同业务错误走一致账号策略；流头提交后不换号重放 | T6 |
| A07 | CN 请求晚于切到 INTL 才返回时，不写 INTL 缓存或上屏；区域切换不停止另一地区 | T7 |
| A08 | 旧键位迁移幂等、文件优先、保存失败如实反馈；重启后不谎称保存成功 | T8 |
| A09 | 外部健康实例复用；退出后可恢复启动；停止失败恢复跟踪；旧 watcher 不污染新代 | T9 |
| A10 | 有更新标记和无标记的计划不同；空闲端口不启动、核心不换血、未知镜像无标记不动 | T9 |
| A11 | 团结池/本地回退、换号、失败、取消、流尽均正确清理，原有计数口径不被顺便改写 | T10 |
| A12 | COMMAND 三种入站协议均在假上游结束前交付首个有效增量；重试前置、黄金字节仍一致 | T11 |

A01–A12 全部为本地组合测试，新场景跳过数为 0。它们不是覆盖全项目的替代物，仍须执行各任务的既有合同测试。

## 通用任务循环

每个 T1–T13 使用以下步骤，具体用例和实现边界在各任务中补充：

- [ ] 读取目标实现及最近调用点，记录目标前后的状态/字节/副作用；基准变化先核实。
- [ ] 对行为缺陷写组合测试，在原代码上运行并保存失败原因；纯重构先用现状测试保护，不能伪造必红。
- [ ] 实施最小调整，再执行新场景和该模块现有测试，收集实际输出与退出码。
- [ ] 涉及新增检查时，在隔离树用最小突变证明会失败，再仅恢复自己的突变并复跑。不能借反向验证碰生产数据或覆盖其他人的文件。
- [ ] 核对 diff，更新 `PROGRESS.md`，精确 stage 本任务文件，提交 `refactor:` 或 `fix:`，正文注明“下一版待办；未升版、未换装”。不使用 `git add .`。

同一检查重复失败三次必须改用不同证据或诊断办法，不能反复同参调用；保留本任务进度并继续无依赖任务。若需要回退，只回退本任务自有改动，不能 `reset --hard` 或清理他人工作。未达到标准的任务保留为未完成。

### T0：隔离、核对基线和命令

**Files:** 只读现有仓库；在隔离树创建 `PROGRESS.md`、`BLOCKED.md` 和证据目录。

**Interfaces:** 产生 `base_commit`、独立分支/工作树路径、逐测试目标的 passed/failed/ignored/SKIP 基线；后续任务都引用此记录。

- [ ] 在原路径完整读取本设计、本计划和项目纪律，检查 `git status`、HEAD、已有工作树。若 HEAD 不是 `0daefd9`，检查后续提交与本计划交集；仅冲突项暂停，不重做整仓审查。
- [ ] 用 `using-git-worktrees` 创建 `simplify-project-handoff` 一类的新分支；名称冲突用唯一后缀，不能删除既有分支。两份任务文档可能尚未提交：先从原树读完，再仅复制本任务文档到隔离树，不带其他未提交文件。
- [ ] 核对 Cargo target 配置和可用命令。已有检查目标通过 `cargo metadata --offline --no-deps --format-version 1` 核实；本计划新目标在创建后才存在。
- [ ] 检查现有测试副作用，再建立工作区基线。候选命令：`cargo test --workspace --locked -- --nocapture`。编写方案阶段未实跑此命令；执行窗口必须核验可用性、捕获 `$LASTEXITCODE`，不能将本文当通过记录。
- [ ] 按测试目标记录失败和输出中的 SKIP，不仅记录 Cargo 最后的绿色汇总；环境不满足写清具体原因。原有失败不能归咎于本次，但影响路径未能验证时不得宣布已验证。
- [ ] 写不超过 10 行开工回执：目标、任务顺序、测试隔离、最大风险和禁区。`BLOCKED.md` 为空时写“无”。

### T1：清理已证明无用的中介、占位和过期说明

**Files:** 文件归属表 T1；跨调用点只改必要 import/名称。更新说明不能改历史日志。

**Interfaces:** 数据目录直接使用 `appdir::data_path(name: &str) -> PathBuf`；Comate 显式 `AppState::for_zulu(...) -> Self`、`AppState::for_cloud(...) -> Self`；`from_env()` 保持生产选择规则。若 `new()` 被库消费者/黄金脚本使用，保留一层清楚标注的兼容入口，不能擅删公共调用面。

- [ ] 核对 `pacing` 目录函数调用、`opencode_session` 字段所有使用，以及各构造器实际生产分支。无调用/无合同才删占位；有用字段不能凭名字删。
- [ ] 对 Comate 两种构造建立断言：显式 Cloud/ Zulu 的模型和 chat 选路正确，`from_env` 原默认不变，旧 Zulu 黄金仍经过 Zulu。
- [ ] 业务模块改为直接访问 `appdir`；移除不再有实际消费者的转发层。模块头写当前职责与不变量，移除错误的“未迁/未接入”现状。
- [ ] 验证：`cargo test -p rs-comate -p rs-proxy -p rs-tuanjie --locked -- --nocapture`。仅文本说明无需制造行为红灯；真实结构调整使用现有合同保护。
- [ ] 按通用任务循环提交。

### T2：修复 WorkBuddy 流头完成度合同

**Files:** T2 范围；新增 `stream_head_contract.rs`。

**Interfaces:** 在本插件定义 `HeadCompleteness::{Incomplete, Complete}` 和 `HeadProbe { bytes, completeness }`，`peek_within(...) -> HeadProbe` 与 `classify_stream_head(head, completeness)` 使用同一语义。保留对完整已知业务错误码的原判定，不把网络首块当成协议完整帧。

- [ ] 新测试 A01 使用真实 `ChunkReader`、真实 classifier，输入 `dat` 后再补 `a: {...}\n\n`；断言结果不为 EmptyStream、保留全部前缀和后续字节；加 HTTP 入口假上游断言不额外换号。
- [ ] 增加预算耗尽、EOF、完整错误帧、DONE 空结果样本；明确达到字节窗口仍可能是协议半帧，不能据此伪称帧完整。
- [ ] 在原代码上运行组合测试证明冲突。修复后若需替换原 `!truncated` 断言，保留“首字节不白等完整预算”和“字节不丢”的原检查，报告原断言冲突。
- [ ] 验证：`cargo test -p rs-codebuddy --locked --test stream_head_contract -- --nocapture`，再跑该 crate 全测试。
- [ ] 按通用任务循环提交。

### T3：让 WorkBuddy 初选和换号遵守同一模型资格

**Files:** T3 范围；新增 `model_routing_contract.rs`。

**Interfaces:** 继续使用现有 `AccountPool::pick_for_model(&str)`；请求上下文区分 `requested_model` 与最终 `sent_model`，以实际上游模型判断资格。

- [ ] A02 从真实 HTTP chat 入口连续发送：先让 A 对 M 返回 11158，再发 M；第二次不能先打 A。再发 N，A 不得因 M 的限制被全局排除。
- [ ] 加 fallback 改写前后的模型资格测试、CN/INTL 隔离、全池无匹配行为；不改变原有错误码含义和配置开关。
- [ ] 收拢初选与后续换号的资格入口，保留有限重试预算和既有负载记账。
- [ ] 验证新目标及 `cargo test -p rs-codebuddy --locked -- --nocapture`，按通用循环提交。

### T4：建立不随凭据轮换的码道账号 ID

**Files:** T4 范围；新增 `src/accounts.rs`、`src/account_lock.rs` 和 `tests/account_lifecycle.rs`。

**Interfaces:** `AccountId(String)` 是内部稳定键；`AccountSource::{Local, File}` 表示来源；`AccountSnapshot { id: AccountId, credentials: config::Account, source: AccountSource, revision: u64 }` 是不可变快照。管理器提供 `resolve_id(&self, legacy_or_current: &str) -> Option<AccountId>`、`snapshot(&self, id: &AccountId) -> Option<AccountSnapshot>` 和凭据替换入口。旧 `identity()` 保留为输入别名/兼容展示，不直接替换成新的 wire 值。新增可选 `account_id`、持久别名及版本元数据须向后兼容；只保证当前已知字段和这些新增字段的读写往返，当前实现本来不保留未知字段，本轮不扩张这一保证。

- [ ] A03 用缺 domainId 且仍有账号记录的两个不同账号，执行 AK 轮换和重新读盘：同一账号 ID 不变，不合并同名账号；启停/移除与运行态保持归属。
- [ ] 明确遗留删除记录的限制：升级前账号已从 accounts 删除、removed 仅剩旧 identity，且没有 domainId/稳定资料时，只保持原 identity 精确匹配，不能保证跨 AK 轮换识别，也不能按显示名猜测。为这条“不错误合并、不丢旧删除字符串”的边界写测试；稳定 ID/别名从本轮仍可识别的账号开始保存，后续删除保留该元数据。
- [ ] 用临时旧格式账号文件、旧 overlay、旧 removed 列表测试已知字段与新增 ID/别名的读—改—写—再读；覆盖同名、改显示名、多实例重新加载、ID 分配并发和落盘失败。
- [ ] `account_lock.rs` 提供 `acquire_account_lock(data_dir, id) -> Result<AccountFileGuard, LockError>` 和短期配置事务锁；锁通过进程退出自动释放的 OS 文件句柄实现。Windows 可用 `OpenOptionsExt::share_mode(0)`，文件残留不能等同于锁仍被占用；不加外部依赖、不改 ACL、不用跨 await 的普通 Mutex 或线程所有权互斥锁。
- [ ] 稳定 ID 分配和文件读改写受短期配置事务锁保护，重载后合并保存，避免两个实例分配不同 ID 或覆盖另一账号。已有 `health/benefit/unavailable/busy/last_touch/archived` 按稳定 ID 归属或明示迁移，不能留一部分旧键。
- [ ] 账号身份与凭据更新归账号管理边界；只迁移隔离测试副本，生产数据不执行迁移。
- [ ] 验证：`cargo test -p rs-codearts --locked --test account_lifecycle -- --nocapture` 和原 crate 测试，按通用循环提交。

### T5：按账号协调码道续期并定义恢复状态

**Files:** `accounts.rs`、`account_lock.rs`、现有 refresh/store/调用点、`account_lifecycle.rs`。依赖 T4。

**Interfaces:** `ensure_fresh(&self, id: &AccountId, observed_revision: u64, mode: RefreshMode) -> Result<AccountSnapshot, RefreshError>`；`RefreshMode::{IfExpired, Force}` 共用协调器。`RefreshError` 区分瞬时失败、绑定凭据版本的确定失效、存储失败和锁等待超时。调用者等待同一轮结果；按账号取得跨进程锁后重新读盘并检查版本，版本已更新就复用结果，包含并发 Force。不持有普通同步锁跨网络 await。

- [ ] A04 先测同一 Server 的 chat/quota/后台并发，再用两个实际测试子进程共用同一临时数据目录和假 STS；一个旧 refresh token 只允许一次成功。两个进程都必须读到同一个新版本，不能仅用两个线程或两个 mock 协调器替代跨进程验证。
- [ ] 网络请求期间只持有该账号 OS 文件锁；保存时短期取得配置事务锁，重新加载并合并账号文件和 overlay。不同账号并发保存不得互相覆盖；测试进程被终止后文件锁可再次取得，锁超时不能把账号判死。
- [ ] A05 覆盖网络/5xx 不标死；确定失效标记绑定凭据版本；旧失败不得覆盖新成功；明确成功的新凭据恢复可重新入选。保持本地桌面运行时不主动抢续期的规则。
- [ ] 将 ensure_fresh、force_refresh、后台路径接入同一协调器，保存账号文件和 overlay 的失败要可观察，不能返回假成功。只有持久化后才向等待者发布新版本，避免进程重启又消费旧 token。
- [ ] 新测试先红后绿，运行码道全测试；仅恢复本任务自有突变后再复跑，提交。

### T6：把码道提交前业务错误交回账号策略

**Files:** T6 范围；新增 `src/chat.rs`、`src/response.rs`、`tests/chat_recovery.rs`。依赖 T4/T5。

**Interfaces:** `prepare_response(resp, wants_stream) -> Result<PreparedResponse, UpError>`；`PreparedResponse` 保存已读前缀和剩余流。账号策略只在 prepare 成功后 `mark_ok`；最终 `into_response()` 标志对客户端提交，不允许提交后重试。

- [ ] A06 同一合成凭据错误分别以 HTTP4xx、HTTP200 JSON、HTTP200 首 SSE 错误表达，断言相同账号恢复/换号决策和次数。
- [ ] 测试非流式聚合错误、模型不可用错误、正常工具帧、客户端断开；已发送正常内容后再报错必须结束当前流，不能换号把另一段内容拼回来。
- [ ] 将分散的 HTTP200 错误转 Response 路径改为先返回 `UpError`；保留错误消息、既有 HTTP 映射、队列/并发/福利策略差异。
- [ ] 生产只调用这一条策略链；旧 handler 的散落分支移除，不留双实现。运行新目标与码道全测试，提交。

### T7：显式 UI 区域状态，隔离异步旧响应

**Files:** UI 原件及自动同步副本；新增 `app/tests/ui-state.test.cjs`。依赖 T1，可在码道任务阻塞时独立推进。

**Interfaces:** 在原 UI 内定义 `PTApp.regionContext(pageId)`，返回不可变 `{pluginId, region, port, generation}`；`PTApp.onRegionChange(handler)` 通知；请求用 `isCurrent(context)` 决定是否渲染。模式变化只通过现有 PTApp 订阅触发刷新。

- [ ] A07 用可控 Promise 让 CN 请求挂起，切到 INTL，再完成 CN：旧值只能写 CN 缓存，当前 UI 不变，未发停服请求；Qoder 同样覆盖。
- [ ] 用 Node 内置 `node:test` 加最小 DOM/bridge 外部替身，运行从生产 HTML 提取的实际脚本/状态模块；不能在测试里复制一套简化状态机。需要 hook 时只暴露纯接口，不换执行算法。
- [ ] 替换 `accPort` 的 DOM 反解析与 200ms 跟随，额度/模型/日志/统计请求捕获上下文；删除错误的 `window.refreshKeyLights` 查询，跨 IIFE 接口归 PTApp。
- [ ] 内部改为 page/plugin/region 语义，持久化 ID、DOM `data-*`、键盘和机械动画不变。无需全量拆 HTML。
- [ ] 验证：`node --test app/tests/ui-state.test.cjs`；创建并运行 `node tools/check-ui-js.cjs`（抽取全部可执行内联脚本，用当前 Node `--check`，扫描集合为空报错）；同步 UI 后跑 GUI 的 `ui_html`、`release_sync`。
- [ ] 开发浏览器检查只能使用隔离预览/假 bridge，不能启动现有 GUI 或命中实际启停接口；提交。

### T8：统一 UI hostApi 与键位读/迁移/保存职责

**Files:** T8 范围；在同一 UI 测试目标和 GUI 单测中增加 A08。依赖 T7。

**Interfaces:** `PTApp.hostApi.call(name, args) -> Promise<object>` 统一 bridge/demodata 的解码与错误；底层仍兼容现有 `cc*` 字符串协议。`keymap` 模块将纯校验/迁移、读取文件、持久化保存分别命名；UI 区分快路缓存与已保存状态。

- [ ] A08 覆盖非法/旧映射、oc 一次性迁移、md 当前视图补齐、用户之后主动移除、文件优先、保存失败与重启读回；不顺手统一被黄金锁住的旧默认值。
- [ ] 收拢 UI 直接 `window.ccX` 的业务调用，保留唯一底层适配器；保存失败不能吞掉并报告已保存。纯视图补齐与文件迁移要显式分开，getter 不再暗含未命名副作用。
- [ ] 保持 bridge 绑定名、位置参数、返回字节和 Promise/字符串兼容。接口结构变化如触及源码锚点测试，用实际行为测试补足再定向适配，不能直接删旧覆盖。
- [ ] 不将 Promise 等同于宿主线程异步；说明同步方法预算和后台方法，避免本轮顺便重写整个 IPC 线程模型。
- [ ] 验证 UI 测试与 JS 语法、GUI `bridge_contract`、`ui_html`、`close_contract`；提交。

### T9：显式插件生命周期与更新计划来源

**Files:** T9 范围；已有托管/壳/更新单测。依赖 T8 接口保持。

**Interfaces:** `RuntimeState::{Stopped, Starting{generation}, OwnedChild{pid,generation}, AdoptedEndpoint}` 与健康快照分开；旧 `running/healthy` DTO 在出口生成。更新计划类型区分 `AfterSuccessfulUpdate` 与 `IdentifiedOldImage`，执行目标不冒称都已证实 stale。

- [ ] A09 用假后端和测试端口验证接管不 spawn/不杀；被接管端口消失后能再次启动；停止失败恢复原跟踪；旧 watcher 不复位新代；已 spawn 未监听子进程可通过 PID 清理。
- [ ] A10 纯计划测试覆盖有标记/无标记、null 标记、未知镜像、空闲插件和核心端口。stop/start 调用必须是测试缝，严禁用真实更新验证。
- [ ] 分开“已启动/接管”和“健康观察”，消除陈旧 running 阻止恢复的路径。不要将停止外部端口改成只读 detach。
- [ ] 将只执行 rename 的 `replace_running_exe` 内部更名为 `vacate_executable_path`，兼容旧公共调用面；生成计划和执行动作分开，不改变腾位、标记消费和更新顺序。
- [ ] 验证：GUI lib 单测和 `bridge_contract`、`shell_contract`、`command_dispatch`、`close_contract`、`winres_contract`，提交。

### T10：收拢团结请求生命周期，保持凭证链和策略差异

**Files:** T10 范围；新增 `request_lifecycle.rs`，扩充现有 `p8b4_chat_behavior`。依赖 T1。

**Interfaces:** 命名 `CredentialRoute::{PoolAccount, LocalCredentialFallback}`，由当前选号结果生成；不按账号数推断。`RequestLifecycle` 持有现有注册和计数守卫，可转移到响应流；同步 Drop 只做同步配平，不偷偷 spawn 额外网络任务。

- [ ] A11 先锁住池可选/池全冷却/只有一个本地账号/无池的原分流，402/429、网络错误、换号、客户端取消和正常流尽。
- [ ] 明确原计数语义：原来 load 绑定初选账号且在响应交付前结束的行为按现状锁住，本轮不借 RAII 迁移到最终账号或延长到流尾；registry、会话租借分别按已锁住的时点配平。
- [ ] 将手工 inc/dec、register/finish 归有名守卫，正常和提前退出只清理一次；签名、会话内容、UA、请求重排、重试次数/预算及两路空响应差异全部保持。
- [ ] 验证团结整 crate 的黄金/HTTP/行为测试，尤其 `golden_p8a`、`golden_p8b`、`p8b4_chat_behavior`。测试不能依赖原 Go 程序或真实凭据；基线已有缺口如实记录，提交。

### T11：COMMAND 生产走增量流，测试独立收集输出

**Files:** T11 范围；新增 `output.rs` 和 `live_stream_contract.rs`。依赖 T1。

**Interfaces:** `EndpointOutcome { status, content_type, body }`，`OutgoingBody::{Full(Vec<u8>), Stream(ByteStream)}`；`ByteStream` 为 `Send + 'static` 的实际异步字节流。明确将 `Proxy.stats` 改为 `Arc<Mutex<UsageStats>>`，构造和 set_stats_file 同步适配；每个输出流 clone 统计句柄，拥有 model、request_id、时间等元数据和 reqwest 响应体，绝不从 `'static` 流借用 `&Proxy`。HTTP handler 仍可传借用参数做提交前准备，返回时流已拥有所有依赖。测试 collector 收集真实流后继续比黄金；上游成功/失败用 Result/枚举，不再用两个 Option 表达互斥状态。

- [ ] A12 假上游发送首个有效事件后，被 oneshot/barrier 卡住，不允许发送末帧。客户端必须在解除卡点前读到首个协议增量；用超时仅防测试永久挂起，不用毫秒竞速判优。
- [ ] Chat、Responses、Messages 三种协议均覆盖。再验证 context overflow 首帧重试一次、第二次失败真实错误、工具帧顺序、usage 单次入账、取消/读错后无新增上游命中。
- [ ] 流持有一次性 `UsageFinalizer`：正常 EOF、读错误和 Drop 都只结束一次；沿用现有各协议的真实 usage 判据。已经收到可用 usage 时仅记录实际观察值一次，未收到完整可用 usage 不估算、不制造成功入账；取消/读错后不发送新的上游请求。测试分别断言三种退出、无 usage、重复 usage 的次数和数值，具体与旧协议判据的必要差异单列。
- [ ] 将上游读取/事件翻译/输出连接起来，生产路径不能先收完整 Vec 再包 Stream。只缓存提交前所需前缀，不用无界 channel 攒完整结果。
- [ ] 正文开始交付后不再重试；统计时点明确；黄金测试继续核对完整收集字节、头及命中数，不改冻结基准。
- [ ] 验证新目标、`golden_proxy`、`http_contract` 和 crate 全测试，提交。

### T12：有限共享兼容原语，去除业务测试对旧机器密文的依赖

**Files:** T12 范围。此任务分两个独立提交；依赖前述行为稳定后再执行。

**Interfaces:** 兼容共享候选限于 `write_go_string(out: &mut String, value: &str)`、已经证明等价的 float/排序原语；保留 map 与 struct 顺序、原始字面量和 Go 错误语境的专属接口。Qoder 测试通过运行时合成输入进入真实 auth/worker/HTTP 路径。

- [ ] 对至少三处候选实现运行同一输入表：控制字符、HTML、Unicode、正负零、指数边界、null、重复键/大小写和原始数字。存在语义差异的解码器不合并。
- [ ] 若至少两种稳定原语在三处消费者一致且可减少重复维护，允许增加无传输依赖的工作区纯兼容 crate；没有这个收益则保留各 crate 独立，只去同 crate 重复并写清边界。不得创建 `rs-common` 杂物箱或让后端互相依赖。
- [ ] Qoder 在 Windows 测试运行时，用当前用户 DPAPI 保护合成 AES key，生成与冻结夹具同逻辑内容的本地临时夹具；非 Windows 的业务测试可注入窄解密边界，独立标记 OS 集成不可运行。绝不能把真实策略整体换成 stub。
- [ ] 冻结旧夹具不修改；旧 DPAPI 采集对照如保留跳过，必须另有实际执行的等价业务场景补齐，报告区分业务检查与历史采集对照。不能只统计跳过减少。
- [ ] 验证 Qoder `golden_qoder`、`http_contract`、新可移植业务测试与受影响兼容消费者全测试；再跑工作区测试覆盖 feature 并集。提交两项及证据。

### T13：自动同步 UI，交付唯一开发入口与验收入口

**Files:** T13 范围。依赖 T7–T12。

**Interfaces:** `tools/sync-ui.ps1 -Check` 只校验；`-Write` 只复制原件至嵌入副本并更新 UI 哈希/长度，禁止改版本或日志。`tools/verify-cognitive-contracts.ps1` 按固定映射运行 A01–A12 和语法/同步检查，任何目标/用例缺失、零扫描、跳过或非零退出码都失败。

- [ ] 为 sync 工具增加临时仓库夹具测试：内容偏差能红、Write 后再 Check 绿、二次 Write 不改文件、版本值及历史日志不变；脚本只能在指定根路径写明确文件，删除临时目录前确认绝对路径。
- [ ] 验收脚本逐项记录实际命令/用例名/输出，不以 grep 函数名、仅 `--list` 或编译成功代替行为。先确认每一目标存在且真实被执行，再运行。
- [ ] 写不超过约 200 行的开发入口地图：单 exe 分发、进程归属、UI/bridge/HTTP 两条链、各平台请求链、状态持久/内存归属、测试入口、保护区与发布纪律链接。不复制实时 PID/端口运行态和模型情报。
- [ ] 更新被改模块的职责说明，历史迁移事实留链接；相关 README 与项目说明非历史部分对齐。版本锚点标题和既有历史日志不改。
- [ ] 多处版本锚点保持值不变，以现有 release_sync 检查；完整版本生成流水线是后续独立工程，不在本轮额外建设。UI 同步自动化就是本次对发布维护成本的交付。
- [ ] 验证 sync 的红→绿与幂等、JS 语法、A01–A12、GUI `ui_html` 和 `release_sync`，提交。

### T14：整体验证、只读复核与可审阅交付

**Files:** `PROGRESS.md`、`BLOCKED.md`、`docs/cognitive-simplification-verification.md`；不借收尾再加功能。

**Interfaces:** 交付分支和提交列表、A01–A12 结果表、旧用例对照表、五条职责追踪、仍受阻事项。

- [ ] 新开只读评审上下文，检查最终 diff 与设计，追踪区域/接管/续期/业务错误/首帧五条链，指出状态所有者、提交点、清理者和重复实现残留。审查发现先核实，再在任务范围内修复。
- [ ] 执行 `cargo test --workspace --locked -- --nocapture`，以及 `node --test app/tests/ui-state.test.cjs`、JS 语法工具、UI 同步 Check、A01–A12 验收脚本；命令首次使用前已在 T0 或所属任务核实，输出必须可追溯。
- [ ] 审查 `git diff --check`、相对 base 的提交/diff。证明成品 exe、冻结黄金、历史日志、版本值未变；没有真实账号文件、密钥、临时产物混入提交。仅格式变更不得掩盖业务 diff。
- [ ] 不要求旧场景总 SKIP 无条件归零：与基线逐用例比较，相关业务已补实际执行场景；新增 A01–A12 全执行且全部通过。不能用新增数覆盖旧用例删除。
- [ ] 最终报告逐任务列出改动、命令、真实结果、反向验证摘要、结构追踪和限制；不承诺未经实测的接手工时降低百分比。
- [ ] 所有自有改动分阶段提交在隔离分支。保留工作树供验收，不自动合并、推送、发版或换装。最终给用户分支名、HEAD、报告路径、已完成/未完成清单。

## 完成与受阻的区别

完整完成需同时满足：

1. A01–A12 的实际业务组合测试全部执行通过、跳过为 0；五条职责链审查能定位唯一状态所有者、响应提交点和清理者，各任务未留下同职责的备用实现。
2. 受保护协议、版本、历史数据、运行服务未被改变；现有用例没有删减/放宽/新增跳过，工作区检查无本次新增失败，证据和分阶段提交完整。

个别任务受环境或真实设计冲突阻塞时，记录精确条件，继续无依赖任务；不能以“写进 BLOCKED”算完成全部。若同一具体阻塞持续三轮且已无可独立推进工作，交付部分成果与未完成清单，明确说明尚未达到完整完成条件，不伪报全绿，也不为凑完成越过禁区。

最终对话贴出真实命令结果摘要、A01–A12 表和被保留的失败/跳过，只有“已完成”四字不算交付。
