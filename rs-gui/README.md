# rs-gui —— ProxyDeck GUI 宿主的 Rust 重写（P7a 桥接层 / P7b 插件托管 / P7c 宿主外壳）

> 本文件按简报的「硬性边界」写：`rs-gui/**` + `.work/**` + 简报 + 两处薄包装
> （`rs-vibex` / `rs-proxy` 的 `main`→`lib`）是本轮改动的全部落点。
> `app/**`（含 `ui.html`）与全部 Go 源码**只读**；`rs-bai` / `rs-codebuddy` / `rs-comate` /
> `rs-qoder` 四个 crate 一个字未动。

## 0. 阶段进度

| 阶段 | 内容 | 状态 |
|---|---|---|
| P7a | 桥接层（26 个 `bind`）+ 状态聚合 + 分发骨架 + ui.html 逐字节复用 | 已提交 |
| P7b | 插件托管（`plugins.go` 全部 + `plugin_modes.go` 剩余面）+ 4 个 `ccPlugin*` 绑定 + `OpenLog`/`StampedWriter` | 已提交 |
| **P7c** | **宿主外壳：`main.go` flag 收口、无边框/图标/DPI、单实例（浮窗）、托盘整套、`--fax` 浮窗 + 3 个传真绑定、`rollStaleBackends` 换血、`appdir.Migrate + UpgradePlaintext`、`-headless` 两处剩余差异** | **本轮** |
| P7d | 打包（单 exe + 图标/版本资源；多二进制取舍与 P9 合并评估） | 待做 |

### P7c 改了哪些文件（逐条登记，供闸门核对）

| 文件 | 改动 |
|---|---|
| `src/win32.rs` | **新建**：`platform_windows.go` 全部——`setDPIAware`、`setFrameless`（含 Win11 圆角 + `SWP_FRAMECHANGED`）、`setWindowIcon`、`showWindowIfHidden`、`windowCmd`（含可测的 `window_cmd_action` 映射）、`setClientSize`、`framelessFullClient`（子类化）、`findFaxPopup`、`foregroundWindow`、`fatalBox`、`enableDarkTitleBar`、`GetSystemMetrics` |
| `src/tray.rs` | **新建**：`tray_windows.go` 全部——常驻托盘线程（独立 OS 线程 = Go `LockOSThread` 的对应物）、`NOTIFYICONDATAW`/`WNDCLASSEXW`/`MSG` 布局、`Shell_NotifyIconW` 挂/摘、`TaskbarCreated` 重挂、单击切换显隐、右键菜单、`trayStartup`/`trayHide`/`trayQuit` |
| `src/single.rs` | **新建**：命名互斥体单实例（`Local\ProxyDeck-FaxWindow` + `faxPopupMutexGuard` 语义） |
| `src/faxtool.rs` | **新建**：`ccOpenFax` / `ccFaxWin` / `ccFaxClose` 三条与浮窗常量、`SpawnTarget` 测试缝 |
| `src/faxwindow.rs` | **新建**：`runFaxWindow` 的对应物（580×784、无边框+客户区铺满、`#fax` 导航、33 桥） |
| `src/rollover.rs` | **新建**：`rollover.go` 全部——`isStaleImage`、`imagePathOfPID`、`parseNetstatListeners`、`classifyHolders`、`waitBackendHealthy`、`waitPortFree`、`rollOne`/`rollAllWith`/`rollStaleBackends`/`rollAllBackends` |
| `src/migrate.rs` | **新建**：`internal/appdir/migrate.go` 全部——`legacyStateFiles` 清单（33 条）、`Migrate`、`UpgradePlaintext` |
| `src/shell_tests.rs` | **新建**：宿主外壳真实行为测试（换血端到端起假进程、端口清理、`headless-error.log`） |
| `tests/shell_contract.rs` | **新建**：P7c 门禁（传真 3 绑定差分 + 源码锚 + 单实例/Migrate/Upgrade 真值表） |
| `src/host.rs` | `WindowHost` 接真实 `win32::window_cmd` / `tray::*`；`NoWindowHost` 的 `tray_hide` 文案改成 Go 的「窗口句柄不可用」；新增 `startup_tray` / `close_after_update` |
| `src/bridge.rs` | `FAX_BINDINGS` 3 条 + 3 个分支 + `Bridge::fax`（`with_fax` 缝）；`Bridge::new` 默认与 `App` 共用同一个 `PluginHost` |
| `src/webview.rs` | 起窗路径补 DPI/无边框/图标/`trayStartup`/600ms 可见性自愈；33 桥注入；`handle_ipc`/`serve_ui_html`/`window_hwnd` 变 `pub` |
| `src/dispatch.rs` | `setDPIAware` 最早调；`run_data_dir_init`（`Migrate` + `UpgradePlaintext`）；`Mode::Fax` 接真实浮窗 + 单实例守卫；`Mode::Proxy` 补「僵死端口先清再听」与失败自写 `headless-error.log`；GUI 分支挂 `roll_stale_backends` 后台线程 |
| `src/state.rs` | `App` 持 `PluginHost`（`plugins()` 访问器）；`App::new` 走 `update::cleanup_old_exe` |
| `src/update.rs` | 关窗那一步改走 `host::close_after_update` |
| `src/appdir.rs` | `file_exists` 注释更正：它是 Go `bridge.go` 版（目录也算），`migrate.rs` 另有私有版本对齐 Go `migrate.go` 版（目录不算，见下条差异 16） |
| `src/lib.rs` | 模块声明 + `#[cfg(test)] mod shell_tests` |
| `tests/command_dispatch.rs` | 删掉 P7a 的「`--fax` 报未实现」；新增「`-headless` 失败自写日志」 |
| `tests/ui_html.rs` | 「我方提供」集合加上 3 个传真绑定（`later` 清空），并断言界面确实用到这 3 个 |
| `tests/fixtures/fake-backend.js` | 新增显式 `--fake-mode=<mode>`（进程级 `RS_GUI_FAKE_MODE` 会被并行用例改掉，换血门禁需要一条稳定的表述方式）→ **`fake_backend_sha256` 已同步重算** |
| `README.md` | 本文件 |

