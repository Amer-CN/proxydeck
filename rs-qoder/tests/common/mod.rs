// tests/common/mod.rs —— rs-qoder 集成测试的共用脚手架。
//
// 三条纪律（与黄金基准的采集口径一一对应）：
// 1. **绝不碰 8785**：被测服务一律 `tokio::net::TcpListener::bind("127.0.0.1:0")` 现取端口；
// 2. **绝不碰真实登录态 / 真实账本 / 真 worker**：登录态读 `tests/golden/auth-fixture/`
//    里的**合成** DPAPI 夹具，账本写临时目录，worker 用 `tests/golden/fake-worker.js`；
// 3. 每个用例用自己的临时目录与随机端口，测试之间不共享可变状态（不用 `set_var`）。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, StatusCode};
use axum::Router;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------- 黄金基准加载 ----------

/// 先 `rs-qoder/tests/golden/`（入库），缺则回落 `.work/rust-rewrite/`（重新采集时的来源）。
pub fn golden_path(file: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rs-qoder 应有父目录")
        .to_path_buf();
    let in_repo = root.join("rs-qoder/tests/golden").join(file);
    if in_repo.exists() {
        return in_repo;
    }
    root.join(".work/rust-rewrite").join(file)
}

pub fn load_golden() -> Value {
    let path = golden_path("golden-qoder-go.json");
    let raw =
        std::fs::read(&path).unwrap_or_else(|e| panic!("读不到黄金基准 {}: {e}", path.display()));
    serde_json::from_slice(&raw)
        .unwrap_or_else(|e| panic!("黄金基准不是合法 JSON {}: {e}", path.display()))
}

/// 合成登录态夹具目录（DPAPI 是**当前用户作用域**，换机解不开）。
pub fn auth_fixture_dir() -> PathBuf {
    golden_path("auth-fixture")
}

// ---------- 采集机字面量（黄金里内嵌的路径） ----------

/// 本机仓库根（`rs-qoder/` 的上级目录）。
pub fn repo_root() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rs-qoder 应有父目录")
        .to_string_lossy()
        .to_string()
}

/// 黄金 `meta.machine_paths` 记下的**采集机**路径字面量（默认 cwd 是采集机的临时目录、
/// 有一条场景用采集机的仓库根当 `QODER_CWD`）。门禁把期望值里的它们换成**本机**等价物
/// 再比——换一台机器跑也不假红；换机不重采时 README 的「采集机字面量」一节有说明。
pub struct MachinePaths {
    pub temp_dir: String,
    pub repo_root: String,
}

impl MachinePaths {
    pub fn from_golden(g: &Value) -> Self {
        let mp = &g["meta"]["machine_paths"];
        Self {
            temp_dir: mp["temp_dir"].as_str().unwrap_or("").to_string(),
            repo_root: mp["repo_root"].as_str().unwrap_or("").to_string(),
        }
    }

    /// 采集机路径 → 本机等价物。只做**整串相等**替换（不做子串替换，免得误伤提示词文本）。
    pub fn local(&self, s: &str) -> String {
        if !self.temp_dir.is_empty() && s == self.temp_dir {
            return trim_trailing_sep(std::env::temp_dir().to_string_lossy().into_owned());
        }
        if !self.repo_root.is_empty() && s == self.repo_root {
            return trim_trailing_sep(repo_root());
        }
        s.to_string()
    }
}

/// 去掉结尾的分隔符（不动 `C:\` 这类根路径）。原因：Windows 上 Rust 的 `env::temp_dir()`
/// 走 `GetTempPath2W`、**返回值带一个结尾 `\`**，而 Go 的 `os.TempDir()` 与 node 的
/// `process.cwd()` 都不带——门禁比的是后者记录下来的观测值。
fn trim_trailing_sep(mut s: String) -> String {
    while s.len() > 3 && (s.ends_with('\\') || s.ends_with('/')) {
        s.pop();
    }
    s
}

