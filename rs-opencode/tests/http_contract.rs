//! tests/http_contract.rs —— 假 opencode 上游 + 本插件的 HTTP 契约用例。
//!
//! 覆盖任务简报 §七 的验收项：
//! * 消息压平（字符串/数组/空/多轮/tool 结果）——`translate` 单测 + 这里的 400 用例；
//! * `/v1/models` 的 provider 白名单 + denylist 过滤；
//! * SSE：text delta / reasoning delta / finish / `[DONE]`；
//! * 429 判定 + 冷却（冷却中的模型如实 429，绝不静默换模型）；
//! * 国家限制 → 400；
//! * 未安装 opencode → `/health` 的 reason + `/v1/*` 503；
//! * `permission.asked` 必须回 `reject`。
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path as FsPath, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
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
    /// 发一条调用外部桥接工具的事件（called + progress + permission），之后挂住：
    /// 插件应进入收集窗、打断、返回 tool_calls。
    ToolCalls,
    /// 同一窗口内两条并行调用。
    ToolCallsParallel,
    /// 只有 called，没有 progress 的 server 前缀：必须 fail-closed 走文本。
    ToolCallsNoProgress,
    /// 没有 progress，但 permission.asked 的 action+source.id 能证明是本桥外部调用。
    ToolCallsPermissionOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpMode {
    Normal,
    /// PUT 直接 500。
    RegisterFail,
    /// PUT 成功，但 GET /api/mcp 报 failed。
    ConnectFail,
}

/// 假 opencode 为注册的 MCP server 真正拉起的子进程。
struct BridgeChild(Child);

impl Drop for BridgeChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct McpEntry {
    #[allow(dead_code)]
    config: Value,
    #[allow(dead_code)]
    dir: PathBuf,
    tools: Vec<String>,
    /// 保持子进程 stdin 打开：关掉会让桥接子进程读到 EOF 退出。
    _stdin: ChildStdin,
    _child: BridgeChild,
}

