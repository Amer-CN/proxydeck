// tests/cloud_contract.rs —— 云直连（`cloud.rs` + `cloud_chat.rs`）的合同测试。
//
// 用进程内假云（axum）复刻官方三步协议：models/available → conversation → task → execute(SSE)，
// 断言的是**我们发给云端的字节**与**翻回客户端的 OpenAI 形状**，不碰真网。
// 真机验证（2026-11-15）见 `README.md` 的「云直连」一节。

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use common::{client, write_license, AppServer, TestDir};
use rs_comate::cloud::CloudModel;
use rs_comate::{AppState, Transport};
use serde_json::{json, Value};
use tokio::sync::Mutex;

// ---------- 假云 ----------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Tool,
}

struct FakeCloudState {
    hits: Mutex<Vec<(String, String)>>, // (path, body)
    mode: Mutex<Mode>,
}

struct FakeCloud {
    base: String,
    state: Arc<FakeCloudState>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeCloud {
    async fn start() -> Self {
        let state = Arc::new(FakeCloudState {
            hits: Mutex::new(Vec::new()),
            mode: Mutex::new(Mode::Text),
        });
        let app = Router::new()
            .route("/api/v2/api/models/available", get(models))
            .route(
                "/api/aidevops/autocomate/rest/autowork/v2/conversation",
                any(conversation),
            )
            .route("/api/aidevops/autocomate/rest/autowork/v2/task", any(task))
            .route("/api/aidevops/autocomate/rest/autowork/v2/execute", any(execute))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base: format!("http://{addr}"),
            state,
            handle,
        }
    }

    async fn set_mode(&self, m: Mode) {
        *self.state.mode.lock().await = m;
    }

    async fn hits(&self) -> Vec<(String, String)> {
        self.state.hits.lock().await.clone()
    }

    async fn take_hits(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.state.hits.lock().await)
    }
}

