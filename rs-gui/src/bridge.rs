//! bridge.rs —— JS 桥接层：**26 个 `w.Bind`**（对应 Go `app/bridge.go`）。
//!
//! 每个绑定都是「前端调一个全局函数 → 回一个 JSON 字符串」：ui.html 用
//! `Promise.resolve(window['cc'+fn](...))` 拿到串后 `JSON.parse`，所以
//! **键名、键序、文案、缺省值都是合同**，逐条对齐 Go 的实现（行号见 README 对齐表）。
//!
//! 与 Go 的两处结构性差异（有意为之，登记在 README）：
//!   1. Go 的 `w.Bind` 由 webview_go 按位置把 JS 实参反序列化进 Go 形参；这里统一走
//!      `dispatch(name, args)`，由本文件的取值器按位置取——**位置语义与实参形态不变**。
//!   2. 窗口/托盘/剪贴板三个外部副作用做成缝（`Host` / `Clipboard`），
//!      好让门禁在没有窗口、不碰真人剪贴板的前提下把 26 条全部跑一遍。

use std::sync::Arc;

use serde_json::{json, Value};

use crate::appdir;
use crate::faxtool::FaxLauncher;
use crate::host::Host;
use crate::httpc;
use crate::jsons;
use crate::keyprobe;
use crate::plugins::{self, PluginHost};
use crate::state::App;
use crate::update;

/// 本阶段实现的 26 个绑定名（**顺序 = bridge.go 的绑定顺序**）。
/// 门禁 `tests/bridge_contract.rs` 断言这 26 个名字在 Rust 侧全部存在且与 Go 黄金一致。
pub const BINDINGS: [&str; 26] = [
    "ccGetState",
    "ccLiveSnapshot",
    "ccStart",
    "ccStop",
    "ccStats",
    "ccUsage",
    "ccUsageRefresh",
    "ccSaveKey",
    "ccProbeKey",
    "ccCheckUpdate",
    "ccCheckUpdateForce",
    "ccOneKeyUpdate",
    "ccCalib",
    "ccModels",
    "ccAutostart",
    "ccOpen",
    "ccCopy",
    "ccDismiss",
    "ccDismissCloseHint",
    "ccWindowCmd",
    "ccRequestClose",
    "ccCloseChoice",
    "ccCloseModeGet",
    "ccCloseModeSet",
    "ccKeyMapGet",
    "ccKeyMapSet",
];

/// 插件托管的 4 个绑定（Go `app/plugin_bindings.go`；P7b 落地）。
/// ui.html 的实际调用面 = 这 4 个 + `BINDINGS`，由 `tests/ui_html.rs` 反向锁定。
pub const PLUGIN_BINDINGS: [&str; 4] = [
    "ccPluginList",
    "ccPluginLog",
    "ccPluginStart",
    "ccPluginStop",
];

/// 注水专线浮窗的 3 个绑定（Go `app/main.go` 的 `bindFaxLauncher` + `runFaxWindow`；
/// P7c 落地）。**顺序即 Go 侧的定义次序**：`ccOpenFax`（主窗与浮窗都绑）在
/// `bindFaxLauncher` 里，`ccFaxWin`/`ccFaxClose` 在 `runFaxWindow` 里按序绑。
pub const FAX_BINDINGS: [&str; 3] = ["ccOpenFax", "ccFaxWin", "ccFaxClose"];

/// 剪贴板缝（Go 用 `atotto/clipboard`；测试用空实现，避免动真人剪贴板）。
pub trait Clipboard: Send + Sync {
    fn write_all(&self, text: &str) -> Result<(), String>;
}

/// 系统剪贴板（CF_UNICODETEXT）。
pub struct SystemClipboard;

impl Clipboard for SystemClipboard {
    fn write_all(&self, text: &str) -> Result<(), String> {
        crate::proc::clipboard_write_all(text)
    }
}

/// 空剪贴板：不碰系统剪贴板，恒成功（门禁专用）。
pub struct NullClipboard;

impl Clipboard for NullClipboard {
    fn write_all(&self, _text: &str) -> Result<(), String> {
        Ok(())
    }
}