## 1. 形态（用户裁决：单 exe 多子模式）

```
rs-gui.exe
├─ 无参                       → GUI（wry + tao 外壳 + 33 个 bridge 绑定 + 托盘 + 600ms 可见性自愈）
├─ -headless                  → rs-proxy（主代理核心，库依赖直调 rs_proxy::run；
│                               僵死端口先清再听；失败自写数据目录 headless-error.log）
├─ --plugin-bai   --port N    → rs-bai     ::run
├─ --plugin-codebuddy --port N→ rs-codebuddy::run
├─ --plugin-codebuddy-intl    → rs-codebuddy::run（翻译成 --plugin-codebuddy --region INTL）
├─ --plugin-comate --port N   → rs-comate  ::run
├─ --plugin-qoder  --port N   → rs-qoder   ::run
├─ --plugin-vibex  --port N   → rs-vibex   ::run
├─ --plugin-tuanjie           → **如实报错**：团结还没有 Rust crate（见 §5）+ 写
│                               `<数据目录>/tuanjie-plugin-error.log`（Go `pluginFailLog` 同源）
├─ -version                   → `ProxyDeck v3.20.10 (proxy core v1.2.0)`（与 Go 同形）
└─ --fax                      → 注水专线独立浮窗（580×784、标题「注水专线 · WATER PROBE FX-01」、
                                进程名互斥体单实例、`#fax` 模式、挂全 33 桥）
