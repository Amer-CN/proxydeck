//! client.rs —— VibeX 协议客户端：REST（带 token 池换号轮换）+ 零依赖手写 WS（RFC6455）。
//!
//! 对应 Go `internal/vibex/client.go`（C35~C48）。
//!
//! 为什么 WS 手写而不是用 `tokio-tungstenite`：Go 侧是零依赖手写（自定握手头、客户端帧必带掩码、
//! 不做扩展协商），要对齐「客户端发出去的那串字节」只能自己控制帧层。TLS 那一段用
//! `tokio-native-tls`（Windows 即 schannel，与 reqwest 的 default-tls 同源）。
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::config::{Config, UPSTREAM_UA};
use crate::gjson::{obj, J};
use crate::pool::{Pool, TokenState};
use crate::util::{clip, first_non_empty, json_unmarshal, shape_of, str_of, to_map};

/// REST 单请求总超时（Go `restTimeout = 120 * time.Second`）。
pub const REST_TIMEOUT: Duration = Duration::from_secs(120);

/// 单条 WS 消息上限（Go `wsMaxPayload = 64 << 20`）。
pub const WS_MAX_PAYLOAD: usize = 64 << 20;

/// WS 握手超时（Go `dialWS(..., 30*time.Second)`）。
pub const WS_DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// `tokenHint` 文案（含 `Rh-Accesstoken`，逐字）。
pub const TOKEN_HINT: &str = "未配置 VibeX token: 请重新从浏览器提取 Rh-Accesstoken, \
                              写入 vibex-config.json 的 tokens 数组, 或设置环境变量 RH_ACCESSTOKEN, \
                              或命令行 --vibex-token";

/// `authFailHint` 文案（逐字）。
pub const AUTH_FAIL_HINT: &str = "鉴权失败: VibeX 拒绝了请求, 请重新从浏览器提取 Rh-Accesstoken 后更新配置";

/// 带 HTTP 状态码与错误码的统一业务异常（Go `VibexError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VibexError {
    pub message: String,
    pub status: u16,
    pub code: String,
}

impl VibexError {
    pub fn new(message: impl Into<String>, status: u16, code: impl Into<String>) -> VibexError {
        VibexError {
            message: message.into(),
            status,
            code: code.into(),
        }
    }
}

impl std::fmt::Display for VibexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// WS 握手阶段非 101 的应答（401 → 该 token 标死换号）。
#[derive(Debug, Clone)]
pub struct WsHandshakeError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for WsHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WS 握手失败: HTTP {} {}",
            self.status,
            clip(&self.body, 200)
        )
    }
}

/// `unauthErr` 识别未授权（C40）。命中返回 401/402/429 级 `VibexError`。
pub fn unauth_err(status: u16, raw: &[u8]) -> Option<VibexError> {
    let v = json_unmarshal(raw);
    if let Some(m) = to_map(&v) {
        let code = str_of(m.get("code"));
        let mut msg = str_of(m.get("msg"));
        if msg.is_empty() {
            msg = str_of(m.get("message"));
        }
        match code.as_str() {
            "401" | "403" | "412" => {
                return Some(VibexError::new(
                    format!("{AUTH_FAIL_HINT}（{}）", first_non_empty(&[&msg, &code])),
                    401,
                    "TOKEN_INVALID",
                ))
            }
            "402" => {
                return Some(VibexError::new(
                    format!("上游返回额度不足: {}", clip(&msg, 200)),
                    402,
                    "BALANCE_INSUFFICIENT",
                ))
            }
            "429" => {
                return Some(VibexError::new(
                    format!("上游限流: {}", clip(&msg, 200)),
                    429,
                    "RATE_LIMITED",
                ))
            }
            _ => {}
        }
        if msg.to_uppercase().contains("TOKEN") {
            return Some(VibexError::new(
                format!("{AUTH_FAIL_HINT}（{}）", clip(&msg, 200)),
                401,
                "TOKEN_INVALID",
            ));
        }
    }
    if status == 401 || status == 403 || status == 412 {
        return Some(VibexError::new(AUTH_FAIL_HINT, 401, "TOKEN_INVALID"));
    }
    None
}

/// 客户端：持有配置快照、token 池与 HTTP 客户端。
pub struct Client {
    base_url: String,
    tenant_id: String,
    pool: Arc<Pool>,
    http: reqwest::Client,
    app_type: String,
}

