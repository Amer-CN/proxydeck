// http_contract.rs —— 端点合同测试（起真 socket、真 HTTP，假上游在本进程内）。
//
// 覆盖验收 1 的可测部分与几条"不能漏给上游"的路由约束：
//   * GET /health 字段与 Go 同形（status/service/uptimeSec）
//   * CORS 头在普通响应与 OPTIONS 预检上都在
//   * /v1/models 透传 Authorization，且把响应原样（含上游自定义头）回给下游
//   * /v1/chat/completions 到达假上游的是**改写后**的体（developer→system、8192 钳制）
//   * >2MB 的请求体经 HTTP 路径照样到达上游（对齐 Go bufferBody 的 32MB，而不是 axum 默认 2MB）
//   * /model/matrix、/v1/stats 由本进程应答、绝不漏给上游（Go 侧这两条也是本地应答；
//     端到端的逐字节对照在 tests/p1_golden.rs）
//   * 上游 5xx 会重试（最多 3 次）
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use serde_json::json;

use rs_bai::MAX_BODY_BYTES;

/// 慢流用例的时长。必须**大于原先那个 600s 总超时**，否则测不出"总超时已去掉"。
const SLOW_STREAM_SECS: usize = 660;

#[derive(Clone, Default)]
struct Recorded {
    calls: Arc<AtomicUsize>,
    last_body: Arc<Mutex<Vec<u8>>>,
    last_headers: Arc<Mutex<HeaderMap>>,
    last_path: Arc<Mutex<String>>,
}

/// 假上游：记录最后一次请求，按场景返回。
async fn fake_upstream(State(rec): State<Recorded>, method: Method, uri: axum::http::Uri, headers: HeaderMap, body: Bytes) -> Response {
    rec.calls.fetch_add(1, Ordering::SeqCst);
    *rec.last_body.lock().unwrap() = body.to_vec();
    *rec.last_headers.lock().unwrap() = headers.clone();
    *rec.last_path.lock().unwrap() = uri.path().to_string();

    if uri.path() == "/boom" {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    if uri.path() == "/slow-stream" {
        // 慢流：先把响应头发出去，然后每 50ms 一个 SSE 帧，持续 SLOW_STREAM_SECS。
        // 用来验证本 crate 对上游**没有含响应体的总超时**（对齐 Go：`http.Server`
        // 未设 ReadTimeout/WriteTimeout，`http.DefaultTransport` 只给 30s 连接超时）。
        let frames = SLOW_STREAM_SECS * 20;
        let stream = futures_util::stream::unfold(0usize, move |n| async move {
            if n >= frames {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let chunk = format!("data: {{\"seq\":{n}}}\n\n");
            Some((
                Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)),
                n + 1,
            ))
        });
        return (
            StatusCode::OK,
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response();
    }
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let payload = json!({
        "object": "list",
        "data": [{"id": "glm-5.3-flash", "object": "model", "created": 1626777600, "owned_by": "system"}],
        "echo_auth": auth,
        "echo_method": method.as_str(),
        "echo_path": uri.path(),
    });
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("x-upstream-marker", "from-fake-upstream"),
        ],
        serde_json::to_vec(&payload).unwrap(),
    )
        .into_response()
}

struct TestServer {
    base: String,
    rec: Recorded,
}

/// 与生产 `run()` 里挂的同一层：把 axum 默认的 405 空体补成 Go `http.Error` 的纯文本。
async fn method_not_allowed_text(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        *resp.body_mut() = axum::body::Body::from("method not allowed\n");
    }
    resp
}