/// 夹具能不能解开。解不开时的处理**必须显式**（打 SKIP 并计入汇总），不许静默跳过。
pub fn auth_fixture_usable() -> bool {
    rs_qoder::auth::load_auth(&auth_fixture_dir()).is_ok()
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
            "rs-qoder-test-{tag}-{}-{nanos}-{seq}",
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

// ---------- 假 worker（与 Go 采集器同一个文件、同一份源码） ----------

/// 把入库的 `fake-worker.js` 拷进临时目录，并准备 `scenario.json` / `worker-log.jsonl`。
pub struct FakeWorker {
    pub dir: TestDir,
}

impl FakeWorker {
    pub fn new(tag: &str) -> Self {
        let dir = TestDir::new(tag);
        let src = golden_path("fake-worker.js");
        let body = std::fs::read(&src)
            .unwrap_or_else(|e| panic!("读不到假 worker {}: {e}", src.display()));
        std::fs::write(dir.join("fake-worker.js"), body).expect("拷假 worker");
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("fake-worker.js")
    }

    pub fn set_scenario(&self, mode: &str) {
        std::fs::write(
            self.dir.join("scenario.json"),
            format!("{{\"mode\":\"{mode}\"}}"),
        )
        .expect("写 scenario.json");
        let _ = std::fs::remove_file(self.dir.join("worker-log.jsonl"));
    }

    /// 读回假 worker 的记录，按与采集器**同一套规则**归一化易变字段。
    pub fn log(&self) -> Vec<Value> {
        let p = self.dir.join("worker-log.jsonl");
        let Ok(raw) = std::fs::read_to_string(&p) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let mut v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("假 worker 日志坏了: {e} ({line})"));
            if let Some(argv) = v.get_mut("argv").and_then(|a| a.as_array_mut()) {
                if !argv.is_empty() {
                    argv[0] = Value::String("<WORKER>".to_string());
                }
            }
            if v.get("env_payload_path").is_some() {
                v["env_payload_path"] = Value::String("<PAYLOAD>".to_string());
            }
            out.push(v);
        }
        out
    }
}

// ---------- 假额度接口（/health?refresh=1 时 Rust 唯一会出网的地方） ----------

