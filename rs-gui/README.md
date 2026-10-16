# rs-gui —— ProxyDeck GUI 宿主的 Rust 重写（P7a 桥接层与状态 / P7b 插件托管）

> 本文件按简报的「硬性边界」写：`rs-gui/**` + `.work/**` + 简报 + 两处薄包装
> （`rs-vibex` / `rs-proxy` 的 `main`→`lib`）是本轮改动的全部落点。
> `app/**`（含 `ui.html`）与全部 Go 源码**只读**；`rs-bai` / `rs-codebuddy` / `rs-comate` /
> `rs-qoder` 四个 crate 一个字未动。

## 0. 阶段进度

| 阶段 | 内容 | 状态 |
|---|---|---|
| P7a | 桥接层（26 个 `bind`）+ 状态聚合 + 分发骨架 + ui.html 逐字节复用 | 已提交（`31c92ce` 之前的 P7a 提交） |
| **P7b** | **插件托管（`plugins.go` 全部 + `plugin_modes.go` 剩余面）+ 4 个 `ccPlugin*` 绑定 + `OpenLog`/`StampedWriter`** | **本轮** |
| P7c | 宿主外壳：`main.go` flag 收口、无边框/图标/DPI、单实例、托盘、`--fax` 浮窗、`rollStaleBackends` 换血、`appdir.Migrate + UpgradePlaintext` | 待做 |
| P7d | 打包（单 exe + 图标/版本资源；多二进制取舍与 P9 合并评估） | 待做 |

### P7b 改了哪些文件（逐条登记，供闸门核对）

| 文件 | 改动 |
|---|---|
| `src/plugins.rs` | 插件托管全部：注册表（`plugin_defs()`）、`PluginHost::{list,start,stop,log_lines}`、`read_log_tail`、`plugin_exit_reason`、`go_lossy_string`/`truncate_300_go`/`trim_space_bytes`（Go 字节级语义）、Python 分支 `py_cmd`/`check_plugin_deps`、`SpawnTarget` |
| `src/appdir.rs` | `open_log` / `rotate_if_big` / `StampedWriter`（Go `logfile.go` 对应物） |
| `src/bridge.rs` | `PLUGIN_BINDINGS` 4 条 + 4 个 `ccPlugin*` 分支 + `Bridge::plugins`（`with_plugins` 测试缝）+ `int_arg` |
| `src/webview.rs` | 注入脚本覆盖 30 个绑定名 |
| `src/dispatch.rs` | `plugin_modes.go` 剩余面：子模式失败写 `<id>-plugin-error.log` |
| `src/update.rs` | `consume_update_marker` 改 `(String, bool)`，对齐 Go 的三处边界 |
| `src/lib.rs` | 模块声明 + `#[cfg(test)] mod plugin_hosting_tests` |
| `src/plugin_hosting_tests.rs` | **新建**：12 条插件托管行为测试（假后端进程 + 测试端口 1981x） |
| `tests/fixtures/fake-backend.js` | **新建**：假后端进程（与 Go 采集器共用同一份） |
| `tests/bridge_contract.rs` | 差分门禁扩到插件场景（9 场景 / 90 步）+ 三组纯函数用例 + spawn argv 断言 |
| `tests/ui_html.rs` | 界面调用面的「我方提供」集合加上 4 个插件绑定 |
| `tests/command_dispatch.rs` | 新增「失败子模式写诊断文件」 |
| `tests/golden/bridge-go-reference.json` | **重采**（P7a 部分逐字节不变，见 §6 最后一行） |
| `README.md` | 本文件 |

## 1. 形态（用户裁决：单 exe 多子模式）

```
rs-gui.exe
├─ 无参                       → GUI（wry + tao 外壳 + 30 个 bridge 绑定）
├─ -headless                  → rs-proxy（主代理核心，库依赖直调 rs_proxy::run）
├─ --plugin-bai   --port N    → rs-bai     ::run
├─ --plugin-codebuddy --port N→ rs-codebuddy::run
├─ --plugin-codebuddy-intl    → rs-codebuddy::run（翻译成 --plugin-codebuddy --region INTL）
├─ --plugin-comate --port N   → rs-comate  ::run
├─ --plugin-qoder  --port N   → rs-qoder   ::run
├─ --plugin-vibex  --port N   → rs-vibex   ::run
├─ --plugin-tuanjie           → **如实报错**：团结还没有 Rust crate（见 §5）+ 写
│                               `<数据目录>/tuanjie-plugin-error.log`（Go `pluginFailLog` 同源）
├─ -version                   → `ProxyDeck v3.20.10 (proxy core v1.2.0)`（与 Go 同形）
└─ --fax                      → **如实报错**：属 P7c
```