```

**插件托管形态（P7b）**：GUI 侧的 `ccPluginStart` spawn **本 exe** 的 `--plugin-<id> --port N`
（`PluginHost`，见 §2.2），与 Go 版逐位同形——打包/一键更新/腾位换血/托盘那套机制因此原样可用。
**换血（P7c）**：`dispatch` 的 GUI 分支起一条后台线程跑 `rollover::roll_stale_backends`
（换血标记优先 → 全量滚；无标记则按镜像名 `ProxyDeck.exe.old*` 判定），与 Go `main.go:244-247` 同位置、同前提。

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

### 三个绑定已全部提供（P7c 落地）

见 §2.0。ui.html 的实际调用面已由 `tests/ui_html.rs` 反向锁定：
界面用到的每个 `window.ccX` / `call('X')` 都必须落在这 26 + 4 + 3 之内，出现别的名字即测试红；
且该测试还断言界面**确实**用到了那 3 个传真绑定（否则"已提供"这件事就没被测到）。

## 2.0 注水专线浮窗的 3 个绑定（P7c）

对应 Go `app/main.go` 的 `bindFaxLauncher`（`ccOpenFax`）+ `runFaxWindow`（另两个）。
实现在 [`src/faxtool.rs`](src/faxtool.rs)，浮窗外壳在 [`src/faxwindow.rs`](src/faxwindow.rs)。

| # | 绑定 | 实参 | 返回形状（JSON） | Go 位置 | Rust 位置 |
|---|---|---|---|---|---|
| 31 | `ccOpenFax` | — | 已有浮窗 → `{"msg":"注水专线浮窗已在前台","ok":true}`（并置前，**不再 spawn**）；否则 spawn 本 exe `--fax` + 便携参数 → `{"msg":"注水专线浮窗已打开","ok":true}`；起不来 → `jsonErr(带程序路径的失败原因)` | `main.go:64-82` | `faxtool::FaxLauncher::open_fax` |
| 32 | `ccFaxWin` | `cmd string` | **`null`**（Go 闭包无返回 → webview 侧 undefined）；有句柄时把命令原样交给 `windowCmd`（`move`/`min`/`max`/`close`） | `main.go:131-135` | `faxtool::FaxLauncher::fax_win` |
| 33 | `ccFaxClose` | — | **`null`**；有句柄时 `windowCmd(hwnd,"close")` → `PostMessage WM_CLOSE` | `main.go:143-147` | `faxtool::FaxLauncher::fax_close` |

**浮窗外壳逐条对齐**（`faxwindow.rs`）：

| 行为 | 判据 / 证据 |
|---|---|
| 标题 | `注水专线 · WATER PROBE FX-01`（`ccOpenFax` 与单实例守卫都按它找窗）——与黄金 `fax_window_title` 同值 |
| 尺寸 | 580×784（第 14 轮裁决的机器原生外廓；**不用 CSS zoom**）——与黄金 `fax_window_size` 同值 |
| 无边框 | `setFrameless` + `framelessFullClient`（子类化吃掉 DWM 保留边框）+ `setClientSize` 钉回外廓 + 窗口图标 |
| 单实例 | 进程名互斥体 `Local\ProxyDeck-FaxWindow`：已存在 → 找到浮窗置前、本进程自退（**绝不重复弹出**） |
| 页面 | 同一个 ui.html + `#fax` + 全 33 桥（与主窗桥面一致） |
| 关闭 | 红圆点 → `ccFaxClose` → `PostMessage WM_CLOSE` 走正常关闭流程（`w.Dispatch(w.Destroy)` 是 0xc0000005 崩溃源，未采用） |

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
| **F. 分发骨架真跑** | `tests/command_dispatch.rs` | 真起 `rs-gui.exe`：`-version` 文案与退出码、缺 crate 的 `--plugin-tuanjie` 如实报错且不占端口、`-headless` 失败自写 `headless-error.log`、失败子模式写 `<id>-plugin-error.log` |
| **G. 宿主外壳真值表 + 源码锚（P7c）** | `tests/shell_contract.rs`（9 项） | `ccOpenFax` 的「已有浮窗」分支与 Go 黄金**逐字节同形** + 不 spawn 证据；`ccFaxWin`/`ccFaxClose` 的 `app/main.go` **逐字源码锚** + 句柄真值表；单实例（互斥体名/首次放行/二次挡住）；`Migrate` 四条语义 + `UpgradePlaintext` 形态与 Go 黄金逐字段同值；`legacyStateFiles` 清单 33 条与 5 条凭据集合 |
| **H. 宿主外壳行为测试（P7c）** | `src/shell_tests.rs`（lib 段）+ 各模块单测 | 换血**端到端**（真占端口的旧后端 → 真被杀 → 真 spawn 新后端 → 真等 health 绿）；真 netstat 快照下关着的端口保持关闭；`waitPortFree` 真端口真值表；`-headless` 的僵死端口清理与失败日志；托盘线程真拉起 + 真挂图标（**子进程隔离**）；`setFrameless` 真清掉 `WS_CAPTION`（子进程隔离）；`Migrate`/`UpgradePlaintext` 临时目录行为 |

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
# 单元测试 + 行为测试（lib 113 含插件托管 13 条 + 外壳行为 5 条；集成 5+4+9+2）
pwsh -NoProfile -Command "& 'F:\AIXM\proxydeck\.work\p7a\_p7atest.ps1' -Targets lib,bridge_contract,ui_html,command_dispatch,shell_contract"
# 或直接跑全量
cargo test --manifest-path rs-gui/Cargo.toml --all-targets       # CARGO_TARGET_DIR=E:\cargo-target\rs-gui
# 突变检验（证明门禁有判别力；18 条全部必须被抓住）
pwsh -NoProfile -File .work/p7c/_p7c-mutations.ps1
```

`_p7atest.ps1` 沿用 P6/P5 的火绒规避（链接后立刻把测试 exe 拷到工作区再跑），
并**加固为「编译失败即抛错」**：绝不回落到旧二进制假装通过（已实测：
参数写错时它确实抛错而不是跑旧包）。构建目录固定 `CARGO_TARGET_DIR=E:\cargo-target\rs-gui`。
⚠ 多目标要用 `-Command "& '…' -Targets a,b,c"` 写法：`-File` 会把 `a,b,c` 当成单个目标名。

**P7c 门禁的形状**：GUI 没有 HTTP 合同，故 G/H 两道各自锁一半边——
G 管「与 Go 同形」（差分 / 源码锚 / 真值表），H 管「真起进程真占端口真写文件的行为」。
`ccFaxWin`/`ccFaxClose` 的 Go 侧实现只在 `runFaxWindow()` 里创建（调用它 = 在用户桌面弹窗），
故它们**没有**「Go 侧实调」的差分，只有源码锚 + 真值表——这条边界写在
`shell_contract.rs` 的头注释里，不冒充覆盖。

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
| 12 | **浮窗启动器 `FaxLauncher` 与 win32 动作都带测试缝**（`SpawnTarget` / `window_cmd_action` / `fax_popup_handle` 可注入） | `ccOpenFax` / `windowCmd` / `findFaxPopup` 的副作用分别是"起窗""拖拽/关窗""按标题找窗"，在门禁里跑任何一种都会**动到用户桌面**。缝只用于门禁，生产路径一个分支都没多（`set_fax_popup_override` 生产代码从不调用） |
| 13 | **托盘状态是进程级单例；托盘端到端用例跑在子进程里** | Go 侧同样是一组包级 var（`trayHWND`/`trayStarted`/…），Rust 侧对应一个 `Mutex<...>`。`cargo test` 默认并行，在一个进程里拉托盘会污染同二进制的其它用例，故 `tray_thread_starts_and_mounts_icon_in_child_process` / `set_frameless_clears_the_caption_bit` 自己 re-exec 同一个测试二进制、只跑自己那一条——**子进程退出即回收托盘线程，不留孤儿图标在用户通知区** |
| 14 | **`window_cmd_action` 是纯函数**（命令 → 动作名） | Go 是一个 `switch`；抽出来的唯一目的是让「命令字认不认」可被门禁驱动（真窗口上 `move` 会进系统拖拽模态循环）。行为零变化：`window_cmd` 仍按同一串命令分派，未知命令同样什么都不做 |
| 15 | **`Migrate` 用 `std::path::absolute` 做同目录判定**（Go 用 `filepath.Abs` + `EqualFold`） | 都是"词法绝对化 + 大小写不敏感比较"，不是 `EvalSymlinks`（Go 也不是）。差异 7 已登记本项目不用符号链接 |
| 16 | **`--fax` 的父侧 spawn 不走 `cmd.Start()` 的错误文案逐字** | Go 是 `jsonErr(err)`，`err.Error()` 形如 `fork/exec <exe>: …`；Rust 补一句 `启动 <exe> 失败: <os error>`（同样带程序路径，可读性更好）。**这是能看到的一处文案差异**，登记在案 |
| 17 | **主 GUI 不做单实例**（与 Go 一致，但值得写明） | Go 侧只有注水浮窗用命名互斥体做单实例，主 GUI 多开是现行行为。Rust 照抄同一范围——**没有**擅自给主 GUI 加锁（那是新行为，需用户裁决） |
| 18 | **`Migrate` 的目标存在判定对齐 Go `migrate.go` 版**（与 `bridge.go` 版不同） | Go 同名函数有两套语义：`bridge.go` 是纯 `err == nil`（目录也算），`migrate.go` 是 `err == nil && !fi.IsDir()`（目录不算）。Rust 此前两处混用同一份"目录也算"的实现——目标是目录时会错走"目标已存在"分支（改名旧文件），而 Go 走"拷贝→改名失败→落位失败"。已改 `migrate.rs` 的私有 `file_exists` 为 `is_file` 判定，并加单测 `migrate_target_is_dir_falls_through_to_placement_failure_like_go`（真 Go 探针取证：note 为"落位失败，保留在原处：＜OS 原文＞"、源文件留、目录不动、tmp 清掉；OS 原文平台相关，门禁只断前缀） |

## 5. **没做**与留给后续阶段的（如实登记）

### P7a 范围内没做到的

1. **`--plugin-tuanjie` 无 Rust 承载**：六个 crate 里没有 rs-tuanjie（团结后端仍是
   `internal/tuanjie` 的 Go 实现）。分发器对它**如实报错并退出 1**，不静默降级、不假装启动。
   要落地必须新增 `rs-tuanjie` crate（体量与 rs-codebuddy 同级）。
   **P7b 补强**：这种失败现在也会写 `<数据目录>/tuanjie-plugin-error.log`（Go `pluginFailLog`
   同源），GUI 的退出监视因此能显示「启动失败：…」而不是停在「启动中」。
2. **`ccCalib` 未被 ui.html 调用**：宿主提供、当前界面零引用（历史绑定）。它的对齐由
   差分门禁覆盖，`tests/ui_html.rs` 的界面调用面断言不涉及它。
3. **GUI 分支只编译未启动**：P7a~P7c 都**没有**运行过 `rs-gui` 的无参模式——启动会弹窗，
   干扰正在用 GUI 的用户。窗口外观/交互/托盘/浮窗的验收属 §8 的人工清单。
   （唯一真正起过窗的自动化路径：lib 测试里那两条**子进程私有隐藏窗**用例，
   窗不可见、进程即退，不碰用户桌面。）

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
5. **日志文案两处平台相关差异（只进日志，不影响 HTTP 合同）**：
   `roll_one` 换血等健康的超时字段 Go 用 `%v` 写 `1m30s`、Rust 写秒数；
   `Migrate` 建数据目录失败文案 Go 带 `err.Error()` 原文、Rust 固定文案。
   两处都只出现在日志/注释里，对外响应体不受影响，故登记不对齐。

### P7d + P8 仍需接手的清单（本轮结束时点）

**P7d（打包）**：
1. 单 exe 内嵌 `icon.ico` + 版本资源（Rust 侧 `.rc` + `winres`/`embed-resource`）——
   **P7c 的窗口图标与托盘图标要等这一步才真好使**（`LoadIconW`/`LoadImageW` 现在拿不到资源）。
2. 版本串同步清单从 Go 的 8 处扩展到含 `rs-gui/src/lib.rs` 的 `APP_VERSION`（`CORE_VERSION` 同）。
3. 多二进制（每插件一个 exe）与单 exe 的取舍，与 P9 打包合并评估。
4. `.work/p7c/p7c-go-reference.json` **已入库**（`tests/golden/p7c-go-reference.json`，2.3KB，
   与 `.work` 副本同哈希；门禁优先读入库副本、缺则回落 `.work`——P1 第 4 轮口径）。

**P8 / 后续阶段**：
1. **`rs-tuanjie` 仍是缺口**：`--plugin-tuanjie` 在 Rust 侧如实报错（不静默降级）。
2. **主 GUI 的单实例**要不要加：Go 侧没有，本轮照抄；要加是**新行为**，需用户裁决。
3. **`-headless` 的 `u16` 端口上限**：`--port 99999` 在 Go 侧（`string` + `net.JoinHostPort`）
   是"端口被其他程序占用/非法"的运行时错，在 Rust 侧被 clap 的 `u16` 解析当场拒掉——
   两者都写 `headless-error.log` 并退出 1，**文案不同**。要逐字一致得把端口参数改成字符串再自己校验。
4. **`ccWindowCmd` 在浮窗上的回退路径**：Go 浮窗优先走 `ccFaxWin`、回退 `ccWindowCmd`；
   Rust 两条都通（`windowCmd` 是同一段），但没有真窗口三灯验证，属人工项。
5. **托盘菜单的图标/勾选态、气泡提示**：Go 侧没用，本轮也没做（范围外）。

### P7c（宿主外壳）——**本轮已做**，逐条对照简报

| 简报要求 | 落地位置 | 门禁 |
|---|---|---|
| 无边框 `setFrameless` | `win32::set_frameless`（主窗 `webview.rs` + 浮窗 `faxwindow.rs`） | `tray::tests::set_frameless_clears_the_caption_bit`（真窗口，断言 `WS_CAPTION` 被清） |
| 窗口图标 | `win32::set_window_icon`（`WM_SETICON` BIG+SMALL）、`tray::tray_icon_handle`（`LoadImageW` 16×16 → `LoadIconW`） | ⚠ **exe 当前还没有图标资源**（属 P7d），`LoadIconW` 返回 0 并原样返回——行为与 Go 在资源缺失时一致，差别只在 Go 的 exe 有资源 |
| DPI 感知 | `win32::set_dpi_aware`（`SetProcessDpiAwarenessContext(-4)` → 回退 `SetProcessDPIAware`），`dispatch::run` 的**第一句**（建窗之前） | `win32::tests::set_dpi_aware_is_idempotent`；位置由 `dispatch.rs` 的调用序锁定 |
| 单实例 | `single::acquire` + `faxtool::fax_popup_mutex_guard`（`Local\ProxyDeck-FaxWindow`） | `shell_contract::single_instance_semantics_match_go_truth_table`（互斥体名/首次放行/二次挡住，与 Go 黄金同值） |
| 托盘整套 | `tray.rs` 全部（常驻线程、挂/摘图标、`TaskbarCreated`、单击切换、右键菜单、`trayStartup`/`trayHide`/`trayQuit`） | `tray::tests::tray_thread_starts_and_mounts_icon_in_child_process`（子进程里真挂图标）+ `host::tests::no_window_host_reports_like_go`（文案） |
| `showWindowIfHidden` 的 600ms 自愈 | `webview::run_gui`（`sleep(600ms)` → `win32::show_window_if_hidden`，与 Go `main.go:287-297` 同序） | ⚠ 只能**人工**验（见 §8）；`win32` 侧只锁了「句柄为 0 时不炸」 |
| `--fax` 浮窗 | `faxwindow::run_fax_window` + `dispatch::Mode::Fax`（单实例守卫 → 起窗） | `faxwindow::tests`（33 桥名、ui.html 的 `#fax` 分支存在）+ §8 人工清单 |
| 最后 3 个绑定 | `bridge::FAX_BINDINGS` + 3 个分支 + `ui_html.rs` 反向锁定 | `shell_contract` 的差分 + 源码锚 + 句柄真值表 |
| `rollStaleBackends` + 换血接入启动流程 | `rollover.rs` 全部；`dispatch::Mode::Gui` 起后台线程跑（**只在 GUI 分支**） | `shell_tests`（端到端换血、真 netstat 下关着的端口保持关闭）+ `rollover::tests`（与 Go 单测同款真值表 6 组） |
| `appdir.Migrate` + `UpgradePlaintext` | `migrate.rs`；`dispatch::run_data_dir_init` 在任何状态文件读写之前 | `shell_contract`（4 条语义 + 形态 + 清单 33/5）+ `migrate::tests`（9 条） |
| `-headless` 僵死端口先清再听 | `dispatch::Mode::Proxy`（`port_busy` → `kill_by_port` → 300ms） | `shell_tests::headless_cleanup_kills_a_stale_holder_on_the_test_port`（真起假后端占端口 → 真杀掉 → 端口空出） |
| `-headless` 失败自写 `headless-error.log` | `dispatch::Mode::Proxy`（参数解析失败与 `run` 失败**同一个收尾**） | `command_dispatch::headless_failure_writes_its_own_log`（真起 exe，断言文件内容与行头） |
| `-headless` **子进程 stdout 落盘**（P7a 已做，未动） | `state.rs::App::start`（父侧捕获子进程 stdout/stderr 写 `headless-error.log`，带行头、1MB×3 轮转） | `state.rs` 既有实现原样保留 |

