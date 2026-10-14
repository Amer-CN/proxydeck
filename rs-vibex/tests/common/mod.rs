// tests/common/mod.rs —— rs-vibex 集成测试的共用脚手架。
//
// 三条纪律（与黄金基准的采集口径一一对应）：
// 1. **绝不碰 8790**：上游、CDP 都是**进程内假服务**（Rust 侧自己起的 axum 服务），不占任何现行端口；
// 2. **绝不碰真实凭据**：token 一律是采集器里那批合成假串（payload 带 `fake:true`）；
// 3. 每个用例用自己的临时目录与随机端口，测试之间不共享可变状态。
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

// ---------- 黄金基准加载 ----------

/// 先 `rs-vibex/tests/golden/`（入库），缺则回落 `.work/rust-rewrite/`（重新采集时的来源）。
pub fn golden_path(file: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rs-vibex 应有父目录")
        .to_path_buf();
    let in_repo = root.join("rs-vibex/tests/golden").join(file);
    if in_repo.exists() {
        return in_repo;
    }
    root.join(".work/rust-rewrite").join(file)
}

pub fn load_golden(file: &str) -> Value {
    let path = golden_path(file);
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("读不到黄金基准 {}: {e}", path.display()));
    serde_json::from_slice(&raw)
        .unwrap_or_else(|e| panic!("黄金基准不是合法 JSON {}: {e}", path.display()))
}

// ---------- 临时目录 ----------

static SEQ: AtomicUsize = AtomicUsize::new(0);

pub struct TestDir {
    pub path: PathBuf,
}

