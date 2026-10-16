//! `internal/proxy/responses.go`：OpenAI Responses API（`/v1/responses`）。
//!
//! 请求转换（`responsesToChatRequest`）复用 chat 端点的 `BuildRequest` 管线；
//! 输出改为 Responses 格式：非流式 `{object:"response", output:[...], usage}`；
//! 流式发 `response.created` / `response.output_item.added` / `response.content_part.added`
//! / `response.output_text.delta` / `response.output_text.done` / `response.output_item.done`
//! / `response.completed`，末尾补 `data: [DONE]`。

use serde_json::{Map, Value};

use crate::api::{
    CCStreamEvent, OpenAIChatRequest, OpenAIErrorResponse, OpenAIResponse, OpenAIResponseItem,
    OpenAIMessage, ResponsesStreamEvent, ResponseContentPart, ResponseUsage,
};
use crate::build::build_request;
use crate::chat::call_upstream_checked;
use crate::gjson::to_go;
use crate::stream::{normalize_finish_reason, read_cc_result, LineReader};
use crate::util::uuid_prefix;
use crate::Proxy;

/// `api.OpenAIResponsesRequest`。
#[derive(Debug, Clone, Default)]
pub struct OpenAIResponsesRequest {
    pub model: String,
    pub input: Option<Value>,
    pub instructions: Option<Value>,
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<i64>,
    pub max_tokens: Option<i64>,
    pub max_completion_tokens: Option<i64>,
    pub stream: bool,
    pub tools: Vec<Value>,
    pub tool_choice: Option<Value>,
    pub parallel_tool_calls: Option<bool>,
    pub response_format: Option<Value>,
    pub stop: Option<Value>,
    pub top_p: Option<f64>,
    pub user: String,
}

/// 解析 `/v1/responses` 入站体。
pub fn parse_responses_request(body: &[u8]) -> Result<OpenAIResponsesRequest, String> {
    let (root, pre_err) = crate::godecode::GoValue::parse_checked(
        body,
        "api.OpenAIResponsesRequest",
        "api.OpenAIResponsesRequest",
    )?;
    if let Some(e) = pre_err {
        return Err(e);
    }
    Ok(OpenAIResponsesRequest {
        model: root
            .field("model", "string", "OpenAIResponsesRequest.model")
            .as_string()?,
        input: root.raw().get("input").cloned(),
        instructions: root.raw().get("instructions").cloned(),
        temperature: root
            .field("temperature", "float64", "OpenAIResponsesRequest.temperature")
            .opt_f64()?,
        max_output_tokens: root
            .field(
                "max_output_tokens",
                "int",
                "OpenAIResponsesRequest.max_output_tokens",
            )
            .opt_int()?,
        max_tokens: root
            .field("max_tokens", "int", "OpenAIResponsesRequest.max_tokens")
            .opt_int()?,
        max_completion_tokens: root
            .field(
                "max_completion_tokens",
                "int",
                "OpenAIResponsesRequest.max_completion_tokens",
            )
            .opt_int()?,
        stream: root
            .field("stream", "bool", "OpenAIResponsesRequest.stream")
            .as_bool()?,
        tools: root
            .field("tools", "[]interface {}", "OpenAIResponsesRequest.tools")
            .opt_array()?
            .map(|v| v.into_iter().filter_map(|x| x.opt_any()).collect())
            .unwrap_or_default(),
        tool_choice: root.raw().get("tool_choice").cloned(),
        parallel_tool_calls: root
            .field(
                "parallel_tool_calls",
                "bool",
                "OpenAIResponsesRequest.parallel_tool_calls",
            )
            .opt_bool()?,
        response_format: root.raw().get("response_format").cloned(),
        stop: root.raw().get("stop").cloned(),
        top_p: root
            .field("top_p", "float64", "OpenAIResponsesRequest.top_p")
            .opt_f64()?,
        user: root
            .field("user", "string", "OpenAIResponsesRequest.user")
            .as_string()?,
    })
}

