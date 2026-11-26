# ProxyDeck 降低接手成本执行进度

## 开工回执
- 目标：按 T0–T14 串行交付，先组合测试确认缺陷，再收拢状态所有者与调用链。
- 顺序：T0 基线 → T1–T6 平台/账号 → T7–T9 UI/宿主 → T10–T12 生命周期/流/兼容 → T13–T14 工具与验收。
- 隔离：分支 `simplify-project-handoff`，worktree `.agents/worktrees/simplify-project-handoff`，基准 `0daefd9`。
- 测试：仅临时目录、合成账号、随机回环端口、假上游/STS/worker；Cargo 共用 `E:\cargo-target\workspace`。
- 最大风险：异步旧响应、并发续期、流头提交与插件 watcher 晚回调；均以真实组合路径验证。
- 禁区：不升版、不改历史日志/冻结黄金、不 push/合并/tag/Release、不换装或启停现有服务、不碰真实账号/权限/DSH。

## 任务账本
| 任务 | 状态 | 验证/提交 |
|---|---|---|
| T0 | complete | 基线已建立；环境还原后 2 个哈希门禁转绿，保留 1 个既有 DPAPI 失败 |
| T1 | complete | 397 passed / 0 failed / 2 ignored；构造器突变红→绿；OpenCode lib 64 passed |
| T2 | complete | A01 5/5；WorkBuddy 全 crate 137 passed / 0 failed；突变 4 红后恢复全绿 |
| T3 | complete | A02 5/5；WorkBuddy 全 crate 142 passed / 0 failed；初选忽略模型突变 4 红后恢复 |
| T4 | complete | A03 6/6；CodeArts 全 crate 62 passed / 0 failed；稳定 ID 替换突变 2 红后恢复 |
| T5 | complete | A04/A05 13/13；CodeArts 全 crate 69 passed / 0 failed；版本复用突变 1 红后恢复 |
| T6 | complete | A06 4/4；CodeArts 全 crate 73 passed / 0 failed；JSON 错误旁路突变 1 红后恢复 |
| T7 | complete | A07 3/3（含真实生产脚本）；JS 语法 4/4；ui_html 4/4；release_sync 1/1；防过期守卫突变 1 红后恢复 |
| T8 | complete | hostApi/A08 6/6；keymap 单测 4/4；ui_html 4/4、release_sync 1/1；bridge_contract 5/5、close_contract 1/1；md 补齐突变 2 红后恢复 |
| T9 | complete | A09/A10 5/5；插件/换血/shell 库测试 35/35；GUI 五合同 23/23；接管过期突变 1 红后恢复 |
| T10 | complete | A11 5/5；团结全 crate 246 passed / 2 ignored；流守卫所有权突变 1 红后恢复 |
| T11 | complete | A12 5/5；rs-proxy 全 crate 106 passed；缓冲回退突变 1 红后恢复 |
| T12 | complete | 六实现转义对照全绿（修团结 \b/\f 分歧）；Qoder 运行时合成夹具 2/2；受影响 6 包与团结全绿 |
| T14 | pending | 未开始 |

## 基线与证据
- 原树：HEAD `0daefd9d5cc218321a4ee7d36d1b6d9148221bd3`，分支 `main`，仅任务文档未提交。
- 隔离树：只复制两份任务文档；原始日志 `.work/cognitive-simplification/baseline-workspace.log` 与 `baseline-workspace-nofailfast.log`。
- Cargo：`metadata --offline --locked --no-deps` exit 0；共享 target `E:\cargo-target\workspace` 已存在。
- 首次 `cargo test --workspace --locked -- --nocapture`：exit 101，在 `rs-gui` lib 的既有 DPAPI 用例失败后停止。
- 全量 `cargo test --workspace --locked --no-fail-fast -- --nocapture`：exit 101；**969 passed / 3 failed / 3 ignored**，另有 2 条运行时 SKIP。
- 失败基线：`rs-gui::dpapi::tests::decrypts_a_go_written_blob`（样本前缀不是 PDAPI1）；`rs-gui::bridge_contract::fake_backend_source_hash_matches_golden`；`rs-qoder::http_contract::fake_worker_source_hash_matches_golden`。
- 两个哈希失败已证明是 worktree 检出时 CRLF 转换：原树/Git 索引为 LF。已恢复原树逐字节副本（`git diff` 无内容差异），两条哈希用例分别 exit 0；未改黄金值或断言。 common Git 配置已恢复原值 `core.autocrlf=true`。
- 有效基线：**971 passed / 1 failed / 3 ignored**；唯一保留失败为上述既有 DPAPI 历史密文样本，不归因于本次、不在本轮越界修复。

