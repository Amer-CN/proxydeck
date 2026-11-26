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
| T2–T14 | pending | 未开始 |

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

## 预检接口
- T2→T3：流头只判完成度，不改变模型资格；两者在真实 HTTP 入口组合验证。
- T4→T5→T6：稳定 ID/锁先落地，续期协调器复用，提交前错误策略最后接入。
- T7→T8：区域上下文先成为唯一页面上下文，hostApi/键位测试复用它。
- T8→T9：bridge 名称/位置参数/返回字节保持，生命周期 DTO 只在出口组装。
- T1→T10/T11/T12：命名清理不得改变凭证、计数、输出或兼容字节合同。
- T13/T14：只消费前序已提交实现与测试，不新增业务行为。

## 裁决记录
- Ruling：执行技能脚本只识别 `Task N` 标题，本计划使用 `T0`–`T14`；直接以计划对应小节为逐任务 brief，并用本文件与 `.superpowers` ledger 双记录。原因：不重写任务书；若误判，成本是需人工核对任务边界。
