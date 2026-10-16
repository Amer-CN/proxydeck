//! update.rs —— 更新检查与一键更新（对应 Go `bridge.go:462-560` 的缓存/踢取、
//! `bridge.go:813-960` 的 `ccCheckUpdate`/`ccCheckUpdateForce`/`ccOneKeyUpdate`、
//! `bridge.go:1113-1346` 的下载→校验→腾位替换→重启，以及 `rollover.go` 的换血标记）。
//!
//! 两个通道的语义照抄 Go：后台常驻循环 + cap=1 通道去重 + 成功缓存 30 分钟 /
//! 负缓存 10 分钟 / 自动踢取 30 秒限频（手动 Force 不限频，靠通道 cap=1 防风暴）。

use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::appdir;
use crate::host::Host;
use crate::httpc;
use crate::jsons;
use crate::proc;
use crate::state::App;

/// 成功缓存保鲜期（Go `30 * time.Minute`）。
pub const CACHE_TTL_OK: Duration = Duration::from_secs(30 * 60);
/// 负缓存保鲜期（Go `10 * time.Minute`）。
pub const CACHE_TTL_NEG: Duration = Duration::from_secs(10 * 60);
/// 自动踢取限频（Go `30*time.Second`）。
pub const KICK_INTERVAL: Duration = Duration::from_secs(30);

/// 一键更新下载镜像链（Go `updateMirrorCandidates`：按序尝试，全部失败回退直连）。
pub const UPDATE_MIRROR_CANDIDATES: &[&str] = &["https://gh-proxy.com", "https://ghfast.top"];

/// 更新检查缓存（Go 的 `updateCache` / `updateCacheAt` / `updateLastFetch`）。
#[derive(Default)]
pub struct UpdateState {
    pub cache: String,
    pub at: Option<Instant>,
    pub last_fetch: Option<Instant>,
}

/// 一键更新流程状态（Go 的 `updFlowActive` / `updPhase` / `updPct` / `updMsg`）。
pub struct UpdFlow {
    pub active: bool,
    pub phase: String,
    pub pct: i64,
    pub msg: String,
}

impl Default for UpdFlow {
    fn default() -> Self {
        // Go 的包级初值：phase="idle"、pct=-1（服务器没给 Content-Length 时前端显示不定进度）、msg=""
        Self {
            active: false,
            phase: "idle".to_string(),
            pct: -1,
            msg: String::new(),
        }
    }
}

/* ---------------- 腾位法残留与替换（Go bridge.go:174-216） ---------------- */

/// 删掉更新替换（腾位法）留在 exe 同目录的旧程序残骸，启动时调用一次。
/// 删除失败不重试、不弹窗，只写日志（旧进程仍持有 `.old` 时删不掉，下次启动再清）。
pub fn cleanup_old_exe() {
    let dir = appdir::exe_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !name.starts_with("proxydeck.exe.old") {
            continue;
        }
        match std::fs::remove_file(e.path()) {
            Ok(()) => eprintln!("[update] 已清理旧程序残骸 {}", e.file_name().to_string_lossy()),
            Err(err) => eprintln!(
                "[update] 旧程序残骸删除失败（不影响使用，下次启动再清）：{}: {err}",
                e.file_name().to_string_lossy()
            ),
        }
    }
}

/// 用腾位法把运行中的 exe 换成 new_file，返回腾位后的旧文件路径。
/// 旧名每次取唯一（带纳秒时间戳），外面套 3 次退避重试（吃掉杀软/同步盘的短暂占用）。
pub fn replace_running_exe(exe: &Path, _new_file: &Path) -> Result<PathBuf, String> {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let mut last_err = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(300 * attempt as u64));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let old = dir.join(format!("ProxyDeck.exe.old-{nanos}"));
        match std::fs::rename(exe, &old) {
            Ok(()) => return Ok(old),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(last_err)
}

/// 换血标记路径（Go `updateMarkerPath`：数据目录 `.update-rollover.json`）。
pub fn update_marker_path() -> PathBuf {
    appdir::path(".update-rollover.json")
}

/// 记录一次成功替换（to=新版本，at=纳秒时间戳），供新 GUI 换血用（Go `writeUpdateMarker`）。
pub fn write_update_marker(to: &str) -> Result<(), String> {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let body = jsons::marshal(&jsons::obj(&[("to", json!(to)), ("at", json!(at))]));
    std::fs::write(update_marker_path(), body).map_err(|e| e.to_string())
}