### 逐包基线（全量 no-fail-fast）
| 包 | passed | failed | ignored | 运行时 SKIP |
|---|---:|---:|---:|---|
| rs-bai | 36 | 0 | 1 | 2（oversized 基线未随仓） |
| rs-codearts | 55 | 0 | 0 | 0 |
| rs-codebuddy | 132 | 0 | 0 | 0 |
| rs-comate | 56 | 0 | 0 | 0 |
| rs-gui | 144 | 2（其中 1 条环境还原后转绿） | 0 | 0 |
| rs-opencode | 96 | 0 | 0 | 0 |
| rs-proxy | 100 | 0 | 0 | 0 |
| rs-qoder | 68 | 1（环境还原后转绿） | 0 | 0 |
| rs-tuanjie | 239 | 0 | 2 | 0 |
| rs-vibex | 43 | 0 | 0 | 0 |

### 基线 SKIP / ignored 清单
- `rs-bai/http_contract::slow_stream_is_not_cut_by_total_timeout`：ignored，设计为 600s+ 按需用例。
- `rs-bai/golden_adapt`：`F7-truncate-oversized`、`F8-truncate-tool-pairing` 两条 `SKIP(oversized)`，基线未随仓。
- `rs-tuanjie/crypto::decrypt_real_file_one_shot`：ignored，一次性真实取证。
- `rs-tuanjie/p8b_pool_behavior::removing_the_dead_check_would_loop_forever`：ignored，反向验证专用。
- 源码另有条件 SKIP 分支（DPAPI/机器路径/无安装/无窗口等）；本次实际输出未触发的不计为已执行。新增 A01–A12 不得跳过。

## T1 实施记录
- Comate 新增 `for_zulu` / `for_cloud`；`new()` 明确保留为 Zulu 兼容入口，`from_env()` 仍按原规则默认 Cloud。云合同测试改用显式构造器，原断言全部保留。
- 新构造器测试先在原实现上编译失败（E0599），实现后 1/1 通过；将 `for_cloud` 突变为 Zulu 时断言失败，恢复后再次通过。
- 团结 pacing 删除 8 个 appdir 转发符号，生产/测试调用直接使用 `crate::appdir`；`Proxy.opencode_session` 无消费者，已删除。
- 更新团结 chat、Comate、OpenCode 的当前职责说明；未改历史日志、模型策略或凭证链。
- 验证：`cargo test -p rs-comate -p rs-proxy -p rs-tuanjie --locked -- --nocapture` → **397 passed / 0 failed / 2 ignored**；`cargo test -p rs-opencode --locked --lib` → 64 passed。
- `cargo fmt --all -- --check` 暴露全仓既有格式差异（约 20k 行输出），未借 T1 重排全仓；本次 `git diff --check` 通过。

## T2 实施记录
- 新增 `HeadCompleteness::{Incomplete, Complete}` 与 `HeadProbe { bytes, completeness }`；`peek_within` 首块即返回，但完整度由 SSE 帧分隔符、完整 JSON/HTML 或干净 EOF 决定。
- 预算耗尽、字节窗口攒满、半个 `data:`、JSON/UTF-8 中途均保持 `Incomplete`；明确业务码仍优先判定，完整错误帧/DONE/HTML 保持原分类。
- A01 真实 HTTP 假上游以 chunked `dat` / `a: {...}` 切分：旧实现日志出现“空流@200，换号重发”并回 502；修复后 1 次上游命中、字节完整。
- 旧 `!truncated` 断言固定了“网络首块=协议完整”的错误语义，改为 `Incomplete`；“首字节不白等预算”和“peek 后字节不丢”原断言保留。旧 bool 分类用例仅做类型适配，断言强度未降。
- 验证：新目标 5/5；`cargo test -p rs-codebuddy --locked -- --nocapture` → **137 passed / 0 failed / 0 ignored**。
- 反向验证：把半帧完成度突变为 Complete 后新目标 1 passed / 4 failed；恢复后 5/5。