**插件托管形态（P7b）**：GUI 侧的 `ccPluginStart` spawn **本 exe** 的 `--plugin-<id> --port N`
（`PluginHost`，见 §2.2），与 Go 版逐位同形——打包/一键更新/腾位换血/托盘那套机制因此原样可用。

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

### 仍然由 Go 宿主提供、本轮**没做**的 3 个绑定（P7c 接手）

| 绑定 | Go 位置 | 归属 |
|---|---|---|
| `ccOpenFax` | `main.go:65` | **P7c**（注水专线浮窗） |
| `ccFaxWin` / `ccFaxClose` | `main.go:131/143` | **P7c** |

> ui.html 的实际调用面已由 `tests/ui_html.rs` 反向锁定：界面用到的每个 `window.ccX` /
> `call('X')` 都必须落在这 26 + 4 + 3 之内，出现别的名字即测试红。

## 2.1 插件托管的 4 个绑定（P7b）

对应 Go `app/plugin_bindings.go`（前三个绑定 + 日志读取），实现主体在
[`src/plugins.rs`](src/plugins.rs) 的 `PluginHost`。

| # | 绑定 | 实参 | 返回形状（JSON） | Go 位置 | Rust 位置 |
|---|---|---|---|---|---|
| 27 | `ccPluginList` | — | 数组，按 `pluginDefs` 顺序；每项 11 个键**排序输出**：`dir,healthy,id,lastErr,log,name,native,port,present,running,url` | `plugin_bindings.go:45` → `plugins.go:158-202` | `bridge.rs` → `PluginHost::list` |
| 28 | `ccPluginLog` | `id string, lines int` | `{"ok":true,"lines":[…尾部 lines 行…]}`；未知 id → `{"ok":false,"msg":"未知插件"}`（**字面量**）；无日志 → `{"ok":false,"msg":"暂无日志"}`（**字面量**）。`lines` 实参形态：`bridge.rs::int_arg` 取第 i 个实参——JSON 数字→整数（非数字落 0），与 Go 侧 webview_go 传参形态一致（该条此前只在代码注释里，补登于此） | `plugin_bindings.go:50` → `readLogTail` | `PluginHost::log_lines` |
| 29 | `ccPluginStart` | `id string` | `jsonOK("已启动")` / `jsonErr(原因)`；id 先 `TrimSpace` | `plugin_bindings.go:79` → `plugins.go:207-296` | `PluginHost::start` |
| 30 | `ccPluginStop` | `id string` | `jsonOK("已停止")` / `jsonErr(原因)`；id 先 `TrimSpace` | `plugin_bindings.go:86` → `plugins.go:335-375` | `PluginHost::stop` |

**托管行为逐条对齐**（差分 + 行为测试都锁着）：

| 行为 | 判据 |
|---|---|
| 探活接管 | 端口上已有健康实例 → `running=true`（外部实例，pid 为 None），**不杀不重启**；再 start 报「<显示名> 已在运行」 |
| 清理非健康占位 | 端口被非健康进程占 → `killByPort` + 300ms → 再 spawn |
| spawn 形态 | 本 exe + `--plugin-<native> --port <port>` + `appdir::portable_args()` + `def.args`（逐位断言） |
| 子进程日志 | 数据目录 `plugins/<id>.log`，`OpenLog` + `StampedWriter`（时间戳行头、追加写、1 MB×3 轮转） |
| 启动前清诊断 | spawn 前 `remove(<id>-plugin-error.log)`（否则上轮旧错会被当成本次原因） |
| 退出监视 | 子进程一死即复位 `running=false` 并带上 `pluginExitReason`；**主动停堆/换新实例不误判**（token 世代号） |
| 失败原因文案 | ①诊断文件（TrimSpace 后 ≤300 **字节**，超出截断加 `…`）→「启动失败：…」；②退出码 →「<显示名> 启动后退出：exit status N」；③都没有 →「<显示名> 启动后退出（无诊断信息）」 |
| 陈旧报错自愈 | 端口已健康 → 把**存下来的** lastErr 清掉（清单要两次探活才看得出差别，见行为用例） |
| 停堆杀后复验 | 杀不掉（如对方以管理员运行）→ 如实报错「旧进程杀不掉（端口 N 仍被占）：…」并**恢复跟踪**（`running=true`） |
| 换血标记消费 | `consume_update_marker() -> (String, bool)`：读到即删；非对象根 → `ok=false`；根为 `null` → `ok=true`（与 Go 的 `map` 语义一致） |