impl Client {
    pub fn new(cfg: &Config, pool: Arc<Pool>) -> Client {
        let http = reqwest::Client::builder()
            .timeout(REST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Client {
            base_url: cfg.base_url(),
            tenant_id: cfg.tenant_id.clone(),
            app_type: cfg.app_type.clone(),
            pool,
            http,
        }
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// 供测试/调试看 base_url（不含 token）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `headersFor`：REST 鉴权头（8 项 + JWT 有 sub 时补 userId/username）。
    pub fn headers_for(&self, token: &str) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![
            ("Content-Type", "application/json".to_string()),
            ("Authorization", format!("Bearer {token}")),
            ("RH-TOKEN", token.to_string()),
            ("X-Tenant-Id", self.tenant_id.clone()),
            ("X-Vibex-Locale", "zh-CN".to_string()),
            ("Origin", self.base_url.clone()),
            ("Referer", format!("{}/", self.base_url)),
            ("User-Agent", UPSTREAM_UA.to_string()),
        ];
        if let Some(info) = crate::jwt::decode_jwt(token) {
            if !info.sub.is_empty() {
                h.push(("userId", info.sub.clone()));
                h.push(("username", info.username.clone()));
            }
        }
        h
    }

    /// `validateToken`：入池前预验证（读体上限 8 KiB）。
    pub async fn validate_token(&self, token: &str) -> Result<(), VibexError> {
        let url = format!("{}/api/llm-providers", self.base_url);
        let mut req = self.http.get(&url);
        for (k, v) in self.headers_for(token) {
            req = req.header(k, v);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(VibexError::new(
                    format!("探活请求失败: {e}"),
                    502,
                    "",
                ))
            }
        };
        let status = resp.status().as_u16();
        let raw = read_limited(resp, 8 << 10).await;
        if let Some(e) = unauth_err(status, &raw) {
            return Err(e);
        }
        if status >= 400 {
            return Err(VibexError::new(
                format!("探活失败 HTTP {}: {}", status, clip(&String::from_utf8_lossy(&raw), 200)),
                502,
                "",
            ));
        }
        Ok(())
    }