## T3 实施记录
- 请求上下文显式区分 `requested_model` 与最终 `sent_model`；hy4 fallback 先确定资格模型，再选账号。
- 初选及 401/403、429、14018、11128、11140、11158、200 流头等所有换号点统一使用 `pick_for_model(&sent_model)`。
- 新增 `AccountPool::has_selectable` 区分“全池不可用”与“池可用但当前模型全黑”；后者直接回既有 `model_unavailable`，不再命中已知失败上游。
- 429 trip 改写 fallback 后，若当前号已知不支持新模型，重放前先换合格账号；身份头仍按原请求体派生，保持原调用顺序。
- A02 五个组合场景：跨请求记忆/其他模型不中毒、fallback 前资格、全黑零命中、CN/INTL 隔离、429 trip 后资格；旧实现 4/4 核心场景红。
- 验证：`cargo test -p rs-codebuddy --locked -- --nocapture` → **142 passed / 0 failed / 0 ignored**。
- 反向验证：初选改回 `pick()` 后新目标 1 passed / 4 failed；恢复后 5/5。

## T4 实施记录
- 新增 `accounts::AccountManager`：稳定 `AccountId`、`AccountSnapshot`、旧 identity 别名、凭据 revision、导入/替换/启停/删除事务。
- `Account.identity()` 保持旧输入/展示合同；新增 `account_id` / `identity_aliases` / `revision` 与 `stable_key()`。缺 domainId 时不再按显示名合并。
- 新增 OS 文件句柄锁：账号锁与短期配置事务锁；Windows `share_mode(0)`，进程退出自动释放，锁文件残留不视为仍占用。
- Server 启动时迁移 ID、overlay 与活动账本键；health/benefit/busy/unavailable/last_touch/archived 全部改用稳定键。账号视图保留 `identity`，新增 `accountId`；管理 HTTP 同时接受旧 alias 与稳定 ID。
- A03 六个场景：同名不合并、AK 轮换/重载、旧 removed 精确边界、稳定删除别名、并发首次分配、已知字段往返/失败可见、真实 HTTP 管理链。
- 验证：`cargo test -p rs-codearts --locked -- --nocapture` → **62 passed / 0 failed / 0 ignored**。
- 反向验证：凭据替换时改分配新 ID，A03 2 红；恢复后 6/6。

## T5 实施记录
- `AccountManager::ensure_fresh(id, observed_revision, mode)` 成为唯一续期入口；`IfExpired` / `Force` 共用按账号 OS 文件锁。
- 锁覆盖网络请求；等待者得锁后重读版本，已更新则直接复用。保存时短期配置锁内重载、合并账号文件与 overlay，全部成功才返回新快照。
- `RefreshError` 区分瞬时失败、绑定 revision 的确定失效、存储失败、锁超时与缺材料；本机桌面运行时的保守不续期规则保留。
- Server 健康态新增 `dead_revision`；旧失败只标死对应凭据版本，新成功/新 revision 自动恢复候选资格。chat/quota/后台统一调用协调器，移除旧 `apply_refresh` 双实现。
- A04：同一 Server 的 chat、quota、后台周期只发一次 STS；两个真实测试子进程共用临时目录与假 STS，也只轮转一次并读到同一新版本。
- A05：5xx 不判死、400/invalid_grant 绑版本标死、新凭据恢复、overlay 落盘失败返回 Storage；锁残留/超时由 OS 句柄测试覆盖。
- 验证：`cargo test -p rs-codearts --locked -- --nocapture` → **69 passed / 0 failed / 0 ignored**。
- 反向验证：取消“新 revision 直接复用”后，双进程 Force 场景 STS 命中 2 次；恢复后 13/13。

## T6 实施记录
- 新增 `response::prepare_response(resp, wants_stream)`：HTTP4xx、HTTP200 JSON、HTTP200 首 SSE 错误、非流式 SSE 聚合错误都在客户端提交前转成 `UpError`。
- `PreparedResponse` 持有完整响应或已预检流；账号策略确认后 `mark_ok`，再 `into_response()` 提交。提交后请求循环已退出，不换号重放。
- 流式只预检首事件；正常内容后的错误原样交付。非流式聚合在提交前发现错误，可换号且不拼接前一个账号的部分内容。
- 模型不可用、福利未领、并发退避、排队、凭据失效/冷却策略继续共用原分支；旧 `finish_chat` / `preflight_stream` / `collect_stream` 双实现移除。
- A06 四场景：三种错误形态一致恢复、非流式晚错恢复/流式不重放、正常工具帧、客户端首字节后断开不重发。
- 验证：`cargo test -p rs-codearts --locked -- --nocapture` → **73 passed / 0 failed / 0 ignored**。
- 反向验证：旁路 HTTP200 JSON 错误检测后 A06 3 passed / 1 failed；恢复后 4/4。

