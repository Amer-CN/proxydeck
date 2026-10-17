//! P8b 第三片：**服务骨架的 HTTP 合同**（在进程内起路由，逐条验骨架本身）。
//!
//! 与 `.work/p8b3/compare.mjs` 的差分对照分工：
//! * 差分对照（`tests/p8b3_scenario/` + `.work/p8b3/run-diff.ps1`）负责「与真 Go 逐条一致」；
//! * 本文件负责**骨架自身的合同**——CORS 头、OPTIONS 短路、405 形态、404 形态、
//!   `writeJSON` 的末尾换行、`writeErr` 的封装形态、未迁端点的显式 501/404。
//!   这些在差分里也有，但这里能在 `cargo test` 单独复跑、不依赖 Go 侧与脚本。
//!
//! 纪律：假上游 + 临时 HOME/数据目录（合成账号）；不碰任何生产端口、不读真实凭据。

#[path = "p8b3_scenario/mod.rs"]
mod scenario;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use rs_tuanjie::server::{router, AppState};

/// 造一个隔离的 `AppState`：测试缝把两个域指到假上游，环境指到临时目录。
///
/// `AppState::new()` 会构造账号池与统计（读数据目录）——`PROXYDECK_DATA_DIR` 与
/// `USERPROFILE` 都已被指向临时目录，所以真实账号池/真实登录态零读取。
fn test_state(up: &scenario::FakeUpstream, root: &std::path::Path) -> Arc<AppState> {
    let env = scenario::Env::create(root);
    env.apply();
    rs_tuanjie::client::set_test_bases(up.base(), up.base());
    Arc::new(AppState::new().expect("构造 AppState"))
}

async fn call(
    state: Arc<AppState>,
    method: &str,
    path: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("构造请求");
    let resp = router(state).oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("读响应体");
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn root_of(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("p8b3-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建临时根目录");
    root
}

#[tokio::test]
async fn cors_headers_on_every_response_and_options_short_circuits() {
    let root = root_of("cors");
    let up = scenario::FakeUpstream::start(vec![]);
    let st = test_state(&up, &root);

    for (method, path, want) in [
        ("GET", "/health", StatusCode::OK),
        ("GET", "/v1/stats", StatusCode::OK),
        ("GET", "/kimi-pacing", StatusCode::OK),
        ("GET", "/activity", StatusCode::OK),
        ("GET", "/inflight", StatusCode::OK),
        ("GET", "/nope", StatusCode::NOT_FOUND),
        ("PUT", "/accounts", StatusCode::METHOD_NOT_ALLOWED),
        ("OPTIONS", "/health", StatusCode::NO_CONTENT),
        ("OPTIONS", "/nope", StatusCode::NO_CONTENT),
    ] {
        let (status, headers, body) = call(st.clone(), method, path).await;
        assert_eq!(status, want, "{method} {path}");
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("*"),
            "{method} {path} 缺 Access-Control-Allow-Origin"
        );
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .and_then(|v| v.to_str().ok()),
            Some("GET, POST, OPTIONS"),
            "{method} {path}"
        );
        assert_eq!(
            headers
                .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
                .and_then(|v| v.to_str().ok()),
            Some("Content-Type, Authorization"),
            "{method} {path}"
        );
        if method == "OPTIONS" {
            assert!(body.is_empty(), "OPTIONS 应短路回空体：{body:?}");
        }
    }
}

