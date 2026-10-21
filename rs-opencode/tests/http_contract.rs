//! tests/http_contract.rs —— 假 opencode 上游 + 本插件的 HTTP 契约用例。
//!
//! 覆盖任务简报 §七 的验收项：
//! * 消息压平（字符串/数组/空/多轮/tool 结果）——`translate` 单测 + 这里的 400 用例；
//! * `/v1/models` 的 provider 白名单 + denylist 过滤；
//! * SSE：text delta / reasoning delta / finish / `[DONE]`；
//! * 429 判定 + 冷却轮换；
//! * 国家限制 → 400；
//! * 未安装 opencode → `/health` 的 reason + `/v1/*` 503；
//! * `permission.asked` 必须回 `reject`。
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{stream, Stream};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use rs_opencode::config::Config;
use rs_opencode::Server;

// ------------------------------ 假上游 ------------------------------

#[derive(Clone, Debug)]
enum Behavior {
    Success,
    Quota,
    Country,
    Permission,
    /// 只发 execution.started，不发 terminal 事件：验证单轮总超时 → 504。
    Hang,
    /// 只发 execution.succeeded，不发文本增量：验证最终消息兜底。
    TextOnly,
}

#[derive(Clone)]
struct Fake {
    base: String,
    tx: broadcast::Sender<String>,
    models: Arc<Vec<Value>>,
    behaviors: Arc<HashMap<String, Behavior>>,
    session_models: Arc<Mutex<HashMap<String, String>>>,
    requests: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<Vec<String>>>,
    seq: Arc<AtomicU64>,
}

fn quota_error() -> Value {
    json!({
        "type": "provider.quota",
        "message": "Rate limit exceeded. Please try again later.",
        "status": 429,
        "response": {
            "body": "{\"type\":\"error\",\"error\":{\"type\":\"FreeUsageLimitError\",\"message\":\"Rate limit exceeded. Please try again later.\"}}"
        }
    })
}

fn country_error() -> Value {
    json!({
        "type": "provider.error",
        "message": "This model is not available in your country",
        "status": 400,
        "response": {
            "body": "{\"type\":\"error\",\"error\":{\"type\":\"ModelRegionError\",\"message\":\"This model is not available in your country\"}}"
        }
    })
}

fn fake_models() -> Vec<Value> {
    vec![
        json!({"id":"model-a","providerID":"opencode","modelID":"model-a","enabled":true,"name":"A"}),
        json!({"id":"model-b","providerID":"opencode","modelID":"model-b","enabled":true,"name":"B"}),
        json!({"id":"model-p","providerID":"opencode","modelID":"model-p","enabled":true}),
        json!({"id":"model-t","providerID":"opencode","modelID":"model-t","enabled":true}),
        json!({"id":"beam-free","providerID":"opencode","modelID":"beam-free","enabled":true}),
        json!({"id":"go-1","providerID":"opencode-go","modelID":"go-1","enabled":true}),
        json!({"id":"other","providerID":"other","modelID":"other","enabled":true}),
        json!({"id":"disabled","providerID":"opencode","modelID":"disabled","enabled":false}),
    ]
}

impl Fake {
    async fn start(behaviors: &[(&str, Behavior)]) -> Fake {
        let (tx, _) = broadcast::channel::<String>(256);
        let fake = Fake {
            base: String::new(),
            tx,
            models: Arc::new(fake_models()),
            behaviors: Arc::new(
                behaviors
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            ),
            session_models: Arc::new(Mutex::new(HashMap::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicU64::new(1)),
        };

        let app = Router::new()
            .route("/api/info", get(info))
            .route("/api/model", get(model_list))
            .route("/api/session", post(create_session))
            .route("/api/session/{id}", delete(delete_session))
            .route("/api/session/{id}/model", post(set_model))
            .route("/api/session/{id}/prompt", post(prompt))
            .route("/api/session/{id}/message", get(messages))
            .route(
                "/api/session/{id}/permission/{pid}/reply",
                post(reply_permission),
            )
            .route("/api/event", get(events))
            .with_state(fake.clone());

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("假上游端口");
        let addr = listener.local_addr().expect("假上游地址");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Fake {
            base: format!("http://{addr}"),
            ..fake
        }
    }

    fn model_requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("requests lock")
            .iter()
            .filter(|r| r.starts_with("model "))
            .cloned()
            .collect()
    }

