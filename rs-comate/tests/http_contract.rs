// tests/http_contract.rs —— HTTP 合同与边界（黄金基准之外的那些）。
//
// 黄金门禁管「与 Go 逐字节一致」；这里管「基准没覆盖到、但对外可见的行为」：
// CORS/OPTIONS、405、32MB 请求体截断、zulu 拉起与超时、流式增量下发、
// 连接失败/上游 500 的形状。全部用测试端口与临时目录，**不碰 8792、不碰真实登录态**。

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::response::IntoResponse;
use axum::Router;
use common::*;
use rs_comate::AppState;

const SENTINEL: &str = "COMATE-LICENSE-SENTINEL-NOT-A-REAL-CREDENTIAL";
const LIST_JSON: &str = r#"[{"modelId":"deepseek-v4-flash_a8668ea86c3d40e7ad1f8ee45715d447","displayName":"DeepSeek-V4-Flash"}]"#;
const OK_SSE: &str =
    "data: {\"kind\":\"delta-batch\",\"chunks\":[{\"kind\":\"text-delta\",\"delta\":\"答\"}]}\n\n\
     data: {\"kind\":\"completed\",\"status\":\"ok\",\"content\":\"答案\"}\n\n";

fn chat_body(stream: bool) -> String {
    format!(
        r#"{{"model":"auto","stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
    )
}

/// 造一套「登录态 + 假 zulu + 假上游 + 假探活」的被测服务。
struct Env {
    _dir: TestDir,
    _zulu: FakeZulu,
    up: FakeUpstream,
    _health: FakeHealth,
    app: AppServer,
}

async fn env_with(zulu_present: bool, license: &str, upstream: Option<String>) -> Env {
    let zulu = FakeZulu::new("contract-zulu");
    zulu.set_list(LIST_JSON);
    let dir = TestDir::new("contract-lic");
    let lic_path = write_license(&dir, "cli-login.json", license);
    let up = FakeUpstream::start().await;
    up.set_sse(OK_SSE).await;
    let health = FakeHealth::start(0).await;
    let base = upstream.unwrap_or_else(|| up.base.clone());
    let st = AppState::new(
        lic_path,
        if zulu_present { Some(zulu.path()) } else { None },
        Some("C:\\Windows".to_string()),
        None,
    )
    .with_upstream(base)
    .with_zulu_health(health.url.clone(), "18992"); // 测试端口，绝不是 8792
    let app = AppServer::start(Arc::new(st)).await;
    Env {
        _dir: dir,
        _zulu: zulu,
        up,
        _health: health,
        app,
    }
}

fn has_cors(headers: &axum::http::HeaderMap) -> bool {
    headers.get("access-control-allow-origin").map(|v| v == "*").unwrap_or(false)
        && headers
            .get("access-control-allow-headers")
            .map(|v| v == "Content-Type, Authorization")
            .unwrap_or(false)
        && headers
            .get("access-control-allow-methods")
            .map(|v| v == "GET, POST, OPTIONS")
            .unwrap_or(false)
}

// ---------- CORS / OPTIONS / 方法 ----------

