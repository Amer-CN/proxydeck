//! 集成测试脚手架：黄金基准加载、假上游、被测算例的真 HTTP 服务、原始 HTTP 客户端与归一化。
//!
//! 与 `rs-qoder/tests/common/mod.rs` 同族（P5 这一轮带的复制件），按 codebuddy 的合同裁剪：
//! * 上游是**进程内假上游**（axum，按黄金里的 `upstream_spec` 回放），不出网；
//! * 被测算例用 crate 自己的 `router()` 起在一个临时端口上（`127.0.0.1:0`），
//!   与 Go 侧采集时一样走真实 TCP（不是函数直调）；
//! * 凭据用 `--cred-stub` 同款的 `StubCredential`（值全部来自黄金的 `cred_seam`）。

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use axum::Router;
use serde_json::Value;

pub const GOLDEN_FILE: &str = "golden-codebuddy-go.json";

/// 黄金基准路径：**先本 crate 的 `tests/golden/`，缺则回落 `.work/rust-rewrite/`**（再生成链）。
pub fn golden_path(file: &str) -> PathBuf {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(file);
    if repo.exists() {
        return repo;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate 必有上级目录")
        .join(".work")
        .join("rust-rewrite")
        .join(file)
}

/// 载入黄金基准（缺文件时给出可执行的再生成提示，不静默跳过）。
pub fn load_golden() -> Value {
    let p = golden_path(GOLDEN_FILE);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "读不到黄金基准 {}: {e}\n再生成链见 rs-codebuddy/tests/golden/README.md",
            p.display()
        )
    });
    serde_json::from_str(&text).expect("黄金基准必须是合法 JSON")
}

/// 临时目录（Drop 时清理）。
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub fn new(tag: &str) -> TestDir {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("rs-codebuddy-{tag}-{stamp}"));
        std::fs::create_dir_all(&p).expect("建临时目录");
        TestDir { path: p }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 上游看到的一次请求（Rust 侧）。
#[derive(Debug, Clone)]
pub struct CapturedHop {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
    /// 头名一律小写（HTTP 头名大小写不敏感；Go 采集的是规范化 `X-User-Id`，见 README 差异）。
    pub headers: Vec<(String, String)>,
}

impl CapturedHop {
    pub fn header(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == n)
            .map(|(_, v)| v.as_str())
    }

    pub fn model(&self) -> String {
        serde_json::from_slice::<Value>(&self.body)
            .ok()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()))
            .unwrap_or_default()
    }
}

/// 假上游的应答。
pub struct FakeResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 假上游：按黄金 `upstream_spec` 回放，并记录每一次请求。
pub struct FakeUpstream {
    pub base: String,
    hits: Arc<Mutex<Vec<CapturedHop>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    pub async fn start(spec: &Value) -> FakeUpstream {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sink = hits.clone();
        let state = Arc::new(Mutex::new(spec.clone()));
        let app = Router::new().fallback(axum::routing::any(
            move |req: Request| {
                let sink = sink.clone();
                let state = state.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let bytes = axum::body::to_bytes(body, 256 * 1024 * 1024)
                        .await
                        .unwrap_or_default();
                    let hop = CapturedHop {
                        method: parts.method.to_string(),
                        path: parts.uri.path().to_string(),
                        body: bytes.to_vec(),
                        headers: parts
                            .headers
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.as_str().to_ascii_lowercase(),
                                    String::from_utf8_lossy(v.as_bytes()).to_string(),
                                )
                            })
                            .collect(),
                    };
                    sink.lock().unwrap().push(hop.clone());
                    let resp = respond(&state, &hop);
                    let mut b = Response::builder().status(resp.status);
                    for (k, v) in &resp.headers {
                        b = b.header(k.as_str(), v.as_str());
                    }
                    b.body(Body::from(resp.body)).unwrap()
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("假上游起监听");
        let addr = listener.local_addr().expect("取假上游地址");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeUpstream {
            base: format!("http://{addr}"),
            hits,
            handle,
        }
    }