/// 起一个真的监听 socket：假上游 + rs-bai（upstream 指向假上游）。
/// 数据目录相关路径全部指向临时目录，**绝不碰 %APPDATA% 下的真实文件**。
async fn start() -> TestServer {
    let rec = Recorded::default();
    let upstream = Router::new()
        .fallback(any(fake_upstream))
        .with_state(rec.clone())
        // 假上游也得放开 2MB 默认体上限，否则 >2MB 的用例在这里就被上游挡掉，
        // 测不到"本 crate 能不能把大体积请求送出去"。
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_addr = up_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(up_listener, upstream).await;
    });

    let client = rs_bai::UpstreamClient::with_proxy(&format!("http://{up_addr}"), None).unwrap();
    let tmp = std::env::temp_dir().join(format!("rs-bai-http-contract-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let channels = tmp.join("tuanjie-water-channels.json");
    // 没有 bai 条目 → 矩阵走 need_key 分支，绝不打上游
    std::fs::write(&channels, br#"[{"id":"command","key":"sk-other"}]"#).unwrap();
    let stats_path = tmp.join("bai-stats.json");
    let _ = std::fs::remove_file(&stats_path);

    let state = Arc::new(rs_bai::AppState::with_paths(client, channels, stats_path));
    // 与生产 `run()` 同一层包装（405 的纯文本补体中间件）
    let app = rs_bai::router(state).layer(axum::middleware::from_fn(method_not_allowed_text));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    TestServer {
        base: format!("http://{addr}"),
        rec,
    }
}

#[tokio::test]
async fn health_fields_match_go_shape() {
    let t = start().await;
    let resp = reqwest::get(format!("{}/health", t.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "*"
    );
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["service"], "bai-go", "service 字段与 Go 侧同值");
    assert!(v["uptimeSec"].as_i64().is_some(), "uptimeSec 必须是整数秒");
    assert_eq!(t.rec.calls.load(Ordering::SeqCst), 0, "/health 不得打上游");
}

#[tokio::test]
async fn options_preflight_is_204_with_cors() {
    let t = start().await;
    let c = reqwest::Client::new();
    let resp = c
        .request(Method::OPTIONS, format!("{}/v1/chat/completions", t.base))
        .header("origin", "http://localhost:1234")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(resp.headers().get("access-control-allow-origin").unwrap(), "*");
    assert_eq!(
        resp.headers().get("access-control-allow-methods").unwrap(),
        "GET, POST, OPTIONS"
    );
    assert_eq!(t.rec.calls.load(Ordering::SeqCst), 0, "预检不得打上游");
}

#[tokio::test]
async fn models_passthrough_keeps_auth_and_upstream_headers() {
    let t = start().await;
    let c = reqwest::Client::new();
    let resp = c
        .get(format!("{}/v1/models", t.base))
        .header("authorization", "Bearer sk-redacted-for-test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("x-upstream-marker").unwrap(),
        "from-fake-upstream",
        "上游自定义响应头必须原样回给下游"
    );
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["echo_auth"], "Bearer sk-redacted-for-test", "Authorization 原样透传");
    assert_eq!(v["echo_path"], "/v1/models");
    let seen: Vec<String> = t
        .rec
        .last_headers
        .lock()
        .unwrap()
        .iter()
        .map(|(k, _)| k.as_str().to_string())
        .collect();
    assert!(seen.contains(&"host".to_string()), "出站应带 host");
}

#[tokio::test]
async fn chat_completions_arrives_adapted() {
    let t = start().await;
    let c = reqwest::Client::new();
    let body = json!({
        "model": "glm-5.3-flash",
        "messages": [
            {"role": "developer", "content": "rules"},
            {"role": "user", "content": "hi"}
        ],
        "max_tokens": 99999,
        "reasoning_effort": "max"
    });
    let resp = c
        .post(format!("{}/v1/chat/completions", t.base))
        .header("authorization", "Bearer sk-redacted-for-test")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let seen: serde_json::Value =
        serde_json::from_slice(&t.rec.last_body.lock().unwrap()).expect("上游收到的应是 JSON");
    assert_eq!(seen["messages"][0]["role"], "system", "developer 应被改写为 system");
    assert_eq!(seen["max_tokens"], 8192, "max_tokens 应被钳到 8192");
    assert_eq!(seen["reasoning_effort"], "high", "'max' 应被改写为 high");
    assert_eq!(*t.rec.last_path.lock().unwrap(), "/v1/chat/completions");
}

/// 回归：>2MB 的请求体必须经 HTTP 路径到达上游。
///
/// 起因：axum 的 `Bytes` 抽取器默认上限 2MB，而 Go 侧 `bufferBody` 是 32MB
/// （`internal/bai/server.go:503`）。未设 `DefaultBodyLimit` 时，2MB~32MB 的请求在本进程
/// 就被 413 挡掉，Go 却能转发。
///
/// 体量刻意选在「超 2MB、但不触发 1M token 截断」这一段：既证明不是本地 413，
/// 又能顺带断言这段体量的请求**原样**到达上游（一个字都不许动）。
#[tokio::test]
async fn large_body_reaches_upstream() {
    let t = start().await;
    let c = reqwest::Client::new();

    // 30 万个「漢」（UTF-8 每字 3 字节≈900KB）× 4 轮，序列化后约 3.6MB，落在
    // 「>2MB 且 <32MB」段内。estTokens 按字符数/4 计（4 字≈1 token），合计约 90 万
    // token < 1M，因此这一条不该被 adaptQuirks 的截断分支改写。
    let chunk = "漢".repeat(300_000);
    let body = json!({
        "model": "glm-5.3-flash",
        "messages": [
            {"role": "system", "content": chunk},
            {"role": "user", "content": chunk},
            {"role": "assistant", "content": chunk},
            {"role": "user", "content": chunk},
        ],
        "max_tokens": 4096
    });
    let raw = serde_json::to_vec(&body).unwrap();

    assert!(
        raw.len() > 2 * 1024 * 1024,
        "测试前提：本请求体必须超过 axum 默认的 2MB 上限（实际 {} 字节）",
        raw.len()
    );
    assert!(
        raw.len() < MAX_BODY_BYTES,
        "测试前提：本请求体必须在上限 {} 字节之内（实际 {} 字节）",
        MAX_BODY_BYTES,
        raw.len()
    );

    let sent = c
        .post(format!("{}/v1/chat/completions", t.base))
        .header("authorization", "Bearer sk-redacted-for-test")
        .header("content-type", "application/json")
        .body(raw.clone())
        .send()
        .await;

    // 未设 DefaultBodyLimit 时，axum 在抽取阶段就按 2MB 拒收：多数平台表现为
    // 连接中止（客户端拿不到状态码），个别平台才收到 413。两种都算失败。
    let resp = match sent {
        Ok(r) => r,
        Err(e) => panic!(
            "请求未能完成：axum 默认 2MB 体上限把 {} 字节的请求挡在本地（{}）",
            raw.len(),
            e
        ),
    };

    assert_eq!(
        resp.status(),
        200,
        "本 crate 不得因体量回 413（413 正是未设 DefaultBodyLimit 时的 axum 默认行为）"
    );
    let got = t.rec.last_body.lock().unwrap().clone();
    assert_eq!(
        got.len(),
        raw.len(),
        "上游收到的体量必须与下游发出的一致（{} 字节），而不是被本地截断",
        raw.len()
    );
    assert_eq!(got, raw, "该体量不该触发任何改写，出站字节应与入站逐字节相同");
}

/// `/model/matrix` 与 `/v1/stats` 由本进程应答，绝不漏给上游（Go 侧这两条也是本地应答）。
/// 逐字节的字段形状对照在 `tests/p1_golden.rs`；这里只守"本地应答 + 不打上游"这条路由约束。
#[tokio::test]
async fn local_endpoints_answer_locally() {
    let t = start().await;
    let c = reqwest::Client::new();

    let resp = c.get(format!("{}/model/matrix", t.base)).send().await.unwrap();
    assert_eq!(resp.status(), 200, "/model/matrix 必须本地 200");
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["ok"], false);
    // 测试夹具的渠道配置里没有 bai 条目 → 走 need_key 分支，且不打上游
    assert_eq!(v["err"], "need_key");
    assert_eq!(v["source"], "cache");
    assert_eq!(v["count"], 0);
    assert!(v["data"].is_null(), "无清单时 data 应为 null（对齐 Go RawMessage(nil)）");

    let resp = c.get(format!("{}/v1/stats", t.base)).send().await.unwrap();
    assert_eq!(resp.status(), 200, "/v1/stats 必须本地 200");
    let v: serde_json::Value = resp.json().await.unwrap();
    assert!(v["models"].is_object(), "统计形状应为 {{models,uptimeSec}}");
    assert!(v["uptimeSec"].as_i64().is_some());

    assert_eq!(
        t.rec.calls.load(Ordering::SeqCst),
        0,
        "本地端点绝不许漏给上游（Go 侧这两条是本地应答）"
    );
}