#[tokio::test]
async fn cors_is_on_every_response_and_options_short_circuits() {
    let e = env_with(true, SENTINEL, None).await;

    // 正常响应
    let resp = client()
        .get(format!("{}/health", e.app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(has_cors(resp.headers()), "GET /health 缺 CORS 头");

    // OPTIONS：corsWith 在路由之前短路，任何路径都 204、无体
    for path in ["/health", "/v1/models", "/nope", "/v1/chat/completions"] {
        let resp = client()
            .request(
                reqwest::Method::OPTIONS,
                format!("{}{path}", e.app.base),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204, "OPTIONS {path} 应为 204");
        assert!(has_cors(resp.headers()), "OPTIONS {path} 缺 CORS 头");
        let body = resp.bytes().await.unwrap();
        assert!(body.is_empty(), "OPTIONS {path} 不该有响应体");
    }

    // 404 也带 CORS 头（corsWith 包在 mux 外面）
    let resp = client()
        .get(format!("{}/nope", e.app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    assert!(has_cors(resp.headers()), "404 缺 CORS 头");
    assert_eq!(
        resp.headers().get("x-content-type-options").map(|v| v.to_str().unwrap()),
        Some("nosniff"),
        "404 应该有 nosniff（Go 的 http.Error 自带）"
    );
}

#[tokio::test]
async fn method_rules_match_go() {
    let e = env_with(true, SENTINEL, None).await;

    // /v1/chat/completions：非 POST → 405 + 错误 JSON（handler 里判，不是路由层）
    for method in [reqwest::Method::GET, reqwest::Method::PUT] {
        let resp = client()
            .request(method.clone(), format!("{}/v1/chat/completions", e.app.base))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 405, "{method} 应为 405");
        let body = resp.text().await.unwrap();
        assert_eq!(
            body,
            "{\"error\":{\"message\":\"仅支持 POST\",\"type\":\"invalid_request\"}}\n",
            "{method} 的 405 文案应与 Go 一致"
        );
    }

    // /v1/models 与 /health 不看方法（POST 也照回）
    let resp = client()
        .post(format!("{}/v1/models", e.app.base))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "POST /v1/models 应照回 200");
    let resp = client()
        .post(format!("{}/health", e.app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "POST /health 应照回 200");
}

// ---------- 请求体上限：截断而不是 413 ----------

#[tokio::test]
async fn oversized_body_is_truncated_not_rejected() {
    let e = env_with(true, SENTINEL, None).await;
    // Go：`io.LimitReader(r.Body, 32<<20)` 只截断不报错，随后 JSON 解析失败 → 400。
    // axum 的默认上限是 2MB 且回 413，那就与 Go 分歧 —— 这里锁住 33MB 的请求体。
    let big = format!(
        r#"{{"model":"auto","messages":[{{"role":"user","content":"{}"}}]}}"#,
        "a".repeat(33 * 1024 * 1024)
    );
    assert!(big.len() > 32 * 1024 * 1024);
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(big)
        .send()
        .await;
    let resp = resp.expect("33MB 请求体应能拿到响应（不是连接被重置）");
    assert_eq!(
        resp.status().as_u16(),
        400,
        "超 32MB 的体应被截断成 400（Go 语义），不是 413"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.starts_with("{\"error\":{\"message\":\"请求体解析失败: "),
        "截断后的解析错误形状不对: {body}"
    );

    // 32MB 以内的正常请求不受影响
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

// ---------- zulu 拉起 / 超时 / 缺失 ----------

#[tokio::test]
async fn zulu_serve_is_spawned_with_the_go_argv() {
    let zulu = FakeZulu::new("spawn-zulu");
    zulu.set_list(LIST_JSON);
    let dir = TestDir::new("spawn-lic");
    let lic_path = write_license(&dir, "cli-login.json", SENTINEL);
    let up = FakeUpstream::start().await;
    up.set_sse(OK_SSE).await;
    // 第一次探活 503（还没起来）→ 触发 spawn；之后 200（起来了）
    let health = FakeHealth::start(2).await; // 前两次探活都 503：ensureZulu 与 ensureZuluServe 各一次，之后才 spawn
    let mut st = AppState::new(
        lic_path,
        Some(zulu.path()),
        Some("C:\\Windows".to_string()),
        None,
    )
    .with_upstream(up.base.clone())
    .with_zulu_health(health.url.clone(), "18992");
    st.zulu_serve_timeout = Duration::from_secs(10);
    let app = AppServer::start(Arc::new(st)).await;

    let resp = client()
        .post(format!("{}/v1/chat/completions", app.base))
        .header("content-type", "application/json")
        .body(chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // spawn 是**分离子进程**：cmd 落 serve.log 与「探活立刻成功」之间有竞态，
    // 这里等它一下（最多 3s），别把调度延迟当成实现错。
    let deadline = Instant::now() + Duration::from_secs(3);
    while zulu.serve_args().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let args = zulu.serve_args();
    assert!(
        args.contains("serve --port 18992 --host 127.0.0.1 -l"),
        "zulu serve 的参数形态与 Go 不一致: {args:?}"
    );
    assert!(args.contains(SENTINEL), "license 应作为 -l 的值传给 zulu serve");
    assert!(
        health.probes() >= 3,
        "两次失败探活 + 拉起后成功探活，至少 3 次（实测 {}）",
        health.probes()
    );
}

#[tokio::test]
async fn zulu_serve_timeout_reports_go_message() {
    let zulu = FakeZulu::new("timeout-zulu");
    let dir = TestDir::new("timeout-lic");
    let lic_path = write_license(&dir, "cli-login.json", SENTINEL);
    // 永远起不来
    let health = FakeHealth::start(usize::MAX / 2).await;
    let mut st = AppState::new(
        lic_path,
        Some(zulu.path()),
        Some("C:\\Windows".to_string()),
        None,
    )
    .with_zulu_health(health.url.clone(), "18992");
    st.zulu_serve_timeout = Duration::from_millis(300); // 生产中这里是 20s
    let app = AppServer::start(Arc::new(st)).await;

    let resp = client()
        .post(format!("{}/v1/chat/completions", app.base))
        .header("content-type", "application/json")
        .body(chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    let body = resp.text().await.unwrap();
    assert_eq!(
        body,
        "{\"error\":{\"message\":\"zulu serve 启动超时（20 秒未就绪）\",\"type\":\"upstream_error\"}}\n",
        "超时文案应与 Go 逐字一致"
    );
    assert!(zulu.serve_args().contains("serve"), "超时前确实起过 zulu");
}

#[tokio::test]
async fn missing_zulu_and_missing_license_report_go_messages() {
    // 没登录态 → 503 no_license（这条黄金基准里也有，这里锁 Rust 侧的文案来源）
    let e = env_with(true, "", None).await;
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(
        resp.text().await.unwrap(),
        "{\"error\":{\"message\":\"未找到 Comate 登录态，请先在 Comate IDE 或 zulu CLI 登录\",\"type\":\"no_license\"}}\n"
    );

    // 找不到 zulu CLI → 503（type 是 no_license，与 Go 一致）；本机 8792 上跑着真 zulu、
    // D: 还装着 Comate，所以这条分支采不到黄金基准，只能按源码逐字对齐（见 README 未覆盖项）。
    let e = env_with(false, SENTINEL, None).await;
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(
        resp.text().await.unwrap(),
        "{\"error\":{\"message\":\"未找到 zulu CLI，请先安装 Comate（文心快码 AI IDE）\",\"type\":\"no_license\"}}\n"
    );
}

// ---------- 上游异常的形状 ----------

#[tokio::test]
async fn upstream_connect_failure_and_http_error_shapes() {
    // 端口 1 上没人监听 → 502 upstream_error（文案与 Go 的措辞不同，见 README 差异清单）
    let e = env_with(true, SENTINEL, Some("http://127.0.0.1:1".to_string())).await;
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    let body = resp.text().await.unwrap();
    assert!(
        body.starts_with("{\"error\":{\"message\":\"上游连接失败: ") && body.contains("\"type\":\"upstream_error\""),
        "连接失败的 502 形状不对: {body}"
    );

    // 上游 500 + 5000 字节原文 → 文案只带前 4096 字节（Go 的 io.LimitReader(4096)）
    let e = env_with(true, SENTINEL, None).await;
    e.up.set_status_with_body(500, &"x".repeat(5000)).await;
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    let body = resp.text().await.unwrap();
    let want = format!(
        "{{\"error\":{{\"message\":\"上游 HTTP 500: {}\",\"type\":\"upstream_error\"}}}}\n",
        "x".repeat(4096)
    );
    assert_eq!(body, want, "上游错误体的截断口径与 Go 不一致（应只取前 4096 字节）");
}

// ---------- 流式增量下发 ----------

/// 逐帧慢速吐 SSE 的假上游：先 150ms 给一个文本增量，再 900ms 给 completed。
async fn start_slow_upstream() -> String {
    async fn handler() -> axum::response::Response {
        let body = Body::from_stream(futures_util::stream::unfold(0usize, |i| async move {
            match i {
                0 => {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Some((
                        Ok::<Bytes, std::io::Error>(Bytes::from_static(
                            b"data: {\"kind\":\"delta-batch\",\"chunks\":[{\"kind\":\"text-delta\",\"delta\":\"\xe9\xa6\x96\"}]}\n\n",
                        )),
                        1usize,
                    ))
                }
                1 => {
                    tokio::time::sleep(Duration::from_millis(900)).await;
                    Some((
                        Ok(Bytes::from_static(
                            b"data: {\"kind\":\"completed\",\"status\":\"ok\"}\n\n",
                        )),
                        2usize,
                    ))
                }
                _ => None,
            }
        }));
        (
            axum::http::StatusCode::OK,
            [("content-type", "text/event-stream")],
            body,
        )
            .into_response()
    }
    let app = Router::new().fallback(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // 让 spawn 有活干：返回后立刻可用
    tokio::time::sleep(Duration::from_millis(20)).await;
    format!("http://{addr}")
}

#[tokio::test]
async fn stream_is_flushed_incrementally() {
    let upstream = start_slow_upstream().await;
    let e = env_with(true, SENTINEL, Some(upstream)).await;

    let started = Instant::now();
    let mut resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    // 第一帧必须在上游还没结束时就到（Go 是 emit 完立刻 Flush）
    let first = resp.chunk().await.unwrap().expect("应有第一帧");
    let first_at = started.elapsed();
    let first_text = String::from_utf8_lossy(&first).to_string();
    assert!(
        first_text.contains("\"content\":\"首\""),
        "第一帧应是文本增量: {first_text:?}"
    );
    assert!(
        first_at < Duration::from_millis(600),
        "第一帧来得太晚（{first_at:?}）——说明实现把整条上游响应缓冲完才下发"
    );

    // 其余帧继续到 [DONE]
    let mut rest = Vec::new();
    while let Some(chunk) = resp.chunk().await.unwrap() {
        rest.extend_from_slice(&chunk);
    }
    let all = format!("{first_text}{}", String::from_utf8_lossy(&rest));
    assert!(all.contains("\"finish_reason\":\"stop\""), "末尾应有 stop 块: {all:?}");
    assert!(all.ends_with("data: [DONE]\n\n"), "末尾应是 [DONE]: {all:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(1000),
        "整条读完应跨过上游的 1050ms"
    );
}

// ---------- 上游 500 与超长行的流式形状 ----------

#[tokio::test]
async fn stream_200_headers_survive_upstream_error_502() {
    let e = env_with(true, SENTINEL, None).await;
    e.up.set_status_with_body(500, "boom").await;
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    // 黄金基准 `stream-error-before-output` 记的就是这三条头（Go 先设了 SSE 头才走到 502）
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(resp.headers().get("connection").unwrap(), "keep-alive");
    assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
}

#[tokio::test]
async fn post_commit_scanner_overflow_still_finishes_the_stream() {
    // 已提交（首帧已下发）之后上游再来一条 >1MB 的行：Go 的读循环直接结束（scanner.Err 不看），
    // 因为 committed 已 true ⇒ 照常补 finish_reason=stop 与 [DONE]，仍是 200。
    let e = env_with(true, SENTINEL, None).await;
    let sse = format!(
        "data: {{\"kind\":\"delta-batch\",\"chunks\":[{{\"kind\":\"text-delta\",\"delta\":\"前半\"}}]}}\n\n\
         data: {{\"kind\":\"completed\",\"status\":\"ok\",\"content\":\"{}\"}}\n\n",
        "长".repeat(600 * 1024)
    );
    e.up.set_sse(&sse).await;
    let resp = client()
        .post(format!("{}/v1/chat/completions", e.app.base))
        .header("content-type", "application/json")
        .body(chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"content\":\"前半\""), "首帧应已下发: {body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "应补 stop 块");
    assert!(body.ends_with("data: [DONE]\n\n"), "应以 [DONE] 收尾");
    assert!(
        !body.contains("上游 SSE 中断"),
        "已提交后不该再回 502 文案: {body}"
    );
}
