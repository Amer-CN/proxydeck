# rs-gui —— ProxyDeck GUI 宿主的 Rust 重写（P7a：桥接层与状态）

> 本文件按简报的「硬性边界」写：`rs-gui/**` + `.work/**` + 简报 + 两处薄包装
> （`rs-vibex` / `rs-proxy` 的 `main`→`lib`）是本轮改动的全部落点。
> `app/**`（含 `ui.html`）与全部 Go 源码**只读**；`rs-bai` / `rs-codebuddy` / `rs-comate` /
> `rs-qoder` 四个 crate 一个字未动。

## 1. 形态（用户裁决：单 exe 多子模式）

```
rs-gui.exe
├─ 无参                       → GUI（wry + tao 外壳 + 26 个 bridge 绑定）
├─ -headless                  → rs-proxy（主代理核心，库依赖直调 rs_proxy::run）
├─ --plugin-bai   --port N    → rs-bai     ::run
├─ --plugin-codebuddy --port N→ rs-codebuddy::run
├─ --plugin-codebuddy-intl    → rs-codebuddy::run（翻译成 --plugin-codebuddy --region INTL）
├─ --plugin-comate --port N   → rs-comate  ::run
├─ --plugin-qoder  --port N   → rs-qoder   ::run
├─ --plugin-vibex  --port N   → rs-vibex   ::run
├─ --plugin-tuanjie           → **如实报错**：团结还没有 Rust crate（见 §5）
├─ -version                   → `ProxyDeck v3.20.10 (proxy core v1.2.0)`（与 Go 同形）
└─ --fax                      → **如实报错**：属 P7c
```

分发实现：[`src/dispatch.rs`](src/dispatch.rs)。三条纪律：

1. **参数口径不重造**——子模式 argv 交给各 crate **自己的 clap `Args` 解析器**
   （`tests/command_dispatch.rs` 与 `dispatch.rs` 的单测都断言「翻译后的 argv 能被各 crate
   的解析器吃下去，且 port/region/debug 落对位置」）。
2. `-flag` 与 `--flag` 两种写法都认（Go `flag` 包如此），分发器统一规范化为 `--flag`。
3. GUI 分支同样认 `-host` / `-port` / `-api-key`（缺省 127.0.0.1 / 55990 / 空，与 Go
   `main.go` 的 flag 缺省一致）；便携模式下把解析好的数据目录钉进 `PROXYDECK_DATA_DIR`，
   保证与没有 `--portable` 参数的那几个 crate 读到同一个目录。

## 2. 26 个 bridge 绑定的对齐表

清单来源（简报点名的那条命令，与黄金里 `bindings26` 逐个对上）：

```powershell
Select-String -Path app/bridge.go -Pattern 'w\.Bind\("([A-Za-z]+)"'   # → 26 个
```

采集器另做了**自检**：假 WebView 实际收到的注册名集合 = 这 26 个 + 4 个 `ccPlugin*`，
清单外出现任何名字即当场失败（防手抄错）。