    fn permission_replies(&self) -> Vec<String> {
        self.replies.lock().expect("replies lock").clone()
    }

    fn behavior(&self, session: &str) -> Behavior {
        let key = self
            .session_models
            .lock()
            .expect("session model lock")
            .get(session)
            .cloned()
            .unwrap_or_else(|| "opencode/model-b".to_string());
        self.behaviors
            .get(&key)
            .cloned()
            .unwrap_or(Behavior::Success)
    }
}

async fn info() -> Json<Value> {
    Json(json!({"version": "fake-1.0.0", "pid": 1, "urls": ["http://127.0.0.1:1"]}))
}

async fn model_list(State(state): State<Fake>) -> Json<Value> {
    Json(json!({"data": (*state.models).clone()}))
}

async fn create_session(State(state): State<Fake>) -> Json<Value> {
    let n = state.seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("ses_test_{n}");
    state
        .requests
        .lock()
        .expect("requests lock")
        .push("POST /api/session".to_string());
    Json(json!({"data": {"id": id}}))
}

async fn set_model(
    State(state): State<Fake>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> StatusCode {
    let provider = body
        .pointer("/model/providerID")
        .and_then(Value::as_str)
        .unwrap_or("");
    let model_id = body
        .pointer("/model/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let key = format!("{provider}/{model_id}");
    state
        .session_models
        .lock()
        .expect("session model lock")
        .insert(id, key.clone());
    state
        .requests
        .lock()
        .expect("requests lock")
        .push(format!("model {key}"));
    StatusCode::NO_CONTENT
}

async fn prompt(
    State(state): State<Fake>,
    Path(id): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    state
        .requests
        .lock()
        .expect("requests lock")
        .push(format!("prompt {id}"));
    let behavior = state.behavior(&id);
    let tx = state.tx.clone();
    tokio::spawn(async move {
        emit_sequence(tx, id, behavior).await;
    });
    Json(json!({"data": {"id": "msg_user"}}))
}

async fn messages(State(state): State<Fake>, Path(id): Path<String>) -> Json<Value> {
    let data = match state.behavior(&id) {
        Behavior::Quota => {
            vec![json!({"type": "assistant", "finish": "error", "error": quota_error()})]
        }
        Behavior::Country => {
            vec![json!({"type": "assistant", "finish": "error", "error": country_error()})]
        }
        _ => vec![json!({
            "type": "assistant",
            "finish": "stop",
            "content": [{"type": "text", "text": "Hello"}]
        })],
    };
    Json(json!({"data": data}))
}

async fn reply_permission(
    State(state): State<Fake>,
    Path((_id, pid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    let decision = body.get("decision").and_then(Value::as_str).unwrap_or("");
    state
        .replies
        .lock()
        .expect("replies lock")
        .push(format!("{pid}:{decision}"));
    StatusCode::NO_CONTENT
}

async fn delete_session(State(state): State<Fake>, Path(id): Path<String>) -> StatusCode {
    state
        .session_models
        .lock()
        .expect("session model lock")
        .remove(&id);
    StatusCode::NO_CONTENT
}

async fn events(
    State(state): State<Fake>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.tx.subscribe();
    let s = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(text) => return Some((Ok(SseEvent::default().data(text)), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(s)
}

async fn emit_sequence(tx: broadcast::Sender<String>, sid: String, behavior: Behavior) {
    tokio::time::sleep(Duration::from_millis(30)).await;
    let send = |ty: &str, data: Value| {
        let _ = tx.send(json!({"type": ty, "data": data}).to_string());
    };
    match behavior {
        Behavior::Success => {
            send(
                "session.execution.started",
                json!({"sessionID": sid.clone()}),
            );
            send(
                "session.reasoning.delta",
                json!({"sessionID": sid.clone(), "delta": "think"}),
            );
            send(
                "session.text.delta",
                json!({"sessionID": sid.clone(), "delta": "Hel"}),
            );
            send(
                "session.text.delta",
                json!({"sessionID": sid.clone(), "delta": "lo"}),
            );
            send(
                "session.step.ended",
                json!({"sessionID": sid.clone(), "tokens": {"input": 10, "output": 3, "reasoning": 2}}),
            );
            send("session.execution.succeeded", json!({"sessionID": sid}));
        }
        Behavior::Quota => {
            let err = quota_error();
            send(
                "session.step.failed",
                json!({"sessionID": sid.clone(), "error": err.clone()}),
            );
            send(
                "session.execution.failed",
                json!({"sessionID": sid, "error": err}),
            );
        }
        Behavior::Country => {
            send(
                "session.execution.failed",
                json!({"sessionID": sid, "error": country_error()}),
            );
        }
        Behavior::Permission => {
            send(
                "permission.asked",
                json!({"sessionID": sid.clone(), "id": "perm_1"}),
            );
            tokio::time::sleep(Duration::from_millis(150)).await;
            send(
                "session.text.delta",
                json!({"sessionID": sid.clone(), "delta": "ok"}),
            );
            send("session.execution.succeeded", json!({"sessionID": sid}));
        }
        Behavior::Hang => {
            // 不发 succeeded/failed/interrupted，SSE 连接保持打开；
            // 本插件应在 timeoutSeconds 后杀会话并回 504。
            send("session.execution.started", json!({"sessionID": sid}));
        }
        Behavior::TextOnly => {
            send("session.execution.succeeded", json!({"sessionID": sid}));
        }
    }
}

// ------------------------------ 本插件测试夹具 ------------------------------

fn test_config() -> Config {
    Config {
        providers: vec!["opencode".to_string(), "opencode-go".to_string()],
        model_denylist: vec!["beam-free".to_string()],
        default_model: "opencode/model-b".to_string(),
        timeout_seconds: 20,
        cooldown_seconds: 60,
    }
}

async fn boot(fake: &Fake) -> String {
    boot_with(fake, test_config()).await
}

async fn boot_with(fake: &Fake, cfg: Config) -> String {
    let srv = Server::with_upstream(cfg, &fake.base, "fake-password", "fake-1.0.0");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("插件端口");
    let addr = listener.local_addr().expect("插件地址");
    tokio::spawn(async move {
        let _ = axum::serve(listener, rs_opencode::router(srv)).await;
    });
    format!("http://{addr}")
}

async fn boot_not_ready(reason: &str) -> String {
    let srv = Server::not_ready(Config::default(), reason);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("插件端口");
    let addr = listener.local_addr().expect("插件地址");
    tokio::spawn(async move {
        let _ = axum::serve(listener, rs_opencode::router(srv)).await;
    });
    format!("http://{addr}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("测试 HTTP 客户端")
}

async fn get_json(base: &str, path: &str) -> (u16, Value) {
    let resp = client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .expect("GET 请求");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value)
}

async fn post_chat(base: &str, body: Value) -> (u16, Value) {
    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).expect("序列化请求"))
        .send()
        .await
        .expect("POST chat");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value)
}

async fn post_chat_raw(base: &str, body: Value) -> (u16, String) {
    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body).expect("序列化请求"))
        .send()
        .await
        .expect("POST chat");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, text)
}