/// `responsesToChatRequest`。
pub fn responses_to_chat_request(req: &OpenAIResponsesRequest) -> OpenAIChatRequest {
    let mut messages = responses_input_to_messages(req.input.as_ref());
    if let Some(instr) = &req.instructions {
        messages.insert(
            0,
            OpenAIMessage {
                role: "system".to_string(),
                content: Some(instr.clone()),
                ..Default::default()
            },
        );
    }
    let max_tokens = req
        .max_completion_tokens
        .or(req.max_output_tokens)
        .or(req.max_tokens);
    OpenAIChatRequest {
        model: req.model.clone(),
        messages,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        max_completion_tokens: max_tokens,
        stream: req.stream,
        tools: req.tools.clone(),
    }
}

fn responses_input_to_messages(input: Option<&Value>) -> Vec<OpenAIMessage> {
    match input {
        None => Vec::new(),
        Some(Value::String(s)) => vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(Value::String(s.clone())),
            ..Default::default()
        }],
        Some(Value::Array(items)) => {
            let msgs = response_items_to_messages(items);
            if !msgs.is_empty() {
                msgs
            } else {
                vec![OpenAIMessage {
                    role: "user".to_string(),
                    content: Some(Value::Array(items.clone())),
                    ..Default::default()
                }]
            }
        }
        Some(v) => vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(v.clone()),
            ..Default::default()
        }],
    }
}

fn response_items_to_messages(items: &[Value]) -> Vec<OpenAIMessage> {
    let mut messages = Vec::with_capacity(items.len());
    for item in items {
        let m = match item.as_object() {
            Some(m) => m,
            None => continue,
        };
        let role = m
            .get("role")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("user")
            .to_string();
        let content = m
            .get("content")
            .or_else(|| m.get("text"))
            .or_else(|| m.get("input"))
            .cloned();
        messages.push(OpenAIMessage {
            role,
            content,
            ..Default::default()
        });
    }
    messages
}

/// 下游输出。
pub struct ResponsesOutcome {
    pub status: u16,
    pub content_type: &'static str,
    pub chunks: Vec<Vec<u8>>,
    pub recorded: bool,
}

impl ResponsesOutcome {
    fn err(status: u16, message: &str, err_type: &str) -> ResponsesOutcome {
        let mut b = OpenAIErrorResponse {
            message: message.to_string(),
            r#type: err_type.to_string(),
        }
        .to_go();
        b.push('\n');
        ResponsesOutcome {
            status,
            content_type: "application/json",
            chunks: vec![b.into_bytes()],
            recorded: false,
        }
    }
}

