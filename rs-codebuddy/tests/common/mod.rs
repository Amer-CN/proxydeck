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

/// 按黄金的 `upstream_spec` 回放（与 Go 采集器的 `cbRespSpec.responder()` 同语义）。
fn respond(state: &Arc<Mutex<Value>>, hop: &CapturedHop) -> FakeResp {
    let spec = state.lock().unwrap().clone();
    let kind = spec.get("kind").and_then(|v| v.as_str()).unwrap_or("");
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