### P7d（打包）

- 单 exe 需内嵌 `icon.ico` + `resource.syso`（Rust 侧对应 `.rc`/`winres`）；
  版本串同步清单要从 Go 的 8 处扩展到含 `rs-gui/src/lib.rs` 的 `APP_VERSION`；
  多二进制与单 exe 的取舍与 P9 打包合并评估。

## 6. P7c 验收（实测；日志见 `.work/p7c/`）

| 项 | 结果 |
|---|---|
| `cargo test`（lib，含 13 条插件托管 + **5 条外壳行为测试**） | **113 项全绿**（0 warning） |
| `bridge_contract` | **5 项全绿**（P7b 的差分 90 步**一步不变**；仅 `fake_backend_sha256` 因仪器改动重算） |
| `shell_contract` | **9 项全绿**（传真差分 + 源码锚 + 单实例 + Migrate/Upgrade 真值表 + 清单 33/5 + 版本常量） |
| `command_dispatch` | **4 项全绿**（真起 exe；新增 `-headless` 失败自写日志） |
| `ui_html` | **2 项全绿**（字节一致 + 哈希 + 界面调用面 ⊆ 26+4+3） |
| `rs-proxy` / `rs-vibex`（薄包装回归） | 各自全绿（86+3+11 / 37+3+3） |
| `go test ./app/... -count=1` / `go vet ./app/...` | Go 侧零改动，`ok` / 干净（临时采集器已删除） |
| 突变检验（`.work/p7c/_p7c-mutations.ps1`） | **18 条突变，0 条漏网**（详见 `.work/p7c/_p7c-mutations.log`） |
| 黄金 | P7c 新基准 `.work/p7c/p7c-go-reference.json`（真闭包驱动采出）；P7b 的 `bridge-go-reference.json` **只改了 `fake_backend_sha256` 一行** |

