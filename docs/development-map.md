# ProxyDeck 开发入口地图

更新：2026-11-26（降低接手成本分支）。本文只写**当前代码里谁拥有什么、从哪进**；
运行态 PID/端口/版本以 `tools/status-facts.ps1` 输出为准，不在本文手抄。历史迁移过程见
`CLAUDE.md` 与 `CHANGELOG.md`，本文不复述。

## 1. 单 exe 与进程归属

- 唯一产物 `ProxyDeck.exe`（`rs-gui`）既是 GUI，也是所有插件子模式的分发器：
  `rs-gui/src/dispatch.rs` 按 `--plugin-<id>` / `-headless` 选择目标。
- GUI 拉杆启动插件 = spawn **本 exe** 的子进程 `--plugin-<id> --port <n>`；子进程独立常驻，
  关 GUI 不结束。**关 GUI 后重启 GUI 不会自动点火**，需要逐个拉杆。
- 插件托管状态由 `rs-gui/src/plugins.rs` 的 `PluginHost` 拥有：
  `RuntimeState::{Stopped, Starting{generation}, OwnedChild{pid,generation}, AdoptedEndpoint}`。
  健康只由端口探活（`httpc::ok`）得出，不由生命周期状态推断。
  - 端口已有健康实例 → `AdoptedEndpoint`，不 spawn、不杀；该实例退出后允许重新点火。
  - 本进程 spawn 的子进程 → `OwnedChild`，退出监视只复位**同一 generation**。
  - `stop` 先杀 PID/端口再复验；失败原样恢复跟踪。
- 更新/换血：`rs-gui/src/update.rs` 只负责下载、校验、腾位（`vacate_executable_path` 只做
  rename）与写标记；`rs-gui/src/rollover.rs` 的 `UpdatePlan` 区分
  `AfterSuccessfulUpdate`（有标记，全量换血）与 `IdentifiedOldImage`（无标记，只动已证实
  `*.old*` 镜像）。空闲端口不点火、核心 55990 不换血、未知镜像不动。

## 2. 两条主调用链

### 2.1 UI / bridge / HTTP

```text
app/ui.html（唯一源）
  ├─ #pt-region-state : PTApp.regionContext / isCurrent / hostApi.call
  ├─ #pt-keymap-state : PTKeymap（快路、md 补齐、文件值覆盖）
  └─ 引擎 + 业务 IIFE：PTApp 绘制/交互，hostApi 调用宿主

hostApi.call(name, args)
  → window.cc<Name>(...args)（WebView2 注入；字符串或 Promise）
  → JSON 解码/错误归一为 {ok,msg,...}
  → rs-gui/src/webview.rs 的 IPC 派发 → rs-gui/src/bridge.rs::dispatch
  → 具体能力（state / plugins / update / keymap / fax / tray …）

业务层 HTTP（平台服务）
  PTApp.regionRequest(context, path)
  → http://127.0.0.1:<context.port><path>
  → 响应返回后 isCurrent(context) 才允许写 DOM/缓存
```

- 区域状态唯一写入点：`cbRegionApply` / `qoderRegionApply` 调 `PTApp.setRegionContext`；
  `selectMode` 推进 generation。切区**不停止另一地区**的后端。
- 键位：`app/ui.html` 内 `PTKeymap` 是唯一状态所有者；配置页只经
  `window.__keymapPersist` 落盘，失败会回读 `KeyMapGet` 恢复文件值。
  Rust 侧权威实现是 `rs-gui/src/keymap.rs`（校验/迁移/读写分离），`bridge.rs` 仅转发。
- UI 同步：改完 `app/ui.html` 必须跑 `pwsh -File tools/sync-ui.ps1 -Write`（只复制副本并更新
  `UI_HTML_SHA256/UI_HTML_LEN`），再跑 `-Check` 与 `cargo test -p rs-gui --test ui_html`。

### 2.2 各平台请求链

共同形状：**请求准备 → 凭据/账号获取 → 单次上游尝试 → 提交前校验 → 策略决策 → 响应交付**。
平台策略不共享算法，只共享已证明一致的原语（如 Go 字符串转义、SSE 行读取）。

