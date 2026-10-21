//! plugins.rs —— 插件托管（对应 Go `app/plugins.go` 全部 + `app/plugin_bindings.go` 的
//! `readLogTail`）。
//!
//! 托管逻辑逐条对齐 Go：
//!   * `pluginDefs` 注册表（顺序即 `ccPluginList` 的输出顺序）
//!   * `pluginList`：**并发**探活（本方法在 WebView UI 线程同步执行，串行探 8 个端口
//!     最坏 8×1.5s 会把整窗拖死）、陈旧报错自愈、字段与键序逐字对齐
//!   * `pluginStart`：探活接管（端口上已有健康实例 → 直接复用，不杀不重启）→ 端口被
//!     非健康进程占用则先清理 → spawn **本 exe** 的 `--plugin-<id>` → 退出监视
//!   * `watchPluginExit` + `pluginExitReason`：子进程一死就复位状态并带上失败原因
//!     （没有它 running 恒为 true，后端秒退后界面永远停在「启动中」）
//!   * `pluginStop`：先杀 → **杀后复验端口**，杀不掉如实报错并恢复跟踪
//!     （不先假装关掉、3 秒后探活又弹回 running）
//!
//! 与 Go 的一处结构性差异（登记在 README）：Go 的 `pluginState` 持有 `*exec.Cmd`，
//! 退出监视 goroutine 调 `cmd.Wait()`；Rust 的 `Child` 归监视线程所有、`pluginStop`
//! 按 **PID** 收（`taskkill /F /PID`，与 `Process.Kill()` 同为 TerminateProcess 语义），
//! 实例身份改用「token 世代号」判定（Go 判定的是 `st.cmd != cmd`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::appdir;
use crate::httpc;
use crate::jsons;
use crate::proc;

/// 停堆后等端口空出的宽限（Go `stopVerifyWait`：交互路径 2s，不冻窗）。
pub const STOP_VERIFY_WAIT: Duration = Duration::from_secs(2);

/// 一个插件的注册信息（字段与 Go `pluginDef` 一一对应）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginDef {
    /// 唯一标识
    pub id: String,
    /// 显示名
    pub name: String,
    /// `plugins` 下目录名（Python 插件用；原生插件为空）
    pub dir: String,
    /// 入口脚本（Python 插件用；原生插件为空）
    pub script: String,
    /// 附加命令行参数（GUI 启动时追加在 `--plugin-* --port N` 之后）
    pub args: Vec<String>,
    /// 监听端口
    pub port: u16,
    /// 健康检查路径
    pub health: String,
    /// 非空 = 原生内置插件（本 exe 的 `--plugin-<id>` 子模式）
    pub native: String,
}

impl PluginDef {
    /// 便捷构造（测试注册表与生产表共用）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        name: &str,
        dir: &str,
        script: &str,
        args: &[&str],
        port: u16,
        health: &str,
        native: &str,
    ) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            dir: dir.to_string(),
            script: script.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            port,
            health: health.to_string(),
            native: native.to_string(),
        }
    }
}

/// 插件注册表（顺序与 Go `pluginDefs` 逐条一致——`ccPluginList` 输出顺序依赖它）。
pub fn plugin_defs() -> Vec<PluginDef> {
    vec![
        PluginDef::new("tuanjie", "团结 Cowork (Codely)", "", "", &[], 8788, "/health", "tuanjie"),
        // --desensitize：对 system 里的技术敏感词（DoS/exploit/credential 等）插入零宽空格，
        // 避免腾讯内容审核误拦 agent 类请求（Go pluginDefs 注释同源）。
        PluginDef::new(
            "codebuddy",
            "WorkBuddy / CodeBuddy",
            "",
            "",
            &["--desensitize"],
            8787,
            "/health",
            "codebuddy",
        ),
        // WorkBuddy 国际版：8789，凭据按 workbuddy.ai 过滤，不刷新 token，首条 system 强制。
        PluginDef::new(
            "codebuddy-intl",
            "WorkBuddy Intl",
            "",
            "",
            &["--desensitize"],
            8789,
            "/health",
            "codebuddy-intl",
        ),
        PluginDef::new("bai", "B.AI", "", "", &[], 8891, "/health", "bai"),
        PluginDef::new("comate", "Comate", "", "", &[], 8786, "/health", "comate"),
        PluginDef::new("qoder", "Qoder", "", "", &[], 8785, "/health", "qoder"),
        // Qoder 国际版：8791，读 com.qoder.app.stable + 国际版 worker + .sh 额度接口。
        // 与 codebuddy-intl 同款：子模式 flag 由 dispatch 翻译成 `--plugin-qoder --region INTL`。
        PluginDef::new(
            "qoder-intl",
            "Qoder Intl",
            "",
            "",
            &[],
            8791,
            "/health",
            "qoder-intl",
        ),
        PluginDef::new("vibex", "VibeX", "", "", &[], 8790, "/health", "vibex"),
        // OpenCode（8784）：2026-10-21 接入 GUI——把本机 `opencode serve` 翻成
        // OpenAI 兼容接口（不打包 opencode；未安装时 /health 如实报 not_installed）。
        // 顺序放在 vibex 之后，不插队（`ccPluginList` 输出顺序依赖它）。
        PluginDef::new("opencode", "OpenCode", "", "", &[], 8784, "/health", "opencode"),
    ]
}