### 突变检验：证明门禁**有判别力**（不是摆设）

`pwsh -File .work/p7c/_p7c-mutations.ps1` 逐条注入「Go 与 Rust 会分道扬镳」的改动，
要求对应测试必须变红，跑完立刻还原。**判据是解析出的失败条数 > 0**（只认
`test result: … N failed` 那行），不去 match 字符串 `FAILED`——那会被诊断文案与脚手架错误骗到
（头一轮就是这么误判的：3 条其实没被抓住却被记成 CAUGHT）。

| 突变 | 结果 |
|---|---|
| `ccOpenFax` 的「已在前台」文案多一个空格 | **CAUGHT**（shell_contract 1 条） |
| 浮窗标题改一个空格 | **CAUGHT**（单实例真值表） |
| 单实例互斥体名改一个字符 | **CAUGHT** |
| 互斥体守卫反向（第二次也放行） | **CAUGHT** |
| 浮窗宽度 580 → 600 | **CAUGHT** |
| `Migrate` 的 notes 多一个句号 | **CAUGHT** |
| `legacyStateFiles` 清单多一条 | **CAUGHT**（清单 33/5 那条：**语义测试抓不到它**，清单多一条在临时目录里只是"文件不存在→跳过"） |
| `Migrate` 目标已存在时仍强行覆盖 | **CAUGHT** |
| `UpgradePlaintext` 连密文也再加密一次 | **CAUGHT** |
| `isStaleImage` 判定反了 | **CAUGHT**（Go 单测 8 条真值表） |
| `classifyHolders` 不排除 `currentExe` | **CAUGHT**（判据是「currentExe 自己名字就长得像旧包」那条） |
| `rollAllWith` 连没活人的端口也滚 | **CAUGHT** |
| `portBusy` 判据失效 | **CAUGHT**（`waitPortFree` 真端口真值表；**不是** `roll_one` 那条——见下） |
| `setFrameless` 不清 `WS_CAPTION` | **CAUGHT**（真窗口断言样式位） |
| `windowCmd` 的命令字映射被改 | **CAUGHT**（`window_cmd_action` 真值表） |
| `headless 启动失败：` 前缀改半角冒号 | **CAUGHT** |
| 无窗口宿主的 `trayHide` 文案改字 | **CAUGHT** |
| 已有浮窗时仍去 spawn | **CAUGHT**（标记文件证据：没被写出来） |

