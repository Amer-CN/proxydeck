//! `internal/proxy/anthropic.go`：Anthropic Messages API（`/v1/messages`）。
//!
//! 请求（system / messages / content blocks / max_tokens / stream）翻成 OpenAI chat 请求后
//! 复用 [`crate::build::build_request`] 的 CommandCode 转换管线；响应与流式事件再转回
//! Anthropic 格式。Claude Code、Cline 等 Anthropic 原生客户端可直接指向本端点。

use serde_json::{Map, Value};

use crate::api::{
    AnthropicContentBlock, AnthropicErrorResponse, AnthropicMessageResponse, AnthropicStreamEvent,
    AnthropicUsage, CCStreamEvent, OpenAIChatRequest, OpenAIMessage,
};
use crate::build::build_request;
use crate::chat::call_upstream_checked;
use crate::gjson::to_go;
use crate::stream::{normalize_finish_reason, read_cc_result, LineReader};
use crate::util::uuid_prefix;
use crate::Proxy;

/// `api.AnthropicMessageRequest`。
#[derive(Debug, Clone, Default)]
pub struct AnthropicMessageRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: Option<i64>,
    pub system: Option<Value>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop_sequences: Vec<String>,
    pub stream: bool,
    pub tools: Vec<Value>,
    pub tool_choice: Option<Value>,
}

/// `api.AnthropicMessage`。
#[derive(Debug, Clone, Default)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Option<Value>,
}

/// 解析 `/v1/messages` 入站体（`api.AnthropicMessageRequest`）。
pub fn parse_anthropic_request(body: &[u8]) -> Result<AnthropicMessageRequest, String> {
    let (root, pre_err) = crate::godecode::GoValue::parse_checked(
        body,
        "api.AnthropicMessageRequest",
        "api.AnthropicMessageRequest",
    )?;
    if let Some(e) = pre_err {
        return Err(e);
    }
    let mut req = AnthropicMessageRequest {
        model: root
            .field("model", "string", "AnthropicMessageRequest.model")
            .as_string()?,
        max_tokens: root
            .field("max_tokens", "int", "AnthropicMessageRequest.max_tokens")
            .opt_int()?,
        system: root.raw().get("system").cloned(),
        temperature: root
            .field("temperature", "float64", "AnthropicMessageRequest.temperature")
            .opt_f64()?,
        top_p: root
            .field("top_p", "float64", "AnthropicMessageRequest.top_p")
            .opt_f64()?,
        stream: root
            .field("stream", "bool", "AnthropicMessageRequest.stream")
            .as_bool()?,
        tools: root
            .field("tools", "[]interface {}", "AnthropicMessageRequest.tools")
            .opt_array()?
            .map(|v| v.into_iter().filter_map(|x| x.opt_any()).collect())
            .unwrap_or_default(),
        tool_choice: root.raw().get("tool_choice").cloned(),
        ..Default::default()
    };
    if let Some(ss) = root
        .field(
            "stop_sequences",
            "[]string",
            "AnthropicMessageRequest.stop_sequences",
        )
        .opt_array()?
    {
        for s in ss {
            req.stop_sequences.push(s.as_string()?);
        }
    }
    if let Some(msgs) = root
        .field(
            "messages",
            "[]api.AnthropicMessage",
            "AnthropicMessageRequest.messages",
        )
        .opt_array()?
    {
        for m in msgs {
            req.messages.push(AnthropicMessage {
                role: m.field("role", "string", "AnthropicMessage.role").as_string()?,
                content: m.raw().get("content").cloned(),
            });
        }
    }
    Ok(req)
}

/// `checkPlanModel`：Go 套餐外的模型（`OnGo=false`）在打上游之前就拒绝。
pub fn check_plan_model(model: &str) -> Result<(), String> {
    let mapped = crate::model::map_model(model);
    for m in crate::models::CATALOG_MODELS {
        if m.id == mapped {
            if !m.on_go {
                return Err(format!(
                    "model {model:?} is not included in your current plan; it requires Pro or Max \
                     (Go plan includes 36 models, see /v1/models?plan=go)"
                ));
            }
            return Ok(());
        }
    }
    Ok(())
}

