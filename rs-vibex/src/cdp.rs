//! cdp.rs —— 从用户浏览器（Chrome/Edge）自动提取 VibeX 登录态。
//!
//! 对应 Go `internal/vibex/cdp_browser.go`（C77~C81）。结构逐字照抄团结那套零依赖手写 CDP：
//! 探测调试口 → `/json` 取 page 的 WS 地址 → `Network.getCookies` → 找登录态 cookie；
//! 读不到再兜底 `Runtime.evaluate` 读 localStorage（上游 README 说 Local Storage、
//! SPEC 说 Cookie，两条路都读才不会在真机上直接失效）。
//!
//! 测试缝：端口列表与「拉起浏览器」两个钩子由 `Seams` 注入（单测用假 CDP 服务端回放，
//! 不必真拉起浏览器）。
use std::time::Duration;

use serde_json::{Map, Value};

use crate::client::{read_frame, write_frame_bytes, WS_OP_CLOSE, WS_OP_PONG, WS_OP_TEXT};
use crate::jwt::decode_jwt;
use crate::logf;
use crate::util::{json_unmarshal, str_of, to_map};

/// 登录态 cookie 名（SPEC-T1 §3.5 的 WS 鉴权同名）。
pub const VIBEX_COOKIE_NAME: &str = "Rh-Accesstoken";
/// 要读取 cookie 的目标域（REST 与 WS 同一个域）。
pub const VIBEX_COOKIE_URL: &str = "https://vibex.runninghub.cn";
/// 常见浏览器调试口（Chrome 惯例 9222；多留几个探测面）。
pub const CDP_BROWSER_PORTS: [&str; 4] = ["9222", "9223", "9224", "9225"];
/// 候选浏览器 exe（按常见安装路径）。
pub const BROWSER_PATHS: [&str; 4] = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
];

/// 从浏览器提取到的 VibeX 登录态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VibexCreds {
    pub token: String,
    pub user_id: String,
    pub source: String,
    pub browser: String,
    pub port: String,
}

/// `isJWTLooking`：三段、首段 `eyJ`、长度 ≥ 40、且 `decodeJWT` 能解出。
pub fn is_jwt_looking(v: &str) -> bool {
    let v = v.trim();
    if !v.starts_with("eyJ") || v.len() < 40 {
        return false;
    }
    decode_jwt(v).is_some()
}

/// `findVibexCookie`：按 cookie 名找 `Rh-Accesstoken`（`EqualFold` 比名 + 值须像 JWT）。
pub fn find_vibex_cookie(cookies: &[Value]) -> String {
    for c in cookies {
        let name = str_of(c.get("name"));
        let value = str_of(c.get("value"));
        if !name.eq_ignore_ascii_case(VIBEX_COOKIE_NAME) {
            continue;
        }
        if is_jwt_looking(&value) {
            return value;
        }
    }
    String::new()
}

/// CDP WS 端点的极简解析结果（对应 Go `urlParts`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub host: String,
    pub request_uri: String,
}

/// `parseWSURL`：解 `ws://127.0.0.1:9222/devtools/page/XXX` 形状的地址。
pub fn parse_ws_url(raw: &str) -> Result<UrlParts, String> {
    let s = raw.trim_start_matches("ws://");
    let s = s.trim_start_matches("wss://");
    let i = s.find('/').ok_or_else(|| format!("CDP 端点非法: {raw}"))?;
    let (host, path) = (&s[..i], &s[i..]);
    if host.is_empty() || path.is_empty() {
        return Err(format!("CDP 端点非法: {raw}"));
    }
    Ok(UrlParts {
        host: host.to_string(),
        request_uri: path.to_string(),
    })
}

/// 极简 HTTP GET（只用一次，CDP 控制面的 `/json` 与 `/json/version`）。
async fn cdp_http_get(url: &str) -> Result<String, String> {
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.text().await.map_err(|e| e.to_string())
}

/// `cdpAnyPageWS`：连 `/json` 列表取第一个 page 的 `webSocketDebuggerUrl`。
pub async fn cdp_any_page_ws(port: &str) -> Result<String, String> {
    let body = cdp_http_get(&format!("http://127.0.0.1:{port}/json")).await?;
    let v = json_unmarshal(body.as_bytes());
    let list = match v {
        Value::Array(l) => l,
        _ => return Err("无可用页面".into()),
    };
    for t in list {
        let m = match t.as_object() {
            Some(m) => m,
            None => continue,
        };
        if str_of(m.get("type")) == "page" {
            let ws = str_of(m.get("webSocketDebuggerUrl"));
            if !ws.is_empty() {
                return Ok(ws);
            }
        }
    }
    Err("无可用页面".into())
}