### 落盘口径（照抄 Go，门禁用「格式 + 能否解回同一明文」比）

| 文件 | 写法 | 形态 |
|---|---|---|
| `api-key.txt` | `appdir.WriteFile` | **DPAPI 密文**（`PDAPI1\n` + base64 + `\n`） |
| `notice_dismissed.flag` / `close_hint_dismissed.flag` | `appdir.WriteFile` | **DPAPI 密文** |
| `close_mode.txt` | 裸 `os.WriteFile` | 明文 |
| `keymap.json` | 裸 `os.WriteFile` | 明文 |
| `stats.json` | 核心进程写 | 明文，GUI 只读 |
| `headless-error.log` | 子进程 stdout/stderr 落盘 | 带 `YYYY-MM-DD HH:MM:SS ` 行头、1 MB × 3 轮转 |
| `plugins/<id>.log` | 插件子进程 stdout/stderr（`OpenLog`+`StampedWriter`） | 同上 |
| `<id>-plugin-error.log` | 插件子模式启动失败自写（`AppendLog`） | 带时间戳行头、正文以「启动失败：」开头 |

## 3. 门禁（GUI 没有 HTTP 合同，自定的六道）

| 门禁 | 位置 | 判据 |
|---|---|---|
| **A. 构建期 ui.html 合同** | `build.rs` | `rs-gui/ui.html` 与 `app/ui.html` 字节不等 → **编译失败** |
| **B. ui.html 逐字节 + 哈希** | `tests/ui_html.rs` | 内嵌字节 = 仓库 `app/ui.html` 现内容，SHA-256 = `uihtml::UI_HTML_SHA256`；界面调用面 ⊆ 我们提供的绑定（26 + 4，P7c 的 3 个除外） |
| **C. Go↔Rust 差分对照** | `tests/bridge_contract.rs` + `tests/golden/bridge-go-reference.json` | 黄金场景 **9 个 / 90 步命令**逐条比对（归一化后逐字节）+ 落盘文件格式与逻辑内容比对 + 30 个绑定的存在性补跑 + 换血标记 10 例 + spawn argv 逐位断言 |
| **D. 纯函数用例对照** | 同上（`exit_reason_cases_match_go` / `stamped_writer_cases_match_go`） | 退出原因 8 例（含 >300 字节截断、多字节断口、4KB 尾部读）、`StampedWriter` 6 例（分行/半行/空行/CRLF/多字节），期望值由 Go 侧实采 |
| **E. 插件托管行为测试** | `src/plugin_hosting_tests.rs`（lib 段，12 条）+ `tests/command_dispatch.rs` | 真起假后端进程（`tests/fixtures/fake-backend.js`）+ 测试端口 1981x：接管/自愈、spawn argv、退出监视三种文案、停堆成功／「杀不掉如实报错」／**按 PID 收掉没上线的子进程**、未知 id、并发探活、失败子模式写诊断文件 |
| **F. 分发骨架真跑** | `tests/command_dispatch.rs` | 真起 `rs-gui.exe`：`-version` 文案与退出码、缺 crate 的 `--plugin-tuanjie` 如实报错且不占端口、`--fax` 如实报未实现、失败子模式写 `<id>-plugin-error.log` |

### 黄金是怎么来的（不是转抄，是**驱动真实 Go 闭包**）

`.work/rust-rewrite/_p7b_collect_test.go`（P7a 采集器的扩表版；跑完从 `app/` 删除，只留档在 `.work/`）：

1. 给 `webview.WebView` 接口写一个假实现（只需 `Bind` / `Unbind` / `Window`）；
2. 让生产代码 `app.bindAll(fake)` 把**真实的 30 个闭包**原样交出来，再 `reflect.Call` 按位置调用；
3. 插件场景临时把包级 `pluginDefs` 换成**测试注册表**（`zz-alpha`/`zz-beta`/`zz-gamma`，
   端口 19788~19790）——**绝不碰现行 8785~8791**；前置动作只有三种：
   `inproc_healthy:<port>`（本进程占端口，killByPort 排除自己）、`node:serve:<port>`、
   `node:raw:<port>`（假后端进程，两边共用同一份脚本）；