/// `anthropicToChatRequest`：Anthropic Messages → OpenAI chat。
///
/// 关键差异：Anthropic 的 `tool_result` 嵌在 user 消息里，这里拆成独立的 `role=tool` 消息
/// （上游只接受 user|assistant|tool）。
pub fn anthropic_to_chat_request(req: &AnthropicMessageRequest) -> OpenAIChatRequest {
    let mut messages: Vec<OpenAIMessage> = Vec::new();
    if let Some(sys) = &req.system {
        messages.push(OpenAIMessage {
            role: "system".to_string(),
            content: Some(Value::String(anthropic_system_to_string(sys))),
            ..Default::default()
        });
    }
    // 收集 assistant 消息里的 tool_use（id → name），供 tool_result 拆解用
    let mut tool_names: Map<String, Value> = Map::new();
    for m in &req.messages {
        if m.role != "assistant" {
            continue;
        }
        if let Some(Value::Array(parts)) = &m.content {
            for part in parts {
                if let Some(pm) = part.as_object() {
                    if pm.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let id = pm.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                        let name = pm.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                        if !id.is_empty() && !name.is_empty() {
                            tool_names.insert(id.to_string(), Value::String(name.to_string()));
                        }
                    }
                }
            }
        }
    }
    for m in &req.messages {
        if m.role == "user" {
            messages.extend(extract_tool_results(m.content.as_ref(), &tool_names));
            // ⚠ Go 的 `dropToolResults` 把「剩下的 parts」**原样**当 content 交给下游
            // （`chat.Messages = append(… Content: rest)`），由 `ConvertMessages`/`parseContent`
            // 再按块翻译；这里若只认字符串，会把「文本块数组」整条丢掉。
            // 实测 `anthropic-convert #4`（`[{tool_result 无 id}, {text:"keep"}]`）：
            // 黄金出站是 `[{type:text,text:keep}]`，早先的 Rust 出站是 `messages:[]`。
            let rest = drop_tool_results(m.content.as_ref());
            let keep = match &rest {
                // Go 的 `rest == ""` 判等：空串（含「全是 tool_result」的回落）不产生 user 消息
                Some(Value::String(s)) => !s.is_empty(),
                Some(Value::Array(a)) => !a.is_empty(),
                _ => false,
            };
            if keep {
                messages.push(OpenAIMessage {
                    role: "user".to_string(),
                    content: rest.clone(),
                    ..Default::default()
                });
            }
        } else {
            messages.push(OpenAIMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                ..Default::default()
            });
        }
    }
    OpenAIChatRequest {
        model: req.model.clone(),
        messages,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        max_completion_tokens: None,
        stream: req.stream,
        tools: req.tools.clone(),
    }
}

/// `anthropicSystemToString`：字符串原样；blocks 数组拼接其中的 text 块。
pub fn anthropic_system_to_string(system: &Value) -> String {
    match system {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut b = String::new();
            for part in parts {
                let pm = match part.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                if pm.get("type").and_then(|v| v.as_str()) != Some("text") {
                    continue;
                }
                if let Some(Value::String(text)) = pm.get("text") {
                    if !text.is_empty() {
                        if !b.is_empty() {
                            b.push('\n');
                        }
                        b.push_str(text);
                    }
                }
            }
            b
        }
        _ => String::new(),
    }
}

/// `extractToolResults`：user 消息里的 `tool_result` 块 → 独立 `role=tool` 消息。
pub fn extract_tool_results(
    content: Option<&Value>,
    tool_names: &Map<String, Value>,
) -> Vec<OpenAIMessage> {
    let parts = match content {
        Some(Value::Array(p)) => p,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for part in parts {
        let pm = match part.as_object() {
            Some(m) => m,
            None => continue,
        };
        if pm.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
            continue;
        }
        let id = pm
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let name = tool_names
            .get(&id)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        out.push(OpenAIMessage {
            role: "tool".to_string(),
            content: Some(Value::String(tool_result_text(pm.get("content")))),
            name,
            tool_call_id: id,
            ..Default::default()
        });
    }
    out
}