/// 桥接器：控制器 + 宿主动作（窗口/托盘）+ 剪贴板 + 插件托管 + 注水浮窗启动器。
pub struct Bridge {
    pub app: Arc<App>,
    pub host: Arc<dyn Host>,
    pub clipboard: Arc<dyn Clipboard>,
    /// 插件托管（探活接管 / 拉杆启停 / 退出监视；门禁可用假注册表替换）
    pub plugins: Arc<PluginHost>,
    /// 注水专线浮窗启动器（`ccOpenFax` 的落地处；门禁可注入假 spawn 目标）
    pub fax: Arc<FaxLauncher>,
}

impl Bridge {
    pub fn new(app: Arc<App>, host: Arc<dyn Host>) -> Self {
        // 默认与 `App` **共用同一个** PluginHost：Go 侧 `pluginList`/`pluginStart` 与
        // 换血流程（`rollStaleBackends`）读的是同一份 `app.plugins` 状态表——两套实例
        // 会让「界面已 running」与「换血认不认它是活人」对不上。
        let plugins = Arc::clone(app.plugins());
        Self {
            app,
            host,
            clipboard: Arc::new(SystemClipboard),
            plugins,
            fax: Arc::new(FaxLauncher::production()),
        }
    }

    /// 换掉剪贴板实现（门禁用 `NullClipboard`，不碰真人剪贴板）。
    pub fn with_clipboard(mut self, clipboard: Arc<dyn Clipboard>) -> Self {
        self.clipboard = clipboard;
        self
    }

    /// 换掉插件托管器（门禁用测试注册表 + 假后端，绝不碰现行服务端口）。
    pub fn with_plugins(mut self, plugins: Arc<PluginHost>) -> Self {
        self.plugins = plugins;
        self
    }

    /// 换掉注水浮窗启动器（门禁用假 spawn 目标，绝不真拉起浮窗）。
    pub fn with_fax(mut self, fax: Arc<FaxLauncher>) -> Self {
        self.fax = fax;
        self
    }

