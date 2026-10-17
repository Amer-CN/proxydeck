//! cdp_browser.rs —— 从用户浏览器（Chrome/Edge）自动提取团结登录态
//! （`internal/tuanjie/cdp_browser.go` 的逐条对应物）。
//!
//! 链路：探测调试口（9222~9225）→ `/json` 取一个 page 的 `webSocketDebuggerUrl`
//! → 手写最小 WebSocket 握手 + `Network.getCookies` → 从 cookie 里找 `eyJ` 开头的
//! token → 解 JWT 的 `sub` 当 user_id。
//!
//! 用户浏览器默认不开调试口；探测不到时由前端引导，后端带 `--remote-debugging-port`
//! 拉起浏览器（**独立一次性 profile**——136+ 对默认 profile 忽略调试参数）。
//!
//! ## 与 Go 的差异（登记）
//!
//! 1. 拉起浏览器的进程用 `std::process::Command`（Go 用 `exec.Command` + `go cmd.Wait()`）——
//!    Rust 侧不主动 `wait`（子进程是用户要用的浏览器窗口，收尸留给系统）；
//! 2. `hiddenCmd`（`tasklist` 查进程、`HideWindow`）在 Rust 侧用 `CREATE_NO_WINDOW`
//!    标志位（`0x08000000`）+ `tasklist`，语义相同；
//! 3. WebSocket 掩码字节沿用 Go 的实现（`mask = {n, n>>8, 0x3a, 0xf7}`）——
//!    这是 Go 侧**写死**的掩码（不是随机掩码），逐字照抄才能与对照日志一致；
//!    真实 CDP 服务端不校验掩码随机性（RFC 只要求客户端掩码），实测可用。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime};

/// 读 cookie 的目标域（团结账号域）—— `cdp_browser.go:31`。
pub const CODEY_COOKIE_URL: &str = "https://codely.tuanjie.cn";

/// 常见浏览器调试口 —— `cdpBrowserPorts`（`cdp_browser.go:34`）。
pub const CDP_BROWSER_PORTS: [&str; 4] = ["9222", "9223", "9224", "9225"];

/// 候选浏览器 exe（按常见安装路径）—— `browserPaths`（`cdp_browser.go:37-42`）。
pub const BROWSER_PATHS: [&str; 4] = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
];

/// 从浏览器提取到的团结凭据 —— `CDPBrowserCreds`（`cdp_browser.go:45-50`）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CdpBrowserCreds {
    pub access_token: String,
    pub user_id: String,
    /// chrome | edge | browser。
    pub browser: String,
    pub port: String,
}

/// `Network.getCookies` 的请求体 —— `cdp_browser.go:382-386`（多域匹配，逐字节照抄）。
pub const GET_COOKIES_MSG: &str = "{\"id\":1,\"method\":\"Network.getCookies\",\"params\":{\"urls\":[\n\t\t\"https://codely.tuanjie.cn\",\n\t\t\"https://tuanjie.cn\",\n\t\t\"https://codely.tuanjie.cn/dashboard/usage\"\n\t]}}";

/// 探测所有候选调试口 —— `probeCDPBrowser`（`cdp_browser.go:54-62`）。
pub fn probe_cdp_browser() -> Option<CdpBrowserCreds> {
    for port in CDP_BROWSER_PORTS {
        if let Some(creds) = fetch_codely_creds(port) {
            return Some(creds);
        }
    }
    None
}

/// 安静版：只探测单个端口 —— `probeCDPBrowserQuiet`（`cdp_browser.go:284-290`）。
pub fn probe_cdp_browser_quiet(port: &str) -> Option<CdpBrowserCreds> {
    fetch_codely_creds(port)
}

/// 连指定端口的 CDP，读 codely 域 cookie 并解析凭据 —— `cdpFetchCodelyCreds`
/// （`cdp_browser.go:67-121`）。返回 `None` 表示「这个端口拿不到登录态」。
pub fn fetch_codely_creds(port: &str) -> Option<CdpBrowserCreds> {
    let ws_url = any_page_ws(port).ok()?;
    let cookies = browser_get_cookies(&ws_url).ok()?;
    let token = pick_token(&cookies)?;
    let uid = jwt_sub(&token);
    if uid.is_empty() {
        return None;
    }
    Some(CdpBrowserCreds {
        access_token: token,
        user_id: uid,
        browser: cdp_browser_name(port),
        port: port.to_string(),
    })
}