| # | 绑定 | 实参 | 返回形状（JSON） | Go 位置 | Rust 位置 |
|---|---|---|---|---|---|
| 1 | `ccGetState` | — | `stateMsg`：15 个键按声明序输出（`running,external,phase,host,port,baseUrl,uptime,autostart,apiKey,noticeDismissed,closeHintDismissed,version,startedOnce,core,pid[,lastErr]`；`lastErr` 空串**不出现**） | `bridge.go:664`（`stateMsg` 定义 `:337-355`，`state()` `:357-406`） | `bridge.rs:106` → `state.rs::App::state` |
| 2 | `ccLiveSnapshot` | — | `{"live":{core+tuanjie+codebuddy+codebuddy-intl+bai+comate+qoder+vibex},"ok":true}`（键序排序，8 个键） | `bridge.go:672` | `bridge.rs:109` |
| 3 | `ccStart` | `key string` | `jsonOK/jsonErr`；接管分支文案「检测到 http://host:port 代理已在运行，已接入」，点火「点火成功 · … 已就绪（后台常驻，关窗不影响）」，超时「后台代理启动超时，请检查端口 N 是否被占用」 | `bridge.go:694` → `start()` `:240-307` | `bridge.rs:134` → `state.rs::App::start` |
| 4 | `ccStop` | — | `jsonOK`：「代理未在运行」/「后台代理已停止」/「代理核心已停堆（后台进程已结束）」；杀不掉回 `jsonErr(killByPort 的错误文案)` | `bridge.go:701` → `stop()` `:311-335` | `bridge.rs:139` → `state.rs::App::stop` |
| 5 | `ccStats` | — | `/v1/stats` 原始体 → 兜底本地 `stats.json` → `{"ok":false,"msg":"代理未运行或无法连接"}` | `bridge.go:709` | `bridge.rs:145` |
| 6 | `ccUsage` | — | 缓存原文；无缓存 `{"ok":false,"msg":"统计加载中"}` | `bridge.go:721` | `bridge.rs:159` |
| 7 | `ccUsageRefresh` | — | `{"ok":true}`（非阻塞踢一次后台刷新，cap=1 去重） | `bridge.go:744` | `bridge.rs:167` |
| 8 | `ccSaveKey` | `key string` | `jsonOK("已清除保存的 Key")` / `jsonOK("API Key 已保存")` / `jsonErr`；同时置失效 key 探活缓存 | `bridge.go:752` | `bridge.rs:173` |
| 9 | `ccProbeKey` | — | 命中缓存回缓存原文；否则 `{"status":"pending"}`（后台真探，结果形如 `{"httpCode":N,"status":"ok\|unauthorized\|error\|nokey"}`，网络错误再补 `"detail"`） | `bridge.go:778`（`ProbeKey` = `internal/proxy/probe.go:15`） | `bridge.rs:192` → `keyprobe.rs` |
| 10 | `ccCheckUpdate` | — | 缓存原文；过期/首跑踢后台拉取并回 `{"ok":false,"msg":"checking"}`（成功缓存 30min / 负缓存 10min / 自动踢取 30s 限频） | `bridge.go:817` | `bridge.rs:216` → `update.rs::cc_check_update` |
| 11 | `ccCheckUpdateForce` | — | 恒 `{"ok":false,"msg":"checking"}`（清缓存 + 无条件踢取） | `bridge.go:853` → `checkUpdateForce()` `:514-524` | `bridge.rs:217` → `update.rs::check_update_force` |
| 12 | `ccOneKeyUpdate` | `action string` | `status` → `{"msg":…,"ok":true,"pct":…,"phase":…}`（初值 `idle/-1/""`）；`""`/`start` → `{"ok":true,"msg":"started"}` 或「更新流程已在进行中」/「当前版本信息没有 exe 直链…」；其它 → `{"ok":false,"msg":"未知 action: X"}` | `bridge.go:948` → 流程 `:1142-1346` | `bridge.rs:218` → `update.rs::one_key_update` |
| 13 | `ccCalib` | `model, v string` | 核心返回体；`jsonErr`：「缺少模型名」/「请输入数字（官网该模型总 Token 数）」/「保存失败: …」 | `bridge.go:963` | `bridge.rs:225` |
| 14 | `ccModels` | `plan string` | 核心返回体（`plan=="go"` 走 `?plan=go`）；回落 `{"ok":false,"msg":"代理未运行或无法连接"}` | `bridge.go:990` | `bridge.rs:248` |
| 15 | `ccAutostart` | `on bool` | `jsonOK`：「开机自启已取消」/「开机自启已设置 · 登录 Windows 后将以无窗口模式自动点火」；写失败 `jsonErr("写入启动文件夹失败（被杀软拦截？）: …")` | `bridge.go:1000` → `setAutostart` `:417-441` | `bridge.rs:259` → `bridge.rs::set_autostart` |
| 16 | `ccOpen` | `u string` | `jsonOK("已打开")`；非 http(s) → `jsonErr("仅允许打开 http(s) 链接")` | `bridge.go:1007` → `openURL` `:636-641` | `bridge.rs:264` → `proc.rs::open_url` |
| 17 | `ccCopy` | `s string` | `jsonOK("已复制")` / `jsonErr` | `bridge.go:1013` | `bridge.rs:269` → `proc.rs::clipboard_write_all` |
| 18 | `ccDismiss` | — | `jsonOK("已记录")`（写 `notice_dismissed.flag`，**DPAPI 密文**） | `bridge.go:1019` | `bridge.rs:274` |
| 19 | `ccDismissCloseHint` | — | `jsonOK("已记录")`（写 `close_hint_dismissed.flag`，DPAPI 密文） | `bridge.go:1025` | `bridge.rs:279` |
| 20 | `ccWindowCmd` | `cmd string` | `jsonOK("")`；拿不到句柄 → `jsonErr("窗口句柄不可用")`；`close` 走 `trayQuit` 语义 | `bridge.go:1035` → `windowCmd` `platform_windows.go:122`、`trayQuit` `tray_windows.go:149` | `bridge.rs:285` → `host.rs` |
| 21 | `ccRequestClose` | — | `ask` → `{"ok":true,"mode":"ask"}`；否则按记忆模式执行（`applyCloseMode`：`{"ok":true,"msg":"已退到托盘"}` / `{"ok":true,"msg":"正在退出"}` / 句柄不可用） | `bridge.go:1053` | `bridge.rs:299` |
| 22 | `ccCloseChoice` | `mode string, remember bool` | 未知模式 → `jsonErr("未知关窗模式: X")`；`remember` 才落盘；句柄不可用 → `jsonErr`；否则同 `applyCloseMode` | `bridge.go:1064` | `bridge.rs:310` |
| 23 | `ccCloseModeGet` | — | `{"mode":"ask\|tray\|quit","ok":true}`（键序排序） | `bridge.go:1080` → `closeMode()` `:62-74` | `bridge.rs:326` → `bridge.rs::close_mode` |
| 24 | `ccCloseModeSet` | `mode string` | `jsonOK("已保存")`；未知模式 → `jsonErr("未知关窗模式: X")`；`ask` = 删文件 | `bridge.go:1084` → `setCloseMode` `:77-88` | `bridge.rs:331` → `bridge.rs::set_close_mode` |
| 25 | `ccKeyMapGet` | — | `{"map":[[…]],"ok":true}`；无文件/非法 → `{"ok":false,"msg":"no saved keymap"}` | `bridge.go:1097` → `keyMapGet` `:127-137` | `bridge.rs:342` → `bridge.rs::key_map_get` |
| 26 | `ccKeyMapSet` | `raw string` | `jsonOK("已保存")`；形态非法 → `jsonErr("非法键位映射（1-4 键、每键 ≥1 页、页名须在白名单且一页一键）")`；JSON 坏 → `jsonErr(解析错文案)` | `bridge.go:1105` → `keyMapSet` `:140-152` | `bridge.rs:356` → `bridge.rs::key_map_set` |

