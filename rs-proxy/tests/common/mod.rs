//! 集成测试脚手架：黄金基准加载、假上游、被测算例的真 HTTP 服务、原始 HTTP 客户端与归一化。
//!
//! 与 `rs-codebuddy/tests/common/mod.rs` 同族（P6 的复制件），按COMMAND 代理的合同裁剪：
//! * 上游是**进程内假上游**（axum，按黄金里的 `upstream_spec` 回放），不出网；
//! * 被测算例用 crate 自己的 `http::router()` 起在临时端口（`127.0.0.1:0`），
//!   与 Go 侧采集时一样走真实 TCP（不是函数直调）；
//! * 归一化与采集器**同源**（`rs-proxy/README.md` 的归一化表）。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use axum::Router;
use serde_json::Value;

pub const GOLDEN_FILE: &str = "golden-proxy-go.json";

/// 黄金基准路径：**先本 crate 的 `tests/golden/`，缺则回落 `.work/rust-rewrite/`**。
///
/// P6 的基准放在 `.work/p6/`（COMMAND 代理这一阶段的采集目录），故回落路径按阶段取。
pub fn golden_path(file: &str) -> PathBuf {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(file);
    if repo.exists() {
        return repo;
    }
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate 必有上级目录")
        .join(".work");
    let p6 = base.join("p6").join(file);
    if p6.exists() {
        return p6;
    }
    base.join("rust-rewrite").join(file)
}