impl TestDir {
    pub fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rs-vibex-test-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self { path }
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------- 归一化（与采集器 meta.normalize 同一套规则） ----------

/// 把 `key` 后面紧跟的整数字面量（可带负号）换成占位符。
fn replace_num_after(s: &str, key: &str, placeholder: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if s[i..].starts_with(key) {
            let start = i + key.len();
            let mut j = start;
            if bytes.get(j) == Some(&b'-') {
                j += 1;
            }
            let digits_from = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > digits_from {
                out.push_str(key);
                out.push_str(placeholder);
                i = j;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 把 `127.0.0.1:<port>` 里的端口换成 `<PORT>`（上游是随机端口，逐字节比会假红）。
pub fn replace_ports(s: &str) -> String {
    let needle = "127.0.0.1:";
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if s[i..].starts_with(needle) {
            let start = i + needle.len();
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                out.push_str(needle);
                out.push_str("<PORT>");
                i = j;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 归一化易变字段：时间戳、随机 id、端口、运行时长、剩余天数。
pub fn norm(s: &str) -> String {
    let mut t = replace_num_after(s, "\"created\":", "<CREATED>");
    t = replace_num_after(&t, "\"uptimeSec\":", "<UPTIME>");
    t = replace_num_after(&t, "\"days_left\":", "<DAYS>");
    t = replace_num_after(&t, "\"days_left_max\":", "<DAYS>");
    t = replace_id(&t);
    replace_ports(&t)
}

/// `"id":"chatcmpl-<hex>"` → `"id":"<ID>"`。
fn replace_id(s: &str) -> String {
    let needle = "\"id\":\"chatcmpl-";
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if s[i..].starts_with(needle) {
            let start = i + needle.len();
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j > start && bytes.get(j) == Some(&b'"') {
                out.push_str("\"id\":\"<ID>\"");
                i = j + 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ---------- 假上游 ----------

/// 契约要求的上游请求头（其余由各自客户端生成，不参与对齐）。
pub const CONTRACT_REQ_HEADERS: [&str; 10] = [
    "content-type",
    "authorization",
    "rh-token",
    "x-tenant-id",
    "x-vibex-locale",
    "origin",
    "referer",
    "user-agent",
    "userid",
    "username",
];
/// WS 握手要看的 3 个头。
pub const CONTRACT_WS_HEADERS: [&str; 3] = ["cookie", "origin", "user-agent"];

#[derive(Debug, Clone, Default)]
pub struct UpReq {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

#[derive(Default)]
pub struct Rec {
    pub reqs: Vec<UpReq>,
    pub ws_conns: Vec<Vec<String>>,
    pub ws_heads: Vec<Vec<String>>,
    pub hits: BTreeMap<String, i64>,
}

fn pick_headers(h: &HeaderMap, keys: &[&str]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for k in keys {
        // **按字节无损读取**：`HeaderValue::to_str()` 只接受可见 ASCII，非 ASCII（obs-text，
        // 如 `username: 小明`）必然 `Err` —— 早先用 `.to_str().ok()` 会把它读成空串，
        // 再被门禁当成「假上游观察不到非 ASCII 头」放行（87 处假 allowance 的真根因）。
        // `as_bytes()` 拿的是 hyper 解析出的原始字节，`from_utf8_lossy` 对合法 UTF-8 无损。
        let v = h
            .get(*k)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).to_string())
            .unwrap_or_default();
        out.insert(k.to_string(), v);
    }
    out
}

struct UpState {
    mode: Mutex<String>,
    reject_tokens: Mutex<BTreeMap<String, u16>>,
    ws_reject_tokens: Mutex<BTreeMap<String, u16>>,
    replay: Mutex<Vec<String>>,
    /// 每片文本增量之间的延迟（测「客户端中途断开」时把流拉长用）。
    replay_delay_ms: Mutex<u64>,
    done_flags: Mutex<Value>,
    after_ready: Mutex<Vec<Value>>,
    ready_error: Mutex<Option<String>>,
    rec: Mutex<Rec>,
}

pub struct FakeUpstream {
    pub base: String,
    state: Arc<UpState>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl FakeUpstream {
    pub async fn start() -> Self {
        let state = Arc::new(UpState {
            mode: Mutex::new("ok".into()),
            reject_tokens: Mutex::new(BTreeMap::new()),
            ws_reject_tokens: Mutex::new(BTreeMap::new()),
            replay: Mutex::new(vec!["你好".into(), "世界".into()]),
            replay_delay_ms: Mutex::new(0),
            done_flags: Mutex::new(json!(null)),
            after_ready: Mutex::new(Vec::new()),
            ready_error: Mutex::new(None),
            rec: Mutex::new(Rec::default()),
        });
        let app = Router::new()
            .route("/app-ws/{id}/ws", axum::routing::get(ws_handler))
            .fallback(rest_handler)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑假上游");
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeUpstream {
            base: format!("http://127.0.0.1:{}", addr.port()),
            state,
            handle,
        }
    }

    pub async fn set_mode(&self, m: &str) {
        *self.state.mode.lock().await = m.to_string();
    }
    pub async fn mode(&self) -> String {
        self.state.mode.lock().await.clone()
    }
    pub async fn reject_token(&self, token: &str, status: u16) {
        self.state
            .reject_tokens
            .lock()
            .await
            .insert(token.to_string(), status);
    }
    pub async fn ws_reject_token(&self, token: &str, status: u16) {
        self.state
            .ws_reject_tokens
            .lock()
            .await
            .insert(token.to_string(), status);
    }
    pub async fn set_replay_delay(&self, ms: u64) {
        *self.state.replay_delay_ms.lock().await = ms;
    }

    pub async fn set_replay(&self, pieces: Vec<&str>) {
        *self.state.replay.lock().await = pieces.into_iter().map(|s| s.to_string()).collect();
    }
    pub async fn set_done_flags(&self, v: Value) {
        *self.state.done_flags.lock().await = v;
    }
    pub async fn set_after_ready(&self, v: Vec<Value>) {
        *self.state.after_ready.lock().await = v;
    }
    pub async fn set_ready_error(&self, code: &str) {
        *self.state.ready_error.lock().await = Some(code.to_string());
    }

    /// 取走并清空累计记录（本步只看本步的上游观测）。
    pub async fn take(&self) -> Rec {
        let mut rec = self.state.rec.lock().await;
        std::mem::take(&mut *rec)
    }
}

fn strip_vc(p: &str) -> &str {
    p.strip_prefix("/vc").unwrap_or(p)
}

async fn rest_handler(State(st): State<Arc<UpState>>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let p = strip_vc(&path).to_string();
    {
        let mut rec = st.rec.lock().await;
        rec.reqs.push(UpReq {
            method: method.clone(),
            path: path.clone(),
            headers: pick_headers(&headers, &CONTRACT_REQ_HEADERS),
            body: String::from_utf8_lossy(&body).to_string(),
        });
        *rec.hits.entry(format!("{method} {path}")).or_insert(0) += 1;
    }
    let mode = st.mode.lock().await.clone();
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim_start_matches("Bearer ")
        .to_string();
    if let Some(code) = st.reject_tokens.lock().await.get(&token).copied() {
        let body = if code == 401 {
            r#"{"code":403,"msg":"TOKEN_MISSION"}"#
        } else {
            "{}"
        };
        return (StatusCode::from_u16(code).unwrap(), body.to_string()).into_response();
    }

    if p == "/api/llm-providers" {
        return providers_response(&mode);
    }
    if p == "/api/apps" && method == "GET" {
        let apps = if mode == "apps-none" { vec![] } else { vec![app_obj(&mode)] };
        return json_resp(200, json!({"apps": apps}));
    }
    if p == "/api/apps" && method == "POST" {
        if mode == "apps-create-free-limit" {
            return json_resp(200, json!({"code": 1, "msg": "FREE_PROJECT_LIMIT"}));
        }
        return json_resp(200, app_obj(&mode));
    }
    if p.ends_with("/start") || p.ends_with("/llm-provider") || p.ends_with("/ensure-llm-settings") {
        return json_resp(200, json!({"code": 0}));
    }
    if p.starts_with("/api/apps/") {
        return json_resp(200, app_obj(&mode));
    }
    json_resp(404, json!({"code": 404, "msg": "not found"}))
}

fn providers_response(mode: &str) -> Response {
    let lite = json!({"id": "lite-1", "billing_tier": "lite", "display_name": "Lite One", "description": "免费档"});
    let pro = json!({"id": "pro-1", "billing_tier": "pro", "display_name": "Pro One", "description": null});
    match mode {
        "providers-empty-list" => json_resp(200, json!({"providers": []})),
        "providers-nested-data" => json_resp(200, json!({"code": 0, "data": {"providers": [lite, pro]}})),
        "providers-top-array" => json_resp(200, json!([lite, pro])),
        "providers-unparsable" => json_resp(200, json!({"code": 0, "msg": "ok"})),
        "providers-body-403" => json_resp(200, json!({"code": 403, "msg": "TOKEN_MISSION"})),
        "providers-http-500" => json_resp(500, json!({"code": 500, "msg": "boom"})),
        "providers-http-402" => json_resp(402, json!({"code": 402, "msg": "no balance"})),
        _ => json_resp(200, json!({"providers": [lite, pro]})),
    }
}

fn app_obj(mode: &str) -> Value {
    let mut app = json!({
        "app_id": "app-1", "name": "vibex2api", "app_type": "web",
        "image": "img:1", "host_ip": "10.0.0.1",
        "host_ports": {"9000": 31000, "7000": 31001},
        "created_at": "2026-09-16T00:00:00Z",
        "pocketbase_url": "https://pb.example",
        "flow": "f-1",
        "enabled_capabilities": ["web"],
        "live": {"status": "running"}
    });
    match mode {
        "app-status-cached-only" => {
            app.as_object_mut().unwrap().remove("live");
            app["status_cached"] = json!("running");
        }
        "app-ports-missing" => {
            app["host_ports"] = json!({"9000": 0, "7000": 0});
        }
        _ => {}
    }
    app
}

fn json_resp(status: u16, v: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "application/json; charset=utf-8")],
        v.to_string(),
    )
        .into_response()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::Path(id): axum::extract::Path<String>,
    State(st): State<Arc<UpState>>,
    headers: HeaderMap,
) -> Response {
    let head = CONTRACT_WS_HEADERS
        .iter()
        .map(|k| {
            format!(
                "{}: {}",
                k,
                headers.get(*k).and_then(|v| v.to_str().ok()).unwrap_or("")
            )
        })
        .collect::<Vec<String>>();
    let token = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix("Rh-Accesstoken="))
        .unwrap_or("")
        .to_string();
    let reject = st.ws_reject_tokens.lock().await.get(&token).copied();
    let idx = {
        let mut rec = st.rec.lock().await;
        rec.ws_heads.push(head);
        rec.ws_conns.push(Vec::new());
        rec.ws_conns.len() - 1
    };
    let _ = id;
    if let Some(code) = reject {
        return (StatusCode::from_u16(code).unwrap(), "").into_response();
    }
    ws.on_upgrade(move |socket| ws_loop(socket, st, idx))
}

async fn ws_loop(mut socket: WebSocket, st: Arc<UpState>, idx: usize) {
    while let Some(msg) = socket.recv().await {
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Binary(_)) => continue,
            Ok(Message::Close(_)) => return,
            Ok(_) => continue,
            Err(_) => return,
        };
        {
            let mut rec = st.rec.lock().await;
            if idx < rec.ws_conns.len() {
                rec.ws_conns[idx].push(text.clone());
            }
        }
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "init" | "init_root" => {
                let ready_error = st.ready_error.lock().await.clone();
                if let Some(code) = ready_error {
                    let _ = socket
                        .send(Message::Text(
                            json!({"type": "error", "code": code, "message": code})
                                .to_string()
                                .into(),
                        ))
                        .await;
                    continue;
                }
                let msg = json!({"type": "ready", "cwd": "/workspace/app", "session_id": "srv-1"});
                if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                    return;
                }
                let extra = st.after_ready.lock().await.clone();
                for m in extra {
                    if socket.send(Message::Text(m.to_string().into())).await.is_err() {
                        return;
                    }
                }
            }
            "prompt" => {
                let pieces = st.replay.lock().await.clone();
                let delay = *st.replay_delay_ms.lock().await;
                for piece in &pieces {
                    if delay > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                    let msg = json!({"type": "claude_event", "event": {
                        "type": "stream_event",
                        "event": {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": piece}}
                    }});
                    if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                        return;
                    }
                }
                let joined = pieces.join("");
                let result = json!({"type": "claude_event", "event": {
                    "type": "result", "subtype": "success", "result": joined,
                    "usage": {"input_tokens": 12, "output_tokens": 7}
                }});
                if socket.send(Message::Text(result.to_string().into())).await.is_err() {
                    return;
                }
                let mut done = json!({"type": "done", "turn": 0});
                if let Some(flags) = st.done_flags.lock().await.as_object() {
                    for (k, v) in flags {
                        done[k] = v.clone();
                    }
                }
                if socket.send(Message::Text(done.to_string().into())).await.is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

// ---------- 假 CDP 服务端 ----------

pub struct FakeCdp {
    pub port: String,
    calls: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for FakeCdp {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl FakeCdp {
    /// `cookie` / `local` 为空串表示读不到（走超时路径）。
    pub async fn start(cookie: &str, local: &str) -> Self {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cookie = cookie.to_string();
        let local = local.to_string();
        let calls2 = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑假 CDP");
        let port = listener.local_addr().unwrap().port().to_string();
        let port2 = port.clone();
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let cookie = cookie.clone();
                let local = local.clone();
                let calls = calls2.clone();
                let port = port2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = match sock.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = head.lines().next().unwrap_or("");
                    let path = first.split_whitespace().nth(1).unwrap_or("");
                    if path == "/json" {
                        let body = json!([
                            {"type": "other", "webSocketDebuggerUrl": ""},
                            {"type": "page", "webSocketDebuggerUrl":
                                format!("ws://127.0.0.1:{port}/devtools/page/fake-1")}
                        ])
                        .to_string();
                        let _ = sock
                            .write_all(http_ok(&body, "application/json").as_bytes())
                            .await;
                        return;
                    }
                    if path == "/json/version" {
                        let body = json!({"Browser": "Chrome/136.0.0.0"}).to_string();
                        let _ = sock
                            .write_all(http_ok(&body, "application/json").as_bytes())
                            .await;
                        return;
                    }
                    // devtools WS
                    let key = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            if k.trim().eq_ignore_ascii_case("sec-websocket-key") {
                                Some(v.trim().to_string())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let accept = rs_vibex::client::ws_accept(&key);
                    let resp = format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                         Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                    );
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                    // 逐帧应答（客户端帧带掩码，read_frame 会自动去掩码）
                    let mut reader = tokio::io::BufReader::new(sock);
                    loop {
                        let frame = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            rs_vibex::client::read_frame(&mut reader),
                        )
                        .await;
                        let (op, _fin, data) = match frame {
                            Ok(Ok(v)) => v,
                            _ => return,
                        };
                        if op == rs_vibex::client::WS_OP_CLOSE {
                            return;
                        }
                        if op != rs_vibex::client::WS_OP_TEXT {
                            continue;
                        }
                        let req: Value = serde_json::from_slice(&data).unwrap_or(Value::Null);
                        let id = req.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                        let method = req
                            .get("method")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        calls.lock().await.push(method.clone());
                        let reply = match method.as_str() {
                            "Network.getCookies" => {
                                let mut list = Vec::new();
                                if !cookie.is_empty() {
                                    list.push(json!({"name": "Rh-Accesstoken", "value": cookie,
                                                     "domain": ".runninghub.cn"}));
                                }
                                json!({"id": id, "result": {"cookies": list}})
                            }
                            "Runtime.evaluate" => {
                                json!({"id": id, "result": {"result": {"value": local}}})
                            }
                            _ => json!({"id": id, "result": {}}),
                        };
                        let payload = reply.to_string();
                        if rs_vibex::client::write_frame_bytes(
                            &mut reader.get_mut(),
                            rs_vibex::client::WS_OP_TEXT,
                            payload.as_bytes(),
                            false,
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        FakeCdp {
            port,
            calls,
            handle,
        }
    }

    pub async fn calls(&self) -> Vec<String> {
        self.calls.lock().await.clone()
    }
}

fn http_ok(body: &str, ct: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// ---------- 被测服务 ----------

pub struct AppServer {
    pub base: String,
    pub addr: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 起一个 Rust 侧 rs-vibex 实例（随机端口 + 临时统计文件）。
pub async fn start_app(cfg: rs_vibex::config::Config, seams: rs_vibex::Seams, dir: &TestDir) -> (Arc<rs_vibex::Server>, AppServer) {
    let stats_path = dir.join("vibex-stats.json");
    let srv = rs_vibex::Server::new_with(cfg, seams, stats_path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑被测端口");
    let addr = listener.local_addr().unwrap();
    let app = rs_vibex::router(srv.clone());
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (
        srv,
        AppServer {
            base: format!("http://127.0.0.1:{}", addr.port()),
            addr: addr.to_string(),
            handle,
        },
    )
}

/// 合成一份测试配置（与采集器的 `newCollectEnv` 同形）。
pub fn test_cfg(base_url: &str, cfg_path: PathBuf) -> rs_vibex::config::Config {
    let mut cfg = rs_vibex::config::Config::defaults(cfg_path);
    cfg.base_url = base_url.to_string();
    cfg.turn_timeout_sec = 20;
    cfg.ready_timeout_sec = 8;
    cfg.fresh_session = true;
    cfg.connect_mode = "auto".into();
    cfg.tenant_id = "0123456789abcdef0123456789abcdef".into();
    cfg.normalize();
    cfg
}

/// 不跟随重定向的 HTTP 客户端。
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("建 HTTP 客户端")
}

/// 发一条下游请求，返回 (状态码, 我们关心的 6 个响应头, 响应体)。
pub async fn send(
    base: &str,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> (u16, BTreeMap<String, String>, String) {
    let cl = client();
    let url = format!("{base}{path}");
    let m = reqwest::Method::from_bytes(method.as_bytes()).expect("方法");
    let mut req = cl.request(m, &url);
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    if !body.is_empty() {
        req = req
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let resp = req.send().await.expect("发下游请求");
    let status = resp.status().as_u16();
    let mut hdrs = BTreeMap::new();
    for k in [
        "content-type",
        "cache-control",
        "x-accel-buffering",
        "access-control-allow-origin",
        "access-control-allow-methods",
        "access-control-allow-headers",
        "x-content-type-options",
    ] {
        hdrs.insert(
            k.to_string(),
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string(),
        );
    }
    let text = resp.text().await.unwrap_or_default();
    (status, hdrs, text)
}

/// 从假上游记录里取出 `(method, path)` 命中次数表（JSON 形态，便于与黄金比）。
pub fn hits_json(rec: &Rec) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in &rec.hits {
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

/// 合成假凭据（与采集器同款：payload 带 `fake:true`）。
pub fn fake_jwt_at(sub: &str, nick: &str, exp: i64, mobile: &str) -> String {
    let mut payload = format!(r#"{{"sub":{sub:?},"exp":{exp},"fake":true"#);
    if !nick.is_empty() {
        payload.push_str(&format!(r#","nickName":{nick:?}"#));
    }
    if !mobile.is_empty() {
        payload.push_str(&format!(r#","mobile":{mobile:?}"#));
    }
    payload.push('}');
    let header = r#"{"alg":"HS256","typ":"JWT","fake":true}"#;
    format!(
        "{}.{}.FAKESIG",
        b64url(header.as_bytes()),
        b64url(payload.as_bytes())
    )
}

fn b64url(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let x = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((x[0] as u32) << 16) | ((x[1] as u32) << 8) | x[2] as u32;
        let idx = [
            ((n >> 18) & 63) as usize,
            ((n >> 12) & 63) as usize,
            ((n >> 6) & 63) as usize,
            (n & 63) as usize,
        ];
        let take = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for k in idx.iter().take(take) {
            out.push(T[*k] as char);
        }
    }
    out
}

/// 便于断言：把响应头转成 JSON 对象。
pub fn headers_json(h: &BTreeMap<String, String>) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in h {
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

/// 读一个字节体（供将来的大体积用例）。
pub fn body_text(b: &Bytes) -> String {
    String::from_utf8_lossy(b).to_string()
}

/// 让 `Body` 在测试里可用（避免未使用导入告警）。
pub fn _touch_body(_b: Body) {}