| 平台 | 入口 | 账号/凭据所有者 | 响应提交点 |
|---|---|---|---|
| COMMAND (`rs-proxy`) | `rs-proxy/src/http.rs` → `chat/responses/anthropic::handle_*_live` | `Proxy`（key、stats Arc） | `output::LiveSse` 发出即提交；提交后不再重试 |
| 团结 (`rs-tuanjie`) | `rs-tuanjie/src/chat.rs::handle_chat` | `accounts::AccountPool` + `request_lifecycle::CredentialRoute` | 流式：`FinishGuard` 随流；非流式：写完前释放 |
| WorkBuddy (`rs-codebuddy`) | `chat::handle_chat` | `pool::AccountPool`（初选与换号都按最终 `sent_model`） | 200 流头：`ChunkReader` 嗅探后交付 |
| 码道 (`rs-codearts`) | `lib.rs::chat_core` | `accounts::AccountManager`（稳定 ID、revision、OS 锁、续期协调） | `response::prepare_response` 成功后提交 |
| Comate (`rs-comate`) | `lib.rs::handle_chat` | 本机登录态 / zulu | 现有流式转发 |
| Qoder (`rs-qoder`) | `lib.rs` HTTP 入口 | `auth::load_auth`（DPAPI+AES-GCM） / worker | 现有转发 |
| 其他插件 | 各自 `lib.rs` 路由表 | 见各 README | 见各 README |

关键不变量：
- 团结签名种子、两层 HMAC、会话头、UA、换 key 端点与请求字节顺序是**红线**，
  详见 `CLAUDE.md` 红线一节；任何重构不得触碰。
- WorkBuddy 200 流头按**协议完成度**判定，不把网络块边界当帧边界。
- 码道 HTTP4xx / HTTP200 JSON / HTTP200 首 SSE 业务错误在提交前统一交回账号策略；
  提交后（流式首帧已发）不换号重放。
- 码道账号身份是稳定 ID；`identity()` 仅是旧输入/展示值，AK 轮换后靠别名解析。

## 3. 状态所有者与持久化

| 状态 | 所有者 | 落盘 |
|---|---|---|
| UI 区域/generation | `PTApp`（`#pt-region-state`） | 内存 |
| 键位映射 | `PTKeymap`（UI）/ `rs-gui::keymap`（文件） | `%APPDATA%\ProxyDeck\keymap.json`（明文） |
| 插件生命周期 | `PluginHost`（`plugins.rs`） | 内存；日志 `plugins/<id>.log` |
| 换血计划 | `rollover::UpdatePlan` | 标记 `.update-rollover.json`（读走即删） |
| 团结账号运行态 | `AccountPool` | 内存（Dead/冷却/计数重启清零）；`tuanjie-accounts.json` 为账号快照 |
| WorkBuddy 账号池 | `AccountPool` | `codebuddy-pool(-intl).json` 只存 Enabled/Removed |
| 码道账号 | `AccountManager` | `codearts-accounts.json`；续期 overlay `codearts-refreshed.json`；活动账本 `codearts-activity.json` |
| COMMAND 统计 | `Proxy.stats`（`Arc<Mutex<UsageStats>>`） | `stats.json`（节流） |
| COMMAND 流式 usage | `output::UsageRecorder` | 与上同（流结束/取消时入账一次） |

写盘纪律：账号/凭据文件只由对应管理器经锁读—改—写；不要手写 JSON 覆盖。

## 4. 测试入口

```powershell
# 全部工作区（共享 target：E:\cargo-target\workspace）
cargo test --workspace --locked -- --nocapture

# A01–A12 验收入口（固定映射；缺目标/缺用例/出现 SKIP/非零退出都失败）
pwsh -File tools/verify-cognitive-contracts.ps1

# UI 状态与宿主接口（生产 HTML 提取脚本 + 最小 DOM 替身）
node --test app/tests/ui-state.test.cjs
node tools/check-ui-js.cjs

# UI 同步（-Check 只校验；-Write 只复制并更新哈希/长度）
pwsh -File tools/sync-ui.ps1 -Check
pwsh -File tools/tests/sync-ui.tests.ps1

# 兼容原语对照（Go 字符串转义：6 个实现 × 8 组输入）
cargo run --manifest-path tools/compat-vectors/Cargo.toml

# 界面/版本门禁
cargo test -p rs-gui --test ui_html
cargo test -p rs-gui --test release_sync
```

测试纪律：只用临时目录、随机回环端口、合成账号、假上游/STS/worker；不得读真实凭据或
操作生产端口。既有 SKIP 清单与基线见 `PROGRESS.md`；新增验收场景不得跳过。

## 5. 保护区与发布纪律

- 团结凭证链（种子/签名/会话/UA/换 key 端点/请求字节顺序）：见 `CLAUDE.md` 红线。
- bridge：`cc` 函数名、位置参数、JSON 字符串返回与字节形态是合同（`rs-gui/tests/bridge_contract.rs`）。
- 冻结黄金：`**/tests/golden/**` 不改；UI 副本与哈希由 `ui_html`/`build.rs` 双重门禁。
- 版本：多处锚点必须同源（`release_sync`）；**历史 CHANGELOG/CHANGELOG_DATA 条目不回改**；
  发版只由用户统一执行，见 `CLAUDE.md`「攒版发布」。
- 运行中的服务：停止/换装前必须先告知用户并等确认；测试用 `--port 18788` 等测试端口。
```