/// 从 cookie 列表里挑 token —— `cdpFetchCodelyCreds` 的第 3 步（`cdp_browser.go:80-112`）。
///
/// 先按**名字**含 token/session/auth/authorization 挑 `eyJ` 开头的值；
/// 再兜底：任何 `eyJ` 开头且长度 ≥60 的值。
pub fn pick_token(cookies: &[serde_json::Value]) -> Option<String> {
    for c in cookies {
        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if value.is_empty() || !value.starts_with("eyJ") {
            continue;
        }
        let ln = name.to_lowercase();
        if ln.contains("token") || ln.contains("session") || ln.contains("auth") {
            return Some(value.to_string());
        }
    }
    for c in cookies {
        let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if value.starts_with("eyJ") && value.len() >= 60 {
            return Some(value.to_string());
        }
    }
    None
}

/// 解 JWT payload 的 `sub`（不校验签名）—— `jwtSub`（`cdp_browser.go:168-193`）。
pub fn jwt_sub(token: &str) -> String {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return String::new();
    }
    let Some(payload) = crate::crypto::base64_decode(parts[1].as_bytes()).ok() else {
        return String::new();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) else {
        return String::new();
    };
    v.get("sub")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

/// 连 `/json` 取第一个 page 的 `webSocketDebuggerUrl` —— `cdpAnyPageWS`（`cdp_browser.go:124-144`）。
pub fn any_page_ws(port: &str) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/json");
    let body = http_get(&url, Duration::from_secs(2))?;
    let targets: Vec<serde_json::Value> =
        serde_json::from_slice(&body).map_err(|e| format!("解析 /json 失败: {e}"))?;
    for t in targets {
        let ty = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ws = t
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if ty == "page" && !ws.is_empty() {
            return Ok(ws.to_string());
        }
    }
    Err("无可用页面".into())
}

/// 从 `/json/version` 的 `Browser` 字段判断浏览器名 —— `cdpBrowserName`（`cdp_browser.go:147-165`）。
pub fn cdp_browser_name(port: &str) -> String {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let Ok(body) = http_get(&url, Duration::from_secs(2)) else {
        return "browser".into();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return "browser".into();
    };
    let browser = v.get("Browser").and_then(|b| b.as_str()).unwrap_or("");
    if browser.contains("Edg") {
        return "edge".into();
    }
    if browser.contains("Chrome") {
        return "chrome".into();
    }
    "browser".into()
}

/// 极简 HTTP GET（只用于回环 CDP 的 `/json`、`/json/version`）——
/// Go 侧用的是 `&http.Client{Timeout: 2s}` 且**不走代理**（`noProxyTransport`）。
pub fn http_get(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("只支持 http://（{url}）"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let mut stream = TcpStream::connect_timeout(
        &addr
            .to_socket_addrs()
            .map_err(|e| format!("解析 {addr} 失败: {e}"))?
            .next()
            .ok_or_else(|| format!("解析 {addr} 无结果"))?,
        timeout,
    )
    .map_err(|e| format!("连接 {addr} 失败: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("写请求失败: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("读响应失败: {e}"))?;
    let pos = find_subslice(&raw, b"\r\n\r\n").ok_or_else(|| "响应缺少头结束".to_string())?;
    let header = String::from_utf8_lossy(&raw[..pos]).to_string();
    let body = raw[pos + 4..].to_vec();
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        return Err(format!("HTTP 非 200: {}", header.lines().next().unwrap_or("")));
    }
    // chunked 传输：CDP 的 /json 实测是 Content-Length 形态；这里对 chunked 做最小解码
    if header.to_lowercase().contains("transfer-encoding: chunked") {
        return decode_chunked(&body);
    }
    Ok(trim_after_header(&header, body))
}

fn trim_after_header(header: &str, body: Vec<u8>) -> Vec<u8> {
    // 有 Content-Length 时按长度截断（Connection: close 通常已给全长，这里只做保险）
    for line in header.lines() {
        let lower = line.to_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return body[..std::cmp::min(n, body.len())].to_vec();
            }
        }
    }
    body
}