    pub fn hits(&self) -> Vec<CapturedHop> {
        self.hits.lock().unwrap().clone()
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 按黄金的 `upstream_spec` 回放（与 Go 采集器 `cbRespSpec.responder()` 同语义）。
///
/// P5b 追加两种形态：
/// * `per_user`（也在 `rules[].per_user` 里）：按 `X-User-Id` 分派应答**序列**
///   （每个 uid 独立计数，用尽后重复最后一条）——池换号场景靠它区分两个号的应答；
/// * `rules`：首条命中的规则生效（`path` 精确 / `path_prefix` 前缀），支持同一场景里
///   池路径（chat）与额度路径（quota）各走各的应答表。
///
/// 计数器按「路径 + uid」分桶，与 Go 采集器 `p5bUpstream.perUser` 的 key 完全一致。
fn respond(state: &Arc<Mutex<Value>>, hop: &CapturedHop) -> FakeResp {
    let spec = state.lock().unwrap().clone();
    let kind = spec.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    // P5b：规则表（kind 为空时也可能带 rules）
    if let Some(rules) = spec.get("rules").and_then(|v| v.as_array()) {
        for rule in rules {
            let want_path = rule.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let want_prefix = rule
                .get("path_prefix")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !want_path.is_empty() && want_path != hop.path {
                continue;
            }
            if !want_prefix.is_empty() && !hop.path.starts_with(want_prefix) {
                continue;
            }
            if let Some(table) = rule.get("per_user").and_then(|v| v.as_object()) {
                return per_user_respond(state, table, hop);
            }
            return spec_resp(rule);
        }
    }
    if let Some(table) = spec.get("per_user").and_then(|v| v.as_object()) {
        return per_user_respond(state, table, hop);
    }
    // 没有 `kind` 但有 `status`/`body` → 当 fixed 处理（P5b 采集器用的是规则表形态，
    // 顶层 spec 就是「一条固定应答」，不带 kind 字段）。
    if kind.is_empty() && (spec.get("status").is_some() || spec.get("body").is_some()) {
        return spec_resp(&spec);
    }
    match kind {
        "fixed" => spec_resp(&spec),
        "probe" => {
            let model = hop.model();
            let ok = spec
                .get("ok_models")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().any(|m| m.as_str() == Some(model.as_str())))
                .unwrap_or(false);
            if ok {
                FakeResp {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n".to_vec(),
                }
            } else {
                FakeResp {
                    status: spec
                        .get("fail_status")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(400) as u16,
                    headers: vec_of_headers(spec.get("fail_headers")),
                    body: spec
                        .get("fail_body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .as_bytes()
                        .to_vec(),
                }
            }
        }
        "sequence" => {
            let items = spec.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let idx = {
                let mut s = state.lock().unwrap();
                let used = s.get("_used").and_then(|v| v.as_u64()).unwrap_or(0);
                let next = (used + 1).min(items.len().max(1) as u64);
                s["_used"] = Value::Number(next.into());
                (next - 1) as usize
            };
            let item = items.get(idx).cloned().unwrap_or(Value::Null);
            spec_resp(&item)
        }
        other => FakeResp {
            status: 500,
            headers: vec![],
            body: format!("unknown spec kind {other}").into_bytes(),
        },
    }
}

/// 按 `X-User-Id` 分派应答序列；未登记的 uid 回 500（与 Go 采集器同款：让错配立刻可见）。
fn per_user_respond(
    state: &Arc<Mutex<Value>>,
    table: &serde_json::Map<String, Value>,
    hop: &CapturedHop,
) -> FakeResp {
    let uid = hop.header("x-user-id").unwrap_or("").to_string();
    let list = table.get(&uid).and_then(|v| v.as_array()).cloned();
    let Some(list) = list else {
        return FakeResp {
            status: 500,
            headers: vec![],
            body: format!("unexpected uid {uid}").into_bytes(),
        };
    };
    let key = format!("{}\u{0}{}", hop.path, uid);
    let idx = {
        let mut s = state.lock().unwrap();
        let used = s
            .get("_per_user")
            .and_then(|v| v.get(&key))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let next = used + 1;
        if s.get("_per_user").is_none() {
            s["_per_user"] = Value::Object(serde_json::Map::new());
        }
        s["_per_user"][&key] = Value::Number(next.into());
        (next - 1) as usize
    };
    let i = idx.min(list.len().saturating_sub(1));
    spec_resp(&list[i])
}

fn spec_resp(spec: &Value) -> FakeResp {
    FakeResp {
        status: spec.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16,
        headers: vec_of_headers(spec.get("headers")),
        body: spec
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .as_bytes()
            .to_vec(),
    }
}

fn vec_of_headers(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 被测算例（crate 自己的 router）起在临时端口。
pub struct AppServer {
    pub base: String,
    handle: tokio::task::JoinHandle<()>,
}

impl AppServer {
    pub async fn start(state: Arc<rs_codebuddy::AppState>) -> AppServer {
        let app = rs_codebuddy::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("被测算例起监听");
        let addr = listener.local_addr().expect("取被测算例地址");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        AppServer {
            base: format!("http://{addr}"),
            handle,
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 一次下游请求的响应（原始形态：**不跟随重定向**，头名小写）。
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == n)
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("建测试客户端")
}

/// 发一次请求（`body=None` 表示不带请求体；`Some(b"")` 表示带**空**请求体）。
pub async fn send(base: &str, method: &str, path: &str, body: Option<&[u8]>) -> Resp {
    let m = reqwest::Method::from_bytes(method.as_bytes()).expect("方法名合法");
    let mut req = client().request(m, format!("{base}{path}"));
    if let Some(b) = body {
        req = req.body(b.to_vec());
    }
    let resp = req.send().await.expect("请求发不出去");
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(v.as_bytes()).to_string(),
            )
        })
        .collect();
    let body = resp.bytes().await.expect("读响应体").to_vec();
    Resp {
        status,
        headers,
        body,
    }
}

/// 响应头的比对清单（与 Go 采集器 `cbPickRespHeaders` 的清单一致）。
pub const COMPARED_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "allow",
    "x-content-type-options",
    "cache-control",
    "x-accel-buffering",
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
];