#[derive(Clone, Debug, Default)]
struct ToolInfo {
    server: String,
    tools: Vec<String>,
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
    session_deletes: Arc<Mutex<Vec<String>>>,
    interrupted: Arc<Mutex<Vec<String>>>,
    prompts: Arc<Mutex<Vec<String>>>,
    seq: Arc<AtomicU64>,
    mcp_mode: Arc<Mutex<McpMode>>,
    mcp_entries: Arc<Mutex<HashMap<String, McpEntry>>>,
    mcp_added: Arc<Mutex<Vec<(String, Value, PathBuf)>>>,
    mcp_deleted: Arc<Mutex<Vec<String>>>,
    mcp_tools_seen: Arc<Mutex<HashMap<String, Vec<String>>>>,
    bridge_dirs: Arc<Mutex<Vec<PathBuf>>>,
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
            session_deletes: Arc::new(Mutex::new(Vec::new())),
            interrupted: Arc::new(Mutex::new(Vec::new())),
            prompts: Arc::new(Mutex::new(Vec::new())),
            seq: Arc::new(AtomicU64::new(1)),
            mcp_mode: Arc::new(Mutex::new(McpMode::Normal)),
            mcp_entries: Arc::new(Mutex::new(HashMap::new())),
            mcp_added: Arc::new(Mutex::new(Vec::new())),
            mcp_deleted: Arc::new(Mutex::new(Vec::new())),
            mcp_tools_seen: Arc::new(Mutex::new(HashMap::new())),
            bridge_dirs: Arc::new(Mutex::new(Vec::new())),
        };

        let app = Router::new()
            .route("/api/info", get(info))
            .route("/api/model", get(model_list))
            .route("/api/session", post(create_session))
            .route("/api/session/{id}", delete(delete_session))
            .route("/api/session/{id}/model", post(set_model))
            .route("/api/session/{id}/prompt", post(prompt))
            .route("/api/session/{id}/message", get(messages))
            .route("/api/session/{id}/interrupt", post(interrupt_session))
            .route(
                "/api/session/{id}/permission/{pid}/reply",
                post(reply_permission),
            )
            .route("/api/mcp", get(mcp_list))
            .route(
                "/api/experimental/mcp/{name}",
                put(mcp_add).delete(mcp_delete),
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

    fn set_mcp_mode(&self, mode: McpMode) {
        *self.mcp_mode.lock().expect("mcp mode lock") = mode;
    }

    fn mcp_added(&self) -> Vec<(String, Value, PathBuf)> {
        self.mcp_added.lock().expect("mcp added lock").clone()
    }

    fn mcp_deleted(&self) -> Vec<String> {
        self.mcp_deleted.lock().expect("mcp deleted lock").clone()
    }

    fn interrupted(&self) -> Vec<String> {
        self.interrupted.lock().expect("interrupted lock").clone()
    }

    fn session_deletes(&self) -> Vec<String> {
        self.session_deletes
            .lock()
            .expect("session deletes lock")
            .clone()
    }

    fn prompt_texts(&self) -> Vec<String> {
        self.prompts.lock().expect("prompts lock").clone()
    }

    fn mcp_tools_seen(&self, name: &str) -> Option<Vec<String>> {
        self.mcp_tools_seen
            .lock()
            .expect("mcp tools seen lock")
            .get(name)
            .cloned()
    }

    /// 测试收尾兜底：杀掉所有还活着的桥接子进程并清掉记录过的桥目录。
    fn kill_all(&self) {
        self.mcp_entries.lock().expect("mcp entries lock").clear();
        let dirs = self.bridge_dirs.lock().expect("bridge dirs lock").clone();
        for dir in dirs {
            let _ = std::fs::write(dir.join("cancel"), b"1");
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// 测试类 Drop 兜底：断言失败（panic）时也不留桥接子进程/临时目录。
struct FakeCleanup(Fake);

impl Drop for FakeCleanup {
    fn drop(&mut self) {
        self.0.kill_all();
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
    Json(body): Json<Value>,
) -> Json<Value> {
    state
        .requests
        .lock()
        .expect("requests lock")
        .push(format!("prompt {id}"));
    if let Some(text) = body.get("text").and_then(Value::as_str) {
        state
            .prompts
            .lock()
            .expect("prompts lock")
            .push(text.to_string());
    }
    let behavior = state.behavior(&id);
    let info = {
        let entries = state.mcp_entries.lock().expect("mcp entries lock");
        entries
            .iter()
            .next()
            .map(|(name, entry)| ToolInfo {
                server: name.clone(),
                tools: entry.tools.clone(),
            })
            .unwrap_or_else(|| ToolInfo {
                server: "pd-test".to_string(),
                tools: vec!["get_weather".to_string()],
            })
    };
    let tx = state.tx.clone();
    tokio::spawn(async move {
        emit_sequence(tx, id, behavior, info).await;
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
    state
        .session_deletes
        .lock()
        .expect("session deletes lock")
        .push(id);
    StatusCode::NO_CONTENT
}

async fn interrupt_session(State(state): State<Fake>, Path(id): Path<String>) -> Json<Value> {
    state.interrupted.lock().expect("interrupted lock").push(id);
    Json(json!({"interrupted": true}))
}

// ------------------------------ 假 MCP server 管理 ------------------------------

async fn mcp_list(State(state): State<Fake>) -> Json<Value> {
    let failed = *state.mcp_mode.lock().expect("mcp mode lock") == McpMode::ConnectFail;
    let names: Vec<String> = state
        .mcp_entries
        .lock()
        .expect("mcp entries lock")
        .keys()
        .cloned()
        .collect();
    let data: Vec<Value> = names
        .into_iter()
        .map(|name| {
            if failed {
                json!({"name": name, "status": {"status": "failed", "error": "connect boom"}})
            } else {
                json!({"name": name, "status": {"status": "connected"}})
            }
        })
        .collect();
    Json(json!({"data": data}))
}

async fn mcp_add(
    State(state): State<Fake>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if *state.mcp_mode.lock().expect("mcp mode lock") == McpMode::RegisterFail {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": "register boom"}})),
        )
            .into_response();
    }
    let config = body.get("config").cloned().unwrap_or(Value::Null);
    let command: Vec<String> = config
        .get("command")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if command.len() < 4 {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": "bad command"}})),
        )
            .into_response();
    }
    let dir = PathBuf::from(&command[command.len() - 1]);
    state
        .bridge_dirs
        .lock()
        .expect("bridge dirs lock")
        .push(dir.clone());
    state.mcp_added.lock().expect("mcp added lock").push((
        name.clone(),
        config.clone(),
        dir.clone(),
    ));

    let cmd = command.clone();
    match tokio::task::spawn_blocking(move || start_bridge_child(&cmd)).await {
        Ok(Ok((child, stdin, tools))) => {
            state
                .mcp_tools_seen
                .lock()
                .expect("mcp tools seen lock")
                .insert(name.clone(), tools.clone());
            state.mcp_entries.lock().expect("mcp entries lock").insert(
                name,
                McpEntry {
                    config,
                    dir,
                    tools,
                    _stdin: stdin,
                    _child: BridgeChild(child),
                },
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": e}})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": format!("join error: {e}")}})),
        )
            .into_response(),
    }
}

async fn mcp_delete(State(state): State<Fake>, Path(name): Path<String>) -> StatusCode {
    let removed = state
        .mcp_entries
        .lock()
        .expect("mcp entries lock")
        .remove(&name);
    if removed.is_some() {
        state
            .mcp_deleted
            .lock()
            .expect("mcp deleted lock")
            .push(name);
    }
    StatusCode::NO_CONTENT
}

/// 真起 `CARGO_BIN_EXE_rs-opencode --plugin-opencode --mcp-bridge <dir>`，
/// 走 initialize + notifications/initialized + tools/list，返回活着的子进程。
fn start_bridge_child(command: &[String]) -> Result<(Child, ChildStdin, Vec<String>), String> {
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn bridge: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "bridge stdin missing".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "bridge stdout missing".to_string())?;
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let result = (|| -> Result<Vec<String>, String> {
        let init = bridge_request(
            &mut stdin,
            &rx,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "fake-opencode", "version": "0"},
                }
            }),
        )?;
        if init.get("result").is_none() {
            return Err(format!("initialize failed: {init}"));
        }
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        writeln!(
            stdin,
            "{}",
            serde_json::to_string(&initialized).unwrap_or_default()
        )
        .map_err(|e| format!("write initialized: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("flush initialized: {e}"))?;
        let list = bridge_request(
            &mut stdin,
            &rx,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )?;
        let tools = list
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(tools)
    })();

    match result {
        Ok(tools) => Ok((child, stdin, tools)),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

fn bridge_request(
    stdin: &mut ChildStdin,
    rx: &mpsc::Receiver<String>,
    value: Value,
) -> Result<Value, String> {
    let text = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    writeln!(stdin, "{text}").map_err(|e| format!("write bridge: {e}"))?;
    stdin.flush().map_err(|e| format!("flush bridge: {e}"))?;
    let line = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "bridge handshake timeout".to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("bridge response: {e}"))
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

async fn emit_sequence(
    tx: broadcast::Sender<String>,
    sid: String,
    behavior: Behavior,
    info: ToolInfo,
) {
    tokio::time::sleep(Duration::from_millis(30)).await;
    let send = |ty: &str, data: Value| {
        let _ = tx.send(json!({"type": ty, "data": data}).to_string());
    };
    let tool_at = |index: usize| -> String {
        let tool = info
            .tools
            .get(index)
            .cloned()
            .unwrap_or_else(|| "get_weather".to_string());
        format!("{}.{}", info.server, tool)
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
        Behavior::ToolCalls => {
            let full = tool_at(1);
            send(
                "session.execution.started",
                json!({"sessionID": sid.clone()}),
            );
            send(
                "session.tool.input.started",
                json!({"sessionID": sid.clone(), "id": "tc_1", "name": full.clone()}),
            );
            send(
                "session.tool.called",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_1",
                    "input": {"city": "Beijing"},
                    "executed": false,
                }),
            );
            send(
                "session.tool.progress",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_1",
                    "metadata": {"toolCalls": [{
                        "tool": full,
                        "status": "running",
                        "input": {"city": "Beijing"},
                    }]},
                }),
            );
            // 外部工具调用会先过 permission；插件必须 reject，但已拿到的事件足以回 tool_calls。
            send(
                "permission.asked",
                json!({"sessionID": sid.clone(), "id": "perm_tc"}),
            );
            // 这里故意不发 terminal：真实场景下 tools/call 挂起，等插件打断。
        }
        Behavior::ToolCallsParallel => {
            let full_a = tool_at(1);
            let full_b = tool_at(0);
            send(
                "session.execution.started",
                json!({"sessionID": sid.clone()}),
            );
            send(
                "session.tool.input.started",
                json!({"sessionID": sid.clone(), "id": "tc_1", "name": full_a.clone()}),
            );
            send(
                "session.tool.called",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_1",
                    "input": {"city": "Beijing"},
                    "executed": false,
                }),
            );
            send(
                "session.tool.progress",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_1",
                    "metadata": {"toolCalls": [{
                        "tool": full_a,
                        "status": "running",
                        "input": {"city": "Beijing"},
                    }]},
                }),
            );
            send(
                "session.tool.input.started",
                json!({"sessionID": sid.clone(), "id": "tc_2", "name": full_b.clone()}),
            );
            send(
                "session.tool.called",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_2",
                    "input": {"q": "x"},
                    "executed": false,
                }),
            );
            send(
                "session.tool.progress",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_2",
                    "metadata": {"toolCalls": [{
                        "tool": full_b,
                        "status": "running",
                        "input": {"q": "x"},
                    }]},
                }),
            );
            send(
                "permission.asked",
                json!({"sessionID": sid.clone(), "id": "perm_tc"}),
            );
        }
        Behavior::ToolCallsNoProgress => {
            let full = tool_at(1);
            send(
                "session.execution.started",
                json!({"sessionID": sid.clone()}),
            );
            send(
                "session.tool.input.started",
                json!({"sessionID": sid.clone(), "id": "tc_1", "name": full}),
            );
            send(
                "session.tool.called",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_1",
                    "input": {"city": "Beijing"},
                    "executed": false,
                }),
            );
            // 没有 progress 的 server 前缀 → 必须 fail-closed 走文本。
            send(
                "session.text.delta",
                json!({"sessionID": sid.clone(), "delta": "Hello"}),
            );
            send("session.execution.succeeded", json!({"sessionID": sid}));
        }
        Behavior::ToolCallsPermissionOnly => {
            let tool = info
                .tools
                .get(1)
                .cloned()
                .unwrap_or_else(|| "get_weather".to_string());
            let full = format!("{}.{}", info.server, tool);
            let action = format!("{}_{}", info.server, tool);
            send(
                "session.execution.started",
                json!({"sessionID": sid.clone()}),
            );
            send(
                "session.tool.input.started",
                json!({"sessionID": sid.clone(), "id": "tc_1", "name": full}),
            );
            send(
                "session.tool.called",
                json!({
                    "sessionID": sid.clone(),
                    "id": "tc_1",
                    "input": {"city": "Beijing"},
                    "executed": false,
                }),
            );
            // 2.0.32 直连 MCP 工具不一定发 progress；permission action 是第二信号。
            send(
                "permission.asked",
                json!({
                    "sessionID": sid.clone(),
                    "id": "perm_tc",
                    "action": action,
                    "resources": ["*"],
                    "source": {"type": "tool", "messageID": "msg_1", "id": "tc_1"},
                    "metadata": {"input": {"city": "Beijing"}},
                }),
            );
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
        auto_install: false,
        opencode_version: "2.0.32".to_string(),
    }
}