4. 全程隔离：`PROXYDECK_DATA_DIR` + `APPDATA` 指临时目录、代理指向死端口；
   ⚠ **PATH 不清空**（P7a 清过）：`portBusy`/`killByPort` 要 `netstat`/`taskkill`，清了就把被测逻辑废掉。

Rust 侧把**同一串命令、同一套场景前置**再跑一遍，两边跑同一套归一化规则后逐字节比。

### 差分里**明示**的六处豁免（不是静默放过，黄金 `comparison_exemptions` 里也登记着）

1. `ccKeyMapSet` 的 JSON 解析错误文案——Go `encoding/json` 与 serde_json 的报错文本不可能相同；
2. `appdir.log`——带时间戳，且内容取决于 `icacls` 是否在 PATH 上；
3. `PDAPI1` 密文字节本身——DPAPI 每次产出不同，改比「能否解回同一明文」；
4. `ccLiveSnapshot` 的取值——随本机各插件端口实时状态变化，只比键集与 `ok` 形状；
5. `PL3` 的 `plugins/zz-beta.log`——两边被 spawn 的子进程不同（Go=测试二进制、Rust=假后端），
   日志正文必然不同；**退出原因本身**由 `ccPluginList` 的 `lastErr` 逐字锁；
6. 纯函数用例的期望值按**值**比——含非法 UTF-8 时 Go 的 json 编码器写 `\ufffd` 转义、
   Rust 侧是同值字符（U+FFFD），字节形态本就不同（值相同）。

### 跑门禁

```powershell
# 单元测试 + 行为测试（76 项：64 lib 含 13 条插件行为 + 2+4+4 集成）
pwsh -NoProfile -Command "& 'F:\AIXM\proxydeck\.work\p7a\_p7atest.ps1' -Targets lib,bridge_contract,ui_html,command_dispatch"
# 突变检验（证明门禁有判别力；15 条全部必须被抓住）
pwsh -NoProfile -File .work/p7b/_p7b-mutations.ps1
```

`_p7atest.ps1` 沿用 P6/P5 的火绒规避（链接后立刻把测试 exe 拷到工作区再跑），
并**加固为「编译失败即抛错」**：绝不回落到旧二进制假装通过（已实测：
参数写错时它确实抛错而不是跑旧包）。构建目录固定 `CARGO_TARGET_DIR=E:\cargo-target\rs-gui`。
⚠ 多目标要用 `-Command "& '…' -Targets a,b,c"` 写法：`-File` 会把 `a,b,c` 当成单个目标名。

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
| 8 | 插件子进程的 `Child` 归**退出监视线程**所有；`pluginStop` 按 **PID** 收（`taskkill /F /PID`），实例身份用 token 世代号 | Go 的 `pluginState` 持 `*exec.Cmd`、`cmd.Wait()` 在监视 goroutine、`Process.Kill()` 按句柄。两者都是 TerminateProcess 语义；「主动停堆不误判」的判定从 `st.cmd != cmd` 换成 `token` 不匹配 |
| 9 | 子进程 stdout/stderr 各起一个读线程，经同一把锁写同一份 `StampedWriter` | Go 是一条 `StampedWriter` 同时当 `Stdout`/`Stderr`（两条管道并发写同一 `buf`，无锁）；Rust 加锁后**同一行不会被两个流撕裂**，日志格式（时间戳行头/分行规则）逐字同 Go |
| 10 | 插件子模式的启动日志文案不再带 Go `log` 包的日期前缀 | 各 crate 自己 `println!`（如 `bai-plugin: …`），内容可比但前缀形态不同；插件日志面板照常显示 |
| 11 | **`SpawnTarget` 是测试缝**：门禁里把 spawn 目标换成假后端（node） | 生产默认 = `std::env::current_exe()`（单测 `production_spawn_target_is_this_exe` 钉住），argv 逐位由行为测试与差分门禁断言。要这么做的原因：Go 采集器 spawn 的是测试二进制，Rust 的测试二进制是 libtest——它收到未定义参数时的退出码是 **101** 而 Go `flag` 是 **2**，不换目标就比不了退出码文案 |

## 5. **没做**与留给后续阶段的（如实登记）

### P7a 范围内没做到的