/// 载入黄金基准（缺文件时给出可执行的再生成提示，不静默跳过）。
pub fn load_golden() -> Value {
    let p = golden_path(GOLDEN_FILE);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "读不到黄金基准 {}: {e}\n再生成链见 rs-proxy/tests/golden/README.md",
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
        let p = std::env::temp_dir().join(format!("rs-proxy-{tag}-{stamp}"));
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
#[derive(Debug, Clone, Default)]
pub struct CapturedHop {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
    /// 头名一律小写（HTTP 头名大小写不敏感）
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
}

/// 假上游：按黄金里的 `upstream_spec` 回放，并记录每一次请求。
pub struct FakeUpstream {
    pub base: String,
    hits: Arc<Mutex<Vec<CapturedHop>>>,
}

impl FakeUpstream {
    /// 起一个假上游：固定状态码 + Content-Type + 体（体是命令式写入，不切分）。
    pub async fn start(status: u16, content_type: &str, body: String) -> FakeUpstream {
        let hits: Arc<Mutex<Vec<CapturedHop>>> = Arc::new(Mutex::new(Vec::new()));
        let hits2 = hits.clone();
        let ct = content_type.to_string();
        let app = Router::new().fallback(move |req: Request| {
            let hits = hits2.clone();
            let body = body.clone();
            let ct = ct.clone();
            async move {
                let (parts, b) = req.into_parts();
                let bytes = axum::body::to_bytes(b, 64 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let mut hs: Vec<(String, String)> = Vec::new();
                for (k, v) in parts.headers.iter() {
                    hs.push((
                        k.as_str().to_ascii_lowercase(),
                        String::from_utf8_lossy(v.as_bytes()).to_string(),
                    ));
                }
                hits.lock().unwrap().push(CapturedHop {
                    method: parts.method.to_string(),
                    path: parts.uri.path().to_string(),
                    body: bytes.to_vec(),
                    headers: hs,
                });
                let status = axum::http::StatusCode::from_u16(status)
                    .unwrap_or(axum::http::StatusCode::OK);
                Response::builder()
                    .status(status)
                    .header("content-type", ct)
                    .body(Body::from(body))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定假上游端口");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeUpstream {
            base: format!("http://127.0.0.1:{}", addr.port()),
            hits,
        }
    }

    pub fn hits(&self) -> Vec<CapturedHop> {
        self.hits.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.hits.lock().unwrap().clear();
    }
}

/// 假「官网用量」上游（采集器段 E/F 的那个 `usageUp` / `badUp`）。
///
/// `/v1/usage` 打的是 **base_url + /alpha/whoami 等四个端点**（不是 /alpha/generate），
/// 所以段 E 单独起了这个服务。Rust 门禁若拿普通 [`FakeUpstream`]（固定一个体）回放，
/// `/v1/usage` 的每个上游调用都会拿到同一份文本 → 整段响应退化成 `whoami:null` 那种形态，
/// 与基准逐字节比必红（实测 `usage-endpoints #0`）。
pub struct FakeUsage {
    pub base: String,
}

impl FakeUsage {
    /// 与采集器逐字相同的四端点语料（`/alpha/whoami`、`/alpha/billing/credits`、
    /// `/alpha/billing/subscriptions`、`/alpha/usage/summary`、`/internal/usage`）。
    pub async fn start() -> FakeUsage {
        let app = Router::new().fallback(|req: Request| async move {
            let path = req.uri().path().to_string();
            match path.as_str() {
                "/alpha/whoami" => json_body(
                    "application/json",
                    r#"{"org":{"id":"org_1","login":"acme"},"user":{"userName":"bob"}}"#,
                ),
                "/alpha/billing/credits" => json_body(
                    "application/json",
                    r#"{"credits":{"monthlyCredits":10,"purchasedCredits":2,"freeCredits":3},"windowLimits":{"limited":true,"fiveHour":{"used":1.5,"cap":5,"resetAt":1790000000000},"weekly":{"used":7,"cap":50,"resetAt":1791000000000}}}"#,
                ),
                "/alpha/billing/subscriptions" => json_body(
                    "application/json",
                    r#"{"data":{"planId":"pro","status":"active","currentPeriodStart":"2026-10-01T00:00:00Z","currentPeriodEnd":"2099-11-01T00:00:00Z"}}"#,
                ),
                "/alpha/usage/summary" => json_body(
                    "application/json",
                    r#"{"totalCost":5.5,"totalCount":12,"totalTokens":100000,"totalTokensIn":60000,"totalTokensOut":40000}"#,
                ),
                "/internal/usage" => json_body(
                    "application/json",
                    r#"{"items":[{"model":"deepseek-v4-flash","inputTokens":100,"outputTokens":50,"cacheReadTokens":10,"cost":0.25,"createdAt":"2099-10-16T01:02:03Z"},{"model":"kimi-k3","inputTokens":7,"outputTokens":3,"cacheReadTokens":0,"amount":0.5,"created_at":"2099-10-16T02:02:03Z"},{"inputTokens":1}],"cursor":"c1"}"#,
                ),
                _ => Response::builder()
                    .status(404)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"e":"nf"}"#))
                    .unwrap(),
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定假用量上游端口");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeUsage {
            base: format!("http://127.0.0.1:{}", addr.port()),
        }
    }
}

/// 假「上游挂掉」的用量上游（采集器段 F 的 `badUp`：任何路径都 503 + HTML）。
pub struct FakeUsageDown {
    pub base: String,
}

impl FakeUsageDown {
    pub async fn start() -> FakeUsageDown {
        let app = Router::new().fallback(|_req: Request| async move {
            Response::builder()
                .status(503)
                .header("content-type", "text/html")
                .body(Body::from("<html>nope</html>"))
                .unwrap()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定假用量上游（503）端口");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeUsageDown {
            base: format!("http://127.0.0.1:{}", addr.port()),
        }
    }
}

fn json_body(content_type: &str, body: &str) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", content_type)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 被测算例：用生产路由表起在临时端口。
pub struct Target {
    pub base: String,
    pub proxy: Arc<rs_proxy::Proxy>,
}

impl Target {
    pub async fn start(proxy: rs_proxy::Proxy) -> Target {
        let proxy = Arc::new(proxy);
        let app = rs_proxy::http::router(proxy.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定被测端口");
        let addr = listener.local_addr().unwrap();
        let app2 = app.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app2).await;
        });
        Target {
            base: format!("http://127.0.0.1:{}", addr.port()),
            proxy,
        }
    }
}

/// 一次下游请求的观测结果。
#[derive(Debug, Clone)]
pub struct Downstream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Downstream {
    pub fn header(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == n)
            .map(|(_, v)| v.as_str())
    }
}

/// 用原始 HTTP 客户端打一次（不经 reqwest，避免它重写头/体）。
pub async fn send_raw(
    base: &str,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: Option<Vec<u8>>,
) -> Downstream {
    let url = format!("{base}{path}");
    let mut req = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("构造原始客户端")
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).expect("方法"),
            &url,
        );
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    let resp = req.send().await.expect("下游请求失败");
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| !k.as_str().eq_ignore_ascii_case("date"))
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(v.as_bytes()).to_string(),
            )
        })
        .collect();
    let body = resp.bytes().await.unwrap_or_default().to_vec();
    Downstream {
        status,
        headers,
        body,
    }
}