### 落盘口径（照抄 Go，门禁用「格式 + 能否解回同一明文」比）

| 文件 | 写法 | 形态 |
|---|---|---|
| `api-key.txt` | `appdir.WriteFile` | **DPAPI 密文**（`PDAPI1\n` + base64 + `\n`） |
| `notice_dismissed.flag` / `close_hint_dismissed.flag` | `appdir.WriteFile` | **DPAPI 密文** |
| `close_mode.txt` | 裸 `os.WriteFile` | 明文 |
| `keymap.json` | 裸 `os.WriteFile` | 明文 |
| `stats.json` | 核心进程写 | 明文，GUI 只读 |
| `headless-error.log` | 子进程 stdout/stderr 落盘 | 带 `YYYY-MM-DD HH:MM:SS ` 行头、1 MB × 3 轮转 |

### 仍然由 Go 宿主提供、本轮**没做**的 7 个绑定（P7b/P7c 接手）

| 绑定 | Go 位置 | 归属 |
|---|---|---|
| `ccPluginList` / `ccPluginLog` / `ccPluginStart` / `ccPluginStop` | `plugin_bindings.go:45/50/79/86` | **P7b**（插件托管） |
| `ccOpenFax` | `main.go:65` | **P7c**（注水专线浮窗） |
| `ccFaxWin` / `ccFaxClose` | `main.go:131/143` | **P7c** |