1. **`--plugin-tuanjie` 无 Rust 承载**：六个 crate 里没有 rs-tuanjie（团结后端仍是
   `internal/tuanjie` 的 Go 实现）。分发器对它**如实报错并退出 1**，不静默降级、不假装启动。
   要落地必须新增 `rs-tuanjie` crate（体量与 rs-codebuddy 同级）。
   **P7b 补强**：这种失败现在也会写 `<数据目录>/tuanjie-plugin-error.log`（Go `pluginFailLog`
   同源），GUI 的退出监视因此能显示「启动失败：…」而不是停在「启动中」。
2. **`ccCalib` 未被 ui.html 调用**：宿主提供、当前界面零引用（历史绑定）。它的对齐由
   差分门禁覆盖，`tests/ui_html.rs` 的界面调用面断言不涉及它。
3. **GUI 分支只编译未启动**：本轮**没有**运行过 `rs-gui` 的无参模式——启动会弹窗，
   干扰正在用 GUI 的用户。窗口外观/交互的验收属 P7c 的人工清单。

### P7b（插件托管）范围内做到与没做到的

**做到**：`plugins.go` 全部（注册表 / 探活接管 / 拉杆启停 / 退出监视+失败原因 / 停堆杀后复验 /
陈旧报错自愈 / Python 插件分支的文案与依赖检查）+ `plugin_modes.go` 的失败诊断落盘 +
`appdir::open_log`/`StampedWriter` + 换血标记消费语义对齐 + 4 个 `ccPlugin*` 绑定。

**没做到（逐条如实登记）**：

1. **Python 插件分支没有门禁覆盖**：本仓 `pluginDefs` 七条全是原生插件，`plugins/*/main.py`
   也不存在 ⇒ 该分支在 Go/Rust 两侧都跑不到（Go 采集器同样只覆盖了「插件目录缺失」这条
   错误路径；`pyCmd`/`checkPluginDeps` 的文案按源码逐字对齐，但**无实测对照**）。
2. **`killByPort` 的真实外部进程收尾**：差分门禁里 `PL2` 真起 node 假后端并被 `taskkill /F /PID`
   收掉（`stop` 返回 Ok 即证明端口真空出）；但**管理员权限进程**那条（杀不掉）只能用
   「本进程占端口」模拟——权限拒绝场景无法在门禁里构造。
3. **`plugins/*.log` 的内容形态**：门禁只锁「存在性 + 尾部读法 + 行窗口」，
   子进程实际写了什么（各 crate 的启动日志）不在比对范围——两边子进程本来就不同。
   `PL3` 场景把该场景的 `plugins/zz-beta.log` 显式排除在落盘比对之外（黄金 `files_exclude`）。
4. **换血标记的消费时机**：`consume_update_marker` 的语义（含 10 例边界）已对齐并锁住，
   但「谁在什么时候调它」（`rollStaleBackends` 的全量/镜像名两条路径）属 **P7c**，本轮未做。

### P7c（宿主外壳，`main.go` / `platform_windows.go` / `tray_windows.go` / `rollover.go`）

- `webview.rs` 已做：窗口（540×760）、进程内 HTTP 提供 ui.html、**30 桥**注入、IPC 派发、WM_CLOSE 关窗。
- 仍缺：无边框 `setFrameless`、窗口图标、DPI 感知、单实例、托盘整套
  （`trayStartup`/`trayHide`/`trayQuit`）、`showWindowIfHidden` 的 600ms 自愈、
  `--fax` 浮窗、`rollStaleBackends` 换血（标记消费已就绪）、`appdir.Migrate` + `UpgradePlaintext`
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

## 6. P7b 验收（实测；日志见 `.work/p7b/`）

| 项 | 结果 |
|---|---|
| `cargo test`（lib，含 13 条插件托管行为测试） | **64 项全绿** |
| `bridge_contract` | **4 项全绿**（差分 **90 步** + 落盘快照 + 30 绑定存在性补跑 + spawn argv + 换血标记 10 例 + 退出原因 8 例 + StampedWriter 6 例） |
| `ui_html` | 2 项全绿（字节一致 + 哈希 + 界面调用面 ⊆ 26+4） |
| `command_dispatch` | **4 项全绿**（真起 exe；新增失败子模式写诊断文件） |
| `go test ./app/... -count=1` / `go vet ./app/...` | Go 侧零改动，`ok` / 干净（临时采集器已删除） |
| 突变检验（`.work/p7b/_p7b-mutations.ps1`） | **15 条突变，0 条漏网**（详见 `.work/p7b/_p7b-mutations.log`） |
| 重采忠实性 | 新旧黄金的 P7a 部分**逐字节相同**（唯二差异：`appdir.log` 因不再清空 PATH 而消失、3 份 `PDAPI1` 密文） |