/// 按 id 取插件定义（生产注册表）。
pub fn find(id: &str) -> Option<PluginDef> {
    plugin_defs().into_iter().find(|d| d.id == id)
}

/// 插件健康检查地址（Go `pluginHealthURL`：「http://127.0.0.1:<port><health>」）。
pub fn plugin_health_url(d: &PluginDef) -> String {
    format!("http://127.0.0.1:{}{}", d.port, d.health)
}

/// 插件运行日志路径（Go `pluginLog`：数据目录下 `plugins/<id>.log`）。
pub fn plugin_log_path(d: &PluginDef) -> PathBuf {
    appdir::path(&format!("plugins/{}.log", d.id))
}

/// 原生插件启动失败时子模式写的诊断文件（Go `pluginErrLog`：数据目录下
/// `<id>-plugin-error.log`）。按 id 取是给分发器用的（它手上只有插件 id）。
pub fn err_log_path_for_id(id: &str) -> PathBuf {
    appdir::path(&format!("{id}-plugin-error.log"))
}

/// 同 [`err_log_path_for_id`]，按定义取（Go `pluginErrLog(d)`）。
pub fn plugin_err_log_path(d: &PluginDef) -> PathBuf {
    err_log_path_for_id(&d.id)
}

/// 插件脚本目录（Go `pluginsDir`：exe 同目录的 `plugins/`）。
pub fn plugins_dir() -> PathBuf {
    appdir::exe_dir().join("plugins")
}

/// 插件入口脚本绝对路径；目录缺失或原生插件时返回空（Go `pluginScript`：
/// 判据是 `os.Stat` 成功——**目录也算存在**，与 Go 逐字同义）。
pub fn plugin_script(d: &PluginDef) -> String {
    if !d.native.is_empty() {
        return String::new(); // 原生插件内置于本 exe
    }
    let mut p = plugins_dir();
    if !d.dir.is_empty() {
        p.push(&d.dir);
    }
    if !d.script.is_empty() {
        p.push(&d.script);
    }
    if std::fs::metadata(&p).is_err() {
        return String::new();
    }
    p.to_string_lossy().to_string()
}

/// `ccPluginList` 里 `dir` 字段的取法（Go `filepath.Join("plugins", d.Dir)`：
/// 空目录名时 Join 的结果就是 `plugins`；非空时按**平台分隔符**拼——Windows 上是反斜杠）。
pub fn plugin_dir_field(d: &PluginDef) -> String {
    if d.dir.is_empty() {
        "plugins".to_string()
    } else {
        Path::new("plugins")
            .join(&d.dir)
            .to_string_lossy()
            .to_string()
    }
}

/// 日志文件名（Go `filepath.Base(a.pluginLog(d))`）。
pub fn plugin_log_name(d: &PluginDef) -> String {
    format!("{}.log", d.id)
}

/* ---------------- 日志尾部读取（Go `readLogTail`） ---------------- */

/// 读日志尾部至多 `max` 字节（文件更小则全读）；截断读的起点常落在行中间，
/// **丢掉首个不完整行**（读到第一个 `\n` 为止）。文件打不开/读失败返回 `None`。
pub fn read_log_tail(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    let mut buf = Vec::new();
    if size > max {
        f.seek(SeekFrom::Start(size - max)).ok()?;
        f.read_to_end(&mut buf).ok()?;
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=i);
        }
        return Some(buf);
    }
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Go `string(b)` 的字节级语义：非法 UTF-8 字节**每个**产出一次 U+FFFD
/// （`utf8.DecodeRuneInString` 对非法序列返回 size=1）。Go 的 `encoding/json`
/// 编码这种串时写 `\ufffd` 转义，本函数产出的是**同值**（U+FFFD 字符）。
pub fn go_lossy_string(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        match std::str::from_utf8(&b[i..]) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    out.push_str(std::str::from_utf8(&b[i..i + valid]).unwrap_or(""));
                }
                out.push('\u{FFFD}');
                i += valid + 1; // Go 对非法序列每次只前进 1 字节
            }
        }
    }
    out
}

/// Go `msg[:300] + "…"`（按**字节**切；切点落在多字节字符中间时按 Go 的字节级语义补 U+FFFD）。
pub fn truncate_300_go(msg: &[u8]) -> String {
    if msg.len() <= 300 {
        return go_lossy_string(msg);
    }
    format!("{}…", go_lossy_string(&msg[..300]))
}