fn decode_chunked(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        let Some(nl) = find_subslice(&body[i..], b"\r\n") else {
            break;
        };
        let size_line = String::from_utf8_lossy(&body[i..i + nl]).to_string();
        let size_hex = size_line.split(';').next().unwrap_or("").trim().to_string();
        let Ok(size) = usize::from_str_radix(&size_hex, 16) else {
            break;
        };
        i += nl + 2;
        if size == 0 {
            break;
        }
        if i + size > body.len() {
            out.extend_from_slice(&body[i..]);
            break;
        }
        out.extend_from_slice(&body[i..i + size]);
        i += size + 2; // 跳过块尾 CRLF
    }
    Ok(out)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 连 WebSocket 端点读 cookie —— `cdpBrowserGetCookies`（`cdp_browser.go:342-410`）。
pub fn browser_get_cookies(page_ws: &str) -> Result<Vec<serde_json::Value>, String> {
    let rest = page_ws
        .strip_prefix("ws://")
        .ok_or_else(|| format!("只支持 ws://（{page_ws}）"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| format!("解析 {addr} 失败: {e}"))?
        .next()
        .ok_or_else(|| format!("解析 {addr} 无结果"))?;
    let mut conn = TcpStream::connect_timeout(&sock, Duration::from_secs(5))
        .map_err(|e| format!("连接 {addr} 失败: {e}"))?;
    let deadline = Duration::from_secs(10);
    conn.set_read_timeout(Some(deadline)).map_err(|e| e.to_string())?;
    conn.set_write_timeout(Some(deadline)).map_err(|e| e.to_string())?;

    // 握手：key 是 uuid 前 16 字符 + "=="（与 Go 逐字同款）
    let key = format!("{}==", &crate::session::new_uuid_v4()[..16]);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    conn.write_all(req.as_bytes())
        .map_err(|e| format!("写握手失败: {e}"))?;
    let mut head: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1];
    while head.len() < 4 || !head.ends_with(b"\r\n\r\n") {
        let n = conn.read(&mut buf).map_err(|e| format!("读握手失败: {e}"))?;
        if n == 0 {
            return Err("握手提前结束".into());
        }
        head.push(buf[0]);
        if head.len() > 8192 {
            return Err("握手响应过长".into());
        }
    }
    if !find_subslice(&head, b"101").is_some() {
        let short = String::from_utf8_lossy(&head[..std::cmp::min(120, head.len())]).to_string();
        return Err(format!("WebSocket 握手失败: {short}"));
    }
    write_ws_frame(&mut conn, GET_COOKIES_MSG.as_bytes())?;
    let mut payload: Vec<u8> = Vec::new();
    loop {
        let frame = read_ws_frame(&mut conn)?;
        payload.extend_from_slice(&frame);
        if let Ok(resp) = serde_json::from_slice::<serde_json::Value>(&payload) {
            if resp.get("id").and_then(|v| v.as_i64()) == Some(1) {
                let cookies = resp
                    .get("result")
                    .and_then(|r| r.get("cookies"))
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default();
                return Ok(cookies);
            }
        }
        if payload.len() > 1 << 20 {
            return Err("CDP 响应过大".into());
        }
    }
}