/// `cdpBrowserName`：从 `/json/version` 的 `Browser` 字段判断浏览器名。
pub async fn cdp_browser_name(port: &str) -> String {
    let body = match cdp_http_get(&format!("http://127.0.0.1:{port}/json/version")).await {
        Ok(b) => b,
        Err(_) => return "browser".into(),
    };
    let v = json_unmarshal(body.as_bytes());
    let browser = v
        .as_object()
        .map(|m| str_of(m.get("Browser")))
        .unwrap_or_default();
    if browser.contains("Edg") {
        return "edge".into();
    }
    if browser.contains("Chrome") {
        return "chrome".into();
    }
    "browser".into()
}

/// CDP 连接（手写 WS 之上的极简请求/应答）。
struct CdpConn {
    stream: tokio::net::TcpStream,
}

impl CdpConn {
    /// `dialCDP`：建一条到 CDP WebSocket 端点的连接（含握手校验）。
    async fn dial(u: &UrlParts) -> Result<CdpConn, String> {
        let host = if u.host.contains(':') {
            u.host.clone()
        } else {
            format!("{}:80", u.host)
        };
        let stream = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(&host))
            .await
            .map_err(|_| "CDP 连接超时".to_string())?
            .map_err(|e| e.to_string())?;
        let mut key = [0u8; 16];
        crate::config::rand_bytes(&mut key)?;
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            u.request_uri,
            u.host,
            crate::crypto::base64_encode(&key)
        );
        let mut conn = CdpConn { stream };
        conn.stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let head = conn.read_head(8192).await?;
        if !head.contains("101") {
            return Err(format!(
                "WebSocket 握手失败: {}",
                &head[..head.len().min(120)]
            ));
        }
        Ok(conn)
    }

    async fn read_head(&mut self, max: usize) -> Result<String, String> {
        let mut head: Vec<u8> = Vec::new();
        loop {
            let mut one = [0u8; 1];
            self.stream
                .read_exact(&mut one)
                .await
                .map_err(|e| e.to_string())?;
            head.push(one[0]);
            if head.len() >= 4 && head.ends_with(b"\r\n\r\n") {
                break;
            }
            if head.len() > max {
                return Err("握手响应过长".into());
            }
        }
        Ok(String::from_utf8_lossy(&head).to_string())
    }

    /// `cdpCall`：发一条 CDP 消息并读回 id 相同的那条响应。
    async fn call(&mut self, id: i64, method: &str, params: Value) -> Result<Value, String> {
        let mut msg = Map::new();
        msg.insert("id".into(), Value::from(id));
        msg.insert("method".into(), Value::from(method));
        msg.insert("params".into(), params);
        let payload = crate::gjson::J::Obj(
            msg.iter()
                .map(|(k, v)| (k.clone(), crate::gjson::J::from_value(v)))
                .collect(),
        )
        .to_go();
        write_frame_bytes(&mut self.stream, WS_OP_TEXT, payload.as_bytes(), true).await?;
        let mut acc: Vec<u8> = Vec::new();
        loop {
            let res = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut self.stream)).await;
            let (op, _fin, data) = match res {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("CDP 读超时".into()),
            };
            if op == WS_OP_CLOSE {
                return Err("连接被关闭".into());
            }
            if op == WS_OP_PONG || op == 0xA {
                continue;
            }
            acc.extend_from_slice(&data);
            let v = json_unmarshal(&acc);
            if let Some(m) = to_map(&v) {
                if crate::prompt::int_of(m.get("id")) == id {
                    return Ok(v);
                }
            }
            if acc.len() > 1 << 20 {
                return Err("CDP 响应过大".into());
            }
        }
    }

    /// `cdpCookies`：读 VibeX 域 cookie（多域匹配）。
    async fn cookies(&mut self) -> Result<Vec<Value>, String> {
        let params = json_unmarshal(
            format!(
                r#"{{"urls":["{VIBEX_COOKIE_URL}","https://runninghub.cn","{VIBEX_COOKIE_URL}/"]}}"#
            )
            .as_bytes(),
        );
        let raw = self.call(1, "Network.getCookies", params).await?;
        let cookies = raw
            .get("result")
            .and_then(|r| r.get("cookies"))
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(cookies)
    }

    /// `cdpLocalStorage`：读该域页面的 localStorage 里同名键。
    async fn local_storage(&mut self) -> Result<String, String> {
        let expr = format!("String(localStorage.getItem('{VIBEX_COOKIE_NAME}') || '')");
        let params = json_unmarshal(
            format!(r#"{{"expression":{},"returnByValue":true}}"#, {
                let mut s = String::new();
                crate::gjson::push_go_string(&mut s, &expr);
                s
            })
            .as_bytes(),
        );
        let raw = self.call(2, "Runtime.evaluate", params).await?;
        Ok(raw
            .get("result")
            .and_then(|r| r.get("result"))
            .and_then(|r| r.get("value"))
            .map(|v| str_of(Some(v)))
            .unwrap_or_default())
    }
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `probeCDPPort`：连指定端口的 CDP，读 VibeX 登录态并解析凭据。
pub async fn probe_cdp_port(port: &str) -> Result<VibexCreds, String> {
    let ws_url = cdp_any_page_ws(port).await?;
    let u = parse_ws_url(&ws_url)?;
    let mut conn = CdpConn::dial(&u).await?;

    let (mut token, mut source) = (String::new(), String::new());
    if let Ok(cookies) = conn.cookies().await {
        token = find_vibex_cookie(&cookies);
        if !token.is_empty() {
            source = "cookie".into();
        }
    }
    if token.is_empty() {
        if let Ok(v) = conn.local_storage().await {
            if is_jwt_looking(&v) {
                token = v.trim().to_string();
                source = "localstorage".into();
            }
        }
    }
    if token.is_empty() {
        return Err("浏览器里没找到 VibeX 登录态（Rh-Accesstoken）——登录 vibex.runninghub.cn 了吗？".into());
    }
    let info = match decode_jwt(&token) {
        Some(i) if !i.sub.is_empty() => i,
        _ => return Err("Rh-Accesstoken 解析失败（不是合法 JWT？）".into()),
    };
    Ok(VibexCreds {
        token,
        user_id: info.sub,
        source,
        browser: cdp_browser_name(port).await,
        port: port.to_string(),
    })
}