/// 读走换血标记（读到即删，保证只换一轮；解析失败也删）——Go `consumeUpdateMarker`。
///
/// 返回 `(to, ok)`：`ok = true` = 刚发生过更新，调用方**必须**对全部插件后端换血。
/// 与 Go 逐条同源的三处边界（P7b 对齐，见 `tests/bridge_contract.rs` 的 marker 用例）：
///   * 文件读不到 → `("", false)`，不删（本来就没有）
///   * 能读到但**不是 JSON 对象**（含非法 JSON）→ `("", false)`，但文件照样删掉
///     （Go 的 `json.Unmarshal` 解不进去；根为 `null` 时 Go 视为空 map → `("", true)`）
///   * 是 JSON 对象 → 恒 `ok = true`，`to` 非字符串/缺键时取空串
pub fn consume_update_marker() -> (String, bool) {
    let path = update_marker_path();
    let Ok(b) = std::fs::read(&path) else {
        return (String::new(), false);
    };
    let _ = std::fs::remove_file(&path);
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) else {
        return (String::new(), false);
    };
    match v {
        serde_json::Value::Null => (String::new(), true),
        serde_json::Value::Object(m) => (
            m.get("to")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            true,
        ),
        _ => (String::new(), false),
    }
}

/// 给 GitHub 下载直链加镜像前缀（Go `applyMirror`：非 GitHub 直链/空镜像原样返回）。
pub fn apply_mirror(dl_url: &str, mirror: &str) -> String {
    if mirror.is_empty() || !dl_url.starts_with("https://github.com/") {
        return dl_url.to_string();
    }
    format!("{}/{}", mirror.trim_end_matches('/'), dl_url)
}

/// 计算文件 sha256（小写十六进制）——Go `fileSHA256`。
pub fn file_sha256(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(crate::uihtml::sha256_hex(&bytes))
}

/* ---------------- 踢取与缓存 ---------------- */

/// 踢一次后台重拉（Go `tryKickUpdateRefresh`）：距上次有效踢取不足 30s 直接返回 false。
pub fn try_kick_update_refresh(state: &Mutex<UpdateState>, tx: &SyncSender<()>) -> bool {
    {
        let mut s = state.lock().unwrap();
        let too_soon = s
            .last_fetch
            .map(|t| t.elapsed() <= KICK_INTERVAL)
            .unwrap_or(false);
        if too_soon {
            return false;
        }
        s.last_fetch = Some(Instant::now());
    }
    let _ = tx.try_send(()); // 已有待处理拉取则合并，不叠加
    true
}

/// `ccCheckUpdate`（Go `bridge.go:817-850`）：只读缓存瞬时返回，过期即清并踢后台重拉。
pub fn cc_check_update(app: &App) -> String {
    let cached = {
        let mut s = app.update.lock().unwrap();
        let ttl = if s.cache.contains(r#""ok":false"#) {
            CACHE_TTL_NEG
        } else {
            CACHE_TTL_OK
        };
        let expired = s
            .at
            .map(|t| t.elapsed() >= ttl)
            .unwrap_or(s.cache.is_empty() && false);
        if !s.cache.is_empty() && expired {
            s.cache.clear();
        }
        s.cache.clone()
    };
    if !cached.is_empty() {
        return cached;
    }
    // 首次/过期：踢后台重拉（通道去重；断网时限频 30s）
    {
        let mut s = app.update.lock().unwrap();
        let can_kick = s
            .last_fetch
            .map(|t| t.elapsed() > KICK_INTERVAL)
            .unwrap_or(true);
        if can_kick {
            s.last_fetch = Some(Instant::now());
        }
        drop(s);
        if !can_kick {
            return r#"{"ok":false,"msg":"checking"}"#.to_string();
        }
    }
    app.kick_update_unconditional();
    r#"{"ok":false,"msg":"checking"}"#.to_string()
}

/// `ccCheckUpdateForce`（Go `checkUpdateForce`）：清缓存＋无条件踢一次＋恒回 checking。
pub fn check_update_force(app: &App) -> String {
    {
        let mut s = app.update.lock().unwrap();
        s.cache.clear();
        s.last_fetch = Some(Instant::now());
    }
    app.kick_update_unconditional();
    r#"{"ok":false,"msg":"checking"}"#.to_string()
}