/// 写一帧（客户端必掩码；掩码字节与 Go 逐字相同）—— `cdpWriteWSFrame`（`cdp_browser.go:412-437`）。
fn write_ws_frame(conn: &mut TcpStream, data: &[u8]) -> Result<(), String> {
    let mut head: Vec<u8> = vec![0x81];
    let n = data.len();
    let mask_bit = 0x80u8;
    if n < 126 {
        head.push(mask_bit | n as u8);
    } else if n < 65536 {
        head.push(mask_bit | 126);
        head.push((n >> 8) as u8);
        head.push(n as u8);
    } else {
        head.push(mask_bit | 127);
        for i in (0..8).rev() {
            head.push((n >> (8 * i)) as u8);
        }
    }
    let mask = [n as u8, (n >> 8) as u8, 0x3a, 0xf7];
    head.extend_from_slice(&mask);
    let mut masked = Vec::with_capacity(n);
    for (i, b) in data.iter().enumerate() {
        masked.push(b ^ mask[i % 4]);
    }
    head.extend_from_slice(&masked);
    conn.write_all(&head).map_err(|e| format!("写帧失败: {e}"))
}

/// 读一帧 —— `cdpReadWSFrame`（`cdp_browser.go:439-473`）。
fn read_ws_frame(conn: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut h = [0u8; 2];
    read_full(conn, &mut h)?;
    let opcode = h[0] & 0x0f;
    let mut length = (h[1] & 0x7f) as u64;
    match length {
        126 => {
            let mut e = [0u8; 2];
            read_full(conn, &mut e)?;
            length = ((e[0] as u64) << 8) | e[1] as u64;
        }
        127 => {
            let mut e = [0u8; 8];
            read_full(conn, &mut e)?;
            length = 0;
            for b in e {
                length = (length << 8) | b as u64;
            }
        }
        _ => {}
    }
    let mut data = vec![0u8; length as usize];
    read_full(conn, &mut data)?;
    if opcode == 0x8 {
        return Err("连接被关闭".into());
    }
    if opcode == 0x9 {
        return read_ws_frame(conn); // ping → 递归读下一帧（与 Go 同）
    }
    Ok(data)
}

fn read_full(conn: &mut TcpStream, buf: &mut [u8]) -> Result<(), String> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = conn
            .read(&mut buf[done..])
            .map_err(|e| format!("读帧失败: {e}"))?;
        if n == 0 {
            return Err("读帧提前结束".into());
        }
        done += n;
    }
    Ok(())
}

/// 用【独立一次性 profile】拉起 Edge/Chrome 带调试口 —— `launchBrowserWithCDP`
/// （`cdp_browser.go:234-254`）。返回被拉起的 exe 路径。
pub fn launch_browser_with_cdp(port: &str, profile_dir: &str) -> Result<String, String> {
    for p in BROWSER_PATHS {
        if !std::path::Path::new(p).is_file() {
            continue;
        }
        let child = std::process::Command::new(p)
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={profile_dir}"))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--no-experiments")
            .arg("https://codely.tuanjie.cn/dashboard/usage")
            .spawn();
        match child {
            Ok(_) => return Ok(p.to_string()),
            Err(_) => continue,
        }
    }
    Err("未找到 Chrome/Edge（请安装其一或手动添加账号）".into())
}

/// 自动探测专用浏览器 profile 目录 —— `probeProfileDir`（`cdp_browser.go:257-259`）。
pub fn probe_profile_dir() -> String {
    format!("{}\\proxydeck-codely-probe", temp_dir())
}

/// 每次探测一个全新临时 profile 目录（UnixNano 后缀）—— `newProbeSessionDir`
/// （`cdp_browser.go:263-265`）。
pub fn new_probe_session_dir() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}\\proxydeck-codely-probe-{nanos}", temp_dir())
}

fn temp_dir() -> String {
    std::env::var("TEMP")
        .or_else(|_| std::env::var("TMP"))
        .unwrap_or_else(|_| ".".to_string())
        .trim_end_matches(['\\', '/'])
        .to_string()
}

/// 从 9222 起到 9229 找第一个可监听的空闲端口 —— `freeCDPPort`（`cdp_browser.go:269-280`）。
pub fn free_cdp_port() -> String {
    for i in 9222..=9229 {
        let port = i.to_string();
        if std::net::TcpListener::bind(("127.0.0.1", i)).is_ok() {
            return port;
        }
    }
    "9222".to_string()
}