P7a 的验收表（45/2/2/3 项）在 P7a 收口时已记录；本表只列 P7b 这一轮实测。

### 突变检验：证明门禁**有判别力**（不是摆设）

`pwsh -File .work/p7b/_p7b-mutations.ps1` 逐条注入「Go 与 Rust 会分道扬镳」的改动，
要求对应测试必须变红，跑完立刻还原；**编译被挡住不算抓住**。

| 突变 | 结果 |
|---|---|
| 退出原因前缀「启动失败：」变半角冒号 | **CAUGHT**（exit_reason 8 条全红） |
| `dir` 字段不再给空目录名特判 | **CAUGHT**（差分 9 处） |
| `healthy` 不再探活 | **CAUGHT**（差分） |
| 停堆不复验端口 | **CAUGHT**（差分 2 处：`PL4` 期望报错却回「已停止」） |
| `ccPluginLog` 取头几行而不是尾几行 | **CAUGHT**（差分 1 处） |
| 退出原因截断阈值 300 → 400 | **CAUGHT**（exit_reason 8 条） |
| 不看诊断文件、直接用退出码文案 | **CAUGHT**（exit_reason 8 条） |
| `ccPluginStart` 文案多一个空格 | **CAUGHT**（差分 3 处） |
| 未知 id 文案用全角冒号 | **CAUGHT**（差分 2 处） |
| `StampedWriter` 不跳过空行 | **CAUGHT**（stamped_writer 6 条） |
| 换血标记：根为 `null` 时判 false | **CAUGHT**（差分 1 处） |
| 陈旧报错不自愈 | **CAUGHT**（lib 行为用例） |
| 「已在运行」不再拦截 | **CAUGHT**（lib 行为用例 4 条） |
| 非原生插件的 `present` 恒 true | **CAUGHT**（差分 9 处） |

脚本以「未抓住条数」为退出码，本轮 `==== mutations: 14, not-caught: 0 ====`（退出码 0）。
头两条最初被漏掉（`STOP_VERIFY_WAIT` 与行窗口 `>` vs `>=`），查明原因是**这两处改法本身
不改变被测行为**；已换成真能分道扬镳的改法，并顺手补强了「陈旧报错自愈」的行为用例
（原先 `start()` 自己会清 lastErr，把自愈路径遮住了）。

## 7. 门禁覆盖不到的地方（如实列出）

- **窗口/托盘/单实例/关窗退托盘**：无法自动化，属 P7c 的人工验收清单。
- **`ccCopy` 的真实剪贴板**：门禁走 `NullClipboard`（不动用户剪贴板），真机行为待人工确认。
- **一键更新的真实替换链**：跑它会替换 exe 并重启，绝不在门禁里跑；只做了「无直链」「状态查询」
  这类无副作用分支的对照。
- **`ccStart` 的 spawn 分支**：差分只覆盖「探活接管」分支（起了子进程就等于在测试里拉后端服务）；
  spawn 路径的代码与 Go 逐行对齐，但本轮未在门禁里实跑。
- **插件场景的固定端口 19788~19790**：不走随机端口（黄金要可复现）。若这两个端口
  在本机被别的进程占着，`PL*` 场景会**当场失败并报出是哪个端口**（不静默跳过）；
  同样地，跨进程（lib 段 19810~19820 vs 集成段 19788~19790）刻意错开，避免 `cargo test`
  并行跑两个测试二进制时互相抢端口。另：并发探活用 19900~19906、回归用例 `stop_failure_*`
  用 19908（只做短暂试监听找空闲口，不绑定服务），超出上面 19810~19820 的带——
  属口径偏差，已在此说明；这些端口只探活/占位，不监听服务，无实害。另：`plugin_hosting_tests.rs` 的并发探活用
  **19900~19906**（只做短暂试监听找空闲口，不绑定服务），超出上面 19810~19820 的带——
  属口径偏差，已在此说明；这些端口只探活不监听，无实害。
- **`kill_by_port` 的 netstat 失败尾巴**：Rust 写死「无法运行 netstat」，Go 用 `%v` 实际错误。
  两侧该返回值都被调用方丢弃、**不可观测**，故只登记不改。
- **真·管理员权限进程**：见 §5 的 P7b 没做到 2。