/// `/v1/responses` 的完整业务逻辑。
pub async fn handle_responses(
    proxy: &Proxy,
    body: &[u8],
    auth: Option<&str>,
    version: &str,
) -> ResponsesOutcome {
    let api_key = match proxy.resolve_api_key(auth) {
        Ok(k) => k,
        Err(()) => {
            return ResponsesOutcome::err(
                401,
                "API key required. Set Authorization header.",
                "authentication_error",
            )
        }
    };
    let req = match parse_responses_request(body) {
        Ok(r) => r,
        Err(e) => {
            return ResponsesOutcome::err(
                400,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    if req.model.is_empty() {
        return ResponsesOutcome::err(400, "model is required", "invalid_request_error");
    }
    let chat_req = responses_to_chat_request(&req);
    if chat_req.messages.is_empty() {
        return ResponsesOutcome::err(400, "input is required", "invalid_request_error");
    }
    let cc_body = build_request(&chat_req);
    proxy.reset_hits();
    let request_id = format!("resp_{}", uuid_prefix(30));
    let created = crate::util::unix_now();

    let out = call_upstream_checked(proxy, &cc_body, &api_key, version).await;
    let resp = match out.ok {
        Some(r) => r,
        None => {
            let (st, msg) = out.error.unwrap();
            return ResponsesOutcome::err(st, &msg, "api_error");
        }
    };
    if req.stream {
        let mut body2 = cc_body.clone();
        let mut first =
            stream_responses_response(proxy, resp, &request_id, &body2.params.model, created).await;
        if first.0 > 0 {
            body2.params.max_tokens = crate::build::CC_CONTEXT_LIMIT - first.0 - 16;
            if body2.params.max_tokens < 1024 {
                body2.params.max_tokens = 1024;
            }
            let out2 = call_upstream_checked(proxy, &body2, &api_key, version).await;
            match out2.ok {
                Some(r2) => {
                    first = stream_responses_response(
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
                    return ResponsesOutcome::err(st, &msg, "api_error");
                }
            }
        }
        ResponsesOutcome {
            status: 200,
            content_type: "text/event-stream",
            chunks: first.2,
            recorded: first.1,
        }
    } else {
        let b = non_stream_responses_response(
            proxy,
            resp,
            &request_id,
            &cc_body.params.model,
            created,
        )
        .await;
        ResponsesOutcome {
            status: 200,
            content_type: "application/json",
            chunks: vec![b],
            recorded: true,
        }
    }
}

/// `NonStreamResponsesResponse`。
pub async fn non_stream_responses_response(
    proxy: &Proxy,
    resp: reqwest::Response,
    request_id: &str,
    model: &str,
    created: i64,
) -> Vec<u8> {
    let res = read_cc_result(resp).await;
    let mut output: Vec<OpenAIResponseItem> = Vec::new();
    if !res.content.is_empty() {
        output.push(OpenAIResponseItem {
            id: format!("msg_{}", uuid_prefix(28)),
            r#type: "message".to_string(),
            status: "completed".to_string(),
            role: "assistant".to_string(),
            content: Some(vec![ResponseContentPart::output_text(&res.content)]),
            ..Default::default()
        });
    }
    for tc in &res.tool_calls {
        output.push(OpenAIResponseItem {
            id: format!("fc_{}", uuid_prefix(28)),
            r#type: "function_call".to_string(),
            status: "completed".to_string(),
            call_id: tc.id.clone(),
            name: tc.function_name.clone(),
            arguments: tc.function_arguments.clone(),
            content: None,
            role: String::new(),
        });
    }
    let response = OpenAIResponse {
        id: request_id.to_string(),
        object: "response".to_string(),
        created_at: created,
        status: "completed".to_string(),
        model: model.to_string(),
        output,
        usage: Some(ResponseUsage {
            input_tokens: res.input_tokens,
            output_tokens: res.output_tokens,
            total_tokens: res.input_tokens + res.output_tokens,
        }),
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

/// Responses 流式翻译的状态机（与 anthropic 同因：Go 的闭包互相调用，Rust 需拆成方法集）。
struct RespStream<'a> {
    request_id: &'a str,
    model: &'a str,
    created: i64,
    chunks: Vec<Vec<u8>>,
    started: bool,
    output_index_counter: i64,
    text_item_added: bool,
    text_output_index: i64,
    text_item_id: String,
    text_builder: String,
    tool_order: Vec<String>,
    tool_indexes: Vec<(String, i64)>,
    tool_items: Vec<(String, OpenAIResponseItem)>,
    tool_args_bufs: Vec<(String, String)>,
    tool_args_streamed: Vec<String>,
    last_tool_id: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_write: i64,
}

impl RespStream<'_> {
    fn send_created(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let resp_obj = OpenAIResponse {
            id: self.request_id.to_string(),
            object: "response".to_string(),
            created_at: self.created,
            status: "in_progress".to_string(),
            model: self.model.to_string(),
            output: Vec::new(),
            usage: None,
        };
        let ev = ResponsesStreamEvent {
            r#type: "response.created",
            response: Some(&resp_obj),
            output_index: None,
            item: None,
            item_id: String::new(),
            content_index: None,
            part: None,
            delta: String::new(),
            text: String::new(),
        };
        let body = ev.to_go();
        self.chunks.push(sse_event("response.created", &body));
    }

    fn add_text_item(&mut self) {
        if self.text_item_added {
            return;
        }
        self.text_item_added = true;
        self.text_output_index = self.output_index_counter;
        self.output_index_counter += 1;
        self.send_created();
        let idx = self.text_output_index;
        let item = OpenAIResponseItem {
            id: self.text_item_id.clone(),
            r#type: "message".to_string(),
            status: "in_progress".to_string(),
            role: "assistant".to_string(),
            // Go：`Content: []api.ResponseContentPart{}` + `json:"content,omitempty"`
            // ⇒ 空切片同样被省略（真 Go 采收的黄金里该事件没有 content 键）。
            content: None,
            ..Default::default()
        };
        let ev1 = ResponsesStreamEvent {
            r#type: "response.output_item.added",
            response: None,
            output_index: Some(idx),
            item: Some(&item),
            item_id: String::new(),
            content_index: None,
            part: None,
            delta: String::new(),
            text: String::new(),
        };
        let b1 = ev1.to_go();
        self.chunks.push(sse_event("response.output_item.added", &b1));
        let part = ResponseContentPart::output_text("");
        let ev2 = ResponsesStreamEvent {
            r#type: "response.content_part.added",
            response: None,
            output_index: Some(idx),
            item: None,
            item_id: self.text_item_id.clone(),
            content_index: Some(0),
            part: Some(&part),
            delta: String::new(),
            text: String::new(),
        };
        let b2 = ev2.to_go();
        self.chunks.push(sse_event("response.content_part.added", &b2));
    }

    fn register_tool(&mut self, id: &str, name: &str) {
        if self.tool_indexes.iter().any(|(k, _)| k == id) {
            return;
        }
        let idx = self.output_index_counter;
        self.output_index_counter += 1;
        self.tool_indexes.push((id.to_string(), idx));
        self.tool_order.push(id.to_string());
        self.last_tool_id = id.to_string();
        let item = OpenAIResponseItem {
            id: format!("fc_{}", uuid_prefix(28)),
            r#type: "function_call".to_string(),
            status: "in_progress".to_string(),
            call_id: id.to_string(),
            name: name.to_string(),
            arguments: String::new(),
            content: None,
            role: String::new(),
        };
        self.tool_items.push((id.to_string(), item.clone()));
        self.send_created();
        let ev = ResponsesStreamEvent {
            r#type: "response.output_item.added",
            response: None,
            output_index: Some(idx),
            item: Some(&item),
            item_id: String::new(),
            content_index: None,
            part: None,
            delta: String::new(),
            text: String::new(),
        };
        let body = ev.to_go();
        self.chunks.push(sse_event("response.output_item.added", &body));
    }

    fn send_tool_delta(&mut self, id: &str, delta: &str) {
        let idx = match self.tool_indexes.iter().find(|(k, _)| k == id) {
            Some((_, v)) => *v,
            None => return,
        };
        if !self.tool_args_streamed.iter().any(|x| x == id) {
            self.tool_args_streamed.push(id.to_string());
        }
        if let Some(slot) = self.tool_args_bufs.iter_mut().find(|(k, _)| k == id) {
            slot.1.push_str(delta);
        } else {
            self.tool_args_bufs.push((id.to_string(), delta.to_string()));
        }
        let item_id = self
            .tool_items
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, it)| it.id.clone())
            .unwrap_or_default();
        let ev = ResponsesStreamEvent {
            r#type: "response.function_call_arguments.delta",
            response: None,
            output_index: Some(idx),
            item: None,
            item_id,
            content_index: None,
            part: None,
            delta: delta.to_string(),
            text: String::new(),
        };
        let body = ev.to_go();
        self.chunks
            .push(sse_event("response.function_call_arguments.delta", &body));
    }

    fn process(&mut self, line: &str) {
        let ev = match CCStreamEvent::from_line(line) {
            Some(e) => e,
            None => return,
        };
        match ev.r#type.as_str() {
            "text-delta" => {
                self.send_created();
                self.add_text_item();
                self.text_builder.push_str(&ev.text);
                let idx = self.text_output_index;
                let evd = ResponsesStreamEvent {
                    r#type: "response.output_text.delta",
                    response: None,
                    output_index: Some(idx),
                    item: None,
                    item_id: self.text_item_id.clone(),
                    content_index: Some(0),
                    part: None,
                    delta: ev.text.clone(),
                    text: String::new(),
                };
                let body = evd.to_go();
                self.chunks.push(sse_event("response.output_text.delta", &body));
            }
            "reasoning-delta" | "reasoning-start" | "reasoning-end" => {}
            "tool-use" => {
                self.send_created();
                self.register_tool(&ev.tool_call_id, &ev.tool_name);
            }
            "tool-delta" => {
                self.send_created();
                if !self.last_tool_id.is_empty() {
                    let id = self.last_tool_id.clone();
                    let text = ev.text.clone();
                    self.send_tool_delta(&id, &text);
                }
            }
            "tool-input-start" => {
                self.send_created();
                self.register_tool(&ev.id, &ev.tool_name);
            }
            "tool-input-delta" => {
                self.send_created();
                if !ev.id.is_empty() {
                    let d = ev.delta.clone();
                    let id = ev.id.clone();
                    self.send_tool_delta(&id, &d);
                }
            }
            "tool-call" => {
                self.send_created();
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
                    let id = ev.tool_call_id.clone();
                    self.send_tool_delta(&id, &args);
                }
            }
            "finish" => {
                if let Some(u) = &ev.total_usage {
                    self.input_tokens = u.input_tokens;
                    self.output_tokens = u.output_tokens;
                    self.cache_read = u.cache_read_tokens;
                    self.cache_write = u.cache_write_tokens;
                }
                self.send_created();
                let mut output: Vec<OpenAIResponseItem> = Vec::new();
                if self.text_item_added {
                    let full = self.text_builder.clone();
                    let idx = self.text_output_index;
                    let evd = ResponsesStreamEvent {
                        r#type: "response.output_text.done",
                        response: None,
                        output_index: Some(idx),
                        item: None,
                        item_id: self.text_item_id.clone(),
                        content_index: Some(0),
                        part: None,
                        delta: String::new(),
                        text: full.clone(),
                    };
                    let body = evd.to_go();
                    self.chunks.push(sse_event("response.output_text.done", &body));
                    let text_item = OpenAIResponseItem {
                        id: self.text_item_id.clone(),
                        r#type: "message".to_string(),
                        status: "completed".to_string(),
                        role: "assistant".to_string(),
                        content: Some(vec![ResponseContentPart::output_text(&full)]),
                        ..Default::default()
                    };
                    let evi = ResponsesStreamEvent {
                        r#type: "response.output_item.done",
                        response: None,
                        output_index: Some(idx),
                        item: Some(&text_item),
                        item_id: String::new(),
                        content_index: None,
                        part: None,
                        delta: String::new(),
                        text: String::new(),
                    };
                    let body2 = evi.to_go();
                    self.chunks.push(sse_event("response.output_item.done", &body2));
                    output.push(text_item);
                }
                let order = self.tool_order.clone();
                for id in order {
                    let idx = match self.tool_indexes.iter().find(|(k, _)| k == &id) {
                        Some((_, v)) => *v,
                        None => 0,
                    };
                    if let Some((_, item)) =
                        self.tool_items.iter_mut().find(|(k, _)| k == &id)
                    {
                        if let Some((_, buf)) = self.tool_args_bufs.iter().find(|(k, _)| k == &id)
                        {
                            item.arguments = buf.clone();
                        }
                        item.status = "completed".to_string();
                        let item_clone = item.clone();
                        let evi = ResponsesStreamEvent {
                            r#type: "response.output_item.done",
                            response: None,
                            output_index: Some(idx),
                            item: Some(&item_clone),
                            item_id: String::new(),
                            content_index: None,
                            part: None,
                            delta: String::new(),
                            text: String::new(),
                        };
                        let body = evi.to_go();
                        self.chunks.push(sse_event("response.output_item.done", &body));
                        output.push(item_clone);
                    }
                }
                let complete = OpenAIResponse {
                    id: self.request_id.to_string(),
                    object: "response".to_string(),
                    created_at: self.created,
                    status: "completed".to_string(),
                    model: self.model.to_string(),
                    output,
                    usage: Some(ResponseUsage {
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                        total_tokens: self.input_tokens + self.output_tokens,
                    }),
                };
                let evc = ResponsesStreamEvent {
                    r#type: "response.completed",
                    response: Some(&complete),
                    output_index: None,
                    item: None,
                    item_id: String::new(),
                    content_index: None,
                    part: None,
                    delta: String::new(),
                    text: String::new(),
                };
                let body3 = evc.to_go();
                self.chunks.push(sse_event("response.completed", &body3));
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

/// `StreamResponsesResponse`：返回 `(retry_signal, recorded, chunks)`。
pub async fn stream_responses_response(
    proxy: &Proxy,
    resp: reqwest::Response,
    request_id: &str,
    model: &str,
    created: i64,
) -> (i64, bool, Vec<Vec<u8>>) {
    let mut st = RespStream {
        request_id,
        model,
        created,
        chunks: Vec::new(),
        started: false,
        output_index_counter: 0,
        text_item_added: false,
        text_output_index: 0,
        text_item_id: format!("msg_{}", uuid_prefix(28)),
        text_builder: String::new(),
        tool_order: Vec::new(),
        tool_indexes: Vec::new(),
        tool_items: Vec::new(),
        tool_args_bufs: Vec::new(),
        tool_args_streamed: Vec::new(),
        last_tool_id: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read: 0,
        cache_write: 0,
    };

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
    if !first_line.is_empty() {
        st.process(&first_line);
    }
    let _ = reader.for_each_remaining(|line| {
        st.process(line);
        Ok(())
    })
    .await;

    let mut recorded = false;
    if st.input_tokens > 0 || st.output_tokens > 0 || st.cache_read > 0 {
        proxy.stats.lock().expect("stats 锁").record(
            model,
            st.input_tokens,
            st.output_tokens,
            st.cache_read,
            st.cache_write,
        );
        recorded = true;
    }
    (0, recorded, st.chunks)
}

/// 供门禁：Responses 请求体 → 出站体 JSON。
pub fn cc_from_responses_body(body: &[u8]) -> Result<String, String> {
    let req = parse_responses_request(body)?;
    let chat = responses_to_chat_request(&req);
    Ok(build_request(&chat).to_go())
}

/// 供门禁：`normalizeFinishReason` 的转发。
pub fn finish_reason_of(r: &str) -> &'static str {
    normalize_finish_reason(r)
}

/// 供门禁：构造对象值。
pub fn obj_value(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn string_input_becomes_user_message() {
        let req = OpenAIResponsesRequest {
            model: "m".to_string(),
            input: Some(json!("hello")),
            ..Default::default()
        };
        let chat = responses_to_chat_request(&req);
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, "user");
    }

    #[test]
    fn instructions_are_prepended_as_system() {
        let req = OpenAIResponsesRequest {
            model: "m".to_string(),
            input: Some(json!("x")),
            instructions: Some(json!("be brief")),
            ..Default::default()
        };
        let chat = responses_to_chat_request(&req);
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[0].role, "system");
        assert_eq!(chat.messages[1].role, "user");
    }

    #[test]
    fn max_token_precedence() {
        let mut req = OpenAIResponsesRequest {
            model: "m".to_string(),
            input: Some(json!("x")),
            max_output_tokens: Some(10),
            max_tokens: Some(20),
            max_completion_tokens: Some(30),
            ..Default::default()
        };
        assert_eq!(
            responses_to_chat_request(&req).max_completion_tokens,
            Some(30)
        );
        req.max_completion_tokens = None;
        assert_eq!(
            responses_to_chat_request(&req).max_completion_tokens,
            Some(10)
        );
        req.max_output_tokens = None;
        assert_eq!(
            responses_to_chat_request(&req).max_completion_tokens,
            Some(20)
        );
    }

    #[test]
    fn item_keys_content_text_input() {
        let req = OpenAIResponsesRequest {
            model: "m".to_string(),
            input: Some(json!([{"role":"user","text":"via-text"}])),
            ..Default::default()
        };
        assert_eq!(
            responses_to_chat_request(&req).messages[0].content,
            Some(json!("via-text"))
        );
        let req2 = OpenAIResponsesRequest {
            model: "m".to_string(),
            input: Some(json!([{"role":"user","input":"via-input"}])),
            ..Default::default()
        };
        assert_eq!(
            responses_to_chat_request(&req2).messages[0].content,
            Some(json!("via-input"))
        );
    }

    #[test]
    fn non_map_items_become_user_content() {
        let req = OpenAIResponsesRequest {
            model: "m".to_string(),
            input: Some(json!([1, 2])),
            ..Default::default()
        };
        let chat = responses_to_chat_request(&req);
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].content, Some(json!([1, 2])));
    }
}
