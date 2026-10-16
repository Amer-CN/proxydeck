//! HTTP 合同与路由边界（黄金门禁之外那几件「合同级」的事）。
//!
//! 覆盖：Go `corsWith` 的 CORS 头与 OPTIONS 204、`ServeMux` 的 404 文案、chat 非 POST 的
//! 405 形状（`Allow: POST` + `text/plain` + `nosniff`）、HEAD 的 `content-length` 自洽、
//! **64MB 读体口径不得退回 axum 的 2MB 默认上限**（3.6MB 体必须逐字节到上游）、
//! `//v1/models` 的**路径清洗**（P5c 已按 Go `ServeMux` 复刻：307 + `Location` + 体）、以及
//! `/v1/stats`（P5c 起为真实应答，不再是 501 占位）。

mod common;

use std::sync::Arc;

use clap::Parser as _;
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
async fn p5c_stats_endpoint_is_live_and_p5b_endpoints_are_live() {
    let dir = TestDir::new("deferred-501");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;
    // P5c：`/v1/stats` 从 501 变成真实应答（空表 + 运行时长），且**不看方法**。
    for m in ["GET", "POST"] {
        let r = send(&app.base, m, "/v1/stats", None).await;
        assert_eq!(r.status, 200, "/v1/stats {m} 应 200：{}", r.text());
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["models"], json!({}), "/v1/stats 空表形状：{}", r.text());
        assert!(v.get("uptimeSec").is_some());
        assert!(
            !r.text().contains("P5c"),
            "/v1/stats 不该再回 P5c 占位文案：{}",
            r.text()
        );
    }

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

/// `//v1/models` 的**路径清洗**（P5c 已对齐，差异 12 关闭）：Go `ServeMux` 在匹配之前
/// 清洗路径并回 307；本实现复刻同一响应（状态码 / `Location` / 头 / 体逐字节）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_slash_path_redirects_like_go_serve_mux() {
    let dir = TestDir::new("dbl-slash");
    let up = FakeUpstream::start(&sse_spec()).await;
    let app = AppServer::start(state_with(&dir, &up.base, rs_codebuddy::region::Region::Cn).await).await;
    let r = send(&app.base, "GET", "//v1/models", None).await;
    assert_eq!(r.status, 307, "Go ServeMux 的清洗重定向是 307");
    assert_eq!(r.header("location"), Some("/v1/models"));
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(
        r.text(),
        "<a href=\"/v1/models\">Temporary Redirect</a>.\n\n"
    );
    // POST 同样 307，但**不带体、不带 Content-Type**（Go 只对 GET 写体）
    let r = send(&app.base, "POST", "//v1/models", None).await;
    assert_eq!(r.status, 307);
    assert_eq!(r.header("location"), Some("/v1/models"));
    assert!(r.body.is_empty(), "POST 的重定向体应为空");
    assert_eq!(r.header("content-type"), None);
    // 点段同样被清洗——但必须用**裸 socket** 发：`reqwest`/`url` 会在客户端就把 `/v1/../x`
    // 规范化成 `/x`（Go 的 `http.Client` 不做），用高级客户端量到的是客户端行为。
    let r = send_raw(&app.base, "GET", "/v1/../v1/models").await;
    assert_eq!(r.status, 307);
    assert_eq!(r.header("location"), Some("/v1/models"));
    // 单斜杠（客户端实际会送出的形态，307 也是往它跳）必须正常
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

/// `--desensitize`（P5c 放行）：生产装配**不再拒绝**，且开关真的落到 `AppState` 上
/// （行为等价由 P5c 黄金门禁的脱敏场景逐字节守——这里只守「开关被接线」）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desensitize_flag_is_wired_into_state() {
    let env = EnvGuard::set(&[]);
    let auth = TestDir::new("desensitize-auth");
    let data = TestDir::new("desensitize-data");
    let home = TestDir::new("desensitize-home");
    write_auth_files(
        auth.path(),
        &json!([{
            "name": "workbuddy-desktop.info",
            "body": "{\"auth\":{\"accessToken\":\"tok-u\",\"domain\":\"codebuddy.cn\",\"expiresAt\":4102416000000},\"account\":{\"uid\":\"u-d\",\"nickname\":\"n\",\"enterpriseId\":\"ent\"}}",
            "mtime_unix_ms": 1_792_000_000_000u64,
        }]),
    );
    let auth_s = auth.path().to_string_lossy().to_string();
    let data_s = data.path().to_string_lossy().to_string();
    env.set_locked(&[
        ("CODEBUDDY_AUTH_DIR", Some(auth_s.as_str())),
        ("PROXYDECK_DATA_DIR", Some(data_s.as_str())),
        ("CODEBUDDY_POOL_DIR", Some(data_s.as_str())),
        ("USERPROFILE", Some(home.path().to_string_lossy().as_ref())),
        ("LOCALAPPDATA", Some("")),
        ("XDG_DATA_HOME", Some("")),
    ]);
    let (st, _cred) = rs_codebuddy::AppState::production_with_data_dir(
        rs_codebuddy::region::Region::Cn,
        data.path().to_path_buf(),
        data_s.clone(),
        Some("http://127.0.0.1:1".to_string()),
        true,
    )
    .expect("--desensitize 不该再被拒绝");
    assert!(st.desensitize, "开关必须落到 AppState（聊天路径据此脱敏）");
    // CLI 层同样不拦：`--desensitize` 是合法开关
    let args = rs_codebuddy::Args::parse_from([
        "rs-codebuddy",
        "--desensitize",
        "--plugin-codebuddy",
        "--port",
        "0",
    ]);
    assert!(args.desensitize);
    env.point_to_empty(data.path());
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