> ui.html 的实际调用面已由 `tests/ui_html.rs` 反向锁定：界面用到的每个 `window.ccX` /
> `call('X')` 都必须落在这 26 + 7 之内，出现别的名字即测试红。

## 3. 门禁（GUI 没有 HTTP 合同，本阶段自定的四道）

| 门禁 | 位置 | 判据 |
|---|---|---|
| **A. 构建期 ui.html 合同** | `build.rs` | `rs-gui/ui.html` 与 `app/ui.html` 字节不等 → **编译失败** |
| **B. ui.html 逐字节 + 哈希** | `tests/ui_html.rs` | 内嵌字节 = 仓库 `app/ui.html` 现内容，SHA-256 = `uihtml::UI_HTML_SHA256`；界面调用面 ⊆ 我们提供的绑定 |
| **C. Go↔Rust 差分对照** | `tests/bridge_contract.rs` + `tests/golden/bridge-go-reference.json` | 黄金场景 5 个 / **53 步命令**逐条比对（归一化后逐字节）+ 落盘文件格式与逻辑内容比对 + 26 个绑定的存在性补跑 |
| **D. 分发骨架真跑** | `tests/command_dispatch.rs` | 真起 `rs-gui.exe`：`-version` 文案与退出码、缺 crate 的 `--plugin-tuanjie` 如实报错且不占端口、`--fax` 如实报未实现 |

### 黄金是怎么来的（不是转抄，是**驱动真实 Go 闭包**）

`.work/rust-rewrite/_p7a_collect_test.go`（跑完从 `app/` 删除，只留档在 `.work/`）：

1. 给 `webview.WebView` 接口写一个假实现（只需 `Bind` / `Unbind` / `Window`）；
2. 让生产代码 `app.bindAll(fake)` 把**真实的 26 个闭包**原样交出来，再 `reflect.Call` 按位置调用；
3. 全程隔离：`PROXYDECK_DATA_DIR` + `APPDATA` 指临时目录，`PATH` 清空 + 代理指向死端口
   （离线、不起外部进程、不碰用户真实状态），`--fax` / GUI 分支一概不进。

Rust 侧把**同一串命令、同一套场景前置**再跑一遍，两边跑同一套归一化规则后逐字节比。

### 差分里**明示**的四处豁免（不是静默放过，黄金 `comparison_exemptions` 里也登记着）

1. `ccKeyMapSet` 的 JSON 解析错误文案——Go `encoding/json` 与 serde_json 的报错文本不可能相同；
2. `appdir.log`——带时间戳，且内容取决于 `icacls` 是否在 PATH 上（采集器为了离线把 PATH 清空了）；
3. `PDAPI1` 密文字节本身——DPAPI 每次产出不同，改比「能否解回同一明文」；
4. `ccLiveSnapshot` 的取值——随本机各插件端口实时状态变化，只比键集与 `ok` 形状。

### 跑门禁

```powershell
# 单元测试（44 项）
pwsh -NoProfile -File .work/p7a/_p7atest.ps1 -Targets lib
# 四个集成测试二进制（ui_html / bridge_contract / command_dispatch）
pwsh -NoProfile -Command "& 'F:\AIXM\proxydeck\.work\p7a\_p7atest.ps1' -Targets ui_html,bridge_contract,command_dispatch"
```

`_p7atest.ps1` 沿用 P6/P5 的火绒规避（链接后立刻把测试 exe 拷到工作区再跑），
并**加固为「编译失败即抛错」**：绝不回落到旧二进制假装通过（已实测：
参数写错时它确实抛错而不是跑旧包）。构建目录固定 `CARGO_TARGET_DIR=E:\cargo-target\rs-gui`。

## 4. 与 Go 实现的结构性差异（有意为之，逐条登记）