/// 后台常驻拉取（Go `bridge.go:861-943`）：gh CLI 优先，匿名 HTTP 兜底。
pub fn refresh_releases(app: &Arc<App>) {
    const URL: &str = "https://api.github.com/repos/Amer-CN/proxydeck/releases?per_page=100";
    let fetch = || -> (String, u16) {
        if let Some(out) = proc::hidden_output(
            "gh",
            &[
                "api",
                "repos/Amer-CN/proxydeck/releases?per_page=100",
                "--jq",
                ".",
            ],
        ) {
            return (out, 200);
        }
        match httpc::get_slow_status(URL) {
            Ok((code, body)) => {
                if code != 200 {
                    eprintln!("[update] releases 状态异常: {code}");
                }
                (body, code)
            }
            Err(e) => {
                eprintln!("[update] releases 拉取失败: {e}");
                (String::new(), 0)
            }
        }
    };
    let (body, status) = fetch();
    if status == 403 {
        let mut s = app.update.lock().unwrap();
        s.cache = r#"{"ok":false,"msg":"github rate limited"}"#.to_string();
        s.at = Some(Instant::now());
        return;
    }
    if status != 200 {
        return;
    }
    let Ok(releases) = serde_json::from_str::<serde_json::Value>(&body) else {
        eprintln!("[update] releases 解析失败或为空");
        return;
    };
    let Some(list) = releases.as_array() else {
        eprintln!("[update] releases 解析失败或为空");
        return;
    };
    if list.is_empty() {
        eprintln!("[update] releases 解析失败或为空");
        return;
    }
    let latest = &list[0]; // GitHub 按时间倒序，第一个即最新
    let mut total_downloads: i64 = 0;
    for rl in list {
        if let Some(assets) = rl.get("assets").and_then(|a| a.as_array()) {
            for a in assets {
                total_downloads += a.get("download_count").and_then(|d| d.as_i64()).unwrap_or(0);
            }
        }
    }
    // 直链只认最新 release 里名字为 ProxyDeck.exe 的 asset
    let (mut dl_url, mut dl_size, mut dl_digest) = (String::new(), 0i64, String::new());
    if let Some(assets) = latest.get("assets").and_then(|a| a.as_array()) {
        for a in assets {
            if a.get("name").and_then(|n| n.as_str()) == Some("ProxyDeck.exe") {
                dl_url = a
                    .get("browser_download_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string();
                dl_size = a.get("size").and_then(|s| s.as_i64()).unwrap_or(0);
                dl_digest = a
                    .get("digest")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                break;
            }
        }
    }
    let payload = jsons::marshal(&jsons::obj(&[
        ("ok", json!(true)),
        (
            "latest",
            json!(latest
                .get("tag_name")
                .and_then(|t| t.as_str())
                .unwrap_or("")),
        ),
        (
            "url",
            json!(latest.get("html_url").and_then(|t| t.as_str()).unwrap_or("")),
        ),
        ("downloads", json!(total_downloads)),
        ("dl_url", json!(dl_url)),
        ("dl_size", json!(dl_size)),
        ("dl_digest", json!(dl_digest)),
    ]));
    let mut s = app.update.lock().unwrap();
    s.cache = payload;
    s.at = Some(Instant::now());
}

/* ---------------- 一键更新（Go bridge.go:944-1346） ---------------- */

/// `ccOneKeyUpdate`：`status` 查进度、`""`/`start` 触发、其它回未知 action。
pub fn one_key_update(app: &Arc<App>, host: Arc<dyn Host>, action: &str) -> String {
    if action == "status" {
        let f = app.upd_flow.lock().unwrap();
        return jsons::marshal(&jsons::obj(&[
            ("ok", json!(true)),
            ("phase", json!(f.phase)),
            ("pct", json!(f.pct)),
            ("msg", json!(f.msg)),
        ]));
    }
    if action.is_empty() || action == "start" {
        return start_one_key_update_flow(app, host);
    }
    jsons::marshal(&jsons::obj(&[
        ("ok", json!(false)),
        ("msg", json!(format!("未知 action: {action}"))),
    ]))
}

/// 触发一键更新：单流程名额抢占后立即返回（下载在后台线程）。
/// 下载地址只从后端更新缓存取（GitHub Releases 数据），不信任前端传参。
pub fn start_one_key_update_flow(app: &Arc<App>, host: Arc<dyn Host>) -> String {
    {
        let mut f = app.upd_flow.lock().unwrap();
        if f.active {
            return r#"{"ok":false,"msg":"更新流程已在进行中"}"#.to_string();
        }
        f.active = true;
    }
    let cached = app.update.lock().unwrap().cache.clone();
    let info: serde_json::Value = serde_json::from_str(&cached).unwrap_or(serde_json::Value::Null);
    let dl_url = info
        .get("dl_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if dl_url.is_empty() {
        app.upd_flow.lock().unwrap().active = false;
        return r#"{"ok":false,"msg":"当前版本信息没有 exe 直链（老 Release 无 ProxyDeck.exe asset），请到仓库下载页手动更新"}"#
            .to_string();
    }
    let latest = info
        .get("latest")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let dl_size = info.get("dl_size").and_then(|v| v.as_i64()).unwrap_or(0);
    let dl_digest = info
        .get("dl_digest")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    {
        let mut f = app.upd_flow.lock().unwrap();
        f.phase = "downloading".into();
        f.pct = -1;
        f.msg = "连接下载源…".into();
    }
    let app2 = Arc::clone(app);
    std::thread::spawn(move || {
        run_one_key_update_flow(&app2, host, &latest, &dl_url, dl_size, &dl_digest);
    });
    r#"{"ok":true,"msg":"started"}"#.to_string()
}

/// 一键更新主流程（后台线程内跑，与桥接派发完全隔离）。
pub fn run_one_key_update_flow(
    app: &Arc<App>,
    host: Arc<dyn Host>,
    latest: &str,
    dl_url: &str,
    dl_size: i64,
    dl_digest: &str,
) {
    struct Guard(Arc<App>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.upd_flow.lock().unwrap().active = false;
        }
    }
    let _guard = Guard(Arc::clone(app));

    let set_phase = |phase: &str, pct: i64, msg: &str| {
        let mut f = app.upd_flow.lock().unwrap();
        f.phase = phase.to_string();
        f.pct = pct;
        f.msg = msg.to_string();
    };
    let fail = |msg: String| {
        set_phase("failed", -1, &msg);
        eprintln!("[update] 失败：{msg}");
    };

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            fail(format!("无法定位自身可执行文件: {e}"));
            return;
        }
    };
    let dir = exe.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let new_file = dir.join("ProxyDeck.exe.new");

    // 下载候选链：镜像常量按序在前、直连兜底最后一位
    let dl_client = httpc::client_with_timeout(Duration::from_secs(30 * 60));
    let mirror_client = httpc::client_with_timeout(Duration::from_secs(120));
    let mut candidates: Vec<(&str, String, &reqwest::blocking::Client)> = Vec::new();
    for m in UPDATE_MIRROR_CANDIDATES {
        let u = apply_mirror(dl_url, m);
        if u != dl_url {
            candidates.push((m, u, &mirror_client));
        }
    }
    candidates.push(("direct", dl_url.to_string(), &dl_client));

    let download_one = |url: &str, client: &reqwest::blocking::Client| -> Result<i64, String> {
        use std::io::{Read, Write};
        let mut resp = client.get(url).send().map_err(|e| format!("下载失败: {e}"))?;
        if resp.status().as_u16() != 200 {
            return Err(format!("下载失败: HTTP {}", resp.status().as_u16()));
        }
        let mut out = std::fs::File::create(&new_file)
            .map_err(|e| format!("创建临时文件失败（{}）: {e}", new_file.display()))?;
        let total = resp.content_length().unwrap_or(0) as i64;
        let mut got: i64 = 0;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match resp.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.write_all(&buf[..n])
                        .map_err(|e| format!("写入临时文件失败（{}）: {e}", new_file.display()))?;
                    got += n as i64;
                    if total > 0 {
                        let mut pct = (got * 100 / total) as i64;
                        if pct > 100 {
                            pct = 100;
                        }
                        set_phase(
                            "downloading",
                            pct,
                            &format!("下载中 {pct}%（{:.1} MB）", got as f64 / 1048576.0),
                        );
                    }
                }
                Err(e) => return Err(format!("下载中断: {e}")),
            }
        }
        Ok(got)
    };

    let mut written: i64 = 0;
    let mut last_err = String::new();
    for (name, url, client) in &candidates {
        eprintln!("[update] 开始下载 {latest} ← {url}（来源 {name}）");
        set_phase("downloading", -1, "连接下载源…");
        match download_one(url, client) {
            Ok(n) => {
                written = n;
                last_err.clear();
            }
            Err(e) => {
                last_err = e;
            }
        }
        if last_err.is_empty() && dl_size > 0 && written != dl_size {
            last_err = format!("校验失败：下载 {written} 字节，应为 {dl_size} 字节");
        }
        if last_err.is_empty() && dl_digest.starts_with("sha256:") {
            match file_sha256(&new_file) {
                Ok(sum) => {
                    if format!("sha256:{sum}").eq_ignore_ascii_case(dl_digest) {
                        eprintln!("[update] sha256 校验通过：{dl_digest}");
                    } else {
                        last_err = format!("sha256 不符，疑似源被篡改：应为 {dl_digest}，实为 sha256:{sum}");
                    }
                }
                Err(e) => {
                    last_err = format!("sha256 校验失败（读取 {}）: {e}", new_file.display());
                }
            }
        }
        if last_err.is_empty() {
            break;
        }
        eprintln!("[update] 下载源 {name} 失败：{last_err}；尝试下一个候选");
        let _ = std::fs::remove_file(&new_file);
    }
    if !last_err.is_empty() {
        fail(last_err);
        return;
    }
    if !dl_digest.is_empty() && !dl_digest.starts_with("sha256:") {
        eprintln!("[update] 跳过 digest 校验（非 sha256 格式: {dl_digest}），沿用字节数校验");
    }

    eprintln!("[update] 下载完成 {:.1} MB，校验通过", written as f64 / 1048576.0);
    set_phase("downloaded", 100, "下载完成 · 校验通过");

    // 腾位法替换：1) 运行中的旧 exe 改成唯一名 → 2) .new 改名顶替原路径
    let old_file = match replace_running_exe(&exe, &new_file) {
        Ok(p) => p,
        Err(e) => {
            fail(format!(
                "替换失败（旧程序改名腾位不成）: {e}。常见原因：上一次更新留下的旧实例或后端进程仍在运行（含托盘里的图标），请把它们全部退出后重试"
            ));
            return;
        }
    };
    if let Err(e) = std::fs::rename(&new_file, &exe) {
        fail(format!(
            "替换失败（新包就位不成）: {e}（旧程序在 {}，新包在 {}，可手动放回）",
            old_file.display(),
            new_file.display()
        ));
        return;
    }
    // 换血标记写在替换成功、拉起新进程之前（拉起失败时标记照样在，手动起新 GUI 照样换血）
    if let Err(e) = write_update_marker(latest) {
        eprintln!("[update] 换血标记写入失败（更新本身已成功，各后端需手动拉杆重启）：{e}");
    }
    eprintln!("[update] 替换完成，拉起新进程并关闭旧窗口");

    let mut cmd = proc::hidden_cmd(&exe.to_string_lossy(), &[]);
    cmd.current_dir(&dir);
    match cmd.spawn() {
        Ok(child) => eprintln!("[update] 新进程已启动 pid={}，旧窗口即将关闭", child.id()),
        Err(e) => {
            fail(format!(
                "新程序启动失败（文件已替换，可手动运行 {}）: {e}",
                exe.display()
            ));
            return;
        }
    }
    set_phase("replaced", 100, "已替换，正在重启…");
    // 关窗路径：PostMessage WM_CLOSE → 消息循环干净返回 → 正常退出（拿不到句柄才直接退）
    crate::host::close_after_update(host.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_mirror_four_branches_match_go() {
        const DL: &str = "https://github.com/Amer-CN/proxydeck/releases/download/v3.10.3/ProxyDeck.exe";
        assert_eq!(apply_mirror(DL, ""), DL, "空镜像原样返回");
        let other = "https://example.com/ProxyDeck.exe";
        assert_eq!(apply_mirror(other, "https://ghfast.top/"), other, "非 GitHub 直链原样");
        assert_eq!(
            apply_mirror(DL, "https://ghfast.top"),
            format!("https://ghfast.top/{DL}")
        );
        assert_eq!(
            apply_mirror(DL, "https://ghfast.top/"),
            format!("https://ghfast.top/{DL}"),
            "尾斜杠归一"
        );
    }

    #[test]
    fn mirror_candidates_are_clean() {
        assert!(!UPDATE_MIRROR_CANDIDATES.is_empty());
        for m in UPDATE_MIRROR_CANDIDATES {
            assert!(m.starts_with("http://") || m.starts_with("https://"));
            assert!(!m.ends_with('/'), "常量层不带尾斜杠");
        }
    }

    #[test]
    fn upd_flow_defaults_match_go_package_init() {
        let f = UpdFlow::default();
        assert_eq!(f.phase, "idle");
        assert_eq!(f.pct, -1, "服务器没给长度时前端显示不定进度");
        assert_eq!(f.msg, "");
        assert!(!f.active);
    }
}