/// 清理 `%TEMP%` 下 mtime 超 24h 的 `proxydeck-codely-probe` 前缀目录 ——
/// `cleanupStaleProbeProfiles`（`cdp_browser.go:294-316`）。
pub fn cleanup_stale_probe_profiles() {
    let dir = temp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("proxydeck-codely-probe") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if !md.is_dir() {
            continue;
        }
        let Ok(modified) = md.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age > Duration::from_secs(24 * 3600) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// 指定进程名是否有实例在跑 —— `processRunning`（`cdp_browser.go:326-332`）。
///
/// Go 侧用 `hiddenCmd("tasklist", "/FI", "IMAGENAME eq <name>")`（隐藏窗口）。
pub fn process_running(name: &str) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("tasklist")
            .arg("/FI")
            .arg(format!("IMAGENAME eq {name}"))
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains(&name.to_lowercase()),
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

/// 一个临时回环 CDP 替身（门禁用：假 `/json` + 假 WebSocket 服务端）——
/// **只在测试里构造**，生产路径不碰它。
#[doc(hidden)]
pub struct FakeCdp {
    pub port: u16,
    listener: Option<std::net::TcpListener>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FakeCdp {
    /// 起一个只回答 `/json`、`/json/version` 与 WebSocket cookie 查询的假服务。
    ///
    /// `browser`：`/json/version` 的 `Browser` 字段；`cookies_json`：`Network.getCookies`
    /// 的 result.cookies 数组文本。
    #[doc(hidden)]
    pub fn start(browser: &str, cookies_json: &str) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("绑定回环");
        let port = listener.local_addr().unwrap().port();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let l2 = listener.try_clone().unwrap();
        let s2 = stop.clone();
        let browser = browser.to_string();
        let cookies = cookies_json.to_string();
        std::thread::spawn(move || {
            for conn in l2.incoming() {
                if s2.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let Ok(mut c) = conn else { continue };
                let browser = browser.clone();
                let cookies = cookies.clone();
                std::thread::spawn(move || {
                    let _ = serve_fake_cdp(&mut c, &browser, &cookies, port);
                });
            }
        });
        FakeCdp {
            port,
            listener: Some(listener),
            stop,
        }
    }
}

impl Drop for FakeCdp {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // 触发一次连接让 accept 循环退出
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        drop(self.listener.take());
    }
}

fn serve_fake_cdp(
    c: &mut TcpStream,
    browser: &str,
    cookies_json: &str,
    port: u16,
) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let n = c.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let first = head.lines().next().unwrap_or("");
    if first.contains("/json/version") {
        let body = format!("{{\"Browser\":\"{browser}\"}}");
        write_http(c, &body)?;
        return Ok(());
    }
    if first.contains("/json") {
        let body = format!(
            "[{{\"type\":\"page\",\"webSocketDebuggerUrl\":\"ws://127.0.0.1:{port}/devtools/page/1\"}}]"
        );
        write_http(c, &body)?;
        return Ok(());
    }
    // WebSocket 升级：回 101，然后回一帧 Network.getCookies 的结果
    let resp = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: c3R1YmJlZA==\r\n\r\n";
    c.write_all(resp.as_bytes())?;
    // 读客户端那一帧（消息长度未知，读一段就够）
    let mut f = [0u8; 8192];
    let _ = c.read(&mut f);
    let payload =
        format!("{{\"id\":1,\"result\":{{\"cookies\":{cookies_json}}}}}");
    let bytes = payload.as_bytes();
    let mut frame = vec![0x81u8];
    let n = bytes.len();
    if n < 126 {
        frame.push(n as u8);
    } else if n < 65536 {
        frame.push(126);
        frame.push((n >> 8) as u8);
        frame.push(n as u8);
    } else {
        frame.push(127);
        for i in (0..8).rev() {
            frame.push((n >> (8 * i)) as u8);
        }
    }
    frame.extend_from_slice(bytes);
    c.write_all(&frame)?;
    Ok(())
}