| # | 差异 | 理由 / 影响 |
|---|---|---|
| 1 | Go 的 `w.Bind` 按位置把 JS 实参反序列化进形参；Rust 走 `dispatch(name, args)` | webview_go 的绑定机制不能照搬；位置语义与实参形态未变，差分门禁逐条比过 |
| 2 | 窗口/托盘 → `Host` trait；剪贴板 → `Clipboard` trait | 让门禁能在**没有窗口、不动真人剪贴板**的前提下驱动 26 条（Go 采集器靠 `w.Window()==nil` 达到同类隔离） |
| 3 | `json!({...})` 不能用来产出桥响应，必须走 `jsons::obj` | 依赖图里 `rs-proxy` 打开了 serde_json 的 `preserve_order`，**特性统一**后本 crate 的 Map 也变成保序的（实测踩到：键序变成书写序而非 Go 的排序）。已改为显式按排序构造，并在单测里钉住 |
| 4 | `acl_user_only` 的 `icacls` 走隐藏窗口 | Go `acl_windows.go` 用裸 `exec.Command`，GUI 下会闪一下黑窗；Rust 侧统一 `CREATE_NO_WINDOW`。副作用层面的改进，文件权限行为不变 |
| 5 | `start()` 的 headless 子进程日志：Go 是 `OpenLog` + `StampedWriter`（写完判大小轮转），Rust 是逐行 `append_log`（写前轮转） | 目的与格式相同（时间戳行头 + 1 MB × 3）；轮转时机差一行，登记在案 |
| 6 | `migrateLegacyStats` 未移植 | Go 侧该函数**全库无调用点**（死代码，`bridge.go:219`），移植它等于搬家一段没人跑的代码 |
| 7 | `eve_dir`/`data_dir` 不做 `EvalSymlinks` | Go 侧 `EvalSymlinks` 在无符号链接时是恒等变换；本项目不使用符号链接。登记以便日后核对 |

## 5. **没做**与留给后续阶段的（如实登记）

### P7a 范围内没做到的

1. **`--plugin-tuanjie` 无 Rust 承载**：六个 crate 里没有 rs-tuanjie（团结后端仍是
   `internal/tuanjie` 的 Go 实现）。分发器对它**如实报错并退出 1**，不静默降级、不假装启动。
   要落地必须新增 `rs-tuanjie` crate（体量与 rs-codebuddy 同级）。
2. **`ccCalib` 未被 ui.html 调用**：宿主提供、当前界面零引用（历史绑定）。它的对齐由
   差分门禁覆盖，`tests/ui_html.rs` 的界面调用面断言不涉及它。
3. **GUI 分支只编译未启动**：本轮**没有**运行过 `rs-gui` 的无参模式——启动会弹窗，
   干扰正在用 GUI 的用户。窗口外观/交互的验收属 P7c 的人工清单。

### P7b（插件托管，`plugins.go` + `plugin_modes.go`）

- `pluginDefs` 注册表**已建**（`src/plugins.rs`，含健康 URL 与日志路径）；
  仍缺：探活接管、拉杆启停、退出监视 + 失败原因、停堆杀后复验、换血标记消费、
  `pluginList`/`pluginLog`/`pluginStart`/`pluginStop` 四个绑定、`OpenLog`/`StampedWriter`。
- 形态已定（单 exe 子模式）：`pluginStart` 应当 spawn **本 exe** 的 `--plugin-<id>`。

### P7c（宿主外壳，`main.go` / `platform_windows.go` / `tray_windows.go` / `rollover.go`）

- `webview.rs` 已做：窗口（540×760）、进程内 HTTP 提供 ui.html、26 桥注入、IPC 派发、WM_CLOSE 关窗。
- 仍缺：无边框 `setFrameless`、窗口图标、DPI 感知、单实例、托盘整套
  （`trayStartup`/`trayHide`/`trayQuit`）、`showWindowIfHidden` 的 600ms 自愈、
  `--fax` 浮窗、`rollStaleBackends` 换血、`appdir.Migrate` + `UpgradePlaintext`
  （exe 同目录历史状态文件搬入 + 存量明文凭据升级为 DPAPI 密文）。