/// 进程内的假 `openapi.qoder.com.cn`：按设定好的状态码+原文应答，并记下被调次数与请求头。
/// **绝不碰真网络**（测试里 `AppState.usage_url` 指向它）。
pub struct FakeUsage {
    pub url: String,
    hits: Arc<std::sync::Mutex<Vec<(String, String, String, String, String)>>>,
    cfg: Arc<std::sync::Mutex<(u16, String)>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeUsage {
    pub async fn start(status: u16, body: &str) -> Self {
        let hits: Arc<std::sync::Mutex<Vec<(String, String, String, String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let cfg = Arc::new(std::sync::Mutex::new((status, body.to_string())));
        let st = (hits.clone(), cfg.clone());
        let app = Router::new()
            .fallback(move |req: axum::extract::Request| {
                let (hits, cfg) = st.clone();
                async move {
                    let h = req.headers();
                    hits.lock().unwrap().push((
                        req.method().to_string(),
                        req.uri().to_string(),
                        h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
                        h.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
                        h.get("cosy-clienttype").and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
                    ));
                    let (status, body) = cfg.lock().unwrap().clone();
                    axum::response::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑假额度接口");
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://{addr}/api/v2/quota/usage"),
            hits,
            cfg,
            handle,
        }
    }

    pub fn set(&self, status: u16, body: &str) {
        *self.cfg.lock().unwrap() = (status, body.to_string());
    }

    pub fn take_hits(&self) -> Vec<(String, String, String, String, String)> {
        std::mem::take(&mut *self.hits.lock().unwrap())
    }

    pub fn hits(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

impl Drop for FakeUsage {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ---------- 被测服务 ----------

pub struct AppServer {
    pub base: String,
    pub addr: String,
    pub port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl AppServer {
    pub async fn start(state: Arc<rs_qoder::AppState>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑被测端口");
        let addr = listener.local_addr().unwrap();
        let app: Router = rs_qoder::router(state);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            addr: addr.to_string(),
            port: addr.port(),
            handle,
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 不跟随重定向的 HTTP 客户端。
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("建 HTTP 客户端")
}

/// 一次本地网关往返的结果（给黄金门禁比对用）。
pub struct Hop {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

/// 打一次本地网关（不跟随重定向）。
pub async fn hop(base: &str, method: &str, path: &str, body: Option<&str>) -> Hop {
    let c = client();
    let m = reqwest::Method::from_bytes(method.as_bytes()).expect("方法名");
    let mut req = c.request(m, format!("{base}{path}"));
    if let Some(b) = body {
        req = req
            .header("content-type", "application/json")
            .body(b.to_string());
    }
    let resp = req.send().await.expect("发请求");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.text().await.unwrap_or_default();
    Hop {
        status,
        headers,
        body,
    }
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

// ---------- 易变字段归一化 ----------

/// `"created":<数字>` → `<CREATED>`、`"id":"chatcmpl-qoder-<数字>"` → `<ID>`、
/// `"uptimeSec":<数字>` → `<UPTIME>`、`"updatedAt":<数字>` → `<AT>`。
/// 与采集器同口径：这些都是进程内现生成/时钟相关的值。
pub fn normalize_volatile(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        for (prefix, repl) in [
            ("\"created\":", "\"created\":<CREATED>"),
            ("\"uptimeSec\":", "\"uptimeSec\":<UPTIME>"),
            ("\"updatedAt\":", "\"updatedAt\":<AT>"),
        ] {
            if s[i..].starts_with(prefix) {
                let start = i + prefix.len();
                let mut j = start;
                while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'-') {
                    j += 1;
                }
                if j > start {
                    out.push_str(repl);
                    i = j;
                    break;
                }
            }
        }
        if i < bytes.len() && s[i..].starts_with("\"id\":\"chatcmpl-qoder-") {
            let start = i + "\"id\":\"chatcmpl-qoder-".len();
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
        if i >= bytes.len() {
            break;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 归一化 JSON 文本里的易变字段（先归一化再比，等价于「形状比对」）。
pub fn normalize_json_text(s: &str) -> String {
    normalize_volatile(s)
}

// ---------- 响应头比对 ----------

/// 与黄金记录的响应头对齐：
/// * `date` 忽略（时钟）；
/// * `content-length`：**非 HEAD** 时只校验「等于本次响应体长度」——黄金里那个值内嵌了
///   随机的 `chatcmpl-qoder-<rand>` 位数，逐字节比会假红；**HEAD** 没有响应体，与黄金值逐字比；
/// * 其余逐条精确比；黄金没有 `content-length` 时也要求本次没有（SSE 流式是这种），
///   唯一放行的是 hyper 对流式响应补的 `transfer-encoding`（已在 README 差异清单登记）。
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
        // 已登记的传输层差异：Go 的 SSE 响应不带 content-length（也不带 transfer-encoding，
        // 它是 Go 在写响应时隐式分块），hyper 对长度未知的 body 会显式补 `transfer-encoding: chunked`。
        // 这里**显式**放行这一种形态，其他任何多出来的头都判红。
        for (k, v) in actual.iter() {
            if k.as_str() == "transfer-encoding" {
                assert_eq!(v.to_str().unwrap_or(""), "chunked", "[{case}] 未登记的 transfer-encoding");
            }
        }
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

/// 状态码断言（顺带把 body 打进失败信息里，排查省事）。
pub fn assert_status(case: &str, want: u16, got: u16, body: &str) {
    assert_eq!(want, got, "[{case}] 状态码不一致；body={body}");
}

pub fn assert_status_code(case: &str, want: u16, got: StatusCode) {
    assert_eq!(want, got.as_u16(), "[{case}] 状态码不一致");
}