#[tokio::test]
async fn method_not_allowed_is_go_http_error_shape() {
    let root = root_of("405");
    let up = scenario::FakeUpstream::start(vec![]);
    let st = test_state(&up, &root);

    let (status, headers, body) = call(st, "PUT", "/accounts").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body, "method not allowed\n");
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    // Go `http.Error` 自带 nosniff
    assert_eq!(
        headers
            .get("X-Content-Type-Options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
}

#[tokio::test]
async fn unknown_path_is_go_servemux_404() {
    let root = root_of("404");
    let up = scenario::FakeUpstream::start(vec![]);
    let st = test_state(&up, &root);

    let (status, headers, body) = call(st, "GET", "/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "404 page not found\n");
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    // Go 的 `http.NotFound` 走 `http.Error`，后者**无条件**带 nosniff
    //（Go 1.26.5 `net/http/server.go:2315-2316`；旧断言写反，已更正）。
    assert_eq!(
        headers
            .get("X-Content-Type-Options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
}

#[tokio::test]
async fn write_json_shape_trailing_newline_and_no_charset() {
    let root = root_of("json");
    let up = scenario::FakeUpstream::start(vec![]);
    let st = test_state(&up, &root);

    for path in ["/v1/stats", "/kimi-pacing", "/activity", "/inflight"] {
        let (status, headers, body) = call(st.clone(), "GET", path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "{path}：Go 的 writeJSON 不写 charset"
        );
        assert!(body.ends_with("\n"), "{path}：json.Encoder 补末尾换行");
        assert!(
            serde_json::from_str::<serde_json::Value>(&body).is_ok(),
            "{path} 体应是合法 JSON：{body:?}"
        );
    }
}

#[tokio::test]
async fn write_err_encapsulates_api_error_shape() {
    let root = root_of("err");
    let up = scenario::FakeUpstream::start(vec![]);
    let st = test_state(&up, &root);

    // `POST /kimi-pacing` 的非法体 → 400 + writeErr 形态
    let req = Request::builder()
        .method("POST")
        .uri("/kimi-pacing")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"enabled":"yes"}"#))
        .expect("构造请求");
    let resp = router(st).oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("读体");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("合法 JSON");
    assert_eq!(
        v["error"]["message"],
        serde_json::Value::String("body 需为 {\"enabled\":true|false}".into())
    );
    assert_eq!(
        v["error"]["type"],
        serde_json::Value::String("api_error".into())
    );
    assert_eq!(v.as_object().unwrap().len(), 1, "顶层只有 error 一个键");
}

#[tokio::test]
async fn unimplemented_routes_say_so_instead_of_pretending() {
    let root = root_of("501");
    let up = scenario::FakeUpstream::start(vec![]);
    let st = test_state(&up, &root);

    for path in ["/model/info", "/v1/models?full=true"] {
        let (status, _h, body) = call(st.clone(), "GET", path).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{path}");
        assert!(
            body.contains("not_implemented_in_pilot"),
            "{path} 应显式说明未迁：{body:?}"
        );
    }
    // 未注册的端点回 404（**不**假装存在）
    for path in ["/account-models"] {
        let (status, _h, _b) = call(st.clone(), "GET", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} 应回 404（本片未注册）");
    }
    // P8c 起 `/water-probe` 已注册：不带 `?history=1` / `?channels=1` 的 GET 回 Go 同款
    // 纯文本 405（`handlers_extra.go:442` 的 `http.Error(w,"method not allowed",405)`）。
    let (status, headers, body) = call(st.clone(), "GET", "/water-probe").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "GET /water-probe 应回 405");
    assert_eq!(body, "method not allowed\n");
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );

    // `/v1/chat/completions` 已在 P8b 第四片迁入（池路径 + 单账号路径）；
    // `/providers`、`/media-config`、`/vision-config`、`/v1/images/generations`、
    // `/v1/videos`、`/agnesapi` 已在 P8b 第五片迁入 —— 它们都不再是 404。
    // GET `/v1/chat/completions` 走 Go 同款的 `http.Error(w,"method not allowed",405)`。
    let (status, headers, body) = call(st.clone(), "GET", "/v1/chat/completions").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "GET chat 应回 405");
    assert_eq!(body, "method not allowed\n");
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    // 第五片的端点：GET 都是**本地应答**（不是 axum 的 404/405）
    for (path, want) in [
        ("/providers", StatusCode::OK),
        ("/media-config", StatusCode::OK),
        ("/vision-config", StatusCode::OK),
        // 路径带 id 但没配 provider ⇒ 404 `provider_not_found`（Go 同）
        ("/v1/videos/v1", StatusCode::NOT_FOUND),
        // 缺 video_id ⇒ 400（Go 同）
        ("/agnesapi", StatusCode::BAD_REQUEST),
    ] {
        let (status, _h, body) = call(st.clone(), "GET", path).await;
        assert_eq!(status, want, "{path} 的 GET 应本地应答：{body:?}");
        assert!(
            !body.contains("not_implemented_in_pilot"),
            "{path} 不该再报未迁：{body:?}"
        );
    }
    // 图片/视频端子：GET 走 Go 同款 405 纯文本
    for path in ["/v1/images/generations", "/v1/videos"] {
        let (status, headers, body) = call(st.clone(), "GET", path).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert_eq!(body, "method not allowed\n", "{path}");
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8"),
            "{path}"
        );
    }
}

/// `--port 8788` + 任一测试缝在场 → 拒绝启动（exit 2），**不监听**（P8b 审查 L7）。
/// 真起二进制实测（只测拒绝路径；无测试缝的 8788 启动会真监听生产端口，
/// 永远不得在门禁里跑——那条只靠代码走查）。
#[test]
fn refuses_test_seams_on_production_port_8788() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rs-tuanjie"))
        .args(["--port", "8788"])
        .env(
            "RS_TUANJIE_TEST_BASES",
            "http://127.0.0.1:1 http://127.0.0.1:1",
        )
        .output()
        .expect("起测试二进制");
    assert_eq!(
        out.status.code(),
        Some(2),
        "测试缝 + 8788 必须拒绝启动"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("8788"),
        "拒绝文案应点名生产端口，实得：{err}"
    );
}