/// 非 GET 打 `/model/matrix`：405 + 纯文本 `method not allowed\n`（对齐 Go `http.Error`，
/// 它同时固定带 `X-Content-Type-Options: nosniff`）。
#[tokio::test]
async fn matrix_non_get_is_405_plain_text() {
    let t = start().await;
    let c = reqwest::Client::new();
    let resp = c
        .post(format!("{}/model/matrix", t.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/plain"),
        "405 应是纯文本（Go http.Error 口径），实得 {ct}"
    );
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "nosniff",
        "Go 的 http.Error 固定带 X-Content-Type-Options: nosniff，本条必须同形"
    );
    let body = resp.text().await.unwrap();
    assert_eq!(body, "method not allowed\n");
    assert_eq!(t.rec.calls.load(Ordering::SeqCst), 0, "405 不得打上游");
}

/// `/health` 不看方法：POST 也回 200 + health JSON（Go 的 handler 不判方法）。
#[tokio::test]
async fn health_answers_any_method() {
    let t = start().await;
    let c = reqwest::Client::new();
    for m in ["GET", "POST"] {
        let resp = if m == "POST" {
            c.post(format!("{}/health", t.base)).send().await.unwrap()
        } else {
            c.get(format!("{}/health", t.base)).send().await.unwrap()
        };
        assert_eq!(resp.status(), 200, "{m} /health 应 200（Go 的 handler 不判方法）");
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["service"], "bai-go");
        assert!(v["uptimeSec"].as_i64().is_some());
    }
    assert_eq!(t.rec.calls.load(Ordering::SeqCst), 0, "/health 不得打上游");
}