**两次「MISSED→查明原因→换成真判据」的如实记录**（不是静默改判据）：
1. `roll_one` 的「杀后复验端口」那条分支只在 **`stop` 返回 Ok 但端口仍被占**时才会走到
   ——真实场景 = 对方以管理员身份运行、`taskkill` 杀不掉，而**门禁里构造不出管理员进程**。
   原先写在这条分支上的突变因此永远测不到它（`stop` 会先报错返回）。已改为把该分支依赖的
   原语 `port_busy` + `wait_port_free` 在**真端口**上单独锁死
   （`wait_port_free_is_false_while_the_port_is_really_held`），并在 `roll_one` 那条用例的
   注释里写明「本次失败来自 stop 自己，别把它当成复验生效的证据」。
2. 「可见性自愈反向」在无窗口路径下不改变可观测行为（句柄 0/1 都拿不到真窗口），
   故换成 `setFrameless` 的真窗口样式位断言——后者才是能分道扬镳的改法。

## 7. 门禁覆盖不到的地方（如实列出）

- **窗口外观 / 无边框拖拽 / 托盘图标与右键菜单 / 关窗退托盘三模式 / 单实例二次启动聚焦 /
  `--fax` 浮窗独立存活与单实例**：无法自动化，见 §8 的人工验收清单。