fn write_http(c: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    c.write_all(resp.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64url(b: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    fn synth_jwt(sub: &str) -> String {
        let h = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let p = b64url(format!(r#"{{"sub":"{sub}","exp":4102444800}}"#).as_bytes());
        let s = b64url(b"sig");
        format!("{h}.{p}.{s}")
    }

    #[test]
    fn jwt_sub_reads_payload_without_padding() {
        let t = synth_jwt("uid-cdp-1");
        assert_eq!(jwt_sub(&t), "uid-cdp-1");
        assert_eq!(jwt_sub("not.a.jwt.extra"), "");
        assert_eq!(jwt_sub("garbage"), "");
    }

    #[test]
    fn pick_token_prefers_named_cookie_then_fallback() {
        let long = format!("eyJ{}", "a".repeat(80));
        // 名字命中优先
        let cookies = vec![
            serde_json::json!({"name":"other","value":long}),
            serde_json::json!({"name":"session_token","value":"eyJSHORT"}),
        ];
        assert_eq!(pick_token(&cookies).unwrap(), "eyJSHORT");
        // 名字都不命中 → 兜底取长 eyJ
        let cookies = vec![serde_json::json!({"name":"x","value":long.clone()})];
        assert_eq!(pick_token(&cookies).unwrap(), long);
        // 都不满足 → None
        assert!(pick_token(&[serde_json::json!({"name":"x","value":"nope"})]).is_none());
    }

    #[test]
    fn fake_cdp_end_to_end_produces_creds() {
        let token = synth_jwt("uid-from-cookie");
        let cookies = format!(
            "[{{\"name\":\"session_token\",\"value\":\"{token}\"}}]"
        );
        let cdp = FakeCdp::start("Chrome/141.0", &cookies);
        let port = cdp.port.to_string();
        let creds = fetch_codely_creds(&port).expect("应读到凭据");
        assert_eq!(creds.user_id, "uid-from-cookie");
        assert_eq!(creds.access_token, token);
        assert_eq!(creds.browser, "chrome", "/json/version 的 Browser 字段判定");
        assert_eq!(creds.port, port);
        // 安静版同源
        assert!(probe_cdp_browser_quiet(&port).is_some());
    }

    #[test]
    fn fake_cdp_edge_naming_and_empty_cookies() {
        let cdp = FakeCdp::start("Edg/141.0", "[]");
        let port = cdp.port.to_string();
        assert!(fetch_codely_creds(&port).is_none(), "无 cookie → None");
        assert_eq!(cdp_browser_name(&port), "edge");
    }

    #[test]
    fn probe_all_ports_returns_none_when_nothing_listens() {
        // 打一个必定没人监听的端口集合：把探测端口换成假端口不可行（常量），
        // 故直接验 `fetch_codely_creds` 对死端口的形态。
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = l.local_addr().unwrap().port().to_string();
        drop(l);
        assert!(fetch_codely_creds(&p).is_none());
    }

    #[test]
    fn profile_dirs_are_under_temp_and_unique() {
        let a = new_probe_session_dir();
        let b = new_probe_session_dir();
        assert!(a.contains("proxydeck-codely-probe-"), "{a}");
        assert_ne!(a, b, "两次调用目录不同");
        assert!(probe_profile_dir().ends_with("proxydeck-codely-probe"));
    }

    #[test]
    fn free_port_is_numeric_and_in_range() {
        let p: u16 = free_cdp_port().parse().unwrap();
        assert!((9222..=9229).contains(&p));
    }

    #[test]
    fn cleanup_removes_only_old_prefixed_dirs() {
        let dir = temp_dir();
        let fresh = format!("{dir}\\proxydeck-codely-probe-fresh-{}", std::process::id());
        let _ = std::fs::remove_dir_all(&fresh);
        std::fs::create_dir_all(&fresh).unwrap();
        let other = format!("{dir}\\not-a-probe-{}", std::process::id());
        std::fs::create_dir_all(&other).unwrap();
        cleanup_stale_probe_profiles();
        assert!(std::path::Path::new(&fresh).exists(), "新目录不该被清");
        assert!(std::path::Path::new(&other).exists(), "非前缀目录不动");
        let _ = std::fs::remove_dir_all(&fresh);
        let _ = std::fs::remove_dir_all(&other);
    }
}
