//! A12：三种协议在真实 HTTP 路径上必须**边到边发**，而不是攒完整条上游再写。
//!
//! 假上游先吐第一帧、停顿 400ms 再吐第二帧；客户端必须在停顿结束前拿到第一帧。
//! 非流式路径仍一次性返回，字节由既有黄金门禁继续锁定。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use rs_proxy::Proxy;
use tokio::net::TcpListener;

const FIRST: &str = concat!(
    r#"{"type":"text-delta","text":"首帧"}"#,
    "\n"
);
const SECOND: &str = concat!(
    r#"{"type":"text-delta","text":"尾帧"}"#,
    "\n",
    r#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":3,"outputTokens":2,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}"#,
    "\n"
);

/// 先发 FIRST，停 400ms，再发 SECOND。
async fn slow_upstream() -> String {
    let app = Router::new().fallback(any(|| async {
        let head = futures_util::stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(FIRST.as_bytes()))
        });
        let tail = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok::<Bytes, std::io::Error>(Bytes::from_static(SECOND.as_bytes()))
        });
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(head.chain(tail)),
        )
            .into_response()
    }));
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ln.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ln, app).await.unwrap() });
    base
}

async fn start_proxy(upstream: &str) -> String {
    let mut proxy = Proxy::new("server-default-key");
    proxy.base_url = upstream.to_string();
    rs_proxy::version::set_version_for_test("9.9.9-live");
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ln.local_addr().unwrap());
    let router = rs_proxy::http::router(Arc::new(proxy));
    tokio::spawn(async move { axum::serve(ln, router).await.unwrap() });
    base
}

async fn post_stream(base: &str, path: &str, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer client-key")
        .body(body.to_string())
        .send()
        .await
        .expect("请求发不出去")
}

async fn first_chunk_before_upstream_finishes(base: &str, path: &str, body: &str, marker: &str) {
    let t0 = Instant::now();
    let resp = post_stream(base, path, body).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("text/event-stream"),
        "必须是 SSE"
    );
    let mut stream = resp.bytes_stream();
    let first = tokio::time::timeout(Duration::from_millis(300), stream.next())
        .await
        .expect("首帧被攒到上游结束之后才到（仍不是真实增量）")
        .expect("流应至少有一块")
        .expect("首块无错");
    let first_elapsed = t0.elapsed();
    assert!(
        first_elapsed < Duration::from_millis(300),
        "首帧必须在假上游 400ms 停顿结束前到达，实耗 {first_elapsed:?}"
    );
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains(marker), "首帧应含 {marker}，实得 {text}");

    let mut all = first.to_vec();
    while let Some(chunk) = stream.next().await {
        all.extend_from_slice(&chunk.expect("后续块无错"));
    }
    let all = String::from_utf8_lossy(&all);
    assert!(all.contains("尾帧"), "尾帧也必须交付：{all}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_delivers_first_frame_before_upstream_finishes() {
    let base = start_proxy(&slow_upstream().await).await;
    first_chunk_before_upstream_finishes(
        &base,
        "/v1/chat/completions",
        r#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        "首帧",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_stream_delivers_created_event_before_upstream_finishes() {
    let base = start_proxy(&slow_upstream().await).await;
    first_chunk_before_upstream_finishes(
        &base,
        "/v1/responses",
        r#"{"model":"deepseek-v4-flash","input":"hi","stream":true}"#,
        "response.created",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_stream_delivers_message_start_before_upstream_finishes() {
    let base = start_proxy(&slow_upstream().await).await;
    first_chunk_before_upstream_finishes(
        &base,
        "/v1/messages",
        r#"{"model":"deepseek-v4-flash","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        "message_start",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_stream_path_still_returns_one_json_body() {
    let base = start_proxy(&slow_upstream().await).await;
    let resp = post_stream(
        &base,
        "/v1/chat/completions",
        r#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let text = resp.text().await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("非流式必须是一份完整 JSON");
    assert_eq!(v["choices"][0]["message"]["content"], "首帧尾帧");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_cancel_after_first_frame_does_not_block_or_panic() {
    let base = start_proxy(&slow_upstream().await).await;
    let resp = post_stream(
        &base,
        "/v1/chat/completions",
        r#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.expect("首块").expect("首块无错");
    assert!(String::from_utf8_lossy(&first).contains("首帧"));
    drop(stream); // 客户端取消
    // 取消后服务端仍在运行：再发一次普通请求必须正常完成。
    let again = post_stream(
        &base,
        "/v1/chat/completions",
        r#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(again.status().as_u16(), 200);
    assert!(again.text().await.unwrap().contains("首帧尾帧"));
}