- 一键更新的**下载→校验→腾位替换→换血标记→重启**已实现（`update.rs`），
  只差「关窗」这一跳接真实托盘/窗口句柄（现在经 `Host`）。
- `-headless` 子模式两处与 Go 的剩余差异（P7a 审查点名，如实登记，属 main.go 剩余面，随 P7c 修）：
  ① Go 的 `runCoreHeadless` 有「僵死端口先清再听」（killByPort + 300ms 等待），Rust 的
  `Mode::Proxy` 直接绑定、占用即报错（GUI 经 `state.rs` 的父侧清理兜住；但开机自启 VBS
  直起 rs-gui 的路径将来会踩到）；② Go 子进程失败自写 `headless-error.log`
  （`headless 启动失败：…`），Rust 只 eprintln（靠父进程捕获，文案不同）。

### P7d（打包）

- 单 exe 需内嵌 `icon.ico` + `resource.syso`（Rust 侧对应 `.rc`/`winres`）；
  版本串同步清单要从 Go 的 8 处扩展到含 `rs-gui/src/lib.rs` 的 `APP_VERSION`；
  多二进制与单 exe 的取舍与 P9 打包合并评估。

## 6. 本轮验收（实测，命令与输出见 `.work/p7a/`）

| 项 | 结果 |
|---|---|
| `cargo test`（lib） | **45 项全绿** |
| `ui_html` | 2 项全绿（字节一致 + 哈希 + 界面调用面覆盖） |
| `bridge_contract` | 2 项全绿（差分 **53 步** + 文件快照 + 26 绑定存在性补跑） |
| `command_dispatch` | 3 项全绿（真起 exe） |
| `rs-vibex` | 37 + 3 + 3 = **43 项全绿**（薄包装后行为零变化） |
| `rs-proxy` | 86 + 11 + 3 = **100 项全绿**（同上） |
| `go test ./app/... -count=1` / `go vet ./app/...` | Go 侧零改动，`ok` / 干净 |
| 薄包装行为对照（真跑二进制） | `rs-vibex` 无参 → 退出码 **2** + 「请用 --plugin-vibex 启动（VibeX 插件服务模式）」；`--help` 六项参数与默认端口 8790 原样；`rs-proxy --version-only` → `rs-proxy 0.1.0 (proxy core P6)` + 退出码 0；`--help` 七项参数与默认端口 55990 原样 |

### 突变检验：证明差分门禁**有判别力**（不是摆设）

`pwsh -File .work/p7a/_p7a-mutations.ps1` 逐条注入「Go 与 Rust 会分道扬镳」的改动，
要求 `bridge_contract` 必须变红，跑完立刻还原；**编译被挡住不算抓住**（那证明不了差分有判别力）。

| 突变 | 结果 |
|---|---|
| `jsons::obj` 不再按排序落键 | **CAUGHT**（差分报 8 处不一致） |
| `ccGetState` 的 `baseUrl` 改名 `baseURL` | **CAUGHT**（4 处） |
| `ccDismiss` 文案多一个空格 | **CAUGHT**（2 处） |
| 关窗模式缺省 `ask → tray` | **CAUGHT**（2 处） |

脚本以「未抓住条数」为退出码，本轮 `==== mutations: 4, not-caught: 0 ====`（退出码 0），
四段原始输出在 `.work/p7a/_p7a-mutations.log`。

## 7. 门禁覆盖不到的地方（如实列出）

- **窗口/托盘/单实例/关窗退托盘**：无法自动化，属 P7c 的人工验收清单。
- **`ccCopy` 的真实剪贴板**：门禁走 `NullClipboard`（不动用户剪贴板），真机行为待人工确认。
- **一键更新的真实替换链**：跑它会替换 exe 并重启，绝不在门禁里跑；只做了「无直链」「状态查询」
  这类无副作用分支的对照。
- **`ccStart` 的 spawn 分支**：差分只覆盖「探活接管」分支（起了子进程就等于在测试里拉后端服务）；
  spawn 路径的代码与 Go 逐行对齐，但本轮未在门禁里实跑。