/// `/health` 的 `uptimeSec` 是易变量：两侧都归一成 0 再比。
pub fn normalize_uptime(s: &str) -> String {
    let re = |s: &str| {
        let mut out = String::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if s[i..].starts_with("\"uptimeSec\":") {
                out.push_str("\"uptimeSec\":0");
                i += "\"uptimeSec\":".len();
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'-') {
                    i += 1;
                }
                continue;
            }
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    };
    re(s)
}

/// 上游 URL 归一成 path（黄金记的是 Go 的区域 base URL，测试里是本地假上游）。
pub fn url_path(u: &str) -> String {
    match u.find("://") {
        Some(i) => match u[i + 3..].find('/') {
            Some(j) => u[i + 3 + j..].to_string(),
            None => "/".to_string(),
        },
        None => u.to_string(),
    }
}

/// 诊断文件名归一（时间戳是纳秒，两侧必然不同）：`diag-11133-<stamp>.json` → `diag-11133-<STAMP>.json`。
pub fn normalize_diag_name(name: &str) -> String {
    let parts: Vec<&str> = name.splitn(3, '-').collect();
    if parts.len() == 3 && parts[0] == "diag" && parts[1] == "11133" {
        return "diag-11133-<STAMP>.json".to_string();
    }
    name.to_string()
}

// ---------- P5b：进程级环境变量（线程安全） ----------

/// 环境变量的**进程级**互斥。
///
/// 为什么需要：`CODEBUDDY_AUTH_DIR` / `PROXYDECK_DATA_DIR` / `WORKBUDDY_AT_REST_SECRET`
/// 是进程级环境变量，而 Rust 的测试默认**多线程并行**——`datadir.rs` / `credential.rs`
/// 的门禁都读环境变量。凡是要改这些变量的用例一律先取这把锁（登记在 README 差异 14：
/// 生产路径自己不会并发改环境变量，这条只为测试确定性）。
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 在锁保护下设置一批环境变量。
///
/// **故意不提供"恢复"**：`std::env::set_var` 是进程级全局状态，而在 async 测试里
/// 在 `Drop`（尤其 panic 展开路径）中回写会把**运行时拆解**与全局环境锁搅在一起
/// （实测把 libtest 主线程卡死）。这里的做法是：调用方自己在门禁末尾把变量改成
/// "指向空的临时目录"（见 `set_empty_env`），Drop 只释放锁——语义等价（后续用例
/// 看不到上一个用例的目录），且不会在展开期动全局状态。
pub struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    keys: Vec<String>,
}