/// `probeCDPBrowser`：探测所有候选调试口，找到能连的且已登录 VibeX 的浏览器。
pub async fn probe_cdp_browser(ports: &[String]) -> Option<VibexCreds> {
    for port in ports {
        if let Ok(creds) = probe_cdp_port(port).await {
            return Some(creds);
        }
    }
    None
}

/// `probeCDPBrowserQuiet`：只探测单个端口（不遍历、不报错）。
pub async fn probe_cdp_browser_quiet(port: &str) -> Option<VibexCreds> {
    probe_cdp_port(port).await.ok()
}

/// `launchBrowserWithCDP`：用【独立一次性 profile】拉起 Edge/Chrome 带调试口。
///
/// 关键（实测）：Edge/Chrome 136+ 对默认 profile 忽略 `--remote-debugging-port`
/// （安全变更），必须指定独立的 `--user-data-dir` 调试口才会开。
pub fn launch_browser_with_cdp(port: &str, profile_dir: &str) -> Result<String, String> {
    for p in BROWSER_PATHS {
        if !std::path::Path::new(p).exists() {
            continue;
        }
        let child = std::process::Command::new(p)
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={profile_dir}"))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--no-experiments")
            .arg(format!("{VIBEX_COOKIE_URL}/"))
            .spawn();
        match child {
            Ok(_) => return Ok(p.to_string()),
            Err(_) => continue,
        }
    }
    Err("未找到 Chrome/Edge（请安装其一，或在甲板上手动粘贴 token）".into())
}

/// `newProbeSessionDir`：每次探测一个全新临时 profile 目录（带纳秒后缀保证唯一）。
pub fn new_probe_session_dir() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir()
        .join(format!("proxydeck-vibex-probe-{nanos}"))
        .to_string_lossy()
        .to_string()
}

/// `cleanupStaleProbeProfiles`：清理 `%TEMP%` 下 mtime 超 24h 的同名前缀目录。
pub fn cleanup_stale_probe_profiles() {
    let dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(24 * 3600);
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("proxydeck-vibex-probe") {
            continue;
        }
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !md.is_dir() {
            continue;
        }
        if let Ok(mtime) = md.modified() {
            if mtime < cutoff {
                let _ = std::fs::remove_dir_all(dir.join(&name));
                logf(&format!("清理过期探测 profile: {name}"));
            }
        }
    }
}

/// `freeCDPPort`：从 9222 起到 9229 找第一个当前可监听的空闲端口。
pub fn free_cdp_port() -> String {
    for i in 9222..=9229 {
        let port = i.to_string();
        if std::net::TcpListener::bind(("127.0.0.1", i)).is_ok() {
            return port;
        }
    }
    "9222".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ws_url_rules() {
        let u = parse_ws_url("ws://127.0.0.1:9222/devtools/page/X").unwrap();
        assert_eq!(u.host, "127.0.0.1:9222");
        assert_eq!(u.request_uri, "/devtools/page/X");
        assert!(parse_ws_url("ws://hostonly").is_err());
        assert!(parse_ws_url("").is_err());
    }

    #[test]
    fn find_cookie_only_accepts_jwt() {
        let fake = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1LTEiLCJleHAiOjE4MDAwMDAwMDAsImZha2UiOnRydWV9.FAKESIG";
        assert_eq!(
            find_vibex_cookie(&[serde_json::json!({"name": "Rh-Accesstoken", "value": fake})]),
            fake
        );
        assert_eq!(
            find_vibex_cookie(&[serde_json::json!({"name": "rh-accesstoken", "value": fake})]),
            fake
        );
        assert_eq!(
            find_vibex_cookie(&[serde_json::json!({"name": "Rh-Accesstoken", "value": "notjwt"})]),
            ""
        );
        assert_eq!(
            find_vibex_cookie(&[serde_json::json!({"name": "other", "value": fake})]),
            ""
        );
    }
}
