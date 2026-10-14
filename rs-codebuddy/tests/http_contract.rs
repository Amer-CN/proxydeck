//! HTTP 合同与路由边界（黄金门禁之外那几件「合同级」的事）。
//!
//! 覆盖：Go `corsWith` 的 CORS 头与 OPTIONS 204、`ServeMux` 的 404 文案、chat 非 POST 的
//! 405 形状（`Allow: POST` + `text/plain` + `nosniff`）、HEAD 的 `content-length` 自洽、
//! **64MB 读体口径不得退回 axum 的 2MB 默认上限**（3.6MB 体必须逐字节到上游）、
//! `//v1/models` 的**路径清洗已知差异**（Go `ServeMux` 307，axum 404）、以及
//! `/v1/stats`（P5c）仍回 501 的诚实性断言。

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{json, Value};

use rs_codebuddy::cred::StubCredential;

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

/// P5c 的端点仍**显式 501**（不是假 404，也不是静默缺路由）；P5b 三条已放开。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p5c_endpoint_answers_501_and_p5b_endpoints_are_live() {
    let dir = TestDir::new("deferred-501");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;
    let r = send(&app.base, "GET", "/v1/stats", None).await;
    assert_eq!(r.status, 501, "/v1/stats 应 501");
    assert!(r.text().contains("P5c"), "501 文案应点名 P5c：{}", r.text());

    // P5b 放开的端点：零值 Server（无池）下的形状与 Go 一致。
    // `/quota` → Go 无池时走当前凭据文件（这里 stub 头可用但上游是假 SSE，解析必然失败
    // → source:none + err，绝不该是 501）。
    let r = send(&app.base, "GET", "/quota", None).await;
    assert_eq!(r.status, 200, "/quota 已实现的证据：不再 501");
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert!(
        v.get("source").is_some(),
        "/quota 应回积分快照形状：{}",
        r.text()
    );
    // `/v1/accounts` → 零值 Server 守卫（Go 回 500 "account pool unavailable"）。
    let r = send(&app.base, "GET", "/v1/accounts", None).await;
    assert_eq!(r.status, 500, "无池时 /v1/accounts 应 500: {}", r.text());
    assert!(r.text().contains("account pool unavailable"));
    // `/v1/failover` → 生产缺省 enabled:true / deepseek-v4-pro（**不是**零值口径）。
    let r = send(&app.base, "GET", "/v1/failover", None).await;
    assert_eq!(r.status, 200);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["enabled"], Value::Bool(true));
    assert_eq!(v["fallback"], Value::String("deepseek-v4-pro".into()));
    assert_eq!(v["hy4_limit"], Value::Null);
}

/// `//v1/models` 的**已知差异**：Go `ServeMux` 把双斜杠清洗成单斜杠并回 307；
/// axum 不做清洗，故这里回 404。补一条对照把差异钉死（防止后人当 bug 追）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_slash_path_is_a_known_deviation() {
    let dir = TestDir::new("dbl-slash");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;
    let r = send(&app.base, "GET", "//v1/models", None).await;
    assert_eq!(
        r.status, 404,
        "Rust 侧不清洗双斜杠：Go 回 307（已知差异，登记在 README），Rust 回 404"
    );
    // 单斜杠（客户端实际会送出的形态，Go 的 307 也是往它跳）必须正常
    let r = send(&app.base, "GET", "/v1/models", None).await;
    assert_eq!(r.status, 200);
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

/// 未接入凭据（`headers_ok=false`）时 `/health` 必须**如实报错**，不得假装 ok。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_credential_error() {
    let dir = TestDir::new("health-cred-err");
    let cred = Arc::new(
        StubCredential::from_json(&json!({
            "auth_dirs": ["__AUTHDIR__"],
            "summary": {"kind":"error","message":"（测试）未找到登录文件，已搜索目录：__AUTHDIR__"},
            "headers": {},
            "headers_ok": false,
        }))
        .unwrap(),
    );
    let state = Arc::new(rs_codebuddy::AppState::new(
        rs_codebuddy::region::Region::Cn,
        cred,
        "http://127.0.0.1:1".to_string(),
        dir.path().to_path_buf(),
    ));
    let app = AppServer::start(state).await;
    let r = send(&app.base, "GET", "/health", None).await;
    assert_eq!(r.status, 200);
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["status"], Value::String("error".into()));
    assert!(
        v["message"]
            .as_str()
            .unwrap_or("")
            .contains("未找到登录文件"),
        "报错文案应如实透出凭据层的原因：{}",
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
        auth_dir: String::new(),
        diag_dir: String::new(),
        pool_dir: String::new(),
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