async fn boot(fake: &Fake) -> String {
    boot_with(fake, test_config()).await
}

async fn boot_with(fake: &Fake, cfg: Config) -> String {
    let srv = Server::with_upstream(cfg, &fake.base, "fake-password", "fake-1.0.0");
    // 集成测试进程是 test harness；桥接子进程必须指回真正的 rs-opencode 二进制。
    srv.set_bridge_exe(PathBuf::from(env!("CARGO_BIN_EXE_rs-opencode")));
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

// ------------------------------ 工具桥接测试夹具 ------------------------------

/// 所有会碰 `%TEMP%\ProxyDeck\opencode-mcp` 的用例串行跑：
/// 这样「请求前后目录集合相等」的断言不会被别的用例并发创建的桥目录干扰。
static BRIDGE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn bridge_dirs_now() -> HashSet<PathBuf> {
    let root = rs_opencode::mcp::bridge_root();
    match std::fs::read_dir(root) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => HashSet::new(),
    }
}

fn tools_payload() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "get the weather",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                // OpenAI 非法字符 → MCP 清洗成 `weird_tool_`；响应必须回填原名。
                "name": "weird tool!",
                "description": "a tool whose name needs sanitizing",
                "parameters": {"type": "object", "properties": {}}
            }
        }
    ])
}