/// `dropToolResults`：去掉 `tool_result` 块；全为 tool_result（或空）时返回空串。
pub fn drop_tool_results(content: Option<&Value>) -> Option<Value> {
    let parts = match content {
        Some(Value::Array(p)) => p,
        other => return other.cloned(),
    };
    let rest: Vec<Value> = parts
        .iter()
        .filter(|part| {
            !matches!(part.as_object(), Some(pm)
                if pm.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
        })
        .cloned()
        .collect();
    if rest.is_empty() {
        return Some(Value::String(String::new()));
    }
    Some(Value::Array(rest))
}

/// `toolResultText`：`tool_result` 的文本内容（string 或 text blocks）。
pub fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut b = String::new();
            for part in parts {
                if let Some(pm) = part.as_object() {
                    if let Some(Value::String(t)) = pm.get("text") {
                        b.push_str(t);
                    }
                }
            }
            b
        }
        Some(Value::Object(o)) => o
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// `writeAnthropicError`：`server_error` → `api_error`（OpenAI 方言映射）。
pub fn anthropic_error_body(status_msg: &str, err_type: &str) -> String {
    let t = if err_type == "server_error" {
        "api_error"
    } else {
        err_type
    };
    let mut b = AnthropicErrorResponse {
        r#type: "error".to_string(),
        error_type: t.to_string(),
        message: status_msg.to_string(),
    }
    .to_go();
    b.push('\n');
    b
}

/// 下游输出。
pub struct AnthropicOutcome {
    pub status: u16,
    pub content_type: &'static str,
    pub chunks: Vec<Vec<u8>>,
    pub recorded: bool,
}

impl AnthropicOutcome {
    fn err(status: u16, message: &str, err_type: &str) -> AnthropicOutcome {
        AnthropicOutcome {
            status,
            content_type: "application/json",
            chunks: vec![anthropic_error_body(message, err_type).into_bytes()],
            recorded: false,
        }
    }
}