    /// 按名字派发一次调用（前端桥的全部入口）。
    /// `Ok(串)` = 回给前端的 JSON；`Err` = 未注册/实参形态不对（宿主据此如实报错，不静默吞）。
    pub fn dispatch(&self, name: &str, args: &[Value]) -> Result<String, String> {
        match name {
            "ccGetState" => Ok(jsons::marshal(&self.app.state())),

            // ccLiveSnapshot：并发实探核心与六插件端口健康，返回键角蓝灯用的 live 表
            "ccLiveSnapshot" => {
                let mut targets: Vec<(String, String)> =
                    vec![("core".to_string(), self.app.health_url())];
                for d in self.plugins.defs() {
                    targets.push((d.id.clone(), plugins::plugin_health_url(d)));
                }
                let mut handles = Vec::with_capacity(targets.len());
                for (name, url) in targets {
                    handles.push(std::thread::spawn(move || (name, httpc::ok(&url))));
                }
                // 键序按排序落（Go map marshal 口径；serde_json 的 Map 在依赖图里是保序的，
                // 不显式排序就会变成线程完成序）
                let mut sorted: std::collections::BTreeMap<String, bool> = Default::default();
                for h in handles {
                    if let Ok((name, ok)) = h.join() {
                        sorted.insert(name, ok);
                    }
                }
                let mut live = serde_json::Map::new();
                for (k, v) in sorted {
                    live.insert(k, Value::Bool(v));
                }
                Ok(jsons::marshal(&jsons::obj(&[("ok", json!(true)), ("live", Value::Object(live))])))
            }

            "ccStart" => match self.app.start(str_arg(args, 0)?.trim()) {
                Ok(msg) => Ok(jsons::json_ok(&msg)),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            "ccStop" => match self.app.stop() {
                Ok(msg) => Ok(jsons::json_ok(&msg)),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            // 透传代理的统计数据（原始 JSON，前端直接解析 total/today/models）
            "ccStats" => {
                if let Some(s) = httpc::get(&format!("{}/v1/stats", self.app.base_url())) {
                    return Ok(s);
                }
                // 代理未运行时兜底读本地 stats.json——历史消耗离线可见
                if let Ok(b) = appdir::read_file(&App::stats_file()) {
                    if !b.is_empty() {
                        return Ok(String::from_utf8_lossy(&b).to_string());
                    }
                }
                Ok(r#"{"ok":false,"msg":"代理未运行或无法连接"}"#.to_string())
            }

            // 官网权威统计：只读缓存（瞬时返回，不阻塞派发）
            "ccUsage" => {
                let cached = self.app.usage.lock().unwrap().clone();
                if !cached.is_empty() {
                    return Ok(cached);
                }
                Ok(r#"{"ok":false,"msg":"统计加载中"}"#.to_string())
            }

            "ccUsageRefresh" => {
                self.app.kick_usage();
                Ok(r#"{"ok":true}"#.to_string())
            }

            // 立即保存 API Key 到 api-key.txt（无需等点火）
            "ccSaveKey" => {
                let key = str_arg(args, 0)?.trim().to_string();
                // key 变了探活缓存立即失效
                self.app.key_probe.lock().unwrap().invalidate();
                if key.is_empty() {
                    if let Err(e) = appdir::remove(&App::key_file()) {
                        return Ok(jsons::json_err(&e));
                    }
                    self.app.inner.lock().unwrap().api_key.clear();
                    return Ok(jsons::json_ok("已清除保存的 Key"));
                }
                if let Err(e) = appdir::write_file(&App::key_file(), key.as_bytes()) {
                    return Ok(jsons::json_err(&e));
                }
                self.app.inner.lock().unwrap().api_key = key;
                Ok(jsons::json_ok("API Key 已保存"))
            }

            // 存储 key 探活：缓存命中直接回，否则后台线程真探 + 立即回 pending
            "ccProbeKey" => {
                let (out, start) = keyprobe::probe_step(&self.app.key_probe);
                if start {
                    let app = Arc::clone(&self.app);
                    std::thread::spawn(move || {
                        let mut key = app.inner.lock().unwrap().api_key.trim().to_string();
                        if key.is_empty() {
                            if let Ok(b) = appdir::read_file(&App::key_file()) {
                                key = String::from_utf8_lossy(&b).trim().to_string();
                            }
                        }
                        let payload = if key.is_empty() {
                            keyprobe::probe_result_json("nokey", 0, None)
                        } else {
                            let (status, code, err) =
                                keyprobe::probe_key(keyprobe::DEFAULT_BASE_URL, &key);
                            keyprobe::probe_result_json(&status, code, err.as_deref())
                        };
                        keyprobe::store_result(&app.key_probe, payload);
                    });
                }
                Ok(out)
            }

            "ccCheckUpdate" => Ok(update::cc_check_update(&self.app)),
            "ccCheckUpdateForce" => Ok(update::check_update_force(&self.app)),
            "ccOneKeyUpdate" => Ok(update::one_key_update(
                &self.app,
                Arc::clone(&self.host),
                &str_arg(args, 0)?,
            )),

            // 校准：写代理核心的 /v1/calibration（3s 本地客户端，防挂死派发队列）
            "ccCalib" => {
                let model = str_arg(args, 0)?.trim().to_string();
                let v = str_arg(args, 1)?.trim().to_string();
                if model.is_empty() {
                    return Ok(jsons::json_err("缺少模型名"));
                }
                if !v.is_empty() && v.parse::<i64>().is_err() {
                    return Ok(jsons::json_err("请输入数字（官网该模型总 Token 数）"));
                }
                match httpc::post_form(
                    &format!("{}/v1/calibration", self.app.base_url()),
                    &[("model", &model), ("tokens", &v)],
                ) {
                    Ok((code, body)) => {
                        if code != 200 {
                            return Ok(jsons::json_err(&format!("保存失败: {body}")));
                        }
                        Ok(body)
                    }
                    Err(e) => Ok(jsons::json_err(&e)),
                }
            }

            "ccModels" => {
                let mut u = format!("{}/v1/models", self.app.base_url());
                if str_arg(args, 0)? == "go" {
                    u.push_str("?plan=go");
                }
                if let Some(s) = httpc::get(&u) {
                    return Ok(s);
                }
                Ok(r#"{"ok":false,"msg":"代理未运行或无法连接"}"#.to_string())
            }

            "ccAutostart" => match set_autostart(bool_arg(args, 0)?, &self.app.port) {
                Ok(msg) => Ok(jsons::json_ok(&msg)),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            "ccOpen" => match crate::proc::open_url(&str_arg(args, 0)?) {
                Ok(()) => Ok(jsons::json_ok("已打开")),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            "ccCopy" => match self.clipboard.write_all(&str_arg(args, 0)?) {
                Ok(()) => Ok(jsons::json_ok("已复制")),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            "ccDismiss" => match appdir::write_file(&App::notice_file(), b"1") {
                Ok(()) => Ok(jsons::json_ok("已记录")),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            "ccDismissCloseHint" => match appdir::write_file(&App::close_hint_file(), b"1") {
                Ok(()) => Ok(jsons::json_ok("已记录")),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            // 自绘窗口控制（无边框窗口的标题栏交互）：move / min / max / close
            "ccWindowCmd" => {
                let cmd = str_arg(args, 0)?.trim().to_string();
                let Some(h) = self.host.window_handle() else {
                    return Ok(jsons::json_err("窗口句柄不可用"));
                };
                if cmd == "close" {
                    self.host.tray_quit(h);
                    return Ok(jsons::json_ok(""));
                }
                self.host.window_cmd(h, &cmd);
                Ok(jsons::json_ok(""))
            }

            // 关窗退托盘三桥：模式裁决与执行都在宿主侧，前端只负责 ask 时弹窗
            "ccRequestClose" => {
                let mode = close_mode();
                if mode == "ask" {
                    return Ok(r#"{"ok":true,"mode":"ask"}"#.to_string());
                }
                let Some(h) = self.host.window_handle() else {
                    return Ok(jsons::json_err("窗口句柄不可用"));
                };
                Ok(self.apply_close_mode(&mode, h))
            }

            "ccCloseChoice" => {
                let mode = str_arg(args, 0)?.trim().to_string();
                if mode != "tray" && mode != "quit" {
                    return Ok(jsons::json_err(&format!("未知关窗模式: {mode}")));
                }
                if bool_arg(args, 1)? {
                    if let Err(e) = set_close_mode(&mode) {
                        return Ok(jsons::json_err(&e));
                    }
                }
                let Some(h) = self.host.window_handle() else {
                    return Ok(jsons::json_err("窗口句柄不可用"));
                };
                Ok(self.apply_close_mode(&mode, h))
            }

            "ccCloseModeGet" => Ok(jsons::marshal(&jsons::obj(&[
                ("ok", json!(true)),
                ("mode", json!(close_mode())),
            ]))),

            "ccCloseModeSet" => {
                let mode = str_arg(args, 0)?.trim().to_string();
                if mode != "ask" && mode != "tray" && mode != "quit" {
                    return Ok(jsons::json_err(&format!("未知关窗模式: {mode}")));
                }
                match set_close_mode(&mode) {
                    Ok(()) => Ok(jsons::json_ok("已保存")),
                    Err(e) => Ok(jsons::json_err(&e)),
                }
            }

            "ccKeyMapGet" => {
                let (m, saved) = key_map_get();
                if !saved {
                    return Ok(r#"{"ok":false,"msg":"no saved keymap"}"#.to_string());
                }
                Ok(jsons::marshal(&jsons::obj(&[
                    ("ok", json!(true)),
                    (
                        "map",
                        serde_json::to_value(&m).unwrap_or(Value::Null),
                    ),
                ])))
            }

            "ccKeyMapSet" => match key_map_set(str_arg(args, 0)?.trim()) {
                Ok(()) => Ok(jsons::json_ok("已保存")),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            /* ---------------- 插件托管 4 绑定（Go `plugin_bindings.go`） ---------------- */

            // 插件列表（含状态）：Go 是 `json.Marshal(a.pluginList())`
            "ccPluginList" => Ok(jsons::marshal(&Value::Array(self.plugins.list()))),

            // 读插件日志尾部（插件视图的实时日志面板用）
            "ccPluginLog" => {
                let id = str_arg(args, 0)?;
                let lines = int_arg(args, 1);
                Ok(self.plugins.log_lines(&id, lines))
            }

            // 启动插件（tuanjie / codebuddy / bai …）：go 侧 id 先 TrimSpace
            "ccPluginStart" => match self.plugins.start(str_arg(args, 0)?.trim()) {
                Ok(()) => Ok(jsons::json_ok("已启动")),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            // 停止插件
            "ccPluginStop" => match self.plugins.stop(str_arg(args, 0)?.trim()) {
                Ok(()) => Ok(jsons::json_ok("已停止")),
                Err(e) => Ok(jsons::json_err(&e)),
            },

            /* ---------------- 注水专线浮窗 3 绑定（Go `app/main.go`） ---------------- */

            // 拉起独立 --fax 子进程；已有浮窗则只置前（Go `bindFaxLauncher` 的闭包体）
            "ccOpenFax" => Ok(self.fax.open_fax()),

            // 浮窗三灯直绑桥：Go 侧只在浮窗里绑，主窗调用时照样能落到 windowCmd
            // （`ccWindowCmd` 走的是同一段），不会"未实现"
            "ccFaxWin" => Ok(self.fax.fax_win(self.host.window_handle(), &str_arg(args, 0)?)),

            // 红圆点关窗（PostMessage WM_CLOSE；原 `w.Dispatch(w.Destroy)` 是崩溃源）
            "ccFaxClose" => Ok(self.fax.fax_close(self.host.window_handle())),

            other => Err(format!("未知绑定: {other}")),
        }
    }

    /// 执行关窗动作（Go `applyCloseMode`）。
    fn apply_close_mode(&self, mode: &str, hwnd: usize) -> String {
        match mode {
            "tray" => {
                if hwnd == 0 {
                    return jsons::json_err("窗口句柄不可用");
                }
                match self.host.tray_hide(hwnd) {
                    Ok(()) => jsons::json_ok("已退到托盘"),
                    Err(e) => jsons::json_err(&e),
                }
            }
            "quit" => {
                self.host.tray_quit(hwnd);
                jsons::json_ok("正在退出")
            }
            _ => jsons::json_ok(""),
        }
    }
}

/* ---------------- 实参取值（位置语义与 Go 形参一致） ---------------- */

pub fn str_arg(args: &[Value], i: usize) -> Result<String, String> {
    match args.get(i) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Null) | None => Ok(String::new()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(Value::Bool(b)) => Ok(b.to_string()),
        Some(_) => Err(format!("第 {} 个实参不是字符串", i + 1)),
    }
}

pub fn bool_arg(args: &[Value], i: usize) -> Result<bool, String> {
    match args.get(i) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::Null) | None => Ok(false),
        Some(Value::String(s)) => Ok(s == "true"),
        Some(Value::Number(n)) => Ok(n.as_i64().unwrap_or(0) != 0),
        Some(_) => Err(format!("第 {} 个实参不是布尔", i + 1)),
    }
}

/// 取第 i 个实参当 Go `int`（`ccPluginLog` 的 `lines`；数字以外的形态落 0，
/// 与 Go 侧「webview_go 解不出 int」的实参形态差异登记在 README）。
pub fn int_arg(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/* ---------------- 关窗模式（Go bridge.go:60-88） ---------------- */

/// 读盘上记住的关窗模式：tray / quit；文件缺席或内容不识别 = ask。
pub fn close_mode() -> String {
    match std::fs::read(App::close_mode_file()) {
        Err(_) => "ask".to_string(),
        Ok(b) => match String::from_utf8_lossy(&b).trim() {
            "tray" => "tray".to_string(),
            "quit" => "quit".to_string(),
            _ => "ask".to_string(),
        },
    }
}

/// 记住关窗模式：ask = 删掉记忆文件（回到每次询问），tray/quit = 写盘（明文，同 Go）。
pub fn set_close_mode(mode: &str) -> Result<(), String> {
    let path = App::close_mode_file();
    if mode == "ask" {
        return appdir::remove(&path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, mode.as_bytes()).map_err(|e| e.to_string())
}

/* ---------------- 键位映射（Go bridge.go:90-152） ---------------- */

/// 默认键位映射（与 ui.html 的 `KEYMAP_DEF` 同源：B.AI 默认不在任何键下；
/// 2026-10-21 起 OPENCODE 作为第 4 键的副页，与 VIBEX 同键轮切）。
pub fn key_map_default() -> Vec<Vec<String>> {
    vec![
        vec!["cmd".to_string()],
        vec!["tj".to_string(), "comate".to_string()],
        vec!["cb".to_string(), "qoder".to_string()],
        vec!["ext".to_string(), "oc".to_string()],
    ]
}

/// 脸名白名单（与 ui.html 的 `SUP_FACES` 同源；2026-10-21 起含 `oc`）。
pub const KEY_MAP_FACES: [&str; 8] = ["cmd", "tj", "comate", "cb", "qoder", "bai", "ext", "oc"];

/// 键位映射形态校验（1-4 键、每键 ≥1 脸、脸名白名单、一页一键）。
pub fn key_map_valid(m: &[Vec<String>]) -> bool {
    if m.is_empty() || m.len() > 4 {
        return false;
    }
    let mut seen: Vec<&str> = Vec::new();
    for k in m {
        if k.is_empty() {
            return false;
        }
        for f in k {
            if !KEY_MAP_FACES.contains(&f.as_str()) {
                return false;
            }
            if seen.contains(&f.as_str()) {
                return false;
            }
            seen.push(f.as_str());
        }
    }
    true
}

/// 一次性迁移标记：2026-10-21 之前落盘的 keymap.json 里还没有 OPENCODE 脸。
/// 标记存在 = 迁移已经做过，之后用户再手动删掉 oc **不再回填**（尊重用户的显式选择）。
fn key_map_oc_migration_marker() -> std::path::PathBuf {
    appdir::path("keymap-oc-migrated.v1")
}

/// 旧映射（合法、缺 `oc` 且有 `ext`）→ 把 `oc` 补成 ext 所在键的副页。
/// 找不到 ext 的映射原样返回（不替用户造键）；已有 oc 的也原样返回。
fn key_map_with_opencode(mut m: Vec<Vec<String>>) -> Vec<Vec<String>> {
    if m.iter().any(|k| k.iter().any(|f| f == "oc")) {
        return m;
    }
    if let Some(ki) = m.iter().position(|k| k.iter().any(|f| f == "ext")) {
        m[ki].push("oc".to_string());
    }
    m
}

/// 读盘：文件缺席/非法 → 返回默认映射 + saved=false。
///
/// 2026-10-21：OPENCODE 接入时对**旧的合法映射**做一次性迁移（补 `oc` 到第 4 键），
/// 否则老用户的 keymap.json 里没有这个脸，甲板永远进不去；迁移标记保证只做一次。
pub fn key_map_get() -> (Vec<Vec<String>>, bool) {
    let Ok(b) = std::fs::read(App::key_map_file()) else {
        return (key_map_default(), false);
    };
    match serde_json::from_slice::<Vec<Vec<String>>>(&b) {
        Ok(m) if key_map_valid(&m) => {
            let has_oc = m.iter().any(|k| k.iter().any(|f| f == "oc"));
            let marker = key_map_oc_migration_marker();
            if !has_oc && !appdir::file_exists(&marker) {
                let migrated = key_map_with_opencode(m);
                if let Ok(raw) = serde_json::to_vec(&migrated) {
                    let _ = std::fs::write(App::key_map_file(), raw);
                }
                let _ = std::fs::write(&marker, b"1");
                (migrated, true)
            } else {
                (m, true)
            }
        }
        _ => (key_map_default(), false),
    }
}

/// 校验后落盘（明文，同 Go `keyMapSet` 的 `os.WriteFile`——**不是** appdir.WriteFile）。
pub fn key_map_set(raw: &str) -> Result<(), String> {
    let m: Vec<Vec<String>> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if !key_map_valid(&m) {
        return Err("非法键位映射（1-4 键、每键 ≥1 页、页名须在白名单且一页一键）".into());
    }
    let path = App::key_map_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, raw.as_bytes()).map_err(|e| e.to_string())
}

/* ---------------- 开机自启（Go bridge.go:408-441） ---------------- */

/// 启动文件夹里的自启脚本路径（Go `autostartVBS`）。
pub fn autostart_vbs() -> std::path::PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    std::path::Path::new(&appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
        .join("command-code-proxy-autostart.vbs")
}

/// 自启是否已装（文件存在即算）。
pub fn autostart_installed() -> bool {
    appdir::file_exists(&autostart_vbs())
}

/// 装/卸自启（Go `setAutostart`）。
pub fn set_autostart(on: bool, port: &str) -> Result<String, String> {
    let vbs = autostart_vbs();
    if !on {
        if appdir::file_exists(&vbs) {
            std::fs::remove_file(&vbs).map_err(|e| e.to_string())?;
        }
        return Ok("开机自启已取消".to_string());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // VBS 字符串内以两个双引号转义一个双引号
    let content = format!(
        "Set sh = CreateObject(\"WScript.Shell\")\r\nsh.Run \"\"\"{}\"\" -headless -port {port}\", 0, False\r\n",
        exe.display()
    );
    // 与 Go 一致：**不** MkdirAll（启动目录不存在时如实报错）
    std::fs::write(&vbs, content.as_bytes())
        .map_err(|e| format!("写入启动文件夹失败（被杀软拦截？）: {e}"))?;
    Ok("开机自启已设置 · 登录 Windows 后将以无窗口模式自动点火".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::env_lock;

    #[test]
    fn key_map_validation_matches_go_rules() {
        assert!(key_map_valid(&key_map_default()));
        assert!(!key_map_valid(&[]), "0 键非法");
        assert!(!key_map_valid(&vec![vec!["cmd".to_string()]; 5]), "5 键非法");
        assert!(!key_map_valid(&vec![vec![]]), "空键非法");
        assert!(
            !key_map_valid(&vec![vec!["nope".to_string()]]),
            "白名单外脸名非法"
        );
        let dup = vec![
            vec!["cmd".to_string()],
            vec!["cmd".to_string()],
        ];
        assert!(!key_map_valid(&dup), "一页一键：同一个脸出现两次非法");
    }

    #[test]
    fn close_mode_round_trip_and_plaintext_on_disk() {
        let _g = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-bridge-closemode");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);

        assert_eq!(close_mode(), "ask", "文件缺席即 ask");
        set_close_mode("tray").unwrap();
        assert_eq!(close_mode(), "tray");
        assert_eq!(
            std::fs::read_to_string(App::close_mode_file()).unwrap(),
            "tray",
            "明文落盘（同 Go os.WriteFile）"
        );
        std::fs::write(App::close_mode_file(), b"garbage").unwrap();
        assert_eq!(close_mode(), "ask", "内容不识别即 ask");
        set_close_mode("ask").unwrap();
        assert!(!appdir::file_exists(&App::close_mode_file()));

        std::env::remove_var(appdir::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_map_get_falls_back_to_default_on_bad_file() {
        let _g = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-bridge-keymap");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);

        let (m, saved) = key_map_get();
        assert_eq!(m, key_map_default(), "无文件 → 默认映射");
        assert!(!saved, "无文件 → saved=false");
        std::fs::write(App::key_map_file(), b"[ [\"cmd\"], [\"nope\"] ]").unwrap();
        let (m, saved) = key_map_get();
        assert_eq!(m, key_map_default(), "非法文件 → 默认映射");
        assert!(!saved);
        std::env::remove_var(appdir::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_map_get_migrates_old_map_with_opencode_once() {
        let _g = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-bridge-keymap-oc");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);

        // 旧默认（VIBEX 单脸）：迁移成 ext+oc 双脸，并落盘
        let old = r#"[["cmd"],["tj"],["cb","qoder"],["ext"]]"#;
        std::fs::write(App::key_map_file(), old).unwrap();
        let (m, saved) = key_map_get();
        assert!(saved);
        assert_eq!(
            m[3],
            vec!["ext".to_string(), "oc".to_string()],
            "旧映射应补上 OPENCODE 副页"
        );
        assert!(appdir::file_exists(&key_map_oc_migration_marker()));
        let on_disk: Vec<Vec<String>> =
            serde_json::from_slice(&std::fs::read(App::key_map_file()).unwrap()).unwrap();
        assert_eq!(on_disk, m, "迁移结果必须落盘（文件为准）");

        // 用户手动删掉 oc 后（模拟再次写回旧映射）：标记已存在，不再回填
        std::fs::write(App::key_map_file(), old).unwrap();
        let (m2, _) = key_map_get();
        assert_eq!(m2[3], vec!["ext".to_string()], "迁移只做一次，不回填用户删掉的脸");

        std::env::remove_var(appdir::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn applied_close_mode_without_window_reports_handle_error() {
        let bridge = Bridge::new(
            Arc::new(App::new("127.0.0.1", "57399", "")),
            Arc::new(crate::host::NoWindowHost),
        );
        assert_eq!(
            bridge
                .dispatch("ccWindowCmd", &[json!("min")])
                .unwrap(),
            r#"{"msg":"窗口句柄不可用","ok":false}"#
        );
    }
}
