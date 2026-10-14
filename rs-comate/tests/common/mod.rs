// tests/common/mod.rs —— rs-comate 集成测试的共用脚手架。
//
// 三条纪律（与黄金基准的采集口径一一对应）：
// 1. **绝不碰 8792**：上游与 zulu 探活都指向进程内的假服务（对应 Go 采集器的
//    `http.DefaultTransport` 拦截器），假 zulu 是临时目录里的 .cmd，只打印/记日志；
// 2. **绝不碰真实 `~/.comate/cli-login.json`**：登录态一律写在临时目录；
// 3. 每个用例用自己的临时目录与随机端口，测试之间不共享可变状态（Rust 的 `set_var`
//    是进程级的，这里一处都不用）。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

// ---------- 黄金基准加载 ----------

/// 先 `rs-comate/tests/golden/`（入库），缺则回落 `.work/rust-rewrite/`（重新采集时的来源）。
pub fn golden_path(file: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rs-comate 应有父目录")
        .to_path_buf();
    let in_repo = root.join("rs-comate/tests/golden").join(file);
    if in_repo.exists() {
        return in_repo;
    }
    root.join(".work/rust-rewrite").join(file)
}

pub fn load_golden(file: &str) -> Value {
    let path = golden_path(file);
    let raw =
        std::fs::read(&path).unwrap_or_else(|e| panic!("读不到黄金基准 {}: {e}", path.display()));
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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rs-comate-test-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self { path }
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.join(name);
        std::fs::write(&p, content).expect("写临时文件");
        p
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 写一个登录态文件，内容为 `{"license":"<license>"}`（license 为空即 `{"license":""}`）。
pub fn write_license(dir: &TestDir, name: &str, license: &str) -> PathBuf {
    let content = serde_json::json!({ "license": license }).to_string();
    dir.write(name, &content)
}

// ---------- 假 zulu CLI ----------

/// 临时目录里的 `zulu.cmd`：`list-model` 打印 `list.txt` 并把调用次数追加进 `calls.log`；
/// `serve` 把自己的参数追加进 `serve.log` 后立即退出（只用于验证 spawn 的 argv 形态，
/// 永不监听端口——**8792 是本机真 zulu 的**）。
pub struct FakeZulu {
    dir: TestDir,
}

impl FakeZulu {
    pub fn new(tag: &str) -> Self {
        let dir = TestDir::new(tag);
        let cmd = "@echo off\r\n\
                   if \"%1\"==\"list-model\" goto listmodel\r\n\
                   if \"%1\"==\"serve\" goto serve\r\n\
                   exit /b 0\r\n\
                   :listmodel\r\n\
                   >>\"%~dp0calls.log\" echo list-model\r\n\
                   type \"%~dp0list.txt\"\r\n\
                   exit /b 0\r\n\
                   :serve\r\n\
                   >>\"%~dp0serve.log\" echo %*\r\n\
                   exit /b 0\r\n";
        dir.write("zulu.cmd", cmd);
        dir.write("list.txt", "");
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("zulu.cmd")
    }

    /// 设置 `list-model` 的输出（逐字节照抄采集器里的 FAKE_ZULU_LIST）。
    pub fn set_list(&self, list: &str) {
        std::fs::write(self.dir.join("list.txt"), list).expect("写 list.txt");
    }

    pub fn reset_calls(&self) {
        let _ = std::fs::remove_file(self.dir.join("calls.log"));
    }

    /// `list-model` 被调用了几次（0 = 日志文件不存在）。
    pub fn calls(&self) -> usize {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    /// `serve` 子命令被调用时的参数行（空 = 没被调用过）。
    pub fn serve_args(&self) -> String {
        std::fs::read_to_string(self.dir.join("serve.log")).unwrap_or_default()
    }
}

// ---------- 假上游（zulu /api/v1/conversations/init） ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub body: String,
}

#[derive(Clone)]
struct UpstreamState {
    cfg: Arc<Mutex<(String, u16)>>, // (SSE 正文, 状态码)
    hits: Arc<Mutex<Vec<Hit>>>,
}

pub struct FakeUpstream {
    pub base: String,
    handle: tokio::task::JoinHandle<()>,
    state: UpstreamState,
}

impl FakeUpstream {
    pub async fn start() -> Self {
        let state = UpstreamState {
            cfg: Arc::new(Mutex::new((String::new(), 200))),
            hits: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .fallback(upstream_handler)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑假上游端口");
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            handle,
            state,
        }
    }

    pub async fn set_sse(&self, sse: &str) {
        let mut cfg = self.state.cfg.lock().await;
        cfg.0 = sse.to_string();
        cfg.1 = 200;
    }

    /// 上游改回 500 + 纯文本 `boom`（对齐采集器里的 "http-500" 形态）。
    pub async fn set_http_500(&self) {
        self.set_status_with_body(500, "boom").await;
    }

    /// 任意状态码 + 原文响应体（用于 4096 字节截断这类边界）。
    pub async fn set_status_with_body(&self, status: u16, body: &str) {
        let mut cfg = self.state.cfg.lock().await;
        cfg.0 = body.to_string();
        cfg.1 = status;
    }

    pub async fn take_hits(&self) -> Vec<Hit> {
        let mut hits = self.state.hits.lock().await;
        std::mem::take(&mut *hits)
    }

    pub async fn hits(&self) -> Vec<Hit> {
        self.state.hits.lock().await.clone()
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn upstream_handler(State(st): State<UpstreamState>, req: Request) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    st.hits.lock().await.push(Hit {
        method,
        path: path.clone(),
        body: String::from_utf8_lossy(&body).to_string(),
    });
    let (sse, status) = {
        let cfg = st.cfg.lock().await;
        (cfg.0.clone(), cfg.1)
    };
    if path != "/api/v1/conversations/init" {
        // zulu 探活：Go 侧只看状态码
        return (StatusCode::OK, "ok").into_response();
    }
    if status != 200 {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [("content-type", "text/plain")],
            sse,
        )
            .into_response();
    }
    (StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response()
}

// ---------- 假 zulu 探活端点 ----------

#[derive(Clone)]
struct HealthState {
    probes: Arc<AtomicUsize>,
    /// 前 N 次探活回 503（模拟「还没起来」），之后回 200。
    fail_first: Arc<Mutex<usize>>,
}

pub struct FakeHealth {
    pub url: String,
    handle: tokio::task::JoinHandle<()>,
    probes: Arc<AtomicUsize>,
}

impl FakeHealth {
    pub async fn start(fail_first: usize) -> Self {
        let probes = Arc::new(AtomicUsize::new(0));
        let st = HealthState {
            probes: probes.clone(),
            fail_first: Arc::new(Mutex::new(fail_first)),
        };
        let app = Router::new().fallback(health_handler).with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑假健康端点");
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://{addr}/health"),
            handle,
            probes,
        }
    }

    pub fn probes(&self) -> usize {
        self.probes.load(Ordering::Relaxed)
    }
}

impl Drop for FakeHealth {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn health_handler(State(st): State<HealthState>, _req: Request) -> Response {
    st.probes.fetch_add(1, Ordering::Relaxed);
    let mut left = st.fail_first.lock().await;
    if *left > 0 {
        *left -= 1;
        return (StatusCode::SERVICE_UNAVAILABLE, "not yet").into_response();
    }
    (StatusCode::OK, "ok").into_response()
}

// ---------- 被测服务 ----------

pub struct AppServer {
    pub base: String,
    pub addr: String,
    handle: tokio::task::JoinHandle<()>,
}

impl AppServer {
    pub async fn start(state: Arc<rs_comate::AppState>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑被测端口");
        let addr = listener.local_addr().unwrap();
        let app = rs_comate::router(state);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            addr: addr.to_string(),
            handle,
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 不跟随重定向的 HTTP 客户端（重定向形状要自己看）。
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("建 HTTP 客户端")
}

// ---------- 原始 HTTP/1.1 客户端（路径清洗用例要用） ----------

/// 逐字节写请求行：`reqwest`/`url` 会按 WHATWG 规则把 `.`、`..` 段与重复斜杠归一化，
/// 那样就测不到 Go ServeMux 的路径清洗了——这几条必须自己拼请求。
pub async fn raw_request(
    addr: &str,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("连被测端口");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.expect("发请求");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("读响应");

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("响应里没有空行");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let body = buf[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("状态行解析");
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    (status, headers, body)
}

// ---------- 易变字段归一化（与采集器 normalizeChunks 同一套规则） ----------

/// `"created":<数字>` → `"created":<CREATED>`；`"id":"chatcmpl-comate-<数字>"` → `"id":"<ID>"`。
pub fn normalize_volatile(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if s[i..].starts_with("\"created\":") {
            let start = i + "\"created\":".len();
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                out.push_str("\"created\":<CREATED>");
                i = j;
                continue;
            }
        }
        if s[i..].starts_with("\"id\":\"chatcmpl-comate-") {
            let start = i + "\"id\":\"chatcmpl-comate-".len();
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
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

// ---------- 上游 SSE 的「重复段规格」重建 ----------

/// 从黄金 ctx 里取上游 SSE 原文：小体量直接读 `sse`，超大按
/// `sse_prefix + sse_repeat × sse_repeat_count + sse_suffix` 机械重建（与采集器同规格）。
pub fn sse_from_ctx(ctx: &Value) -> Option<String> {
    if let Some(s) = ctx.get("sse").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    let prefix = ctx.get("sse_prefix")?.as_str()?;
    let repeat = ctx.get("sse_repeat")?.as_str()?;
    let count = ctx.get("sse_repeat_count")?.as_u64()?;
    let suffix = ctx.get("sse_suffix").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::with_capacity(prefix.len() + suffix.len() + repeat.len() * count as usize);
    out.push_str(prefix);
    for _ in 0..count {
        out.push_str(repeat);
    }
    out.push_str(suffix);
    Some(out)
}

// ---------- 响应头比对 ----------

/// 与黄金记录的响应头对齐：
/// * `date` 忽略（时钟）；
/// * `content-length`：**非 HEAD** 时只校验存在性与自洽（= 本次响应体长度）——黄金里那个值
///   内嵌了随机的 `chatcmpl-comate-<rand>` 位数，逐字节比会假红；**HEAD** 没有响应体，
///   长度就是「GET 会回多少字节」，与黄金值逐字比（Go 的 HEAD 头是稳定的）；
/// * 其余逐条精确比；黄金没有 `content-length` 时也要求本次没有（SSE 与 HEAD 307 是这种）。
pub fn assert_headers_like_go(
    case: &str,
    golden: &Value,
    actual: &HeaderMap,
    body_len: usize,
    head: bool,
) {
    let golden_map = golden
        .as_object()
        .unwrap_or_else(|| panic!("[{case}] 黄金响应头不是对象"));
    let actual_len = actual
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    for (k, v) in golden_map {
        let key = k.to_ascii_lowercase();
        if key == "date" {
            continue;
        }
        if key == "content-length" {
            let got = actual_len.unwrap_or_else(|| panic!("[{case}] 缺少 content-length"));
            let want: usize = v.as_str().unwrap_or("0").parse().unwrap_or_default();
            if head {
                assert_eq!(got, want, "[{case}] HEAD 的 content-length 与 Go 不一致");
            } else {
                assert_eq!(
                    got, body_len,
                    "[{case}] content-length 与本次响应体长度不符（黄金记的是它那次的随机 id 长度）"
                );
            }
            continue;
        }
        let want = v.as_str().unwrap_or_default();
        let got = actual
            .get(&key)
            .unwrap_or_else(|| panic!("[{case}] 缺响应头 {key}（黄金={want:?}）"))
            .to_str()
            .unwrap_or("");
        assert_eq!(got, want, "[{case}] 响应头 {key} 与 Go 不一致");
    }
    if !golden_map.contains_key("content-length") {
        // 已登记的传输层差异（见 rs-comate/README.md 差异清单第 1 条）：HEAD 请求上
        // Go 可以不写 content-length（它没写体、也不算长度），hyper 一定补 `content-length: 0`。
        // 这里**显式**把这一种形态放行（且只放行它），其他任何多出来的长度都判红。
        match actual.get("content-length") {
            None => {}
            Some(v) if head && v == "0" => {}
            Some(v) => panic!("[{case}] Go 没给 content-length，本次给了 {v:?}（未登记的差异）"),
        }
    }
}

/// 把原始 HTTP 客户端的响应头列表转成 `HeaderMap`（比对用）。
pub fn headers_from_raw(raw: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in raw {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(v),
        ) {
            map.insert(name, value);
        }
    }
    map
}

/// 把字节体读成文本（黄金基准里的 body 都是 UTF-8 文本）。
pub fn body_text(b: &Bytes) -> String {
    String::from_utf8_lossy(b).to_string()
}