#[tokio::test]
async fn unknown_paths_are_forwarded_and_5xx_retried() {
    let t = start().await;
    let c = reqwest::Client::new();
    // 未注册路径照样转发（Go 侧 mux 的 "/" 透传）
    let resp = c.get(format!("{}/some/unknown", t.base)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(*t.rec.last_path.lock().unwrap(), "/some/unknown");

    // 5xx 重试：最多 3 次尝试
    let before = t.rec.calls.load(Ordering::SeqCst);
    let resp = c.get(format!("{}/boom", t.base)).send().await.unwrap();
    assert_eq!(resp.status(), 502, "三次都 502 时如实回 502");
    let attempts = t.rec.calls.load(Ordering::SeqCst) - before;
    assert_eq!(attempts, 3, "5xx 应重试到上限 3 次，实际 {attempts}");
}

#[test]
fn golden_files_exist() {
    // 黄金门禁的必需基准已**入库**在 rs-bai/tests/golden/（缺则回落 .work/rust-rewrite/）。
    // 这条守「入库副本别被误删」——入库副本不在、且 .work 也被清掉时，整批黄金门禁会转红。
    //
    // 第 4 轮起入库的是**小号基准**：`fixtures.small.json` / `golden-adapt-go.small.json`
    // （常规用例 F1~F6、F9~F12）。全量的那两个（65MB 工作树体积）不入库、只留 .work/，
    // 其中 F7/F8 两条超大用例在缺基准时由 `golden_adapt` 显式跳过并计数。
    for f in [
        "fixtures.small.json",
        "golden-adapt-go.small.json",
        "golden-p1-go.json",
        "golden-compact-go.json",
    ] {
        let p = rs_bai::golden_path(f);
        assert!(p.exists(), "缺黄金材料 {}", p.display());
    }
}

/// 长流式不断流：上游把 SSE 帧一个一个推，本 crate 必须一直中继下去，
/// 越过原先 reqwest 那个 600s 含响应体的总超时点。
///
/// 判定方式是"由客户端自己数帧、跨过 600s 就停"，而不是等活动结束——
/// 测试客户端不设总超时，否则失败原因会变成客户端自己的时限（第一版就栽在这里：
/// 给客户端设了 660+60=720s，比 660s 的流只多 60s，实际读了 720s 被客户端自己掐掉）。
///
/// 这条**默认不跑**（`#[ignore]`，约 10 分钟）：它证明的是"总超时已去掉"这个结构性事实。
/// 要验证时显式跑：
///   cargo test --test http_contract slow_stream -- --ignored --nocapture
/// 若将来有人给 reqwest 再加回 `.timeout(...)`，对流式 body 来说会在 600s 处断掉，
/// 这条就会失败（最后收到的帧对应的时刻早于 600s）。
#[tokio::test]
#[ignore = "耗时 600s+（> 原 600s 总超时），按需显式运行"]
async fn slow_stream_is_not_cut_by_total_timeout() {
    let t = start().await;
    // 客户端**不设总超时**：本用例要测的是本 crate 的时限，测试脚手架不能自己先到点。
    let c = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();

    let started = std::time::Instant::now();
    let resp = c
        .get(format!("{}/slow-stream", t.base))
        .send()
        .await
        .expect("上游先回响应头，长流的等待发生在响应体阶段");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let mut stream = resp.bytes_stream();
    let mut frames = 0usize;
    let mut last_frame_at = std::time::Duration::ZERO;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect(
            "帧流中断——若本 crate 有含响应体的总超时，会先在 600s 处断掉",
        );
        frames += String::from_utf8_lossy(&chunk)
            .lines()
            .filter(|l| l.starts_with("data:"))
            .count();
        last_frame_at = started.elapsed();
        if last_frame_at >= std::time::Duration::from_secs(600) {
            break;
        }
    }

    assert!(
        last_frame_at >= std::time::Duration::from_secs(600),
        "只读到 {last_frame_at:?}（{frames} 帧）就停了：未跨过原 600s 总超时点"
    );
    println!(
        "长流验证通过：跨过 600s 仍在收帧（收到第 {frames} 帧时 {last_frame_at:?}）"
    );
}