/// Go `utf8.DecodeRuneInString`：返回 (rune, size)；非法字节返回 (None, 1)。
fn decode_rune_go(b: &[u8]) -> (Option<char>, usize) {
    if b.is_empty() {
        return (None, 0);
    }
    match std::str::from_utf8(b) {
        Ok(s) => match s.chars().next() {
            Some(c) => (Some(c), c.len_utf8()),
            None => (None, 0),
        },
        Err(e) => {
            let valid = e.valid_up_to();
            if valid > 0 {
                let s = std::str::from_utf8(&b[..valid]).unwrap_or("");
                match s.chars().next() {
                    Some(c) => (Some(c), c.len_utf8()),
                    None => (None, 1),
                }
            } else {
                (None, 1)
            }
        }
    }
}

/// Go `utf8.DecodeLastRuneInString`：返回 (rune, size)；非法尾字节返回 (None, 1)。
fn decode_last_rune_go(b: &[u8]) -> (Option<char>, usize) {
    if b.is_empty() {
        return (None, 0);
    }
    let mut i = b.len() - 1;
    while i > 0 && (b[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    match std::str::from_utf8(&b[i..]) {
        Ok(s) if !s.is_empty() => match s.chars().next() {
            Some(c) => (Some(c), s.len()),
            None => (None, 1),
        },
        _ => (None, 1),
    }
}

/// Go `strings.TrimSpace(string(b))` 的**字节级**实现：两端按 rune 裁空白，
/// 非法字节不是空白（与 Go 的 RuneError/size=1 判定一致），故会终止裁剪。
pub fn trim_space_bytes(b: &[u8]) -> &[u8] {
    let mut start = 0usize;
    while start < b.len() {
        let (r, size) = decode_rune_go(&b[start..]);
        match r {
            Some(c) if c.is_whitespace() => start += size,
            _ => break,
        }
    }
    let mut end = b.len();
    while end > start {
        let (r, size) = decode_last_rune_go(&b[..end]);
        match r {
            Some(c) if c.is_whitespace() => end -= size,
            _ => break,
        }
    }
    &b[start..end]
}

/// 组装退出原因（Go `pluginExitReason`）：优先用子进程自己写的诊断文件，
/// 其次用退出错误；只回可读短句，避免把整份日志灌进界面。
pub fn plugin_exit_reason(err_log: &Path, wait_err: Option<&str>, d: &PluginDef) -> String {
    if let Some(b) = read_log_tail(err_log, 4 << 10) {
        let msg = trim_space_bytes(&b);
        if !msg.is_empty() {
            return format!("启动失败：{}", truncate_300_go(msg));
        }
    }
    if let Some(e) = wait_err {
        return format!("{} 启动后退出：{}", d.name, e);
    }
    format!("{} 启动后退出（无诊断信息）", d.name)
}

/// `wait()` 的退出状态 → Go `*exec.ExitError` 的文案（`exit status N`）；正常退出返回 `None`。
pub fn go_wait_error(status: &std::process::ExitStatus) -> Option<String> {
    match status.code() {
        Some(0) => None,
        Some(n) => Some(format!("exit status {n}")),
        // Windows 上恒有退出码；真拿不到时给一个可读短句，不假装是 0
        None => Some("exit status 1".to_string()),
    }
}

/* ---------------- Python 插件分支（Go `pyCmd` / `checkPluginDeps`） ---------------- */

/// 探测可用的 Python 命令（Go `pyCmd`：`python` → `py -3`）。
pub fn py_cmd() -> Result<(String, Vec<String>), String> {
    for (name, pre) in [("python", vec![]), ("py", vec!["-3".to_string()])] {
        if which(name).is_some() {
            return Ok((name.to_string(), pre));
        }
    }
    Err("未检测到 Python，请安装 Python 3.8+（https://www.python.org/downloads/）".to_string())
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for ext in ["", ".exe", ".cmd", ".bat"] {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 检查插件依赖（Go `checkPluginDeps`：fastapi / uvicorn / httpx），文案逐字对齐
/// （Go 用的是 `CombinedOutput`：stdout 与 stderr 合并在前）。
pub fn check_plugin_deps() -> Result<(), String> {
    let (exe, pre) = py_cmd()?;
    let mut args = pre;
    args.push("-c".to_string());
    args.push("import fastapi,uvicorn,httpx".to_string());
    let out = proc::hidden_cmd(&exe, &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let mut combined = out.stdout.clone();
        combined.extend_from_slice(&out.stderr);
        return Err(format!(
            "缺少依赖：{}\n请运行：pip install fastapi \"uvicorn[standard]\" httpx\n（详情：{}）",
            String::from_utf8_lossy(&combined).trim(),
            go_wait_error(&out.status).unwrap_or_default()
        ));
    }
    Ok(())
}

/* ---------------- 托管层 ---------------- */

/// 子进程启动目标：生产 = `os.Executable()`；测试可用「假后端」替换（见 README 的测试缝）。
#[derive(Clone, Debug, Default)]
pub struct SpawnTarget {
    /// 显式可执行文件（`None` = 本 exe，即 Go 的 `os.Executable()`）
    pub exe: Option<PathBuf>,
    /// 前置参数（测试缝用：把假后端脚本作为 argv[1..] 交给解释器）
    pub prefix: Vec<String>,
}

impl SpawnTarget {
    /// 解析出「程序 + 前置参数」；定位失败时给与 Go 同源的文案。
    pub fn resolve(&self) -> Result<(PathBuf, Vec<String>), String> {
        match &self.exe {
            Some(p) => Ok((p.clone(), self.prefix.clone())),
            None => std::env::current_exe()
                .map(|p| (p, Vec::new()))
                .map_err(|e| format!("定位本程序失败: {e}")),
        }
    }
}

#[derive(Default)]
struct PluginState {
    running: bool,
    /// 自 spawn 的子进程 pid（外部接管的实例为 None）
    pid: Option<u32>,
    /// 实例世代号：非 0 = 有一个活跃的退出监视（Go 用 `st.cmd == cmd` 判定同一实例）
    token: u64,
    started: Option<Instant>,
    last_err: String,
}

/// 插件托管器（Go `app` 上的 `plugins` 状态表 + 那几个方法）。
pub struct PluginHost {
    defs: Vec<PluginDef>,
    states: Mutex<HashMap<String, Arc<Mutex<PluginState>>>>,
    spawn: SpawnTarget,
}

static TOKEN_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_token() -> u64 {
    TOKEN_SEQ.fetch_add(1, Ordering::SeqCst)
}

impl PluginHost {
    /// 生产托管器：注册表 = [`plugin_defs`]，spawn 目标 = 本 exe。
    pub fn production() -> Self {
        Self::new(plugin_defs(), SpawnTarget::default())
    }

    /// 显式注册表 + spawn 目标（测试缝；生产走 [`PluginHost::production`]）。
    pub fn new(defs: Vec<PluginDef>, spawn: SpawnTarget) -> Self {
        Self {
            defs,
            states: Mutex::new(HashMap::new()),
            spawn,
        }
    }

    /// 注册表（顺序即 `ccPluginList` 输出顺序）。
    pub fn defs(&self) -> &[PluginDef] {
        &self.defs
    }

    /// 按 id 取定义。
    pub fn def(&self, id: &str) -> Option<PluginDef> {
        self.defs.iter().find(|d| d.id == id).cloned()
    }

    fn state_of(&self, id: &str) -> Arc<Mutex<PluginState>> {
        let mut m = self.states.lock().unwrap();
        Arc::clone(
            m.entry(id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(PluginState::default()))),
        )
    }

    /// 测试缝：读内部 `(pid, token)` 快照。只用于门禁锁定"失败恢复原样恢复"的语义
    /// （生产逻辑、JSON 输出一律不经过这里；`listing()` 不暴露 token）。
    #[cfg(test)]
    pub(crate) fn test_state_snapshot(&self, id: &str) -> (Option<u32>, u64) {
        let st = self.state_of(id);
        let g = st.lock().unwrap();
        (g.pid, g.token)
    }

    /* ---------------- pluginList ---------------- */

    /// 插件状态 JSON 列表（Go `pluginList`）：**并发**探活，结果按索引写预分配
    /// 槽位，输出结构与顺序与串行版完全一致。
    pub fn list(&self) -> Vec<Value> {
        let mut handles = Vec::with_capacity(self.defs.len());
        for (i, d) in self.defs.iter().enumerate() {
            let d = d.clone();
            let st = self.state_of(&d.id);
            handles.push(std::thread::spawn(move || (i, list_one(&d, &st))));
        }
        let mut slots: Vec<Option<Value>> = vec![None; self.defs.len()];
        for h in handles {
            if let Ok((i, v)) = h.join() {
                slots[i] = Some(v);
            }
        }
        slots.into_iter().map(|v| v.unwrap_or(Value::Null)).collect()
    }

    /* ---------------- pluginStart ---------------- */

    /// 启动一个插件（独立常驻子进程，关 GUI 不影响）——Go `pluginStart`。
    pub fn start(&self, id: &str) -> Result<(), String> {
        let Some(d) = self.def(id) else {
            return Err(format!("未知插件: {id}"));
        };
        let st = self.state_of(&d.id);
        {
            let g = st.lock().unwrap();
            if g.running {
                return Err(format!("{} 已在运行", d.name));
            }
        }

        // 端口已有健康实例（可能是 CLI/上一会话/别的工具拉起的）→ 直接接管复用，
        // 不杀不重启——正在用它跑任务的客户端（子智能体等）不受影响。
        if httpc::ok_fresh(&plugin_health_url(&d)) {
            let mut g = st.lock().unwrap();
            g.running = true; // GUI 语义：端口上有可用服务 = 运行中（外部实例，pid 为 None）
            g.pid = None;
            g.token = 0;
            g.started = Some(Instant::now());
            g.last_err.clear();
            return Ok(());
        }
        // 只有端口被"非健康"进程占用（僵尸/残留）才清理后重启。
        let port = d.port.to_string();
        if proc::port_busy(&port) {
            let _ = proc::kill_by_port(&port);
            std::thread::sleep(Duration::from_millis(300));
        }

        let prog: String;
        let argv: Vec<String>;
        let mut script_dir: Option<PathBuf> = None;
        if !d.native.is_empty() {
            // 原生插件：spawn 本 exe（内置于二进制，无目录/Python 依赖）；
            // 便携模式带上 --portable，让子进程与 GUI 解析到同一个数据目录。
            let (exe, prefix) = self.spawn.resolve()?;
            let mut args = prefix;
            args.push(format!("--plugin-{}", d.native));
            args.push("--port".to_string());
            args.push(port.clone());
            args.extend(appdir::portable_args());
            args.extend(d.args.iter().cloned());
            prog = exe.to_string_lossy().to_string();
            argv = args;
        } else {
            let script = plugin_script(&d);
            if script.is_empty() {
                return Err(format!(
                    "插件目录缺失：{}\n请确认 plugins/{} 已随程序放置",
                    plugins_dir().join(&d.dir).display(),
                    d.dir
                ));
            }
            check_plugin_deps()?;
            let (exe, pre) = py_cmd()?;
            let mut args = pre;
            args.push(script.clone());
            args.extend(d.args.iter().cloned());
            script_dir = Path::new(&script).parent().map(|p| p.to_path_buf());
            prog = exe;
            argv = args;
        }
        let arg_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        let mut cmd = proc::hidden_cmd(&prog, &arg_refs);
        if let Some(dir) = script_dir {
            cmd.current_dir(dir); // Go: cmd.Dir = filepath.Dir(script)
        }
        // 子进程 stdout/stderr 落数据目录日志：带时间戳行头、追加写、超 1 MB 轮转。
        let log_path = plugin_log_path(&d);
        let writer = appdir::StampedWriter::open(log_path.clone());
        let shared = Arc::new(Mutex::new(writer));
        match shared.lock().unwrap().is_open() {
            true => {
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
            }
            false => {
                // Go: OpenLog 返回 nil 时 cmd.Stdout/Stderr 保持 nil → 子进程输出丢弃
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
            }
        }
        // 清掉上一次的失败诊断：否则本轮启动成功、旧错误仍会被当成本次原因读出。
        let _ = std::fs::remove_file(plugin_err_log_path(&d));
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动 {} 失败: {}", d.name, e))?;

        // 两条管道各起一个读线程，统一经同一把锁写同一份时间戳日志（Go 是一条
        // StampedWriter 同时当 Stdout/Stderr；Rust 的 Stdio 不能共享 File 之外的对象）。
        for pipe in [
            child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let w = Arc::clone(&shared);
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut r = pipe;
                let mut chunk = [0u8; 8 * 1024];
                loop {
                    match r.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut g = w.lock().unwrap();
                            let _ = g.write_all(&chunk[..n]);
                            let _ = g.flush();
                        }
                    }
                }
            });
        }

        let pid = child.id();
        let token = next_token();
        {
            let mut g = st.lock().unwrap();
            g.running = true;
            g.pid = Some(pid);
            g.token = token;
            g.started = Some(Instant::now());
            g.last_err.clear();
        }
        self.watch_plugin_exit(Arc::clone(&st), child, token, d);
        Ok(())
    }

    /// 监视子进程退出，进程一死就把状态复位并带上失败原因（Go `watchPluginExit`）。
    /// 判定用 token：主动停堆（`pluginStop` 把 token 置 0）与换新实例都不会被误判成失败。
    fn watch_plugin_exit(
        &self,
        st: Arc<Mutex<PluginState>>,
        mut child: std::process::Child,
        token: u64,
        d: PluginDef,
    ) {
        let err_log = plugin_err_log_path(&d);
        std::thread::spawn(move || {
            let status = child.wait();
            let wait_err = match &status {
                Ok(s) => go_wait_error(s),
                Err(e) => Some(e.to_string()),
            };
            let mut g = st.lock().unwrap();
            if g.token != token {
                return;
            }
            g.running = false;
            g.pid = None;
            g.token = 0;
            g.last_err = plugin_exit_reason(&err_log, wait_err.as_deref(), &d);
        });
    }

    /* ---------------- pluginStop ---------------- */

    /// 停止一个插件（按端口结束进程）——Go `pluginStop`。
    pub fn stop(&self, id: &str) -> Result<(), String> {
        let Some(d) = self.def(id) else {
            return Err(format!("未知插件: {id}"));
        };
        let st = self.state_of(&d.id);
        let port = d.port.to_string();
        // 先杀：自 spawn 的直接 Kill，外部拉起的一律按端口杀（原逻辑）。
        // 注意 pid/token 先摘不先清 running：杀失败要恢复跟踪（恢复旧 token，
        // 进程将来退出时 watch 照常复位并记录退出原因——同 Go 恢复 st.cmd 的语义）。
        let (pid, token) = {
            let mut g = st.lock().unwrap();
            let pid = g.pid.take();
            let token = g.token;
            g.token = 0; // 摘掉退出监视的实例身份（Go: st.cmd = nil）
            (pid, token)
        };
        if let Some(pid) = pid {
            // Go: cmd.Process.Kill()（失败忽略）；Rust 的 Child 归监视线程所有，按 PID 收
            let _ = proc::hidden_cmd("taskkill", &["/F", "/PID", &pid.to_string()]).status();
        }
        let _ = proc::kill_by_port(&port);
        // 再验：端口还被占着 = 没杀掉（如对方以管理员身份运行），如实报错并恢复跟踪，
        // 界面显示失败而不是先假装关掉、3 秒后探活又弹回 running。
        if !proc::wait_port_free(&port, STOP_VERIFY_WAIT) {
            let mut g = st.lock().unwrap();
            g.pid = pid;
            g.token = token; // 恢复旧实例身份（Go: st.cmd = cmd），watch 将来照常复位
            g.running = true;
            g.last_err.clear();
            return Err(format!(
                "旧进程杀不掉（端口 {} 仍被占）：对方可能以管理员身份在运行，请右键以管理员身份运行本程序后重试，或从任务管理器结束该进程",
                d.port
            ));
        }
        let mut g = st.lock().unwrap();
        g.running = false;
        g.pid = None;
        g.last_err.clear();
        Ok(())
    }

    /* ---------------- ccPluginLog ---------------- */

    /// `ccPluginLog` 的返回串（Go `plugin_bindings.go` 的同名闭包）。
    pub fn log_lines(&self, id: &str, lines: i64) -> String {
        let lines = if lines <= 0 || lines > 500 { 200 } else { lines };
        let Some(d) = self.def(id) else {
            return r#"{"ok":false,"msg":"未知插件"}"#.to_string();
        };
        // 只读尾部：日志每 3 秒拉一次且只用尾 lines 行，整份读入时日志越大越卡。
        let Some(data) = read_log_tail(&plugin_log_path(&d), 64 << 10) else {
            return r#"{"ok":false,"msg":"暂无日志"}"#.to_string();
        };
        let text = go_lossy_string(&data);
        let trimmed = text.trim_end_matches('\n');
        let mut all: Vec<&str> = trimmed.split('\n').collect();
        if all.len() > lines as usize {
            all = all[all.len() - lines as usize..].to_vec();
        }
        jsons::marshal(&jsons::obj(&[
            ("ok", json!(true)),
            ("lines", json!(all)),
        ]))
    }
}