/// `/v1/messages` 的完整业务逻辑。
pub async fn handle_messages(
    proxy: &Proxy,
    body: &[u8],
    auth: Option<&str>,
    version: &str,
) -> AnthropicOutcome {
    let api_key = match proxy.resolve_api_key(auth) {
        Ok(k) => k,
        Err(()) => {
            return AnthropicOutcome::err(
                401,
                "API key required. Set Authorization header.",
                "authentication_error",
            )
        }
    };
    let req = match parse_anthropic_request(body) {
        Ok(r) => r,
        Err(e) => {
            return AnthropicOutcome::err(
                400,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    if req.model.is_empty() {
        return AnthropicOutcome::err(400, "model is required", "invalid_request_error");
    }
    if req.messages.is_empty() {
        return AnthropicOutcome::err(400, "messages array is required", "invalid_request_error");
    }
    if let Err(e) = check_plan_model(&req.model) {
        return AnthropicOutcome::err(403, &e, "permission_error");
    }
    let chat = anthropic_to_chat_request(&req);
    let cc_body = build_request(&chat);
    proxy.reset_hits();
    let request_id = format!("msg_{}", uuid_prefix(32));
    let created = crate::util::unix_now();

    let out = call_upstream_checked(proxy, &cc_body, &api_key, version).await;
    let resp = match out.ok {
        Some(r) => r,
        None => {
            let (st, msg) = out.error.unwrap();
            return AnthropicOutcome::err(st, &msg, "api_error");
        }
    };
    if req.stream {
        let mut body2 = cc_body.clone();
        let mut first =
            stream_anthropic_response(proxy, resp, &request_id, &body2.params.model, created).await;
        if first.0 > 0 {
            body2.params.max_tokens = crate::build::CC_CONTEXT_LIMIT - first.0 - 16;
            if body2.params.max_tokens < 1024 {
                body2.params.max_tokens = 1024;
            }
            let out2 = call_upstream_checked(proxy, &body2, &api_key, version).await;
            match out2.ok {
                Some(r2) => {
                    first = stream_anthropic_response(
                        proxy,
                        r2,
                        &request_id,
                        &body2.params.model,
                        created,
                    )
                    .await;
                }
                // Go：重发失败由 `callUpstreamChecked` 写出**真实错误**（原先这里静默落
                // 200 空体，属分歧）。
                None => {
                    let (st, msg) = out2
                        .error
                        .unwrap_or((502, "upstream error: retry failed".to_string()));
                    return AnthropicOutcome::err(st, &msg, "api_error");
                }
            }
        }
        AnthropicOutcome {
            status: 200,
            content_type: "text/event-stream",
            chunks: first.2,
            recorded: first.1,
        }
    } else {
        let b =
            non_stream_anthropic_response(proxy, resp, &request_id, &cc_body.params.model, created)
                .await;
        AnthropicOutcome {
            status: 200,
            content_type: "application/json",
            chunks: vec![b],
            recorded: true,
        }
    }
}

/// `NonStreamAnthropicResponse`。
pub async fn non_stream_anthropic_response(
    proxy: &Proxy,
    resp: reqwest::Response,
    request_id: &str,
    model: &str,
    _created: i64,
) -> Vec<u8> {
    let res = read_cc_result(resp).await;
    let mut blocks: Vec<AnthropicContentBlock> = Vec::new();
    let mut stop_reason = "end_turn".to_string();
    if res.has_tool_calls {
        stop_reason = "tool_use".to_string();
        for tc in &res.tool_calls {
            let input = if tc.function_arguments.is_empty() {
                Some(Value::Object(Map::new()))
            } else {
                match crate::godecode::parse_value(tc.function_arguments.as_bytes()) {
                    Ok(v) => Some(v),
                    Err(_) => {
                        let mut m = Map::new();
                        m.insert(
                            "arguments".to_string(),
                            Value::String(tc.function_arguments.clone()),
                        );
                        Some(Value::Object(m))
                    }
                }
            };
            blocks.push(AnthropicContentBlock {
                r#type: "tool_use".to_string(),
                text: String::new(),
                id: tc.id.clone(),
                name: tc.function_name.clone(),
                input,
            });
        }
    } else if !res.content.is_empty() {
        blocks.push(AnthropicContentBlock::text(&res.content));
    }
    if res.finish_reason == "length" || res.finish_reason == "max_tokens" {
        stop_reason = "max_tokens".to_string();
    }
    let response = AnthropicMessageResponse {
        id: request_id.to_string(),
        r#type: "message".to_string(),
        role: "assistant".to_string(),
        model: model.to_string(),
        content: blocks,
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        usage: AnthropicUsage {
            input_tokens: res.input_tokens,
            output_tokens: res.output_tokens,
        },
    };
    if res.input_tokens > 0 || res.output_tokens > 0 || res.cache_read_tokens > 0 {
        proxy.stats.lock().expect("stats 锁").record(
            model,
            res.input_tokens,
            res.output_tokens,
            res.cache_read_tokens,
            res.cache_write_tokens,
        );
    }
    let mut b = response.to_go().into_bytes();
    b.push(b'\n');
    b
}

fn sse_event(name: &str, body: &str) -> Vec<u8> {
    format!("event: {name}\ndata: {body}\n\n").into_bytes()
}

/// Anthropic 流式翻译的状态机。
///
/// 为什么写成 struct 而不是若干嵌套闭包：Go 里 `sendStart` / `sendTextStart` /
/// `registerTool` / `sendToolDelta` 是**互相调用**的闭包，Rust 里那会让它们同时可变借用
/// 同一批状态（编译不过）。改成「一个 handler 方法集」语义完全等价。
struct AnthStream<'a> {
    request_id: &'a str,
    model: &'a str,
    chunks: Vec<Vec<u8>>,
    started: bool,
    text_started: bool,
    text_idx: i64,
    block_index: i64,
    block_stop_order: Vec<i64>,
    stop_reason: String,
    output_tokens: i64,
    tool_indexes: Vec<(String, i64)>,
    tool_args_streamed: Vec<String>,
    last_tool_id: String,
}

impl AnthStream<'_> {
    fn send_start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let msg = AnthropicMessageResponse {
            id: self.request_id.to_string(),
            r#type: "message".to_string(),
            role: "assistant".to_string(),
            model: self.model.to_string(),
            content: Vec::new(),
            stop_reason: None,
            stop_sequence: None,
            usage: AnthropicUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
        };
        let ev = AnthropicStreamEvent {
            r#type: "message_start",
            message: Some(&msg),
            index: None,
            content_block: None,
            delta: None,
            usage: None,
        };
        let body = ev.to_go();
        self.chunks.push(sse_event("message_start", &body));
    }

    fn send_text_start(&mut self) {
        if self.text_started {
            return;
        }
        self.text_started = true;
        self.text_idx = self.block_index;
        self.block_index += 1;
        self.block_stop_order.push(self.text_idx);
        let block = AnthropicContentBlock::text("");
        let ev = AnthropicStreamEvent {
            r#type: "content_block_start",
            message: None,
            index: Some(self.text_idx),
            content_block: Some(&block),
            delta: None,
            usage: None,
        };
        let body = ev.to_go();
        self.chunks.push(sse_event("content_block_start", &body));
    }

    fn register_tool(&mut self, id: &str, name: &str) {
        if self.tool_indexes.iter().any(|(k, _)| k == id) {
            return;
        }
        let idx = self.block_index;
        self.block_index += 1;
        self.tool_indexes.push((id.to_string(), idx));
        self.block_stop_order.push(idx);
        self.last_tool_id = id.to_string();
        let block = AnthropicContentBlock {
            r#type: "tool_use".to_string(),
            text: String::new(),
            id: id.to_string(),
            name: name.to_string(),
            input: Some(Value::Object(Map::new())),
        };
        let ev = AnthropicStreamEvent {
            r#type: "content_block_start",
            message: None,
            index: Some(idx),
            content_block: Some(&block),
            delta: None,
            usage: None,
        };
        let body = ev.to_go();
        self.chunks.push(sse_event("content_block_start", &body));
    }

    fn send_tool_delta(&mut self, id: &str, partial: &str) {
        let idx = match self.tool_indexes.iter().find(|(k, _)| k == id) {
            Some((_, v)) => *v,
            None => return,
        };
        if !self.tool_args_streamed.iter().any(|x| x == id) {
            self.tool_args_streamed.push(id.to_string());
        }
        let mut d = Map::new();
        d.insert(
            "type".to_string(),
            Value::String("input_json_delta".to_string()),
        );
        d.insert(
            "partial_json".to_string(),
            Value::String(partial.to_string()),
        );
        let ev = AnthropicStreamEvent {
            r#type: "content_block_delta",
            message: None,
            index: Some(idx),
            content_block: None,
            delta: Some(Value::Object(d)),
            usage: None,
        };
        let body = ev.to_go();
        self.chunks.push(sse_event("content_block_delta", &body));
    }

    fn process(&mut self, line: &str) {
        let ev = match CCStreamEvent::from_line(line) {
            Some(e) => e,
            None => return,
        };
        match ev.r#type.as_str() {
            "text-delta" => {
                self.send_start();
                self.send_text_start();
                let mut d = Map::new();
                d.insert("type".to_string(), Value::String("text_delta".to_string()));
                d.insert("text".to_string(), Value::String(ev.text.clone()));
                let idx = self.text_idx;
                let e2 = AnthropicStreamEvent {
                    r#type: "content_block_delta",
                    message: None,
                    index: Some(idx),
                    content_block: None,
                    delta: Some(Value::Object(d)),
                    usage: None,
                };
                let body = e2.to_go();
                self.chunks.push(sse_event("content_block_delta", &body));
            }
            // 思考内容在 Anthropic 协议里无直接对应事件（thinking 块），跳过
            "reasoning-delta" | "reasoning-start" | "reasoning-end" => {}
            "tool-use" => {
                self.send_start();
                self.register_tool(&ev.tool_call_id, &ev.tool_name);
            }
            "tool-delta" => {
                self.send_start();
                if !self.last_tool_id.is_empty() {
                    let id = self.last_tool_id.clone();
                    self.send_tool_delta(&id, &ev.text);
                }
            }
            "tool-input-start" => {
                self.send_start();
                self.register_tool(&ev.id, &ev.tool_name);
            }
            "tool-input-delta" => {
                self.send_start();
                if !ev.id.is_empty() {
                    self.send_tool_delta(&ev.id, &ev.delta);
                }
            }
            "tool-call" => {
                self.send_start();
                self.register_tool(&ev.tool_call_id, &ev.tool_name);
                if !self
                    .tool_args_streamed
                    .iter()
                    .any(|x| x == &ev.tool_call_id)
                {
                    let args = match &ev.input {
                        Some(v) => to_go(v),
                        None => String::new(),
                    };
                    self.send_tool_delta(&ev.tool_call_id, &args);
                }
            }
            "finish" => {
                match normalize_finish_reason(&ev.finish_reason) {
                    "tool_calls" => self.stop_reason = "tool_use".to_string(),
                    "length" => self.stop_reason = "max_tokens".to_string(),
                    _ => {}
                }
                if let Some(u) = &ev.total_usage {
                    self.output_tokens = u.output_tokens;
                }
                self.send_start();
                let order = self.block_stop_order.clone();
                for idx in order {
                    let e2 = AnthropicStreamEvent {
                        r#type: "content_block_stop",
                        message: None,
                        index: Some(idx),
                        content_block: None,
                        delta: None,
                        usage: None,
                    };
                    let body = e2.to_go();
                    self.chunks.push(sse_event("content_block_stop", &body));
                }
                let mut d = Map::new();
                d.insert(
                    "stop_reason".to_string(),
                    Value::String(self.stop_reason.clone()),
                );
                d.insert("stop_sequence".to_string(), Value::Null);
                let usage = AnthropicUsage {
                    input_tokens: 0,
                    output_tokens: self.output_tokens,
                };
                let e3 = AnthropicStreamEvent {
                    r#type: "message_delta",
                    message: None,
                    index: None,
                    content_block: None,
                    delta: Some(Value::Object(d)),
                    usage: Some(&usage),
                };
                let body3 = e3.to_go();
                self.chunks.push(sse_event("message_delta", &body3));
                let e4 = AnthropicStreamEvent {
                    r#type: "message_stop",
                    message: None,
                    index: None,
                    content_block: None,
                    delta: None,
                    usage: None,
                };
                let body4 = e4.to_go();
                self.chunks.push(sse_event("message_stop", &body4));
                self.chunks.push(b"data: [DONE]\n\n".to_vec());
            }
            "error" => {
                if let Some(m) = &ev.error_message {
                    println!("[ERROR] Stream error: {m}");
                }
            }
            _ => {}
        }
    }
}