/// 上游出站请求的规范化（与采集器同源：端口 + 保留头集合）。
pub fn norm_hop_body(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body).to_string();
    let s = norm_volatile(&s);
    // 假上游端口无关
    let re = regex_lite_replace_port(&s);
    re
}

/// 极简「端口 → `<PORT>`」（不引 regex 依赖：只处理 `127.0.0.1:<数字>`）。
fn regex_lite_replace_port(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("127.0.0.1:") {
            out.push_str("127.0.0.1:<PORT>");
            i += "127.0.0.1:".len();
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 易变字段的规范化（与采集器的 `p6Placeholders` 同源同形）。
pub fn norm_volatile(s: &str) -> String {
    let mut out = s.to_string();
    // 生成型 id：chatcmpl-<uuid[:29]> / msg_<…> / resp_<…> / fc_<…>
    out = replace_id(&out, "\"id\":\"", &["chatcmpl-", "msg_", "resp_", "fc_"]);
    out = replace_id(&out, "\"item_id\":\"", &["msg_", "fc_"]);
    out = replace_id(&out, "\"call_id\":\"", &["fc_"]);
    // threadId（uuid v4）：值形态不定长（uuid 串里有连字符），单独处理
    out = replace_thread_id(&out);
    // created / created_at（10 位 unix 秒）
    out = replace_int_field(&out, "created", "<CREATED>");
    out = replace_int_field(&out, "created_at", "<CREATED>");
    out = replace_int_field(&out, "started", "<STARTED>");
    out = replace_int_field(&out, "uptimeSec", "<UPTIME>");
    // `daysLeft`：由「距周期结束的天数」算出（Go `int(Hours()/24)+1`），**随墙钟每天递减**
    // ——采集日定值不能直接比，必须双侧同源归一（P6 修正轮 F1：不加这条，采集次日必红）。
    out = replace_num_field(&out, "daysLeft", "<DAYSLEFT>");
    // config.date
    out = replace_quoted_field(&out, "date", "<DATE>", 10);
    out = norm_header_value(&out, "x-command-code-version", "<CCVER>");
    // 统计里的「按日」分桶键（本地时区 `YYYY-MM-DD`）会随日期变；采集器同源规则是
    // `([{,])"YYYY-MM-DD":{` → `$1"<DAY>":{`（见 `.work/p6` 采集器的 p6Placeholders）。
    out = replace_day_key(&out);
    out
}

/// 把 `"YYYY-MM-DD":{`（前面紧跟 `{` 或 `,`）的值换成 `<DAY>`——与采集器同源的日键归一化。
///
/// ⚠ 全程走字节、**不做 `&str` 下标切片**：`s[i..i+14]` 一旦落在多字节字符中间就直接 panic
/// （实测：体内有中文时 `byte index 715 is not a char boundary (好)`），
/// 而 `is_ascii()` 只能保护它之后的判断，护不住切片本身。
fn replace_day_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || bytes[i - 1] == b'{' || bytes[i - 1] == b',';
        if boundary && bytes[i] == b'"' && i + 14 <= bytes.len() && bytes[i..i + 14].is_ascii() {
            let all_digit_or_dash = bytes[i + 1..i + 11]
                .iter()
                .enumerate()
                .all(|(k, c)| if k == 4 || k == 7 { *c == b'-' } else { c.is_ascii_digit() });
            if all_digit_or_dash
                && bytes[i + 11] == b'"'
                && bytes[i + 12] == b':'
                && bytes[i + 13] == b'{'
            {
                out.push_str("\"<DAY>\":{");
                i += 14;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 把某个头名的值换成占位符（形态：`Name: value`，行内或 JSON 数组成员均可）。
fn norm_header_value(s: &str, header: &str, ph: &str) -> String {
    let needle = format!("{header}:");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        // ⚠ 每轮都在**当前 rest** 里找（不能拿整串小写化的偏移配 rest，会越界）
        let lower = rest.to_ascii_lowercase();
        let pos = match lower.find(&needle) {
            Some(p) => p,
            None => break,
        };
        out.push_str(&rest[..pos + needle.len()]);
        let tail = &rest[pos + needle.len()..];
        let mut i = 0;
        let tb = tail.as_bytes();
        while i < tb.len() && tb[i] == b' ' {
            i += 1;
        }
        let start = i;
        while i < tb.len() && tb[i] != b',' && tb[i] != b'\n' && tb[i] != b'}' && tb[i] != b'"' {
            i += 1;
        }
        let val = &tail[start..i];
        if val.is_empty() {
            out.push_str(&tail[..i]);
        } else {
            out.push_str(ph);
            if i == start {
                // 空值：直接落占位即可（上面已处理）
                out.truncate(out.len() - ph.len());
                out.push_str(ph);
            }
        }
        rest = &tail[i..];
    }
    out.push_str(rest);
    out
}

/// 把 `"<key>":"<value>"` 形态里长度等于 given 的值换成占位符。
fn replace_quoted_field(s: &str, key: &str, ph: &str, len: usize) -> String {
    let needle = format!("\"{key}\":\"");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(&needle) {
        out.push_str(&rest[..pos + needle.len()]);
        let tail = &rest[pos + needle.len()..];
        let end = tail.find('"').unwrap_or(tail.len());
        let val = &tail[..end];
        if val.len() == len && val.as_bytes().get(4) == Some(&b'-') {
            out.push_str(ph);
        } else {
            out.push_str(val);
        }
        out.push('"');
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `"threadId":"<uuid>"` → `"threadId":"<THREADID>"`（值为 uuid 形态才换）。
fn replace_thread_id(s: &str) -> String {
    const NEEDLE: &str = "\"threadId\":\"";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(NEEDLE) {
        out.push_str(&rest[..pos + NEEDLE.len()]);
        let tail = &rest[pos + NEEDLE.len()..];
        let end = tail.find('"').unwrap_or(tail.len());
        let val = &tail[..end];
        if val.len() == 36 && val.chars().filter(|c| *c == '-').count() == 4 {
            out.push_str("<THREADID>");
        } else {
            out.push_str(val);
        }
        out.push('"');
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 把 `"<key>":<digits>`（10 位）换成占位符。
fn replace_int_field(s: &str, key: &str, ph: &str) -> String {
    let needle = format!("\"{key}\":");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(&needle) {
        out.push_str(&rest[..pos + needle.len()]);
        let tail = &rest[pos + needle.len()..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() == 10 && digits.starts_with('1') {
            out.push_str(ph);
            rest = &tail[digits.len()..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// 把 `"<key>":<整数>` 的整数值换成占位符（允许前导 `-`，不限定位数）。
///
/// 与 [`replace_int_field`]（只认 10 位 unix 秒）分开：`daysLeft` 是 5 位天数，
/// 且**随墙钟每天递减**，必须双侧同源归一（P6 修正轮 F1）。
fn replace_num_field(s: &str, key: &str, ph: &str) -> String {
    let needle = format!("\"{key}\":");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(&needle) {
        out.push_str(&rest[..pos + needle.len()]);
        let tail = &rest[pos + needle.len()..];
        let bytes = tail.as_bytes();
        let mut end = 0usize;
        if end < bytes.len() && bytes[end] == b'-' {
            end += 1;
        }
        let first_digit = end;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > first_digit {
            out.push_str(ph);
            rest = &tail[end..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// 把 `"<prefix>…"`（前缀命中）整体换成占位符。
fn replace_id(s: &str, key_needle: &str, prefixes: &[&str]) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(key_needle) {
        out.push_str(&rest[..pos + key_needle.len()]);
        let tail = &rest[pos + key_needle.len()..];
        let end = tail.find('"').unwrap_or(tail.len());
        let val = &tail[..end];
        if prefixes.iter().any(|p| val.starts_with(p)) {
            out.push_str("<GEN_ID>");
        } else {
            out.push_str(val);
        }
        out.push('"');
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 把采集器写进基准的超大入参规格串展开（`[BIG:n:c]` → n 个字符 c）。
///
/// 两侧同源：采集器写占位、门禁展开，避免把 1.8MB 重复字符塞进基准。
pub fn expand_big(s: &str) -> String {
    const MARK: &str = "[BIG:";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(MARK) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + MARK.len()..];
        let close = match tail.find(']') {
            Some(c) => c,
            None => {
                out.push_str(&rest[pos..]);
                return out;
            }
        };
        let spec = &tail[..close];
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() == 2 {
            let n: usize = parts[0].parse().unwrap_or(0);
            let c = parts[1];
            out.push_str(&c.repeat(n));
        }
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    out
}

/// 黄金基准里的上游期望（把 `[BIG:…]` 展开后的体）。
pub fn golden_upstream_body(body: &str) -> String {
    expand_big(body)
}

pub fn json_of(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}