/// 单个插件的 `ccPluginList` 条目（Go `pluginList` 里那个 goroutine 的闭包体）。
fn list_one(d: &PluginDef, st: &Mutex<PluginState>) -> Value {
    let (running, mut last_err) = {
        let g = st.lock().unwrap();
        (g.running, g.last_err.clone())
    };
    let script = plugin_script(d);
    let mut present = !d.native.is_empty(); // 原生插件内置于本 exe，恒为存在
    if !present {
        present = !script.is_empty();
    }
    // healthy 无条件查端口：外部启动的实例（终端/上次 GUI）同样识别为运行中，
    // 否则视图会误判"未启动"，模型矩阵/日志全部不加载。
    let alive = present && httpc::ok(&plugin_health_url(d));
    // 陈旧报错自愈：端口已健康就把上一次的失败原因清掉（判据是可观测的当前状态）。
    if alive && !last_err.is_empty() {
        st.lock().unwrap().last_err.clear();
        last_err.clear();
    }
    jsons::obj(&[
        ("id", json!(d.id)),
        ("name", json!(d.name)),
        ("port", json!(d.port)),
        ("native", json!(!d.native.is_empty())),
        ("dir", json!(plugin_dir_field(d))),
        ("present", json!(present)),
        ("running", json!(running)),
        ("healthy", json!(alive)),
        ("lastErr", json!(last_err)),
        ("log", json!(plugin_log_name(d))),
        ("url", json!(format!("http://127.0.0.1:{}/v1", d.port))),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::env_lock;

    #[test]
    fn registry_matches_go_ports_and_order() {
        let defs = plugin_defs();
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "tuanjie",
                "codebuddy",
                "codebuddy-intl",
                "bai",
                "comate",
                "qoder",
                "qoder-intl",
                "vibex",
                "opencode"
            ],
            "顺序与 Go pluginDefs 一致（ccPluginList 输出顺序依赖它）；qoder-intl 为 2026-10-20 新增，opencode 为 2026-10-21 新增"
        );
        let ports: Vec<(String, u16)> = defs.iter().map(|d| (d.id.clone(), d.port)).collect();
        assert_eq!(
            ports,
            vec![
                ("tuanjie".to_string(), 8788),
                ("codebuddy".to_string(), 8787),
                ("codebuddy-intl".to_string(), 8789),
                ("bai".to_string(), 8891),
                ("comate".to_string(), 8786),
                ("qoder".to_string(), 8785),
                ("qoder-intl".to_string(), 8791),
                ("vibex".to_string(), 8790),
                ("opencode".to_string(), 8784),
            ]
        );
        // 八个都是原生内置插件（Go pluginDefs 同款）
        assert!(defs.iter().all(|d| !d.native.is_empty()));
        let d = find("tuanjie").unwrap();
        assert_eq!(plugin_health_url(&d), "http://127.0.0.1:8788/health");
        assert_eq!(plugin_dir_field(&d), "plugins");
        assert_eq!(plugin_log_name(&d), "tuanjie.log");
        assert_eq!(plugin_defs()[1].args, vec!["--desensitize".to_string()]);
        assert!(plugin_defs()[0].args.is_empty());
    }

    #[test]
    fn dir_field_joins_like_go_filepath_join() {
        // Go: filepath.Join("plugins", "") == "plugins"
        let d = PluginDef::new("x", "X", "", "", &[], 1, "/health", "x");
        assert_eq!(plugin_dir_field(&d), "plugins");
        let d = PluginDef::new("x", "X", "sub", "main.py", &[], 1, "/health", "");
        // 非空目录名按平台分隔符拼（Windows 上 Go 的 filepath.Join 给反斜杠）
        assert_eq!(
            plugin_dir_field(&d),
            Path::new("plugins").join("sub").to_string_lossy()
        );
        #[cfg(windows)]
        assert_eq!(plugin_dir_field(&d), r"plugins\sub");
    }

    #[test]
    fn log_tail_small_file_is_read_whole() {
        let dir = std::env::temp_dir().join("rs-gui-plugins-tail-small");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.log");
        std::fs::write(&p, b"l1\nl2\n").unwrap();
        assert_eq!(read_log_tail(&p, 64 << 10).unwrap(), b"l1\nl2\n".to_vec());
        assert!(read_log_tail(&dir.join("nope.log"), 64).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_tail_big_file_drops_partial_first_line_like_go() {
        let dir = std::env::temp_dir().join("rs-gui-plugins-tail-big");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.log");
        // 101 字节一行（`NNN-` + 96 个 x + `\n`），共 500 行 = 50500 字节
        let mut body = String::new();
        for i in 0..500 {
            body.push_str(&format!("{i:03}-{}\n", "x".repeat(96)));
        }
        std::fs::write(&p, body.as_bytes()).unwrap();

        // 1000 字节的尾部起点落在行中间 → 丢掉首个不完整行
        let tail = String::from_utf8(read_log_tail(&p, 1000).unwrap()).unwrap();
        assert!(tail.len() <= 1000, "只读尾部 1000 字节");
        assert!(body.ends_with(&tail), "尾部必须是原文的后缀");
        assert!(body[..body.len() - tail.len()].ends_with('\n'), "起点必须落在行首（半行已丢）");
        assert!(tail.ends_with('\n'), "末行完整");
        assert!(tail.lines().all(|l| l.len() == 100 && l.ends_with("x")));

        // 起点正好落在行首：**Go 仍会切掉第一行**（只要 size > max 就按第一个 '\n' 截断）
        let exact = String::from_utf8(read_log_tail(&p, 101 * 5).unwrap()).unwrap();
        assert_eq!(exact.lines().count(), 4, "Go 的截断读恒丢首个（这里恰好完整的）行");
        assert!(exact.starts_with("496-"));
        // 文件不大于 max 时全读、一行不丢
        let whole = String::from_utf8(read_log_tail(&p, 1 << 20).unwrap()).unwrap();
        assert_eq!(whole.lines().count(), 500);
        assert_eq!(whole, body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn go_lossy_string_counts_one_replacement_per_invalid_byte() {
        // 与 Go 的 utf8.DecodeRuneInString(size=1) 同口径：截断的多字节序列每个字节一个 U+FFFD
        assert_eq!(go_lossy_string("中".as_bytes()), "中"); // E4 B8 AD
        assert_eq!(go_lossy_string(&[0xE4, 0xB8]), "\u{FFFD}\u{FFFD}");
        assert_eq!(go_lossy_string(&[0xE4, 0xB8, 0xAD]), "中");
        assert_eq!(go_lossy_string(&[0xC3, 0x28]), "\u{FFFD}(");
        // 300 字节切在多字节字符中间：截断到 300 字节 + 省略号；断口那一个字节补成 U+FFFD
        let msg = format!("AB{}", "中".repeat(200));
        let cut = truncate_300_go(msg.as_bytes());
        assert_eq!(cut.len(), 2 + 99 * 3 + 3 + 3, "AB + 99 个中 + 1 个 U+FFFD + …");
        assert_eq!(cut.chars().filter(|c| *c == '\u{FFFD}').count(), 1);
        // 非法字节按字节计数（不是按 U+FFFD 的 3 字节）：400 个坏字节 → 300 个 U+FFFD + 省略号
        let bad = vec![0xFFu8; 400];
        let cut = truncate_300_go(&bad);
        assert_eq!(cut.chars().filter(|c| *c == '\u{FFFD}').count(), 300);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn trim_space_bytes_matches_go_on_ascii_and_unicode() {
        assert_eq!(trim_space_bytes(b"  \t\n x \r\n "), b"x");
        assert_eq!(trim_space_bytes(b"   "), b"");
        // U+3000（全角空格，3 字节）：Go unicode.IsSpace 为真
        let mut v = "　x　".as_bytes().to_vec();
        assert_eq!(trim_space_bytes(&v), b"x");
        // 非法字节不是空白：裁剪在它前面停住
        v = vec![0xFF, b'x'];
        assert_eq!(trim_space_bytes(&v), v.as_slice());
        v = vec![b'x', 0xFF];
        assert_eq!(trim_space_bytes(&v), v.as_slice());
    }

    #[test]
    fn exit_reason_prefers_diagnostic_file_then_wait_error() {
        let _g = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-plugins-reason");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let safe = PluginDef::new("zz", "Zz Alpha", "", "", &[], 1, "/health", "zz");

        // 无文件 + 有 waitErr
        assert_eq!(
            plugin_exit_reason(&dir.join("none.log"), Some("exit status 7"), &safe),
            "Zz Alpha 启动后退出：exit status 7"
        );
        // 无文件 + 无 waitErr
        assert_eq!(
            plugin_exit_reason(&dir.join("none.log"), None, &safe),
            "Zz Alpha 启动后退出（无诊断信息）"
        );
        // 空白文件等同无内容
        let blank = dir.join("blank.log");
        std::fs::write(&blank, b"  \n\t\n").unwrap();
        assert_eq!(
            plugin_exit_reason(&blank, Some("exit status 7"), &safe),
            "Zz Alpha 启动后退出：exit status 7"
        );
        // 有内容 → 前缀 + TrimSpace 后的正文
        let log = dir.join("diag.log");
        std::fs::write(&log, " 2026-10-16 18:00:00 启动失败：端口 1 被占用 \n").unwrap();
        assert_eq!(
            plugin_exit_reason(&log, Some("exit status 7"), &safe),
            "启动失败：2026-10-16 18:00:00 启动失败：端口 1 被占用"
        );
        // 超 300 字节 → 截断 + 省略号
        let big = dir.join("big.log");
        std::fs::write(&big, "A".repeat(350)).unwrap();
        assert_eq!(
            plugin_exit_reason(&big, None, &safe),
            format!("启动失败：{}…", "A".repeat(300))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn go_wait_error_text_matches_go_exit_error() {
        let dir = std::env::temp_dir().join("rs-gui-plugins-exitcode");
        let _ = std::fs::create_dir_all(&dir);
        let cmd = dir.join("exit3.cmd");
        std::fs::write(&cmd, "@echo off\r\nexit /b 3\r\n").unwrap();
        let st = std::process::Command::new("cmd")
            .args(["/c", &cmd.to_string_lossy()])
            .status()
            .unwrap();
        assert_eq!(go_wait_error(&st).as_deref(), Some("exit status 3"));
        let ok = std::process::Command::new("cmd")
            .args(["/c", "exit 0"])
            .status()
            .unwrap();
        assert_eq!(go_wait_error(&ok), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_lines_windows_and_unknown_id() {
        let _g = env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-plugins-loglines");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);

        let defs = vec![PluginDef::new("zz", "Zz", "", "", &[], 1, "/health", "zz")];
        let host = PluginHost::new(defs, SpawnTarget::default());

        assert_eq!(
            host.log_lines("nope", 200),
            r#"{"ok":false,"msg":"未知插件"}"#
        );
        assert_eq!(host.log_lines("zz", 200), r#"{"ok":false,"msg":"暂无日志"}"#);

        // 空文件：Go `strings.Split("", "\n")` = [""] → lines: [""]（键序是 map marshal 的排序）
        std::fs::create_dir_all(dir.join("plugins")).unwrap();
        std::fs::write(dir.join("plugins").join("zz.log"), b"").unwrap();
        assert_eq!(host.log_lines("zz", 200), r#"{"lines":[""],"ok":true}"#);

        let mut body = String::new();
        for i in 1..=10 {
            body.push_str(&format!("line{i}\n"));
        }
        body.push_str("\n\n"); // 尾部空行被 TrimRight 掉
        std::fs::write(dir.join("plugins").join("zz.log"), body.as_bytes()).unwrap();
        assert_eq!(
            host.log_lines("zz", 2),
            r#"{"lines":["line9","line10"],"ok":true}"#
        );
        // lines <= 0 / > 500 都落到 200（这里只有 10 行，等价于全给）
        assert_eq!(host.log_lines("zz", 0), host.log_lines("zz", 200));
        assert_eq!(host.log_lines("zz", 501), host.log_lines("zz", 200));
        assert_eq!(host.log_lines("zz", -5), host.log_lines("zz", 200));

        std::env::remove_var(appdir::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