## T7 实施记录
- 新增 UI 内 `<script id="pt-region-state">`：`PTApp.regionContext(pageId)` 返回冻结的 `{pluginId, region, port, generation}`，`onRegionChange` 通知，`isCurrent(context)` 是异步响应唯一提交门槛；`regionRequest` 把端口与 generation 绑定。
- 删除 `accPort()` DOM 文本反解析与 200ms 点击/键盘跟随；WorkBuddy 账号池改为订阅区域状态，响应过期即丢弃。
- `cbRegionApply` / `qoderRegionApply` 成为区域状态唯一写入点；`refreshPluginView` 的插件列表、模型、元数据、日志、统计与 WorkBuddy/Qoder/码道额度请求全部携带上下文，晚到响应不写新区域缓存。
- `selectMode` 推进 generation，跨 IIFE 的 `window.refreshKeyLights` 查询改为 `PTApp.refreshKeyLights`。
- 新增 `app/tests/ui-state.test.cjs`：从生产 HTML 提取 `#pt-region-state` 与 WorkBuddy 账号 IIFE 真脚本，最小 DOM/fetch 替身；覆盖冻结/通知/失效、CN 晚到丢弃 INT L、Qoder 晚到拒绝。
- 新增 `tools/check-ui-js.cjs`：抽取全部内联脚本逐个 `node --check`，扫描集合为 0 或任一脚本语法错误即失败（实测 4 个脚本）。
- 验证：`node --test app/tests/ui-state.test.cjs` 3/3；`node tools/check-ui-js.cjs` 4/4；同步副本与哈希后 `ui_html` 4/4、`release_sync` 1/1。
- 反向验证：删掉 `accPull` 的 `isCurrent` 守卫，WorkBuddy 晚到用例转红；恢复后 3/3。

## T8 实施记录
- Rust 新增 `rs-gui/src/keymap.rs`：`is_valid`（纯校验）、`migrate_with_opencode`（纯迁移）、`load_from/load`（文件读取 + 一次性迁移标记）、`save_to/save`（校验后落盘，失败如实返回）。bridge 的 `key_map_*` 改为兼容转发，旧单测保持原断言通过。
- UI 新增 `<script id="pt-keymap-state">`：`PTKeymap` 成为快路缓存/合法性/md 视图补齐/文件值覆盖唯一所有者；`__keymapApplyFile` 只覆盖快路不回写文件。
- 新增 `PTApp.hostApi.call(name, args)`：统一 `cc*` 字符串/Promise 解码与错误（非法 JSON/异常/缺接口均返回 `{ok:false,msg}`），业务层 `call()` 与平台请求全部经它；demo 处理器经 `hostApi.demo` 注册。
- 配置页四条保存路径改用 `__keymapPersist` + `kmPersist`：只有桥确认成功才写“配置已持久化”；失败 toast 原文并回读 `KeyMapGet` 恢复文件值，不再吞结果谎报成功。
- `ui_html` 合同锚点随统一入口定向适配为“业务层仅经 PTApp.hostApi.call + 底层 cc* 串协议”，行为覆盖由 `app/tests/ui-state.test.cjs` 承担（未删旧覆盖）。
- A08 覆盖：非法映射拒绝、旧映射补 md、文件值优先、保存失败如实返回/成功载荷逐字节核对；Rust 侧覆盖 oc 一次性迁移、用户移除不回填、非法规避、保存失败。
- 验证：`node --test app/tests/ui-state.test.cjs` 6/6；`node tools/check-ui-js.cjs` 5/5；`cargo test -p rs-gui --lib key_map` 3/3；`cargo test -p rs-gui --test ui_html --test release_sync` 4/4+1/1；`--test bridge_contract --test close_contract` 5/5+1/1。
- 反向验证：删除 md 补齐的 concat 后 A08 2 红；恢复后 6/6。