fn chat_with_tools(stream: bool) -> Value {
    json!({
        "model": "opencode/model-b",
        "messages": [{"role": "user", "content": "use the weird tool"}],
        "tools": tools_payload(),
        "stream": stream
    })
}

/// 轮询等待条件成立（测试里等异步清理落地）。
async fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cond()
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
        vec!["model-a", "model-b", "model-p", "model-t", "go-1"],
        "beam-free / other provider / disabled 都应被过滤，且 id 必须是裸模型名（不带 provider 前缀）：{j}"
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
async fn bare_model_name_resolves_by_provider_order() {
    let fake = Fake::start(&[]).await;
    let base = boot(&fake).await;

    // /v1/models 给的是裸名；客户端照抄裸名发回来，必须能用。
    // fake 里 model-b 只在 opencode 下有，应还原成 opencode/model-b。
    let (status, j) = post_chat(
        &base,
        json!({
            "model": "model-b",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "Hello");
    assert_eq!(
        fake.model_requests(),
        vec!["model opencode/model-b"],
        "裸名 model-b 必须还原成完整引用再打上游：{j}"
    );
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
async fn rate_limit_marks_cooling_and_never_switches_model() {
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
    assert!(
        j1["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("opencode/model-a"),
        "本轮 429 也要点名是哪个模型：{j1}"
    );

    // 同一模型第二次：它已冷却，必须如实再回 429（文案点名模型 + retry-after），
    // 绝不静默换成默认模型 model-b。
    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(
            serde_json::to_vec(&json!({
                "model": "opencode/model-a",
                "messages": [{"role": "user", "content": "x"}]
            }))
            .expect("序列化请求"),
        )
        .send()
        .await
        .expect("POST chat");
    let s2 = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let t2 = resp.text().await.unwrap_or_default();
    let j2: Value = serde_json::from_str(&t2).unwrap_or(Value::Null);
    assert_eq!(s2, 429, "{j2}");
    assert_eq!(j2["error"]["type"], "rate_limit_error");
    assert!(
        j2["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("opencode/model-a"),
        "429 文案必须点名是哪个模型在冷却：{j2}"
    );
    assert!(
        !retry_after.is_empty() && retry_after != "0",
        "429 必须带 retry-after：{retry_after}"
    );
    assert_eq!(
        fake.model_requests(),
        vec!["model opencode/model-a"],
        "第二次不许再向上游发 set_model（没换模型，自然也没有新请求）"
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
async fn install_status_endpoint_shape_and_cancel_are_available() {
    let base = boot_not_ready("opencode_not_installed").await;

    let (status, j) = get_json(&base, "/install/status").await;
    assert_eq!(status, 200, "{j}");
    for key in ["installed", "downloading", "downloaded", "total"] {
        assert!(j.get(key).is_some(), "/install/status 缺字段 {key}：{j}");
    }

    let resp = client()
        .post(format!("{base}/install/cancel"))
        .send()
        .await
        .expect("POST /install/cancel");
    assert_eq!(resp.status().as_u16(), 202);
    let text = resp.text().await.expect("cancel body");
    let j: Value = serde_json::from_str(&text).expect("cancel JSON");
    assert_eq!(j["cancelled"], true);
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

// ------------------------------ MCP 桥接契约用例 ------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_success_returns_finish_tool_calls_and_cleans_up() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-b", Behavior::ToolCalls)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, j) = post_chat(&base, chat_with_tools(false)).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["object"], "chat.completion");
    assert_eq!(j["choices"][0]["finish_reason"], "tool_calls", "{j}");
    assert!(
        j["choices"][0]["message"]["content"].is_null(),
        "tool_calls 轮 content 应为 null：{j}"
    );
    let calls = j["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls 数组");
    assert_eq!(calls.len(), 1, "{j}");
    assert_eq!(calls[0]["type"], "function");
    assert_eq!(calls[0]["function"]["name"], "weird tool!");
    let id = calls[0]["id"].as_str().expect("tool_call id");
    assert!(id.starts_with("call_"), "id 形状：{id}");
    let args: Value = serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap())
        .expect("arguments 必须是合法 JSON 字符串");
    assert_eq!(args["city"], "Beijing");

    assert_eq!(
        fake.permission_replies(),
        vec!["perm_tc:reject"],
        "外部桥接工具的 permission 也必须 reject"
    );
    assert_eq!(fake.interrupted().len(), 1, "应先 interrupt 再返回");
    assert!(!fake.session_deletes().is_empty(), "会话必须 DELETE");

    let added = fake.mcp_added();
    assert_eq!(added.len(), 1, "{added:?}");
    let (name, config, dir) = &added[0];
    assert!(name.starts_with("pd-"), "server 名：{name}");
    assert_eq!(config["type"], "local");
    assert_eq!(config["codemode"], false, "codemode 必须为 false");
    assert_eq!(config["command"][1], "--plugin-opencode");
    assert_eq!(config["command"][2], "--mcp-bridge");
    assert_eq!(
        config["command"][3],
        dir.to_string_lossy().as_ref(),
        "最后一个参数必须指向桥目录"
    );
    assert_eq!(
        fake.mcp_tools_seen(name),
        Some(vec!["get_weather".to_string(), "weird_tool_".to_string()]),
        "tools/list 应返回清洗后的工具名"
    );
    assert_eq!(
        fake.mcp_deleted(),
        vec![name.clone()],
        "MCP 注册必须 DELETE"
    );
    assert!(!dir.exists(), "桥目录必须删：{}", dir.display());
    assert_eq!(bridge_dirs_now(), before, "不应残留任何桥目录");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_parallel_calls_are_collected_in_one_window() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-b", Behavior::ToolCallsParallel)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, j) = post_chat(&base, chat_with_tools(false)).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["finish_reason"], "tool_calls", "{j}");
    let calls = j["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls 数组");
    assert_eq!(calls.len(), 2, "1500ms 收集窗应收到两条并行调用：{j}");
    assert_eq!(calls[0]["function"]["name"], "weird tool!");
    assert_eq!(calls[1]["function"]["name"], "get_weather");
    let a: Value =
        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    let b: Value =
        serde_json::from_str(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(a["city"], "Beijing");
    assert_eq!(b["q"], "x");
    assert_ne!(
        calls[0]["id"].as_str(),
        calls[1]["id"].as_str(),
        "每条 tool_call 的 id 必须唯一"
    );

    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert!(!added[0].2.exists(), "桥目录必须删");
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_stream_emits_tool_call_delta_and_finish() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-b", Behavior::ToolCalls)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, text) = post_chat_raw(&base, chat_with_tools(true)).await;
    assert_eq!(status, 200, "{text}");
    assert!(text.trim_end().ends_with("data: [DONE]"), "{text}");

    let frames: Vec<Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let mut tool_delta: Option<Value> = None;
    let mut finish: Option<String> = None;
    for frame in &frames {
        if let Some(delta) = frame["choices"][0]["delta"]["tool_calls"].as_array() {
            tool_delta = Some(Value::Array(delta.clone()));
        }
        if let Some(reason) = frame["choices"][0]["finish_reason"].as_str() {
            finish = Some(reason.to_string());
        }
    }
    let delta = tool_delta.expect("流式应有一帧 delta.tool_calls");
    assert_eq!(delta[0]["type"], "function");
    assert_eq!(delta[0]["function"]["name"], "weird tool!");
    assert!(delta[0]["id"].as_str().unwrap().starts_with("call_"));
    assert_eq!(finish.as_deref(), Some("tool_calls"), "{text}");
    assert!(
        !text.contains("\"finish_reason\":\"stop\""),
        "工具轮不能同时发 stop：{text}"
    );

    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert!(!added[0].2.exists(), "桥目录必须删");
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_connect_failure_falls_back_to_text_and_cleans_up() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[]).await;
    fake.set_mcp_mode(McpMode::ConnectFail);
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, j) = post_chat(&base, chat_with_tools(false)).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["finish_reason"], "stop", "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "Hello");
    assert!(
        j["choices"][0]["message"].get("tool_calls").is_none(),
        "失败闭合必须走纯文本：{j}"
    );
    assert!(fake.interrupted().is_empty(), "没有调用就不该 interrupt");

    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert_eq!(fake.mcp_deleted(), vec![added[0].0.clone()]);
    assert!(!added[0].2.exists(), "桥目录必须删");
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_register_failure_falls_back_to_text() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[]).await;
    fake.set_mcp_mode(McpMode::RegisterFail);
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, j) = post_chat(&base, chat_with_tools(false)).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["finish_reason"], "stop", "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "Hello");
    assert!(fake.mcp_added().is_empty(), "PUT 失败后不应有注册记录");
    assert!(fake.mcp_deleted().is_empty());
    assert!(fake.interrupted().is_empty());
    assert_eq!(bridge_dirs_now(), before, "PUT 失败也必须清掉临时桥目录");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_missing_progress_falls_back_closed() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-b", Behavior::ToolCallsNoProgress)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, j) = post_chat(&base, chat_with_tools(false)).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["finish_reason"], "stop", "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "Hello");
    assert!(
        j["choices"][0]["message"].get("tool_calls").is_none(),
        "没有 progress 的 server 前缀就必须 fail-closed：{j}"
    );
    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert_eq!(fake.mcp_deleted(), vec![added[0].0.clone()]);
    assert!(!added[0].2.exists());
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_permission_action_is_a_second_signal() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-b", Behavior::ToolCallsPermissionOnly)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, j) = post_chat(&base, chat_with_tools(false)).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(
        j["choices"][0]["finish_reason"], "tool_calls",
        "没有 progress 时 permission action+source.id 也应能判定：{j}"
    );
    let calls = j["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls 数组");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["name"], "weird tool!");
    let args: Value =
        serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Beijing");
    assert_eq!(fake.permission_replies(), vec!["perm_tc:reject"]);

    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert_eq!(fake.mcp_deleted(), vec![added[0].0.clone()]);
    assert!(!added[0].2.exists());
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_turn_timeout_cleans_session_and_mcp() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-a", Behavior::Hang)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let mut cfg = test_config();
    cfg.timeout_seconds = 1;
    let base = boot_with(&fake, cfg).await;

    let body = json!({
        "model": "opencode/model-a",
        "messages": [{"role": "user", "content": "hang"}],
        "tools": tools_payload()
    });
    let (status, j) = post_chat(&base, body).await;
    assert_eq!(status, 504, "{j}");
    assert_eq!(j["error"]["type"], "timeout_error");
    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert_eq!(fake.mcp_deleted(), vec![added[0].0.clone()]);
    assert!(!fake.session_deletes().is_empty());
    assert!(!added[0].2.exists(), "桥目录必须删");
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_rate_limit_still_cleans_up() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-a", Behavior::Quota)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let body = json!({
        "model": "opencode/model-a",
        "messages": [{"role": "user", "content": "x"}],
        "tools": tools_payload()
    });
    let (status, j) = post_chat(&base, body).await;
    assert_eq!(status, 429, "{j}");
    assert_eq!(j["error"]["type"], "rate_limit_error");
    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert_eq!(fake.mcp_deleted(), vec![added[0].0.clone()]);
    assert!(!added[0].2.exists(), "上游错误也必须清掉桥目录");
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_bridge_client_disconnect_cancels_and_cleans_up() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[("opencode/model-b", Behavior::ToolCalls)]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    // 只等响应头，随后立刻丢掉 body（模拟客户端取消/断连）。
    let resp = client()
        .post(format!("{base}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&chat_with_tools(true)).unwrap())
        .send()
        .await
        .expect("stream send");
    assert_eq!(resp.status().as_u16(), 200);
    drop(resp);

    let cleaned = wait_until(
        || !fake.mcp_deleted().is_empty() && !fake.session_deletes().is_empty(),
        Duration::from_secs(5),
    )
    .await;
    assert!(cleaned, "客户端断连后必须清理会话与 MCP 注册");

    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    let gone = wait_until(|| !added[0].2.exists(), Duration::from_secs(2)).await;
    assert!(gone, "桥目录必须删：{}", added[0].2.display());
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_tools_request_never_registers_mcp() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let (status, j) = post_chat(
        &base,
        json!({
            "model": "opencode/model-b",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["finish_reason"], "stop");
    assert_eq!(j["choices"][0]["message"]["content"], "Hello");
    assert!(fake.mcp_added().is_empty(), "无 tools 请求不应注册 MCP");
    assert!(fake.mcp_deleted().is_empty());
    assert!(fake.session_deletes().len() == 1);
    assert_eq!(bridge_dirs_now(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_round_tool_result_uses_flattened_text() {
    let _lock = BRIDGE_TEST_LOCK.lock().await;
    let before = bridge_dirs_now();
    let fake = Fake::start(&[]).await;
    let _cleanup = FakeCleanup(fake.clone());
    let base = boot(&fake).await;

    let body = json!({
        "model": "opencode/model-b",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_prev",
                "type": "function",
                "function": {"name": "weird tool!", "arguments": "{\"city\":\"Beijing\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_prev", "content": "42"}
        ],
        "tools": tools_payload()
    });
    let (status, j) = post_chat(&base, body).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["choices"][0]["finish_reason"], "stop", "{j}");
    assert_eq!(j["choices"][0]["message"]["content"], "Hello");

    let prompts = fake.prompt_texts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(
        prompts[0].contains("[user]\nhi"),
        "老压平路径应保留 user：{}",
        prompts[0]
    );
    assert!(
        prompts[0].contains("[tool result for call_prev]\n42"),
        "下一轮 tool 结果必须压成 [tool result for <id>]：{}",
        prompts[0]
    );
    // 这一轮没有模型发起的工具调用：桥注册后应正常清理。
    let added = fake.mcp_added();
    assert_eq!(added.len(), 1);
    assert_eq!(fake.mcp_deleted(), vec![added[0].0.clone()]);
    assert!(!added[0].2.exists());
    assert_eq!(bridge_dirs_now(), before);
}

// ------------------------------ 真实 MCP 子进程直测 ------------------------------

struct TempBridgeDir(PathBuf);

impl TempBridgeDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ProxyDeck-mcp-bridge-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建测试桥目录");
        Self(dir)
    }

    fn path(&self) -> &FsPath {
        &self.0
    }
}

impl Drop for TempBridgeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_tools_json(dir: &FsPath, timeout_ms: u64) {
    let payload = json!({
        "version": 1,
        "timeout_ms": timeout_ms,
        "tools": [
            {
                "name": "get_weather",
                "original": "get weather!",
                "description": "d",
                "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}}
            },
            {
                "name": "second_tool",
                "original": "second.tool",
                "description": "",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    });
    fs::write(dir.join("tools.json"), payload.to_string()).expect("写 tools.json");
}

struct BridgeProc {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<String>,
}

impl BridgeProc {
    fn spawn(dir: &FsPath) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rs-opencode"))
            .arg("--plugin-opencode")
            .arg("--mcp-bridge")
            .arg(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rs-opencode --mcp-bridge");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            rx,
        }
    }

    fn send_line(&mut self, value: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{}", serde_json::to_string(value).unwrap()).expect("write child");
        stdin.flush().expect("flush child");
    }

    fn request(&mut self, value: Value) -> Value {
        self.send_line(&value);
        self.response(Duration::from_secs(10))
            .expect("child response")
    }

    fn response(&mut self, wait: Duration) -> Option<Value> {
        self.rx
            .recv_timeout(wait)
            .ok()
            .and_then(|line| serde_json::from_str(&line).ok())
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn wait_exit(&mut self, wait: Duration) -> Option<std::process::ExitStatus> {
        let started = std::time::Instant::now();
        while started.elapsed() < wait {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for BridgeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "http-contract-test", "version": "0"}
        }
    })
}

#[test]
fn mcp_bridge_child_real_stdio_handshake_list_call_cancel() {
    let dir = TempBridgeDir::new("handshake");
    write_tools_json(dir.path(), 10_000);
    let mut proc = BridgeProc::spawn(dir.path());

    let init = proc.request(initialize_request(1));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
    proc.send_line(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let list = proc.request(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = list["result"]["tools"].as_array().expect("tools 数组");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["city"]["type"],
        "string"
    );

    proc.send_line(&json!({
        "jsonrpc": "2.0",
        "id": "r1",
        "method": "tools/call",
        "params": {"name": "get_weather", "arguments": {"city": "Beijing"}}
    }));
    assert!(
        proc.response(Duration::from_millis(300)).is_none(),
        "tools/call 必须挂起等 parent 决定"
    );
    let calls = fs::read_to_string(dir.path().join("calls.jsonl")).expect("calls.jsonl");
    assert!(calls.contains("\"name\":\"get_weather\""));
    assert!(calls.contains("\"city\":\"Beijing\""));

    fs::write(dir.path().join("cancel"), b"1").expect("写 cancel");
    let err = proc
        .response(Duration::from_secs(5))
        .expect("cancel 后应回错误");
    assert_eq!(err["id"], "r1");
    assert_eq!(err["error"]["code"], -32000);
    let status = proc
        .wait_exit(Duration::from_secs(5))
        .expect("子进程应退出");
    assert_eq!(status.code(), Some(0), "cancel 是正常退出");
}

#[test]
fn mcp_bridge_child_real_stdio_result_file_roundtrip() {
    let dir = TempBridgeDir::new("result");
    write_tools_json(dir.path(), 10_000);
    fs::create_dir_all(dir.path().join("results")).unwrap();
    fs::write(
        dir.path().join("results").join("r2.json"),
        r#"{"ok":true,"value":42}"#,
    )
    .unwrap();
    let mut proc = BridgeProc::spawn(dir.path());
    let _ = proc.request(initialize_request(1));
    let result = proc.request(json!({
        "jsonrpc": "2.0",
        "id": "r2",
        "method": "tools/call",
        "params": {"name": "second_tool", "arguments": {}}
    }));
    assert_eq!(result["id"], "r2");
    assert_eq!(result["result"]["isError"], false);
    assert!(result["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("\"value\":42"));
    proc.close_stdin();
    let status = proc.wait_exit(Duration::from_secs(5)).expect("EOF 后退出");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn mcp_bridge_child_absolute_timeout_exits_nonzero() {
    let dir = TempBridgeDir::new("timeout");
    write_tools_json(dir.path(), 300);
    let mut proc = BridgeProc::spawn(dir.path());
    let _ = proc.request(initialize_request(1));
    proc.send_line(&json!({
        "jsonrpc": "2.0",
        "id": "slow",
        "method": "tools/call",
        "params": {"name": "get_weather", "arguments": {}}
    }));
    let err = proc.response(Duration::from_secs(5)).expect("超时应回错误");
    assert_eq!(err["error"]["code"], -32000);
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("timeout"));
    let status = proc.wait_exit(Duration::from_secs(5)).expect("超时应退出");
    assert_ne!(status.code(), Some(0), "超时退出码必须非 0");
}

#[test]
fn mcp_bridge_child_missing_or_invalid_tools_json_exits_nonzero() {
    let missing = TempBridgeDir::new("missing");
    let mut proc = BridgeProc::spawn(missing.path());
    let status = proc
        .wait_exit(Duration::from_secs(5))
        .expect("缺 tools.json 应退出");
    assert_ne!(status.code(), Some(0));

    let invalid = TempBridgeDir::new("invalid");
    fs::write(invalid.path().join("tools.json"), "{not json").unwrap();
    let mut proc = BridgeProc::spawn(invalid.path());
    let status = proc
        .wait_exit(Duration::from_secs(5))
        .expect("坏 tools.json 应退出");
    assert_ne!(status.code(), Some(0));
}