- **`showWindowIfHidden` 的语义**：只在「新进程起窗但旧窗仍持前台」那一刻有意义，
  门禁里造不出那个前台竞争；只锁了「句柄为 0 时不炸」。
- **`ccOpenFax` 的「没有浮窗 → spawn」那一支**：会**真的弹一个窗口**在用户桌面上
  （生产 exe 的 `--fax` 就是起窗），本轮纪律是不动用户桌面，故**未做 Go 侧实调**；
  Rust 侧由 `faxtool` 的单测覆盖（假 spawn 目标证明「已有浮窗时不 spawn」「起不来时回 jsonErr」），
  argv 形态由 `shell_contract` 的源码锚锁定。
- **`ccFaxWin` / `ccFaxClose` 的「有句柄」路径**：有副作用（`move` 会进系统拖拽模态循环、
  `close` 会关窗），拿真窗口当夹具会动到用户桌面；判据是源码锚 + 句柄真值表（见 §3 的 G 道）。
- **`ccCopy` 的真实剪贴板**：门禁走 `NullClipboard`（不动用户剪贴板），真机行为待人工确认。
- **一键更新的真实替换链**：跑它会替换 exe 并重启，绝不在门禁里跑；只做了「无直链」「状态查询」
  这类无副作用分支的对照。**换血**那一段（替换之后的事）本轮有端到端行为测试。
- **`ccStart` 的 spawn 分支**：差分只覆盖「探活接管」分支（起了子进程就等于在测试里拉后端服务）；
  spawn 路径的代码与 Go 逐行对齐，本轮仍未在门禁里实跑。
- **`-headless` 的「端口被占 → 先清再听」完整链**：门禁覆盖到「清掉了僵死占位者、端口真空出」
  这一步（`shell_tests`）；**没有**真跑 `-headless` 起一个常驻服务（那会把测试进程变成服务）。
- **插件场景的固定端口 19788~19790**：不走随机端口（黄金要可复现）。若这两个端口
  在本机被别的进程占着，`PL*` 场景会**当场失败并报出是哪个端口**（不静默跳过）；
  同样地，跨进程（lib 段 19810~19820 vs 集成段 19788~19790）刻意错开，避免 `cargo test`
  并行跑两个测试二进制时互相抢端口。P7c 新增的端口带：`shell_tests` 用 **19920~19923**
  （换血/端口清理），另有两处用**系统分配的空闲端口**（`bind("127.0.0.1:0")` 立刻释放，
  只做一次真绑定，不与他人抢固定口）。