## T9 实施记录
- 插件托管改为显式 `RuntimeState::{Stopped, Starting{generation}, OwnedChild{pid,generation}, AdoptedEndpoint}`；健康仍是端口探活快照，旧 `running/healthy` DTO 只在 `list` 出口组装。
- 接管实例退出后 `start` 先复查端口健康，允许重新点火（陈旧 running 不再永久卡死）；自 spawn 用世代号，`finish_generation` 只复位当前代，旧 watcher 晚回调被忽略。
- `stop` 先摘运行时状态再杀 PID/端口；失败原样恢复（含世代号），成功回 `Stopped`；新增测试缝证明未监听子进程也能按 PID 清理。
- 换血计划显式化：`UpdatePlan::{AfterSuccessfulUpdate{to}, IdentifiedOldImage(Vec<StaleTarget>), Noop}` 与 `plan_update`；标记路径全量滚、无标记只动已证实旧镜像；核心端口与未知镜像不产生目标。
- `replace_running_exe` 拆成只做 rename 的 `vacate_executable_path`；更新流程调用新名，行为不变（T14 复核后删除无调用方的兼容别名）。
- 验证：`cargo test -p rs-gui --lib -- plugin_hosting_tests rollover:: shell_tests` 35/35；`--test bridge_contract --test shell_contract --test command_dispatch --test winres_contract --test close_contract` 5+9+5+3+1 全绿；`--lib update::` 3/3。
- 反向验证：把接管分支改成无条件「已在运行」，A09 重新点火用例转红；恢复后 5/5。

## T10 实施记录
- 新增 `rs-tuanjie/src/request_lifecycle.rs`：`CredentialRoute::{PoolAccount, LocalCredentialFallback}` 由 `resolve_route`（选号结果）决定；`RequestLifecycle` 统一持有进行中注册号与**初选账号** load 计数；`FinishGuard` 幂等、`Drop` 兜底；`PacingGuard` 只做同步 +1/−1。
- `chat.rs` 池路径/单账号路径改为经生命周期守卫：删除手工 `registry.register/finish`、`inc_load/dec_load`、`pacing.pending_inc/dec`；流式把注册所有权经 `take_finish_guard` 交给响应流，`Drop` 时删除（客户端取消不再残留）。
- 计数口径保持：load 仍绑定初选账号并在响应交付前配平；换号不迁移计数；registry 非流式收尾删除、流式流尽/取消删除；pacing 仅兜底启用时计数。单账号 429 等待循环内网络错误提前返回的 pacing 漏配平由守卫修正（A11 覆盖正常完成路径，守卫单测覆盖 Drop）。
- 请求体脱敏/重排、签名/会话/UA、重试次数与预算、两路空响应差异均未改动。
- 验证：`cargo test -p rs-tuanjie --locked --test p8b4_chat_behavior` 28/28；`cargo test -p rs-tuanjie --locked -- --nocapture` 全 crate **246 passed / 2 ignored**（与基线同两处 ignored）。
- 反向验证：`take_finish_guard` 不转移 rid（新建空守卫）时，流取消用例转红；恢复后全绿。

## T11 实施记录
- 新增 `rs-proxy/src/output.rs`：`LiveSse`（可交给 `Body::from_stream` 的增量字节流）、`SseSink` 统一出口（缓冲写 Vec / 流式写通道）、`UsageRecorder` 在流结束/取消/解码错误时同步入账一次（不 spawn 额外任务）。
- 三协议翻译器（Chat / Responses / Anthropic）共用 `LineReader` + `SseSink`：原有逐行翻译字节不变，只是出口可切换；删除缓冲副本，HTTP 与门禁/黄金统一走 live 入口；首事件前判超限，提交后不重试。
- Chat/Responses/Anthropic 新增 `stream_*_response_live`：首事件先判 context 超限（未发头，可安全重发），随后后台任务逐行翻译并立刻写下游；客户端断开导致发送失败时任务结束、上游读取停止。
- 非流式与 `stream:false` 字节不变；`stream_options` 兼容逻辑未改；黄金下游字节（含工具调用 delta）经 live 路径重跑仍逐字节一致。
- A12：假上游首帧后停顿 400ms，Chat/Responses/Anthropic 首帧均在 300ms 内到达；非流式仍单 JSON；客户端取消后服务继续可用。计时从发请求前起算（排除只测「响应返回后首块」的假阳性）。
- 验证：`cargo test -p rs-proxy --locked --no-fail-fast` → **87 lib + 3 golden + 11 http + 5 live = 106 passed / 0 failed**。
- 反向验证：把 HTTP chat 路径改回缓冲 `handle_chat` 后，A12 首帧用例红（实耗 415ms）；恢复 live 后全绿。