// ------------------------------ 用例 ------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_list_filters_provider_and_denylist() {
    let fake = Fake::start(&[]).await;
    let base = boot(&fake).await;

    let (status, j) = get_json(&base, "/v1/models").await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["object"], "list");
    let ids: Vec<&str> = j["data"]
        .as_array()
        .expect("data 数组")
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .collect();
    assert_eq!(
        ids,
        vec![
            "opencode/model-a",
            "opencode/model-b",
            "opencode/model-p",
            "opencode/model-t",
            "opencode-go/go-1"
        ],
        "beam-free / other provider / disabled 都应被过滤：{j}"
    );
    assert_eq!(j["data"][0]["created"], 0);
    assert_eq!(j["data"][0]["owned_by"], "opencode");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_ready_reports_version_and_filtered_model_count() {
    let fake = Fake::start(&[]).await;
    let base = boot(&fake).await;

    let (status, j) = get_json(&base, "/health").await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["ok"], true, "{j}");
    assert_eq!(j["opencode"], "fake-1.0.0");
    assert_eq!(j["models"], 5, "过滤后应为 5 个：{j}");
    assert_eq!(j["ready"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_stream_success_shape_and_usage() {
    let fake = Fake::start(&[]).await;
    let base = boot(&fake).await;

    let (status, j) = post_chat(
        &base,
        json!({
            "model": "opencode/model-b",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [{"type": "text", "text": "hi"}]}
            ]
        }),
    )
    .await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["object"], "chat.completion");
    assert_eq!(j["choices"][0]["message"]["role"], "assistant");
    assert_eq!(j["choices"][0]["message"]["content"], "Hello");
    assert_eq!(j["choices"][0]["message"]["reasoning_content"], "think");
    assert_eq!(j["choices"][0]["finish_reason"], "stop");
    assert_eq!(j["usage"]["prompt_tokens"], 10);
    assert_eq!(j["usage"]["completion_tokens"], 5, "output 3 + reasoning 2");
    assert_eq!(j["usage"]["total_tokens"], 15);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_translates_text_reasoning_and_done() {
    let fake = Fake::start(&[]).await;
    let base = boot(&fake).await;

    let (status, text) = post_chat_raw(
        &base,
        json!({
            "model": "opencode/model-b",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert!(
        text.contains("\"object\":\"chat.completion.chunk\""),
        "{text}"
    );
    assert!(
        text.trim_end().ends_with("data: [DONE]"),
        "结尾必须是 [DONE]：{text}"
    );

    let frames: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();

    assert_eq!(
        frames[0]["choices"][0]["delta"]["role"], "assistant",
        "首帧必须是 role：{text}"
    );
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = None;
    for f in &frames {
        let delta = &f["choices"][0]["delta"];
        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            content.push_str(c);
        }
        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
            reasoning.push_str(r);
        }
        if let Some(fr) = f["choices"][0]["finish_reason"].as_str() {
            finish = Some(fr.to_string());
        }
    }
    assert_eq!(content, "Hello");
    assert_eq!(reasoning, "think");
    assert_eq!(finish.as_deref(), Some("stop"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_marks_cooling_and_rotates_next_request() {
    let fake = Fake::start(&[("opencode/model-a", Behavior::Quota)]).await;
    let base = boot(&fake).await;

    let (s1, j1) = post_chat(
        &base,
        json!({
            "model": "opencode/model-a",
            "messages": [{"role": "user", "content": "x"}]
        }),
    )
    .await;
    assert_eq!(s1, 429, "{j1}");
    assert_eq!(j1["error"]["type"], "rate_limit_error");
    assert!(
        j1["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Rate limit exceeded"),
        "{j1}"
    );

    // 同一模型第二次：它已冷却，应自动换默认模型 model-b 并成功。
    let (s2, j2) = post_chat(
        &base,
        json!({
            "model": "opencode/model-a",
            "messages": [{"role": "user", "content": "x"}]
        }),
    )
    .await;
    assert_eq!(s2, 200, "{j2}");
    assert_eq!(j2["choices"][0]["message"]["content"], "Hello");
    assert_eq!(
        fake.model_requests(),
        vec!["model opencode/model-a", "model opencode/model-b"],
        "第一次用 model-a，429 后第二次应轮换到 model-b"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_rate_limit_returns_429_before_headers() {
    let fake = Fake::start(&[("opencode/model-a", Behavior::Quota)]).await;
    let base = boot(&fake).await;

    let (status, text) = post_chat_raw(
        &base,
        json!({
            "model": "opencode/model-a",
            "messages": [{"role": "user", "content": "x"}],
            "stream": true
        }),
    )
    .await;
    assert_eq!(status, 429, "流式也应在发 SSE 头之前识别 429：{text}");
    let j: Value = serde_json::from_str(&text).expect("429 body 是 JSON");
    assert_eq!(j["error"]["type"], "rate_limit_error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn country_restriction_maps_to_400() {
    let fake = Fake::start(&[("opencode/model-a", Behavior::Country)]).await;
    let base = boot(&fake).await;

    let (status, j) = post_chat(
        &base,
        json!({
            "model": "opencode/model-a",
            "messages": [{"role": "user", "content": "x"}]
        }),
    )
    .await;
    assert_eq!(status, 400, "{j}");
    assert_eq!(j["error"]["type"], "invalid_request_error");
    assert!(
        j["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not available in your country"),
        "{j}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_asked_is_rejected() {
    let fake = Fake::start(&[("opencode/model-p", Behavior::Permission)]).await;
    let base = boot(&fake).await;

    let (status, j) = post_chat(
        &base,
        json!({
            "model": "opencode/model-p",
            "messages": [{"role": "user", "content": "x"}]
        }),
    )
    .await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "ok");
    assert_eq!(
        fake.permission_replies(),
        vec!["perm_1:reject"],
        "权限询问必须固定 reject，绝不能 approve"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_permission_asked_is_rejected_too() {
    let fake = Fake::start(&[("opencode/model-p", Behavior::Permission)]).await;
    let base = boot(&fake).await;

    let (status, text) = post_chat_raw(
        &base,
        json!({
            "model": "opencode/model-p",
            "messages": [{"role": "user", "content": "x"}],
            "stream": true
        }),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("\"content\":\"ok\""), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
    assert_eq!(
        fake.permission_replies(),
        vec!["perm_1:reject"],
        "流式预热阶段也必须固定 reject"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_message_fallback_when_no_text_deltas() {
    let fake = Fake::start(&[("opencode/model-t", Behavior::TextOnly)]).await;
    let base = boot(&fake).await;

    let (status, j) = post_chat(
        &base,
        json!({
            "model": "opencode/model-t",
            "messages": [{"role": "user", "content": "x"}]
        }),
    )
    .await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(
        j["choices"][0]["message"]["content"], "Hello",
        "没有 text.delta 时应从 /message 取最终内容：{j}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_timeout_returns_504() {
    let fake = Fake::start(&[("opencode/model-a", Behavior::Hang)]).await;
    let mut cfg = test_config();
    cfg.timeout_seconds = 1;
    let base = boot_with(&fake, cfg).await;

    let started = std::time::Instant::now();
    let (status, j) = post_chat(
        &base,
        json!({
            "model": "opencode/model-a",
            "messages": [{"role": "user", "content": "x"}]
        }),
    )
    .await;
    assert_eq!(status, 504, "{j}");
    assert_eq!(j["error"]["type"], "timeout_error");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "超时后不应继续挂住：{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_text_is_rejected_with_400() {
    let fake = Fake::start(&[]).await;
    let base = boot(&fake).await;

    for body in [
        json!({"messages": []}),
        json!({"messages": [{"role": "user", "content": ""}]}),
        json!({"messages": [{"role": "user", "content": [{"type": "image_url"}]}]}),
        json!({"messages": [{"role": "user", "content": null}]}),
        json!({"model": "opencode/model-b"}),
    ] {
        let (status, j) = post_chat(&base, body.clone()).await;
        assert_eq!(status, 400, "输入 {body} → {j}");
        assert_eq!(j["error"]["type"], "invalid_request_error");
    }

    let (status, j) = post_chat(
        &base,
        json!({"messages": [{"role": "user", "content": "   "}]}),
    )
    .await;
    assert_eq!(status, 400, "纯空白也应视为空：{j}");
    assert_eq!(j["error"]["message"], "request contains no text content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_installed_reports_health_reason_and_503() {
    let base = boot_not_ready("opencode_not_installed").await;

    let (status, j) = get_json(&base, "/health").await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["ok"], false);
    assert_eq!(j["reason"], "opencode_not_installed");

    let (status, j) = get_json(&base, "/v1/models").await;
    assert_eq!(status, 503, "{j}");
    assert_eq!(j["error"]["type"], "service_unavailable");

    let (status, j) = post_chat(
        &base,
        json!({"messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, 503, "{j}");
    assert_eq!(j["error"]["type"], "service_unavailable");
    assert!(
        j["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("opencode_not_installed"),
        "{j}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_route_is_go_style_404() {
    let base = boot_not_ready("opencode_not_installed").await;
    let resp = client()
        .get(format!("{base}/nope"))
        .send()
        .await
        .expect("GET");
    assert_eq!(resp.status().as_u16(), 404);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(ct, "text/plain; charset=utf-8");
    assert_eq!(
        resp.text().await.unwrap_or_default(),
        "404 page not found\n"
    );
}