impl Drop for FakeCloud {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn models() -> Response {
    let list = vec![
        json!({"modelId":"glm-5.3_37c550fc6d5b4fb2825abc2c1c88a6b3","displayName":"GLM-5.3","modelType":"glm-5.3","provider":"anthropic","supportImage":false,"supportThinking":true,"isDefault":false}),
        json!({"modelId":"deepseek-v4.1-flash_4827e158760447029e6594bb2b1ab066","displayName":"Deepseek V4.1 Flash","modelType":"deepseek-v4.1-flash","provider":"anthropic","supportImage":true,"supportThinking":true,"isDefault":true}),
        json!({"modelId":"ernie-5.5-flash-preview_996d9d76bb7c45ecbff286216aaeeab2","displayName":"ERNIE 5.5 Flash Preview","modelType":"ernie-5.5-flash-preview","provider":"anthropic","supportImage":true,"supportThinking":true}),
    ];
    axum::Json(json!({"status":"OK","message":"操作成功！","data":list})).into_response()
}

async fn conversation(
    State(st): State<Arc<FakeCloudState>>,
    body: String,
) -> Response {
    st.hits
        .lock()
        .await
        .push(("/conversation".into(), body));
    axum::Json(json!({"code":200,"message":null,"data":{"id":11},"traceId":""})).into_response()
}

async fn task(State(st): State<Arc<FakeCloudState>>, body: String) -> Response {
    st.hits.lock().await.push(("/task".into(), body));
    axum::Json(json!({"code":200,"message":null,"data":{"taskId":22},"traceId":""})).into_response()
}

async fn execute(State(st): State<Arc<FakeCloudState>>, body: String) -> Response {
    st.hits.lock().await.push(("/execute".into(), body));
    let mode = *st.mode.lock().await;
    let sse = match mode {
        Mode::Text => vec![
            r#"data: {"content":{"type":"ANSWER","detail":{"delta":"","reasoningDelta":"想","end":false},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"ANSWER","detail":{"delta":"你","reasoningDelta":"","end":false},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"ANSWER","detail":{"delta":"好","reasoningDelta":"","end":true},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"TOKEN_USAGE","detail":{"usage":{"inputTokens":10,"cachedInputTokens":2,"outputTokens":5}},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"ANSWER","detail":{"delta":"","reasoningDelta":"","end":true},"end":true},"end":true}"#.to_string(),
        ],
        Mode::Tool => vec![
            r#"data: {"content":{"type":"ANSWER","detail":{"delta":"","reasoningDelta":"我得查一下","end":false},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"FUNCTION_CALL_START","detail":{"toolUse":[{"input":{},"name":"UseMcpTool","id":"call_x","type":"tool_use","version":12}]},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"arguments":"{\"ci"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"arguments":"ty\":\"上海\"}"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"server_name":"openai_client"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"tool_name":"get_weather"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"FUNCTION_CALL_END","detail":{"toolUse":[{"input":{},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"TOKEN_USAGE","detail":{"usage":{"inputTokens":11,"cachedInputTokens":7,"outputTokens":20}},"end":false},"end":false}"#.to_string(),
            r#"data: {"content":{"type":"ANSWER","detail":{"delta":"","reasoningDelta":"","end":true},"end":true},"end":true}"#.to_string(),
        ],
    };
    let mut body = String::new();
    for line in sse {
        body.push_str(&line);
        body.push_str("\n\n");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

// ---------- 被测 App ----------

async fn cloud_app(cloud: &FakeCloud) -> (AppServer, TestDir) {
    let dir = TestDir::new("cloud-contract");
    let lic = write_license(&dir, "cli-login.json", "test-license-0001");
    let mut st = AppState::new(lic, None, None, None);
    st.transport = Transport::Cloud;
    st.cloud_base = cloud.base.clone();
    let app = AppServer::start(Arc::new(st)).await;
    (app, dir)
}

async fn post_chat(base: &str, body: &str) -> (u16, String) {
    let resp = client()
        .request(Method::POST, format!("{base}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("发请求");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, text)
}

// ---------- 用例 ----------

#[tokio::test]
async fn models_come_from_cloud_catalog() {
    let cloud = FakeCloud::start().await;
    let (app, _dir) = cloud_app(&cloud).await;
    let resp = client()
        .get(format!("{}/v1/models", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let text = resp.text().await.unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"auto"), "{ids:?}");
    // 展示去后缀短名（与 zulu 路径口径一致）
    assert!(ids.contains(&"glm-5.3"), "{ids:?}");
    assert!(ids.contains(&"deepseek-v4.1-flash"), "{ids:?}");
    assert!(ids.contains(&"ernie-5.5-flash-preview"), "{ids:?}");
}

#[tokio::test]
async fn nonstream_chat_returns_content_usage_and_reasoning() {
    let cloud = FakeCloud::start().await;
    let (app, _dir) = cloud_app(&cloud).await;
    let body = r#"{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"你好"}],"stream":false}"#;
    let (status, text) = post_chat(&app.base, body).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let msg = &v["choices"][0]["message"];
    assert_eq!(msg["content"], "你好");
    assert_eq!(msg["reasoning_content"], "想");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    // prompt = input + cached = 10 + 2
    assert_eq!(v["usage"]["prompt_tokens"], 12);
    assert_eq!(v["usage"]["completion_tokens"], 5);
    assert_eq!(v["usage"]["total_tokens"], 17);
    assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 2);

    // 出站：三步都打了，modelKey 是完整 modelId
    let hits = cloud.take_hits().await;
    let paths: Vec<&str> = hits.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, vec!["/conversation", "/task", "/execute"]);
    let ex: Value = serde_json::from_str(&hits[2].1).unwrap();
    assert_eq!(ex["modelKey"], "deepseek-v4.1-flash_4827e158760447029e6594bb2b1ab066");
    assert_eq!(ex["conversationId"], 11);
    assert_eq!(ex["taskId"], 22);
    assert_eq!(ex["isFirstQuery"], true);
    assert_eq!(ex["hasMcp"], false);
    assert_eq!(ex["toolUseResults"][0]["id"], "init");
    // 认证头
    let conv: Value = serde_json::from_str(&hits[0].1).unwrap();
    assert_eq!(conv["uploadBaseInfo"]["license"], "test-license-0001");
    // plugin-version 跟着本机安装的 IDE 扩展版本走（`zulucli-1.8.1` 是 @comate/zulu 包版本，
    // 不是官方上报的那个字段；2026-11-21 按 magpie-comate PROTOCOL.md 纠正）。
    let expect = rs_comate::cloud::resolve_plugin_version();
    assert_eq!(conv["pluginVersion"], expect.as_str());
    assert!(
        expect.starts_with("zulucli-"),
        "形态必须是 zulucli-<version>：{expect}"
    );
}

#[tokio::test]
async fn stream_chat_emits_openai_chunks() {
    let cloud = FakeCloud::start().await;
    let (app, _dir) = cloud_app(&cloud).await;
    let body = r#"{"model":"glm-5.3","messages":[{"role":"user","content":"你好"}],"stream":true}"#;
    let (status, text) = post_chat(&app.base, body).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.contains(r#"{"content":"你"}"#), "{text}");
    assert!(text.contains(r#"{"content":"好"}"#), "{text}");
    assert!(text.contains(r#""reasoning_content":"想""#), "{text}");
    assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
    assert!(text.contains(r#""prompt_tokens":12"#), "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");
}

#[tokio::test]
async fn tool_call_then_tool_result_resumes_same_session() {
    let cloud = FakeCloud::start().await;
    cloud.set_mode(Mode::Tool).await;
    let (app, _dir) = cloud_app(&cloud).await;

    // 第一轮：带工具，模型要调 get_weather
    let first = r#"{"model":"glm-5.3","messages":[{"role":"user","content":"上海天气？"}],
        "tools":[{"type":"function","function":{"name":"get_weather","description":"查天气","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],
        "stream":false}"#;
    let (status, text) = post_chat(&app.base, first).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let msg = &v["choices"][0]["message"];
    let calls = msg["tool_calls"].as_array().expect("tool_calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], "call_x");
    assert_eq!(calls[0]["function"]["name"], "get_weather");
    assert_eq!(calls[0]["function"]["arguments"], r#"{"city":"上海"}"#);
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(msg["reasoning_content"], "我得查一下");

    // 出站：agentInfo 里声明了假 MCP 服务器
    let hits = cloud.take_hits().await;
    let ex: Value = serde_json::from_str(&hits[2].1).unwrap();
    assert_eq!(ex["hasMcp"], true);
    assert_eq!(ex["agentInfo"]["toolNames"][0], "use_mcp_tool");
    assert_eq!(ex["agentInfo"]["mcpInfos"][0]["serverName"], "openai_client");
    assert_eq!(ex["agentInfo"]["mcpInfos"][0]["tools"][0]["name"], "get_weather");

    // 第二轮：把工具结果带回来 → 必须续同一个 conversation/task，query 为空
    cloud.set_mode(Mode::Text).await;
    let second = r#"{"model":"glm-5.3","messages":[
        {"role":"user","content":"上海天气？"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_x","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"上海\"}"}}]},
        {"role":"tool","tool_call_id":"call_x","content":"晴，18 度"}
      ],"tools":[{"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],
      "stream":false}"#;
    let (status, text) = post_chat(&app.base, second).await;
    assert_eq!(status, 200, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "你好");

    let hits = cloud.take_hits().await;
    assert_eq!(hits.len(), 1, "续接不该再建会话: {hits:?}");
    assert_eq!(hits[0].0, "/execute");
    let ex: Value = serde_json::from_str(&hits[0].1).unwrap();
    assert_eq!(ex["query"], "");
    assert_eq!(ex["conversationId"], 11);
    assert_eq!(ex["taskId"], 22);
    assert_eq!(ex["isFirstQuery"], false);
    assert_eq!(ex["toolUseResults"][0]["id"], "call_x");
    assert_eq!(ex["toolUseResults"][0]["name"], "UseMcpTool");
    assert_eq!(ex["toolUseResults"][0]["message"], "晴，18 度");
    assert_eq!(
        ex["toolUseResults"][0]["params"]["tool_name"], "get_weather",
        "native 参数要原样回填: {ex}"
    );
}

#[tokio::test]
async fn unknown_model_is_400_with_available_list() {
    let cloud = FakeCloud::start().await;
    let (app, _dir) = cloud_app(&cloud).await;
    let body = r#"{"model":"not-a-model","messages":[{"role":"user","content":"hi"}],"stream":false}"#;
    let (status, text) = post_chat(&app.base, body).await;
    assert_eq!(status, 400, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not-a-model"));
}

#[tokio::test]
async fn cloud_models_struct_is_send_sync() {
    // 编译期契约：模型结构能跨任务用（路由/缓存都在 Arc<AppState> 里）。
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CloudModel>();
}