## T12 实施记录（两项独立提交）
- 兼容原语对照：新增 `tools/compat-vectors`（独立包、不参与产品构建、不让后端互相依赖），把 8 组输入（控制字符/HTML/Unicode/U+2028-9/引号反斜杠/DEL/emoji）喂给 6 个 crate 的 Go 字符串转义实现；发现**团结 `gojson` 对 \b/\f 输出 `\u0008/\u000c`，其余实现与 Go 黄金一致输出 `\b/\f`**，已修团结一侧并复跑对照（6/6 一致）。共享 crate 判定：只有一个已证明一致的原语，收益不足，保留各 crate 独立实现，不新增 `rs-common`。
- 证据：`cargo run --manifest-path tools/compat-vectors/Cargo.toml` → “全部 8 组输入在 6 个转义实现上逐字节一致”，exit 0；受影响的 rs-proxy / rs-codebuddy / rs-comate / rs-vibex / rs-bai / rs-qoder 全测试与 rs-tuanjie 全测试（246 passed / 2 ignored）均无失败。
- Qoder 登录态：新增 `crypto::protect`（当前用户 DPAPI）与 `auth::write_synthetic_fixture`，在 Windows 测试运行时生成与冻结夹具同逻辑内容的临时夹具；`tests/common::auth_fixture_dir()` 冻结夹具可解则用冻结，否则用本机合成（非 Windows DPAPI 不可用则显式 SKIP）。冻结入库夹具一个字节未改。
- 新增 `tests/synthetic_auth.rs`：强制走一次运行时合成→真实 `load_auth` 解密链；截断/非法文件安全报错且错误信息不含 token。
- 验证：`cargo test -p rs-qoder --locked --test synthetic_auth` 2/2；`cargo test -p rs-qoder --locked` 全 crate 无失败。

## T13 实施记录
- 新增 `tools/sync-ui.ps1`（`-Check` / `-Write`）：只复制 `app/ui.html` → `rs-gui/ui.html` 并更新 `UI_HTML_SHA256`/`UI_HTML_LEN`；不改版本、CHANGELOG、其它常量。`tools/tests/sync-ui.tests.ps1` 用临时仓库夹具覆盖「偏差红 → 修复绿 → 二次写幂等 → 不改原件」，4/4。
- 新增 `tools/verify-cognitive-contracts.ps1`：固定 A01–A12 映射（用例名 + 最低通过数 + 无 SKIP/非零），另跑 UI 同步、内联 JS 语法、兼容向量、`ui_html`、`release_sync`；缺目标/缺用例/空扫描/跳过都红。
- 新增 `docs/development-map.md`（≤200 行）：单 exe 分发、插件生命周期/换血、UI/bridge/HTTP 与各平台请求链、状态所有者与持久化、测试入口、保护区。
- `CLAUDE.md` 增加「新协作者先读开发地图」引导；rs-proxy / rs-qoder README 补增量交付与合成夹具说明（不碰历史条目）。
- 验证：`pwsh -File tools/verify-cognitive-contracts.ps1` → **19/19 PASS，exit 0**（日志 `.work/cognitive-simplification/verify-*.log`）；`tools/tests/sync-ui.tests.ps1` 4/4；`tools/sync-ui.ps1 -Check` OK（sha256=cb90c47a… len=1543562）。

## 预检接口
- T2→T3：流头只判完成度，不改变模型资格；两者在真实 HTTP 入口组合验证。
- T4→T5→T6：稳定 ID/锁先落地，续期协调器复用，提交前错误策略最后接入。
- T7→T8：区域上下文先成为唯一页面上下文，hostApi/键位测试复用它。
- T8→T9：bridge 名称/位置参数/返回字节保持，生命周期 DTO 只在出口组装。
- T1→T10/T11/T12：命名清理不得改变凭证、计数、输出或兼容字节合同。
- T13/T14：只消费前序已提交实现与测试，不新增业务行为。

## 裁决记录
- Ruling：执行技能脚本只识别 `Task N` 标题，本计划使用 `T0`–`T14`；直接以计划对应小节为逐任务 brief，并用本文件与 `.superpowers` ledger 双记录。原因：不重写任务书；若误判，成本是需人工核对任务边界。