/// `StreamAnthropicResponse`：返回 `(retry_signal, recorded, chunks)`。
pub async fn stream_anthropic_response(
    proxy: &Proxy,
    resp: reqwest::Response,
    request_id: &str,
    model: &str,
    _created: i64,
) -> (i64, bool, Vec<Vec<u8>>) {
    let mut st = AnthStream {
        request_id,
        model,
        chunks: Vec::new(),
        started: false,
        text_started: false,
        text_idx: 0,
        block_index: 0,
        block_stop_order: Vec::new(),
        stop_reason: "end_turn".to_string(),
        output_tokens: 0,
        tool_indexes: Vec::new(),
        tool_args_streamed: Vec::new(),
        last_tool_id: String::new(),
    };

    let mut stream_in = 0i64;
    let mut stream_out = 0i64;
    let mut stream_cr = 0i64;
    let mut stream_cw = 0i64;

    // 首行判 context 超限
    let mut reader = LineReader::new(resp);
    let first_line = reader.first_line().await;
    if !first_line.is_empty() {
        if let Some(e) = CCStreamEvent::from_line(&first_line) {
            if e.r#type == "error" {
                if let Some(m) = &e.error_message {
                    let n = crate::build::parse_context_overflow(m);
                    if n > 0 {
                        return (n, false, st.chunks);
                    }
                }
            }
        }
    }

    let collect_usage = |line: &str, si: &mut i64, so: &mut i64, cr: &mut i64, cw: &mut i64| {
        if let Some(e) = CCStreamEvent::from_line(line) {
            if e.r#type == "finish" {
                if let Some(u) = &e.total_usage {
                    *si = u.input_tokens;
                    *so = u.output_tokens;
                    *cr = u.cache_read_tokens;
                    *cw = u.cache_write_tokens;
                }
            }
        }
    };

    if !first_line.is_empty() {
        st.process(&first_line);
        collect_usage(
            &first_line,
            &mut stream_in,
            &mut stream_out,
            &mut stream_cr,
            &mut stream_cw,
        );
    }
    let _ = reader.for_each_remaining(|line| {
        st.process(line);
        collect_usage(
            line,
            &mut stream_in,
            &mut stream_out,
            &mut stream_cr,
            &mut stream_cw,
        );
        Ok(())
    })
    .await;

    let mut recorded = false;
    if stream_in > 0 || stream_out > 0 || stream_cr > 0 {
        proxy.stats.lock().expect("stats 锁").record(
            model,
            stream_in,
            stream_out,
            stream_cr,
            stream_cw,
        );
        recorded = true;
    }
    (0, recorded, st.chunks)
}