impl EnvGuard {
    /// 取锁并设置一批环境变量（`None` = 删除该变量）。
    pub fn set(pairs: &[(&str, Option<&str>)]) -> EnvGuard {
        let lock = env_lock();
        let mut keys = Vec::new();
        for (k, v) in pairs {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
            keys.push(k.to_string());
        }
        EnvGuard { _lock: lock, keys }
    }

    /// 设置一批环境变量（已持锁的调用方用；不重复取锁）。
    ///
    /// 与 `set` 的差别只有「不取锁」——门禁在启动时取一次锁（`EnvGuard::set`），
    /// 之后每个场景用本方法换目录。
    pub fn set_locked(&self, pairs: &[(&str, Option<&str>)]) {
        for (k, v) in pairs {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    /// 把本 guard 设过的变量改成"指向空目录"（门禁收尾调用；变量仍在，避免删变量引入
    /// 新的分支差异，但目录为空 → 池/凭据扫描都是零命中）。
    pub fn point_to_empty(&self, dir: &std::path::Path) {
        let d = dir.to_string_lossy().to_string();
        for k in &self.keys {
            std::env::set_var(k, &d);
        }
    }
}

/// 把黄金场景里的 `auth_files` 重建到临时目录（合成 `.info`，含 `$wbEncrypted`），
/// 并把每个文件的 mtime 落回采集时的时刻——池内排序按 mtime 新→旧，不回填就会挑中别的号。
pub fn write_auth_files(dir: &std::path::Path, files: &Value) {
    if let Some(arr) = files.as_array() {
        for f in arr {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let body = f.get("body").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let p = dir.join(name);
            std::fs::write(&p, body.as_bytes()).expect("写合成 auth 文件");
            if let Some(ms) = f.get("mtime_unix_ms").and_then(|v| v.as_i64()) {
                if ms > 0 {
                    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64);
                    if let Ok(fh) = std::fs::File::options().write(true).open(&p) {
                        let _ = fh.set_modified(t);
                    }
                }
            }
        }
    }
}

/// 门禁串行锁（环境变量是进程级的；`golden_codebuddy.rs` 与 `p5b_golden.rs` 共用）。
pub fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// `last_used` 取值归一：**非空**一律换成 `__LAST_USED__`，**空串保留**。
///
/// 为什么只归这一处（README 差异 17）：它是墙钟时间，两侧运行时刻必然不同；
/// 而「从未使用 = 空串」是响应合同的一部分，归一时必须留住这条区分。
fn normalize_last_used(s: &str) -> String {
    let key = "\"last_used\":\"";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(key) {
        let start = i + key.len();
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        match rest.find('"') {
            Some(end) => {
                if end == 0 {
                    out.push('"');
                } else {
                    out.push_str("__LAST_USED__\"");
                }
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str(rest);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `inflight` 取值的归一：**只归一到位标记**，保留「0 = 空闲」的语义。
///
/// 为什么（README 差异 18）：`inflight` 是**进程内存态**的进行中计数，同一个快照
/// 在两侧的**采样时刻**不同——Go 采集侧的 `/v1/accounts` 是**在请求处理中**发起的
/// （被挑中的号此刻 inflight=1），Rust 回放侧则是请求结束后才发快照（defer 已 Dec、
/// inflight=0）。这是**采样时刻**差异，不是产品行为差异（Rust 的 Inc/Dec 与 Go 同构：
/// 选中即 Inc、请求结束 defer Dec）。门禁因此只锁 `use_count` 与其余字段，`inflight`
/// 归一成 `__INFLIGHT__`；另有一条独立单测守 Inc/Dec 的钳位与配对
/// （`pool.rs::tests::inflight_inc_dec_pairs_and_clamps`）。
fn normalize_inflight(s: &str) -> String {
    let key = "\"inflight\":";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(key) {
        out.push_str(&rest[..i + key.len()]);
        rest = &rest[i + key.len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        out.push_str("__INFLIGHT__");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// 起一个**生产装配**的回放环境（真凭据 + 真池 + 真 failover），对齐 Go 采集器的装配方式。
///
/// 为什么两个门禁文件共用它：P5a 的合同场景与 P5b 的池场景现在都用生产装配采收
/// （零值 Server 会让「缺省 reasoning_effort 注入」这类生产行为不出现），
/// 回放侧也必须走同一条装配路径，否则比的是两套东西。
pub struct RealReplay<'a> {
    /// 门禁级的进程环境锁（整个门禁只取一次；**不在 guard 的 Drop 里回写变量**）。
    pub env: &'a EnvGuard,
    /// 「收尾指向」用的空目录（单个场景结束时把环境变量指到这里）。
    pub empty: TestDir,
    pub auth: TestDir,
    pub data: TestDir,
    pub app: AppServer,
    pub upstream: FakeUpstream,
}

impl RealReplay<'_> {
    /// 门禁收尾：把环境变量指向空目录（下一个场景会重新设置）。
    pub fn finish(self) {
        self.env.point_to_empty(self.empty.path());
    }

    /// 把采集机临时目录占位符还原成本机临时目录（与采集侧 `localize` 逆运算）。
    ///
    /// 为什么需要：`/health` 的 `auth_search_dirs` / `credential.auth_file`、
    /// `credentials-missing` 的报错文案都带**绝对路径**；黄金里是 `__AUTHDIR__` /
    /// `__DATA__`，Rust 侧要换成本次运行的临时目录再逐字节比。
    pub fn localize(&self, s: &str) -> String {
        s.replace(
            "__AUTHDIR__",
            &self.auth.path().to_string_lossy(),
        )
        .replace("__DATA__", &self.data.path().to_string_lossy())
    }

    /// 响应体归一：先还原占位符，再把 `\\` 折成 `\`。
    ///
    /// 为什么需要折叠：Windows 绝对路径在 JSON 体里是**转义态**，而 Go 采集侧的
    /// `localize` 跑在 `json.Marshal` **之前**——于是落盘文本里 `__AUTHDIR__` 之后的
    /// 反斜杠层数与运行时不同（同一份语义、不同层数）。两侧都折一次后比的是路径本身
    /// 而不是转义层数：**这是本门禁唯一的形态归一**，只动绝对路径的转义，
    /// 不动任何业务字节（登记在 README 差异 15）。
    pub fn normalize_body(&self, s: &str) -> String {
        // `last_used` 是墙钟时间、`inflight` 是采样时刻相关的内存计数，
        // 两处归一成固定标记；「从未使用 = 空串」等区分仍保留（README 差异 17/18）。
        normalize_inflight(&normalize_last_used(
            &self.localize(s).replace("\\\\", "\\"),
        ))
    }

    /// 「标准库 JSON 语法错」的归一（README 差异 16）。
    ///
    /// Go 用 `encoding/json.Unmarshal`，Rust 用 `serde_json`：同一份坏输入的**措辞不同**
    /// （`unexpected end of JSON input` vs `EOF while parsing a value at line 1 column 1`），
    /// 行/列信息也只有后者有。**只归一这一类**，而且只归一到「库的措辞」这一层——
    /// 前缀（`bad json: ` / `auth 文件格式错误: `）、状态码、包裹结构仍逐字节比对。
    ///
    /// 归属：本 crate 的 `godecode` 已逐字复刻了 Go scanner 的**大部分**文案（
    /// `invalid character 'x' …` 那一族），但「输入提前结束」这一族 Go 与 serde 用词不同。
    /// 改这两句话没有意义（都是标准库的话），故登记为已知差异、不当门禁强度。
    pub fn normalize_json_lib_error(&self, s: &str) -> String {
        const KEYS: &[&str] = &["auth 文件格式错误: ", "bad json: "];
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        loop {
            // 找最靠前的一个键
            let mut best: Option<(usize, &str)> = None;
            for k in KEYS {
                if let Some(i) = rest.find(k) {
                    if best.map(|(bi, _)| i < bi).unwrap_or(true) {
                        best = Some((i, k));
                    }
                }
            }
            let Some((i, k)) = best else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..i + k.len()]);
            out.push_str("__JSON_LIB_MSG__");
            rest = &rest[i + k.len()..];
            // 该段到字符串结尾（错误文案里不会再出现别的 JSON 结构）或到 `"` 为止
            match rest.find('"') {
                Some(j) => rest = &rest[j..],
                None => rest = "",
            }
        }
        out
    }
}

pub async fn start_real_replay<'a>(
    name: &str,
    region: rs_codebuddy::region::Region,
    scenario: &Value,
    base_url_override: Option<String>,
    env: &'a EnvGuard,
) -> RealReplay<'a> {
    let auth = TestDir::new(&format!("auth-{name}"));
    let data = TestDir::new(&format!("data-{name}"));
    let home = TestDir::new(&format!("home-{name}"));
    let empty = TestDir::new(&format!("empty-{name}"));
    write_auth_files(auth.path(), scenario.get("auth_files").unwrap_or(&Value::Null));
    let auth_s = auth.path().to_string_lossy().to_string();
    let data_s = data.path().to_string_lossy().to_string();
    let home_s = home.path().to_string_lossy().to_string();
    // 环境变量必须在**建服务之前**设好：凭据层/池/数据目录全在装配期读它们。
    // `USERPROFILE` 指向临时目录 + 清空 LOCALAPPDATA/XDG_DATA_HOME：让 `find_auth_dirs`
    // 只认 `CODEBUDDY_AUTH_DIR`，绝不碰本机真实登录态。
    env.set_locked(&[
        ("CODEBUDDY_AUTH_DIR", Some(auth_s.as_str())),
        ("PROXYDECK_DATA_DIR", Some(data_s.as_str())),
        ("CODEBUDDY_POOL_DIR", Some(data_s.as_str())),
        ("WORKBUDDY_AT_REST_SECRET", Some("c2VjcmV0LWdvbGRlbi1zZWNyZXQ=")),
        ("USERPROFILE", Some(home_s.as_str())),
        ("LOCALAPPDATA", Some("")),
        ("XDG_DATA_HOME", Some("")),
    ]);
    rs_codebuddy::atrest::reset_key_cache();
    // 「上限 10 份现场」场景：Go 采集时先预置了 10 份现场文件，这里按黄金原样预置同一批。
    if scenario
        .get("diag_preseed_from_files")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        for f in scenario
            .get("diag_files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let n = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let b = f.get("body").and_then(|v| v.as_str()).unwrap_or("");
            std::fs::write(data.path().join(n), b.as_bytes()).expect("预置现场文件");
        }
    }
    let upstream = FakeUpstream::start(scenario.get("upstream_spec").unwrap_or(&Value::Null)).await;
    let base = base_url_override.unwrap_or_else(|| upstream.base.clone());
    // `failover_seed`：装配**之前**预置 `codebuddy-failover.json`（采集侧的场景里，
    // 「档位来自配置」是这么做的：先写磁盘，再让 `loadFailover` 读）。
    if let Some(seed) = scenario.get("failover_seed").and_then(|v| v.as_str()) {
        let suffix = if region == rs_codebuddy::region::Region::Intl {
            "-intl"
        } else {
            ""
        };
        std::fs::write(
            data.path().join(format!("codebuddy-failover{suffix}.json")),
            seed.as_bytes(),
        )
        .expect("预置 failover 配置");
    }
    let (state, _cred) = rs_codebuddy::AppState::production_with_data_dir(
        region,
        data.path().to_path_buf(),
        data_s.clone(),
        Some(base),
    )
    .unwrap_or_else(|e| panic!("[{name}] 生产装配应成功: {e}"));
    // 装配完成后再按 `auth_files_after` 改写 auth 目录（采集侧就是这么做的：
    // 先把服务建起来，再破坏/改动登录文件，故坏文件只影响后续读取路径）。
    if let Some(after) = scenario.get("auth_files_after") {
        write_auth_files(auth.path(), after);
    }
    let app = AppServer::start(std::sync::Arc::new(state)).await;
    RealReplay {
        env,
        empty,
        auth,
        data,
        app,
        upstream,
    }
}

/// 两侧共用的确定性排序键（与采集侧 `p5bSortHits` / 采集器的 body 排序逐条同义）。
pub fn hop_sort_key(h: &CapturedHop) -> (String, String) {
    let body = String::from_utf8_lossy(&h.body).to_string();
    (body.chars().take(200).collect(), h.path.clone())
}
