//! A06：码道 HTTP/JSON/SSE 业务错误在提交前走同一账号策略。

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use rs_codearts::accounts::AccountManager;
use rs_codearts::config::Account;
use serde_json::{json, Value};

const NORMAL_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
const ERROR_JSON: &str = r#"{"error_code":"APIG.0602.400","error_msg":"the security token has expired"}"#;
const ERROR_SSE: &str = "data: {\"error_code\":\"APIG.0602.400\",\"error_msg\":\"the security token has expired\"}\n\n";

struct FakeUpstream {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    async fn start(mode: u8) -> Self {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let mode = Arc::new(Mutex::new(mode));
        let app = Router::new()
            .fallback(any(respond))
            .with_state((hits.clone(), mode.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, hits, task }
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(
    State((hits, mode)): State<(Arc<Mutex<Vec<String>>>, Arc<Mutex<u8>>)>,
    req: Request,
) -> Response {
    let token = req
        .headers()
        .get("x-security-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    hits.lock().unwrap().push(token.clone());
    if token == "TOK-A" {
        match *mode.lock().unwrap() {
            0 => (
                axum::http::StatusCode::BAD_REQUEST,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                Body::from(ERROR_JSON),
            )
                .into_response(),
            1 => (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                Body::from(ERROR_JSON),
            )
                .into_response(),
            2 => (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                Body::from(ERROR_SSE),
            )
                .into_response(),
            3 => (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                Body::from(format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"partial\"}}}}]}}\n\n{ERROR_SSE}"
                )),
            )
                .into_response(),
            4 => (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                Body::from(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"weather\",\"arguments\":\"{}\"}}]}}]}\n\ndata: [DONE]\n\n",
                ),
            )
                .into_response(),
            5 => {
                use axum::body::Bytes;
                use futures_util::StreamExt;
                let head = futures_util::stream::once(async move {
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
                    ))
                });
                let stalled = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(head.chain(stalled)),
                )
                    .into_response()
            }
            _ => unreachable!(),
        }
    } else {
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            Body::from(NORMAL_SSE),
        )
            .into_response()
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rs-codearts-chat-recovery-{tag}-{stamp}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn account(ak: &str, token: &str, name: &str) -> Account {
    Account {
        name: name.into(),
        enabled: true,
        access_key: ak.into(),
        secret_key: format!("sk-{ak}"),
        security_token: token.into(),
        expires_at: Some("2099-01-01T00:00:00Z".into()),
        ..Default::default()
    }
}

struct Fixture {
    server: Arc<rs_codearts::Server>,
    base: String,
    upstream: FakeUpstream,
    _dir: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start(mode: u8) -> Self {
        let dir = temp_dir(&mode.to_string());
        let path = dir.join("codearts-accounts.json");
        let manager = AccountManager::new(path.clone());
        manager.upsert(account("AK-A", "TOK-A", "A")).unwrap();
        manager.upsert(account("AK-B", "TOK-B", "B")).unwrap();
        let upstream = FakeUpstream::start(mode).await;
        let server = rs_codearts::Server::new(
                rs_codearts::upstream::Upstream::new(&upstream.base).unwrap(),
                path,
                false,
                "http://127.0.0.1:1".into(),
                false,
                3,
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn({
            let server = server.clone();
            async move { axum::serve(listener, rs_codearts::router(server)).await.unwrap() }
        });
        Self { server, base, upstream, _dir: dir, task }
    }

    async fn chat(&self, stream: bool) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&json!({
                "model":"glm-5.3",
                "messages":[{"role":"user","content":"hi"}],
                "stream":stream
            }))
            .send()
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_json_and_sse_errors_share_account_recovery_policy() {
    for mode in 0..=2 {
        let fx = Fixture::start(mode).await;
        let response = fx.chat(false).await;
        assert_eq!(response.status(), 200, "mode {mode}: {}", response.text().await.unwrap());
        assert_eq!(
            *fx.upstream.hits.lock().unwrap(),
            vec!["TOK-A".to_string(), "TOK-B".to_string()],
            "mode {mode} 应在提交前换号"
        );
        let accounts: Value = reqwest::Client::new()
            .get(format!("{}/accounts", fx.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(accounts["accounts"][0]["dead"], true, "mode {mode} 应标记当前凭据失效");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonstream_late_error_recovers_before_commit_but_stream_never_replays_after_head() {
    let nonstream = Fixture::start(3).await;
    let response = nonstream.chat(false).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"content\":\"ok\""), "{body}");
    assert!(!body.contains("partial"), "A 的部分内容不得与 B 拼接: {body}");
    assert_eq!(
        *nonstream.upstream.hits.lock().unwrap(),
        vec!["TOK-A".to_string(), "TOK-B".to_string()]
    );

    let stream = Fixture::start(3).await;
    let response = stream.chat(true).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("partial"), "{body}");
    assert!(body.contains("APIG.0602"), "{body}");
    assert_eq!(*stream.upstream.hits.lock().unwrap(), vec!["TOK-A".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_tool_frame_is_not_treated_as_business_error() {
    let fx = Fixture::start(4).await;
    let response = fx.chat(true).await;
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("tool_calls"), "{body}");
    assert_eq!(*fx.upstream.hits.lock().unwrap(), vec!["TOK-A".to_string()]);
    let _ = fx.server;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_disconnect_after_first_delivery_does_not_resend() {
    let fx = Fixture::start(5).await;
    let mut response = fx.chat(true).await;
    assert_eq!(response.status(), 200);
    let first = response.chunk().await.unwrap().unwrap();
    assert!(first.windows(5).any(|w| w == b"first"), "{first:?}");
    drop(response);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(*fx.upstream.hits.lock().unwrap(), vec!["TOK-A".to_string()]);
}