/// 供门禁：Anthropic 请求体 → 出站体 JSON。
pub fn cc_from_anthropic_body(body: &[u8]) -> Result<String, String> {
    let req = parse_anthropic_request(body)?;
    let chat = anthropic_to_chat_request(&req);
    Ok(build_request(&chat).to_go())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_string_and_blocks() {
        assert_eq!(anthropic_system_to_string(&json!("plain")), "plain");
        assert_eq!(
            anthropic_system_to_string(&json!([
                {"type":"text","text":"第一段"},
                {"type":"text","text":"第二段"},
                {"type":"image","source":{"type":"base64","data":"x"}}
            ])),
            "第一段\n第二段"
        );
    }

    #[test]
    fn tool_result_is_flattened_into_tool_message() {
        let req = AnthropicMessageRequest {
            model: "deepseek-v4-flash".to_string(),
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: Some(json!("你好")),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: Some(json!([
                        {"type":"text","text":"好的"},
                        {"type":"tool_use","id":"toolu_1","name":"bash","input":{"command":"ls"}}
                    ])),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: Some(json!([
                        {"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"ok"}]}
                    ])),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: Some(json!("继续")),
                },
            ],
            max_tokens: Some(4096),
            ..Default::default()
        };
        let chat = anthropic_to_chat_request(&req);
        let tool = chat
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("tool_result 未拆成 tool 消息");
        assert_eq!(tool.tool_call_id, "toolu_1");
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.content, Some(json!("ok")));
        assert!(chat
            .messages
            .iter()
            .any(|m| m.role == "user" && m.content == Some(json!("继续"))));
    }

    #[test]
    fn tool_result_without_id_is_dropped() {
        let msgs = extract_tool_results(
            Some(&json!([{"type":"tool_result","content":"x"},{"type":"text","text":"keep"}])),
            &Map::new(),
        );
        assert!(msgs.is_empty());
    }

    #[test]
    fn error_shape_maps_server_error() {
        let b = anthropic_error_body("boom", "server_error");
        assert_eq!(
            b,
            "{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n"
        );
    }

    #[test]
    fn plan_gate_matches_go() {
        for m in [
            "claude-sonnet-5",
            "claude-opus-5",
            "sonnet-5",
            "gpt-5.4",
            "gemini-3.5-flash",
            "grok-4.6",
        ] {
            assert!(check_plan_model(m).is_err(), "{m} 应被套餐门槛拒绝");
        }
        for m in [
            "deepseek-v4-flash",
            "deepseek/deepseek-v4-pro",
            "gpt-5.6-luna",
            "some-unknown-model",
        ] {
            assert!(check_plan_model(m).is_ok(), "{m} 应放行");
        }
    }
}
