//! HTTP 合同与路由边界（黄金门禁之外那几件「合同级」的事）。
//!
//! 覆盖：Go `corsWith` 的 CORS 头与 OPTIONS 204、`ServeMux` 的 404 文案、chat 非 POST 的
//! 405 形状（`Allow: POST` + `text/plain` + `nosniff`）、HEAD 的 `content-length` 自洽、
//! **64MB 读体口径不得退回 axum 的 2MB 默认上限**（3.6MB 体必须逐字节到上游）、
//! 以及两条「本轮显式不做」的诚实性断言：P5b/P5c 端点的 501、未接入凭据时的 health 报错。

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{json, Value};

use rs_codebuddy::cred::{PendingCredential, StubCredential};

const SSE_SPEC: &str = r#"{"kind":"fixed","status":200,"body":"data: {\"choices\":[{\"delta\":{}}]}\n\ndata: [DONE]\n\n","headers":{"Content-Type":"text/event-stream"}}"#;

fn sse_spec() -> Value {
    serde_json::from_str(SSE_SPEC).unwrap()
}

async fn state_with(dir: &TestDir, upstream: &str, region: rs_codebuddy::region::Region) -> Arc<rs_codebuddy::AppState> {
    let cred = Arc::new(StubCredential::from_json(&json!({
        "auth_dirs": ["__AUTHDIR__"],
        "summary": {"kind":"ok","pairs":{"uid":"u-golden"}},
        "headers": {"Content-Type":"application/json","Authorization":"Bearer tok-test","X-User-Id":"u-golden"},
        "headers_ok": true,
    }))
    .unwrap());
    Arc::new(rs_codebuddy::AppState::new(
        region,
        cred,
        upstream.to_string(),
        dir.path().to_path_buf(),
    ))
}

/// Go `ServeMux` 的兜底文案与 CORS 头（每条响应都要带）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_boundaries_match_go() {
    let dir = TestDir::new("route-boundaries");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;

    // 未知路径：404 + `404 page not found\n` + nosniff + CORS
    let r = send(&app.base, "GET", "/nope", None).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.text(), "404 page not found\n");
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));

    // 尾斜杠不匹配（Go 的 ServeMux 同样 404）
    let r = send(&app.base, "GET", "/v1/models/", None).await;
    assert_eq!(r.status, 404);

    // chat 非 POST：405 + Allow + 同一套 text/plain/nosniff 形状
    for m in ["GET", "PUT", "DELETE", "HEAD"] {
        let r = send(&app.base, m, "/v1/chat/completions", None).await;
        assert_eq!(r.status, 405, "{m} 应 405");
        assert_eq!(r.header("allow"), Some("POST"), "{m} 缺 Allow");
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
        if m != "HEAD" {
            assert_eq!(r.text(), "method not allowed\n");
        }
    }

    // CORS 预检：204 + 三个头，且不打上游
    let hits_before = up.hits().len();
    let r = send(&app.base, "OPTIONS", "/health", None).await;
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-methods"), Some("GET, POST, OPTIONS"));
    assert_eq!(
        r.header("access-control-allow-headers"),
        Some("Content-Type, Authorization")
    );
    assert_eq!(r.body.len(), 0);
    assert_eq!(up.hits().len(), hits_before, "OPTIONS 不该打上游");
}

/// 本轮**显式不做**的端点：回 501（不是假 404，也不是静默缺路由）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_endpoints_answer_501() {
    let dir = TestDir::new("deferred-501");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;
    for (path, round) in [
        ("/quota", "P5b"),
        ("/v1/failover", "P5b"),
        ("/v1/accounts", "P5b"),
        ("/v1/stats", "P5c"),
    ] {
        let r = send(&app.base, "GET", path, None).await;
        assert_eq!(r.status, 501, "{path} 应 501");
        assert!(
            r.text().contains(round),
            "{path} 的 501 文案应点名 {round}：{}",
            r.text()
        );
    }
    assert_eq!(up.hits().len(), 0, "未迁端点不该打上游");
}

/// HEAD 的 `content-length` 自洽（黄金里那一处 allowance 的替代证据）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_content_length_matches_get_body() {
    let dir = TestDir::new("head-content-length");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;
    let get = send(&app.base, "GET", "/health", None).await;
    let head = send(&app.base, "HEAD", "/health", None).await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty(), "HEAD 不该带体");
    let want = get.body.len().to_string();
    assert_eq!(
        head.header("content-length"),
        Some(want.as_str()),
        "HEAD 的 content-length 应等于同请求 GET 的体长"
    );
}

/// **不得退回 axum 的 2MB 默认上限**：Go 用 `io.LimitReader(64<<20)`，3.6MB 体必须逐字节到上游。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_body_reaches_upstream() {
    let dir = TestDir::new("large-body");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;

    let filler = "x".repeat(3_600_000);
    let payload = json!({
        "model": "auto",
        "messages": [{"role": "user", "content": filler}],
        "stream": true,
    });
    let body = serde_json::to_vec(&payload).unwrap();
    assert!(body.len() > 2 * 1024 * 1024, "用例要跨过 axum 的 2MB 默认上限");

    let r = send(&app.base, "POST", "/v1/chat/completions", Some(&body)).await;
    assert_eq!(r.status, 200, "大体积请求不该在本地被挡：{}", r.text());

    let hits = up.hits();
    assert_eq!(hits.len(), 1, "应只有一次上游请求");
    let sent: Value = serde_json::from_slice(&hits[0].body).expect("上游收到的是合法 JSON");
    let content = sent["messages"][0]["content"].as_str().expect("content 是字符串");
    assert_eq!(content.len(), 3_600_000, "请求体被截断或被改写");
    assert_eq!(sent["stream"], Value::Bool(true));
}

/// 未接入凭据层（P5a 默认）时 `/health` 必须**如实报错**，不得假装 ok。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_missing_credential_layer() {
    let dir = TestDir::new("health-pending");
    let state = Arc::new(rs_codebuddy::AppState::new(
        rs_codebuddy::region::Region::Cn,
        Arc::new(PendingCredential),
        "http://127.0.0.1:1".to_string(),
        dir.path().to_path_buf(),
    ));
    let app = AppServer::start(state).await;
    let r = send(&app.base, "GET", "/health", None).await;
    assert_eq!(r.status, 200);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["status"], Value::String("error".into()));
    assert!(
        v["message"].as_str().unwrap_or("").contains("P5b"),
        "报错文案应点名 P5b：{}",
        r.text()
    );
    assert!(v.get("credential").is_none(), "错误路径不该带 credential 块");
}

/// `--desensitize` 属 P5c：传了就拒绝启动（不静默忽略）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desensitize_flag_is_refused() {
    let args = rs_codebuddy::Args {
        plugin_codebuddy: false,
        port: 0,
        host: "127.0.0.1".to_string(),
        region: "CN".to_string(),
        desensitize: true,
        upstream_base: String::new(),
        cred_stub: String::new(),
        diag_dir: String::new(),
    };
    let err = rs_codebuddy::run(args).await.unwrap_err();
    assert!(err.contains("P5c"), "应点名 P5c：{err}");
}

/// 黄金基准必须入库（缺了就是静默少覆盖）。
#[test]
fn golden_files_exist() {
    for f in [GOLDEN_FILE] {
        let p = golden_path(f);
        assert!(p.exists(), "缺黄金基准 {}", p.display());
    }
    let g = load_golden();
    assert!(
        g["scenarios"].as_array().map(|a| a.len()).unwrap_or(0) >= 12,
        "黄金场景数不该缩水"
    );
}
