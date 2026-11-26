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
| T6–T14 | pending | 未开始 |

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

## 预检接口
- T2→T3：流头只判完成度，不改变模型资格；两者在真实 HTTP 入口组合验证。
- T4→T5→T6：稳定 ID/锁先落地，续期协调器复用，提交前错误策略最后接入。
- T7→T8：区域上下文先成为唯一页面上下文，hostApi/键位测试复用它。
- T8→T9：bridge 名称/位置参数/返回字节保持，生命周期 DTO 只在出口组装。
- T1→T10/T11/T12：命名清理不得改变凭证、计数、输出或兼容字节合同。
- T13/T14：只消费前序已提交实现与测试，不新增业务行为。

## 裁决记录
- Ruling：执行技能脚本只识别 `Task N` 标题，本计划使用 `T0`–`T14`；直接以计划对应小节为逐任务 brief，并用本文件与 `.superpowers` ledger 双记录。原因：不重写任务书；若误判，成本是需人工核对任务边界。