- **`kill_by_port` 的 netstat 失败尾巴**：Rust 写死「无法运行 netstat」，Go 用 `%v` 实际错误。
  两侧该返回值都被调用方丢弃、**不可观测**，故只登记不改。
- **真·管理员权限进程**：见 §5。

## 8. 人工验收清单（无法自动化，交用户手测）

跑法：先在**测试数据目录**里起一个 rs-gui（不碰现行状态），逐条对照。
⚠ 下面第 5~6 条会真的弹窗/挂托盘图标，请在自己方便的时候做。

```powershell
# 起测试实例（临时数据目录，不动 %APPDATA%\ProxyDeck；也不要和正式 GUI 同时开着看）
$env:PROXYDECK_DATA_DIR = 'F:\AIXM\proxydeck\.work\p7c\manual-data'
$env:CARGO_TARGET_DIR    = 'E:\cargo-target\rs-gui'
cargo run --manifest-path rs-gui\Cargo.toml            # = rs-gui 无参 → GUI
```

| # | 验收项 | 怎么看 | 期望 |
|---|---|---|---|
| 1 | **窗口外观** | 起窗后看顶栏 | 540×760；**没有**原生标题栏/边框（机身自绘的红黄绿灯条就是标题栏）；Win11 上窗口是**圆角**；界面渲染清晰（高 DPI 屏不糊） |
| 2 | **无边框拖拽** | 拖机身台肩（非按钮处） | 鼠标按住能拖动窗口（`ccWindowCmd('move')` → 系统移动模态循环；拖到屏幕顶会触发 Windows 的最大化）；黄灯最小化、绿灯最大化/还原、红灯关窗 |
| 3 | **窗口图标** | 看任务栏条目 + 窗口左上 | ⚠ **本轮拿不到图标**：Rust exe 还没有内嵌图标资源（P7d 才有 `.rc`），故显示的是系统默认图标。**这一条要等 P7d 才可能通过**，现在只确认不报错、不崩 |
| 4 | **托盘图标常驻** | 看通知区域（可能折叠在 `^` 里） | 进程一起来图标就在（**窗口可见时也在**）；提示文字 = `ProxyDeck · 多平台代理控制台`。⚠ 同上：图标资源属 P7d，现在大概率是默认空白图标 |
| 5 | **托盘单击切换** | 单击托盘图标 | 第一次：窗口藏进托盘；再点：带出并置前。**图标始终不消失** |
| 6 | **托盘右键菜单** | 右键托盘图标 | 菜单两项：`显示界面` / `退出界面`。点菜单外部菜单要消失（`TrackPopupMenu` 前已 `SetForegroundWindow`）；点「退出界面」= 图标消失 + 进程退出 |
| 7 | **关窗退托盘三模式** | 界面里把「关窗行为」切三档后各关一次窗 | `ask`（默认）：每次都弹问；`tray`：点红灯只藏窗、进程与后端继续活着，从托盘能带回来；`quit`：点红灯进程退出、托盘图标消失。落盘文件：`.work\p7c\manual-data\close_mode.txt`（内容 `tray`/`quit` 明文；`ask` = 文件不存在） |
| 8 | **单实例二次启动（浮窗）** | 界面上点「☌ 注水检测」两次 | 只出现**一个**注水专线浮窗；第二次点击把已有浮窗置前、回「注水专线浮窗已在前台」（不会多开） |
| 9 | **`--fax` 浮窗独立存活** | 浮窗起来后，把主窗关掉（`quit` 模式） | 浮窗**仍然活着**（它是独立进程）；浮窗自己的红灯可关掉它；浮窗里 `Esc` 也关窗 |
| 10 | **`--fax` 浮窗外观与桥** | 看浮窗 | 580×784、无边框（客户区铺满，**没有**多出来的 8px 灰边）、标题为 `注水专线 · WATER PROBE FX-01`（任务栏悬停可见）；页面是 `#fax` 独立模式（真渠道/真检测/真历史，不是空白页） |
| 11 | **单实例二次启动（主 GUI）** | 再双击一次 rs-gui.exe | ⚠ **与 Go 版一致：会再开一个窗口**——Go 侧对主 GUI **没有**单实例守卫，本轮照抄同一范围（不擅自加）。这一条是"确认与 Go 行为相同"，不是"应当只开一个" |
| 12 | **`showWindowIfHidden` 的 600ms 自愈** | 一键更新重启时观察（或人工造「旧窗仍持前台」的场景） | 新进程的窗不会停在"进程活着但窗口看不见"；600ms 内自愈带出。⚠ 门禁覆盖不到，只能这样看 |