    /// `api`：在 token 池上跑一次 REST 调用，必要时换号（C38）。
    pub async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(usize, Value), VibexError> {
        let n = self.pool.len();
        if n == 0 {
            return Err(VibexError::new(TOKEN_HINT, 401, "TOKEN_NOT_CONFIGURED"));
        }
        let mut last_err: Option<VibexError> = None;
        for _ in 0..n {
            let idx = match self.pool.acquire() {
                Some(i) => i,
                None => break,
            };
            let token = self.pool.value_of(idx);
            match self.do_once(&token, method.clone(), path, body.clone()).await {
                Ok(v) => {
                    self.pool.inc_calls(idx);
                    return Ok((idx, v));
                }
                Err(e) => match e.status {
                    401 => {
                        self.pool.mark_dead(idx);
                        last_err = Some(e);
                        continue;
                    }
                    402 | 429 => {
                        self.pool.mark_cooldown(idx);
                        last_err = Some(e);
                        continue;
                    }
                    _ => return Err(e),
                },
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Err(VibexError::new(
                format!("token 池中没有可用 token（全部标死/冷却中/已过期）: {TOKEN_HINT}"),
                401,
                "TOKEN_EXHAUSTED",
            )),
        }
    }

    /// `do`：单 token 发一次请求并做容错解包 + 未授权识别（C39）。
    pub async fn do_once(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Value, VibexError> {
        let url = format!("{}{}", self.base_url, path);
        let mut req = self.http.request(method.clone(), &url);
        for (k, v) in self.headers_for(token) {
            req = req.header(k, v);
        }
        if let Some(p) = &body {
            req = req.body(p.clone());
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let what = format!("{} {}", method.as_str(), path);
                if e.is_timeout() {
                    return Err(VibexError::new(format!("请求超时 {what}"), 504, ""));
                }
                return Err(VibexError::new(format!("请求失败 {what}: {e}"), 502, ""));
            }
        };
        let status = resp.status().as_u16();
        let raw = read_limited(resp, 8 << 20).await;
        let parsed = json_unmarshal(&raw);
        if let Some(e) = unauth_err(status, &raw) {
            return Err(e);
        }
        match status {
            402 | 429 => Err(VibexError::new(
                format!(
                    "上游返回 HTTP {}: {}",
                    status,
                    clip(&String::from_utf8_lossy(&raw), 200)
                ),
                status,
                "BALANCE_OR_LIMIT",
            )),
            s if s >= 400 => Err(VibexError::new(
                format!(
                    "接口 {} 返回 HTTP {}: {}",
                    path,
                    s,
                    clip(&String::from_utf8_lossy(&raw), 200)
                ),
                502,
                "",
            )),
            _ => Ok(parsed),
        }
    }

    /// `providers`：取 llm-providers 列表（容错解包；未解出返回 `None` 并记结构日志）。
    pub async fn providers(&self) -> Result<Option<Vec<Value>>, VibexError> {
        let (_, v) = self.api(reqwest::Method::GET, "/api/llm-providers", None).await?;
        if let Value::Array(items) = &v {
            return Ok(Some(items.clone()));
        }
        if let Some(m) = to_map(&v) {
            for k in ["providers", "items", "data"] {
                match m.get(k) {
                    Some(Value::Array(items)) => return Ok(Some(items.clone())),
                    Some(Value::Object(inner)) => {
                        for k2 in ["providers", "items", "data", "list"] {
                            if let Some(Value::Array(items)) = inner.get(k2) {
                                return Ok(Some(items.clone()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // 未解出列表：记顶层结构自证（只记键名/类型与数量，绝不记 body 与 token）
        let m = to_map(&v);
        crate::logf(&format!(
            "providers 上游 200 但未解出列表（结构={} code={} msg={}）",
            shape_of(&v),
            m.map(|m| str_of(m.get("code"))).unwrap_or_default(),
            clip(
                &m.map(|m| str_of(m.get("msg"))).unwrap_or_default(),
                120
            )
        ));
        Ok(None)
    }

    /// `appsList`：取沙箱项目列表。
    pub async fn apps_list(&self) -> Result<Vec<Value>, VibexError> {
        let (_, v) = self.api(reqwest::Method::GET, "/api/apps", None).await?;
        Ok(crate::util::apps_from(&v).unwrap_or_default())
    }

    /// `appGet`：取单个沙箱项目对象。
    pub async fn app_get(&self, app_id: &str) -> Result<Value, VibexError> {
        let path = format!("/api/apps/{}", url_path_escape(app_id));
        let (_, v) = self.api(reqwest::Method::GET, &path, None).await?;
        Ok(crate::util::unwrap_obj(&v))
    }

    /// `appCreate`：新建沙箱项目（`FREE_PROJECT_LIMIT` → 409）。
    pub async fn app_create(&self, name: &str, provider_id: &str) -> Result<Value, VibexError> {
        let body = obj(vec![
            ("app_type", J::Str(self.app_type())),
            ("enabled_capabilities", J::Null),
            ("flow", J::Null),
            ("llm_provider_id", J::Str(provider_id.to_string())),
            ("name", J::Str(name.to_string())),
            ("pinned_host_ip", J::Null),
        ])
        .to_go();
        let (_, v) = self
            .api(reqwest::Method::POST, "/api/apps", Some(body))
            .await?;
        if let Some(m) = to_map(&v) {
            let code = first_non_empty(&[&str_of(m.get("code")), "0"]);
            if code != "0" {
                let dump = J::from_value(&v).to_go();
                if dump.contains("FREE_PROJECT_LIMIT") {
                    return Err(VibexError::new(
                        "免费项目已达上限(约10个), 请在 VibeX 网页端删除旧项目后重试",
                        409,
                        "FREE_PROJECT_LIMIT",
                    ));
                }
                return Err(VibexError::new(
                    format!(
                        "创建项目失败: {}",
                        clip(
                            &first_non_empty(&[
                                &str_of(m.get("msg")),
                                &str_of(m.get("message")),
                                &code
                            ]),
                            200
                        )
                    ),
                    502,
                    "",
                ));
            }
        }
        Ok(crate::util::unwrap_obj(&v))
    }

    /// `appStart`：POST start 后轮询到 running 且端口就绪（每 2s，最长 240s）。
    pub async fn app_start(&self, app_id: &str) -> Result<Value, VibexError> {
        let path = format!("/api/apps/{}/start", url_path_escape(app_id));
        self.api(reqwest::Method::POST, &path, Some("{}".to_string()))
            .await?;
        let deadline = crate::pool::now_ms() + 240_000;
        let mut last: Option<Value> = None;
        while crate::pool::now_ms() < deadline {
            match self.app_get(app_id).await {
                Ok(app) if !app.is_null() => {
                    if live_status_of(&app) == "running" && ports_ok_of(&app) {
                        return Ok(app);
                    }
                    last = Some(app);
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if let Some(l) = &last {
            if live_status_of(l) == "running" {
                return Ok(last.unwrap());
            }
        }
        Err(VibexError::new(
            format!("项目 {app_id} 启动超时(240s)"),
            504,
            "APP_START_TIMEOUT",
        ))
    }

    /// `setLLMProvider`：绑定模型通道。
    pub async fn set_llm_provider(&self, app_id: &str, provider_id: &str) -> Result<(), VibexError> {
        let path = format!("/api/apps/{}/llm-provider", url_path_escape(app_id));
        let body = obj(vec![
            ("llm_test", J::Bool(false)),
            ("provider_id", J::Str(provider_id.to_string())),
        ])
        .to_go();
        self.api(reqwest::Method::PATCH, &path, Some(body)).await?;
        Ok(())
    }

    /// `ensureLLMSettings`：让上游把 LLM 配置写进沙箱（失败仅告警，不抛）。
    pub async fn ensure_llm_settings(&self, app_id: &str) {
        let path = format!("/api/apps/{}/ensure-llm-settings", url_path_escape(app_id));
        if let Err(e) = self
            .api(reqwest::Method::POST, &path, Some("{}".to_string()))
            .await
        {
            crate::logf(&format!("ensure-llm-settings 调用失败(忽略): {e}"));
        }
    }

    fn app_type(&self) -> String {
        self.app_type.clone()
    }
}

fn live_status_of(app: &Value) -> String {
    crate::util::live_status(app.as_object())
}

fn ports_ok_of(app: &Value) -> bool {
    match app.as_object() {
        Some(m) => crate::util::ports_ok(m),
        None => false,
    }
}

/// `url.PathEscape` 的等价物（Go 只转义 `/` 等保留字符，这里按 path segment 规则转义）。
pub fn url_path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b',' | b':' | b'='
                | b'@');
        if ok {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 带上限的响应读取（Go `io.ReadAll(io.LimitReader(body, n))`）。
async fn read_limited(resp: reqwest::Response, limit: usize) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => {
                let remain = limit.saturating_sub(out.len());
                if remain == 0 {
                    break;
                }
                let take = b.len().min(remain);
                out.extend_from_slice(&b[..take]);
                if take < b.len() || out.len() >= limit {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    out
}

// ============================== WS 客户端（RFC6455，手写） ==============================

pub const WS_OP_CONTINUATION: u8 = 0x0;
pub const WS_OP_TEXT: u8 = 0x1;
pub const WS_OP_BINARY: u8 = 0x2;
pub const WS_OP_CLOSE: u8 = 0x8;
pub const WS_OP_PING: u8 = 0x9;
pub const WS_OP_PONG: u8 = 0xA;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// `wsAccept`：`base64(sha1(key + GUID))`。
pub fn ws_accept(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(WS_GUID.as_bytes());
    crate::crypto::base64_encode(&h.finalize())
}

/// `wsURL`：由 base_url 推导 WS 地址（C45：只取 host，不带 base path）。
pub fn ws_url(base_url: &str, app_id: &str) -> Result<String, String> {
    let (scheme, rest) = split_scheme(base_url).ok_or_else(|| format!("base_url 非法: {base_url}"))?;
    let host = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    if host.is_empty() {
        return Err(format!("base_url 非法: {base_url}"));
    }
    let ws_scheme = if scheme == "https" || scheme == "wss" {
        "wss"
    } else {
        "ws"
    };
    Ok(format!(
        "{ws_scheme}://{host}/app-ws/{}/ws",
        url_path_escape(app_id)
    ))
}

fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let idx = s.find("://")?;
    let scheme = &s[..idx];
    if scheme.is_empty() {
        return None;
    }
    Some((scheme, &s[idx + 3..]))
}

/// 一条已建立的 WS 连接。
///
/// 写半边放在 `Arc<Mutex<…>>` 里：ping 保活任务与主事件泵会**并发写**同一条连接
/// （对应 Go 的 `wsConn.wmu` 写锁），读半边仍由主任务独占。
pub struct WsConn {
    reader: BufReader<tokio::io::ReadHalf<Box<dyn AsyncStream>>>,
    writer: SharedWriter,
}

/// 共享的写半边。
pub type SharedWriter =
    Arc<tokio::sync::Mutex<tokio::io::WriteHalf<Box<dyn AsyncStream>>>>;

/// 在共享写半边上发一帧 masked text（ping 任务用）。
pub async fn write_text_on(w: &SharedWriter, s: &str) -> Result<(), String> {
    let mut g = w.lock().await;
    write_frame_bytes(&mut *g, WS_OP_TEXT, s.as_bytes(), true).await
}

/// 读写合一的流（TCP 或 TLS）。
pub trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> AsyncStream for T {}

impl WsConn {
    /// 共享写半边（ping 任务用）。
    pub fn writer_handle(&self) -> SharedWriter {
        self.writer.clone()
    }

    /// 发一帧 masked text（客户端必须 mask）。
    pub async fn write_text(&mut self, s: &str) -> Result<(), String> {
        let mut g = self.writer.lock().await;
        write_frame_bytes(&mut *g, WS_OP_TEXT, s.as_bytes(), true).await
    }

    /// 发控制帧（pong/close）。
    pub async fn write_control(&mut self, op: u8, payload: &[u8]) -> Result<(), String> {
        let mut g = self.writer.lock().await;
        write_frame_bytes(&mut *g, op, payload, true).await
    }

    /// 读一条完整消息（自动回 pong、拼续帧）；`timeout_ms <= 0` 表示不设读超时。
    /// 读超时归一到 `Err("__WS_READ_TIMEOUT__")`（调用方据此继续等，不算错误）。
    pub async fn read_message(&mut self, timeout_ms: i64) -> Result<String, String> {
        let fut = self.read_message_inner();
        if timeout_ms <= 0 {
            return fut.await;
        }
        match timeout(Duration::from_millis(timeout_ms as u64), fut).await {
            Ok(r) => r,
            Err(_) => Err(WS_READ_TIMEOUT.to_string()),
        }
    }

    async fn read_message_inner(&mut self) -> Result<String, String> {
        let mut payload: Vec<u8> = Vec::new();
        loop {
            let (op, fin, data) = read_frame(&mut self.reader).await?;
            match op {
                WS_OP_PING => {
                    let mut g = self.writer.lock().await;
                    let _ = write_frame_bytes(&mut *g, WS_OP_PONG, &data, true).await;
                }
                WS_OP_PONG => {}
                WS_OP_CLOSE => {
                    let mut g = self.writer.lock().await;
                    let _ = write_frame_bytes(&mut *g, WS_OP_CLOSE, &[], true).await;
                    return Err(WS_CLOSED.to_string());
                }
                WS_OP_TEXT | WS_OP_BINARY | WS_OP_CONTINUATION => {
                    payload.extend_from_slice(&data);
                    if payload.len() > WS_MAX_PAYLOAD {
                        return Err(format!("ws 消息超过 {WS_MAX_PAYLOAD} 字节上限"));
                    }
                    if !fin {
                        continue;
                    }
                    return Ok(String::from_utf8_lossy(&payload).to_string());
                }
                _ => {}
            }
        }
    }

    /// 发 close 帧并关闭底层连接。
    pub async fn close(&mut self) {
        {
            let mut g = self.writer.lock().await;
            let _ = write_frame_bytes(&mut *g, WS_OP_CLOSE, &[], true).await;
            let _ = g.shutdown().await;
        }
    }
}

/// 写一帧（mask=true 时按客户端要求加掩码）。
pub async fn write_frame_bytes<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    op: u8,
    payload: &[u8],
    mask: bool,
) -> Result<(), String> {
    let mut hdr: Vec<u8> = Vec::with_capacity(14);
    hdr.push(0x80 | (op & 0x0f)); // FIN=1
    let n = payload.len();
    let mask_bit = if mask { 0x80u8 } else { 0 };
    if n < 126 {
        hdr.push(mask_bit | (n as u8));
    } else if n <= 0xffff {
        hdr.push(mask_bit | 126);
        hdr.push((n >> 8) as u8);
        hdr.push(n as u8);
    } else {
        hdr.push(mask_bit | 127);
        hdr.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mut body: Vec<u8> = Vec::with_capacity(n + 4);
    if mask {
        let mut key = [0u8; 4];
        crate::config::rand_bytes(&mut key)?;
        body.extend_from_slice(&key);
        for (i, b) in payload.iter().enumerate() {
            body.push(b ^ key[i % 4]);
        }
    } else {
        body.extend_from_slice(payload);
    }
    w.write_all(&hdr).await.map_err(|e| e.to_string())?;
    w.write_all(&body).await.map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}


/// 「对端发了 close 帧或连接已断」（Go `errWSClosed`）。
pub const WS_CLOSED: &str = "__WS_CLOSED__";
/// 「这一小段读窗口内没有消息」（Go `errWSReadTimeout`）。
pub const WS_READ_TIMEOUT: &str = "__WS_READ_TIMEOUT__";

/// 读一帧（服务端帧不 mask；对端 mask 时自动去掩码）。
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
) -> Result<(u8, bool, Vec<u8>), String> {
    let mut hdr = [0u8; 2];
    r.read_exact(&mut hdr).await.map_err(|e| e.to_string())?;
    let fin = hdr[0] & 0x80 != 0;
    let op = hdr[0] & 0x0f;
    let masked = hdr[1] & 0x80 != 0;
    let mut n = (hdr[1] & 0x7f) as u64;
    match n {
        126 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            n = u16::from_be_bytes(b) as u64;
        }
        127 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            n = u64::from_be_bytes(b);
        }
        _ => {}
    }
    if n > WS_MAX_PAYLOAD as u64 {
        return Err(format!("ws 帧长度非法: {n}"));
    }
    let mut key = [0u8; 4];
    if masked {
        r.read_exact(&mut key).await.map_err(|e| e.to_string())?;
    }
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    if masked {
        for i in 0..buf.len() {
            buf[i] ^= key[i % 4];
        }
    }
    Ok((op, fin, buf))
}

/// `dialWS`：建立 WS 连接（握手鉴权头由调用方给），超时 30s。
pub async fn dial_ws(
    raw_url: &str,
    headers: &[(&str, String)],
    dial_timeout: Duration,
) -> Result<WsConn, DialErr> {
    let (scheme, rest) = split_scheme(raw_url).ok_or_else(|| DialErr::Other("ws 地址非法".into()))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rfind(':') {
        Some(i) if !hostport[i + 1..].contains(']') => (
            &hostport[..i],
            hostport[i + 1..]
                .parse::<u16>()
                .unwrap_or(if scheme == "wss" { 443 } else { 80 }),
        ),
        _ => (
            hostport,
            if scheme == "wss" { 443 } else { 80 },
        ),
    };
    let connect = TcpStream::connect((host, port));
    let tcp = match timeout(dial_timeout, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(DialErr::Other(e.to_string())),
        Err(_) => return Err(DialErr::Other("连接超时".into())),
    };
    let stream: Box<dyn AsyncStream> = if scheme == "wss" {
        let connector = tokio_native_tls::TlsConnector::from(
            tokio_native_tls::native_tls::TlsConnector::new().map_err(|e| DialErr::Other(e.to_string()))?,
        );
        let tls = match timeout(dial_timeout, connector.connect(host, tcp)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(DialErr::Other(e.to_string())),
            Err(_) => return Err(DialErr::Other("TLS 握手超时".into())),
        };
        Box::new(tls)
    } else {
        Box::new(tcp)
    };

    let mut key_raw = [0u8; 16];
    crate::config::rand_bytes(&mut key_raw).map_err(DialErr::Other)?;
    let key = crate::crypto::base64_encode(&key_raw);
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");

    let (rd, mut wr) = tokio::io::split(stream);
    let mut reader = BufReader::new(rd);
    if let Err(e) = wr.write_all(req.as_bytes()).await {
        return Err(DialErr::Other(e.to_string()));
    }
    let (status, resp_headers) = match timeout(dial_timeout, read_http_head(&mut reader)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(DialErr::Other(e)),
        Err(_) => return Err(DialErr::Other("读握手响应超时".into())),
    };
    if status != 101 {
        // 非 101：读最多 512 字节的 body 作为错误文案（对齐 Go 的 wsHandshakeError）
        let mut body = vec![0u8; 512];
        let n = match timeout(Duration::from_millis(500), reader.read(&mut body)).await {
            Ok(Ok(n)) => n,
            _ => 0,
        };
        body.truncate(n);
        return Err(DialErr::Handshake(WsHandshakeError {
            status,
            body: String::from_utf8_lossy(&body).to_string(),
        }));
    }
    let got = resp_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-accept"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if got != ws_accept(&key) {
        return Err(DialErr::Other(format!(
            "ws 握手校验失败: Sec-WebSocket-Accept={got:?}"
        )));
    }
    Ok(WsConn {
        reader,
        writer: Arc::new(tokio::sync::Mutex::new(wr)),
    })
}

/// 握手阶段的错误：非 101（可能触发换号）或其它。
#[derive(Debug, Clone)]
pub enum DialErr {
    Handshake(WsHandshakeError),
    Other(String),
}

impl std::fmt::Display for DialErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialErr::Handshake(e) => write!(f, "{e}"),
            DialErr::Other(s) => write!(f, "{s}"),
        }
    }
}

/// 逐字节读到 `\r\n\r\n`（最多 8192 字节），解析状态行与头。
async fn read_http_head<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
) -> Result<(u16, Vec<(String, String)>), String> {
    let mut head: Vec<u8> = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let n = r.read(&mut one).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("握手响应被截断".into());
        }
        head.push(one[0]);
        if head.len() >= 4 && head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 8192 {
            return Err("握手响应过长".into());
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

/// 构造 WS 握手头三项（C56）：Cookie / Origin / User-Agent。
pub fn ws_auth_headers(token: &str, base_url: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Cookie", format!("Rh-Accesstoken={token}")),
        ("Origin", base_url.to_string()),
        ("User-Agent", UPSTREAM_UA.to_string()),
    ]
}

/// 便于测试：把 token 状态映射成可比较的描述。
pub fn describe_token_state(st: &TokenState) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("dead".into(), Value::Bool(st.dead));
    m.insert("calls".into(), Value::from(st.calls));
    m.insert("sub".into(), Value::String(st.sub.clone()));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_drops_base_path() {
        assert_eq!(
            ws_url("https://vibex.runninghub.cn/vc", "app-1").unwrap(),
            "wss://vibex.runninghub.cn/app-ws/app-1/ws"
        );
        assert_eq!(
            ws_url("http://127.0.0.1:8790/vc", "app-1").unwrap(),
            "ws://127.0.0.1:8790/app-ws/app-1/ws"
        );
        assert!(ws_url("::bad::", "a").is_err());
    }

    #[test]
    fn ws_accept_matches_rfc_example() {
        // RFC6455 的样例：key=dGhlIHNhbXBsZSBub25jZQ== → accept=s3pPLMBiTxaQ9kYGzzhZRbK+xOo=
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn unauth_err_covers_all_branches() {
        assert_eq!(
            unauth_err(200, br#"{"code":403,"msg":"TOKEN_MISSION"}"#).unwrap().code,
            "TOKEN_INVALID"
        );
        assert_eq!(
            unauth_err(200, br#"{"code":"401","msg":"m"}"#).unwrap().status,
            401
        );
        assert_eq!(
            unauth_err(200, br#"{"code":402,"msg":"no balance"}"#).unwrap().code,
            "BALANCE_INSUFFICIENT"
        );
        assert_eq!(
            unauth_err(200, br#"{"code":429,"msg":"slow"}"#).unwrap().code,
            "RATE_LIMITED"
        );
        assert_eq!(
            unauth_err(200, br#"{"message":"bad TOKEN here"}"#).unwrap().code,
            "TOKEN_INVALID"
        );
        assert!(unauth_err(200, br#"{"code":500,"msg":"other"}"#).is_none());
        assert!(unauth_err(500, br#"{}"#).is_none());
        assert_eq!(unauth_err(401, b"").unwrap().status, 401);
        assert_eq!(unauth_err(412, b"not json").unwrap().code, "TOKEN_INVALID");
        assert!(unauth_err(200, b"[]").is_none());
        assert!(unauth_err(200, b"null").is_none());
    }
}
