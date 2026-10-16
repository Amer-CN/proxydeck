//! `internal/proxy/proxy.go` 的 `/v1/chat/completions`：解析 → 构造 → 上游 → 翻译下游。
//!
//! 上游一直是 SSE（CommandCode 的 `/alpha/generate` 只有流式），所以「非流式」= 把整段
//! SSE 读完再聚合成 `chat.completion`；「流式」= 逐事件翻成 OpenAI chunk。

use serde_json::Value;

use crate::api::{
    CCMessage, CCStreamEvent, OpenAIChatRequest, OpenAIDelta, OpenAIDeltaToolCall,
    OpenAIChoice, OpenAIErrorResponse, OpenAIMessage, OpenAIChatResponse, OpenAIUsage, OutMessage,
    ToolCall,
};
use crate::build::{build_request, create_upstream_request};
use crate::convert::est_tokens;
use crate::gjson::to_go;
use crate::stream::{normalize_finish_reason, read_cc_result, LineReader};
use crate::util::uuid_prefix;
use crate::Proxy;

/// 解析 `/v1/chat/completions` 的入站体（`json.Unmarshal` 到 `api.OpenAIChatRequest` 的等价物，
/// 错误文案逐字同 Go）。
pub fn parse_chat_request(body: &[u8]) -> Result<OpenAIChatRequest, String> {
    let (root, pre_err) =
        crate::godecode::GoValue::parse_checked(body, "api.OpenAIChatRequest", "api.OpenAIChatRequest")?;
    if let Some(e) = pre_err {
        return Err(e);
    }
    let mut req = OpenAIChatRequest::default();
    req.model = root
        .field("model", "string", "OpenAIChatRequest.model")
        .as_string()?;
    req.temperature = root
        .field("temperature", "float64", "OpenAIChatRequest.temperature")
        .opt_f64()?;
    req.max_tokens = root
        .field("max_tokens", "int", "OpenAIChatRequest.max_tokens")
        .opt_int()?;
    req.max_completion_tokens = root
        .field(
            "max_completion_tokens",
            "int",
            "OpenAIChatRequest.max_completion_tokens",
        )
        .opt_int()?;
    req.stream = root
        .field("stream", "bool", "OpenAIChatRequest.stream")
        .as_bool()?;

    if let Some(msgs) = root
        .field("messages", "[]api.OpenAIMessage", "OpenAIChatRequest.messages")
        .opt_array()?
    {
        for m in &msgs {
            req.messages.push(parse_message(m)?);
        }
    }
    if let Some(tools) = root
        .field("tools", "[]interface {}", "OpenAIChatRequest.tools")
        .opt_array()?
    {
        for t in &tools {
            if let Some(v) = t.opt_any() {
                req.tools.push(v);
            }
        }
    }
    Ok(req)
}

fn parse_message(m: &crate::godecode::GoValue) -> Result<OpenAIMessage, String> {
    let mut out = OpenAIMessage {
        role: m.field("role", "string", "OpenAIMessage.role").as_string()?,
        content: match m.raw().get("content") {
            None => None,
            Some(Value::Null) => Some(Value::Null),
            Some(v) => Some(v.clone()),
        },
        name: m.field("name", "string", "OpenAIMessage.name").as_string()?,
        tool_call_id: m
            .field("tool_call_id", "string", "OpenAIMessage.tool_call_id")
            .as_string()?,
        reasoning_content: m
            .field("reasoning_content", "string", "OpenAIMessage.reasoning_content")
            .as_string()?,
        tool_calls: Vec::new(),
    };
    if let Some(tcs) = m
        .field("tool_calls", "[]api.ToolCall", "OpenAIMessage.tool_calls")
        .opt_array()?
    {
        for tc in &tcs {
            out.tool_calls.push(ToolCall {
                id: tc.field("id", "string", "ToolCall.id").as_string()?,
                r#type: tc.field("type", "string", "ToolCall.type").as_string()?,
                function_name: tc
                    .field(
                        "function",
                        "api.FunctionCall",
                        "ToolCall.function",
                    )
                    .field("name", "string", "FunctionCall.name")
                    .as_string()?,
                function_arguments: tc
                    .field("function", "api.FunctionCall", "ToolCall.function")
                    .field("arguments", "string", "FunctionCall.arguments")
                    .as_string()?,
            });
        }
    }
    Ok(out)
}

/// `writeOpenAIError`：`json.Encoder` 的末尾换行也要有。
pub fn openai_error_body(status_msg: &str, err_type: &str) -> String {
    let mut b = OpenAIErrorResponse {
        message: status_msg.to_string(),
        r#type: err_type.to_string(),
    }
    .to_go();
    b.push('\n');
    b
}

/// 上游 /alpha/generate 的调用结果（成功时含响应；失败时已生成下游错误体）。
pub struct UpstreamOutcome {
    pub ok: Option<reqwest::Response>,
    pub error: Option<(u16, String)>,
}

/// `callUpstreamChecked` 的等价物。
pub async fn call_upstream_checked(
    proxy: &Proxy,
    body: &crate::api::CCRequestBody,
    api_key: &str,
    version: &str,
) -> UpstreamOutcome {
    let spec = create_upstream_request(&proxy.base_url, body, api_key, version);
    proxy.add_hit();
    proxy.debugf(&format!(
        "[DEBUG] CommandCode request body: {}",
        crate::truncate_log(&String::from_utf8_lossy(&spec.body))
    ));
    let mut rb = proxy.client.raw().post(&spec.url);
    for (k, v) in &spec.headers {
        rb = rb.header(*k, v);
    }
    let resp = match rb.body(spec.body).send().await {
        Ok(r) => r,
        Err(e) => {
            return UpstreamOutcome {
                ok: None,
                error: Some((502, format!("upstream error: {e}"))),
            }
        }
    };
    if resp.status().as_u16() != 200 {
        let status = resp.status().as_u16();
        let err_body = resp.text().await.unwrap_or_default();
        let message = format!("Upstream error: {err_body}");
        println!("[ERROR] Upstream returned {status}: {err_body}");
        let downstream = if (400..500).contains(&status) { status } else { 502 };
        return UpstreamOutcome {
            ok: None,
            error: Some((downstream, message)),
        };
    }
    UpstreamOutcome {
        ok: Some(resp),
        error: None,
    }
}

/// 一次 chat 请求的下游输出（已按 Go 的形态拼好字节），供 handler 写回。
pub struct ChatOutcome {
    pub status: u16,
    pub content_type: &'static str,
    /// 流式：逐块写出（已带 `data: ...\n\n`）；非流式：单块
    pub chunks: Vec<Vec<u8>>,
    /// 上游命中次数（门禁用）
    pub upstream_hits: usize,
    /// 本次是否入账（门禁用）
    pub recorded: bool,
}

/// `/v1/chat/completions` 的完整业务逻辑（与 HTTP 层解耦，便于门禁直接驱动）。
pub async fn handle_chat(
    proxy: &Proxy,
    body: &[u8],
    auth: Option<&str>,
    version: &str,
) -> ChatOutcome {
    let api_key = match proxy.resolve_api_key(auth) {
        Ok(k) => k,
        Err(()) => {
            return err(
                401,
                "API key required. Set Authorization header.",
                "authentication_error",
            )
        }
    };
    let req = match parse_chat_request(body) {
        Ok(r) => r,
        Err(e) => return err(400, &format!("Invalid JSON: {e}"), "invalid_request_error"),
    };
    if req.messages.is_empty() {
        return err(400, "messages array is required", "invalid_request_error");
    }
    let cc_body = build_request(&req);
    proxy.reset_hits();
    let request_id = format!("chatcmpl-{}", uuid_prefix(29));
    let created = crate::util::unix_now();

    if req.stream {
        stream_chat_with_retry(proxy, body, &cc_body, &api_key, &request_id, created, version).await
    } else {
        let out = call_upstream_checked(proxy, &cc_body, &api_key, version).await;
        match out.ok {
            Some(resp) => {
                let body_bytes = non_stream_response(proxy, resp, &request_id, &cc_body.params.model, created).await;
                ChatOutcome {
                    status: 200,
                    content_type: "application/json",
                    chunks: vec![body_bytes],
                    upstream_hits: 1,
                    recorded: true,
                }
            }
            None => {
                let (st, msg) = out.error.unwrap();
                err(st, &msg, "api_error")
            }
        }
    }
}

fn err(status: u16, message: &str, err_type: &str) -> ChatOutcome {
    ChatOutcome {
        status,
        content_type: "application/json",
        chunks: vec![openai_error_body(message, err_type).into_bytes()],
        upstream_hits: 0,
        recorded: false,
    }
}

/// `streamChatWithRetry`：上游以 context 超限拒绝时压缩 max_tokens 后重发一次。
#[allow(clippy::too_many_arguments)]
async fn stream_chat_with_retry(
    proxy: &Proxy,
    _raw: &[u8],
    cc_body: &crate::api::CCRequestBody,
    api_key: &str,
    request_id: &str,
    created: i64,
    version: &str,
) -> ChatOutcome {
    let mut body = cc_body.clone();
    let out = call_upstream_checked(proxy, &body, api_key, version).await;
    let mut hits = 1usize;
    let resp = match out.ok {
        Some(r) => r,
        None => {
            let (st, msg) = out.error.unwrap();
            return err(st, &msg, "api_error");
        }
    };
    let model = body.params.model.clone();
    let first = stream_response(proxy, resp, request_id, &model, created).await;
    if first.retry_tokens > 0 {
        body.params.max_tokens = crate::build::CC_CONTEXT_LIMIT - first.retry_tokens - 16;
        if body.params.max_tokens < 1024 {
            body.params.max_tokens = 1024;
        }
        println!(
            "[RETRY] context overflow retry with max_tokens={}",
            body.params.max_tokens
        );
        let out2 = call_upstream_checked(proxy, &body, api_key, version).await;
        if let Some(resp2) = out2.ok {
            hits += 1;
            let second = stream_response(proxy, resp2, request_id, &model, created).await;
            return second.into_outcome(hits);
        }
        // Go：重发失败由 `callUpstreamChecked` 自己 `writeErr(真实状态码, 真实消息)` 后返回；
        // 这里必须同样写出**真实错误**（原先捏造 502 "retry failed" 属分歧）。
        if let Some((st, msg)) = out2.error {
            return err(st, &msg, "api_error");
        }
        return err(502, "upstream error: retry failed", "api_error");
    }
    first.into_outcome(hits)
}

/// 流式翻译的产物。
pub struct StreamOutcome {
    pub chunks: Vec<Vec<u8>>,
    pub retry_tokens: i64,
    pub recorded: bool,
}

impl StreamOutcome {
    fn into_outcome(self, hits: usize) -> ChatOutcome {
        ChatOutcome {
            status: 200,
            content_type: "text/event-stream",
            chunks: self.chunks,
            upstream_hits: hits,
            recorded: self.recorded,
        }
    }
}

/// 把一条响应体拼成 SSE chunk 文本（`WriteSSE`）。
pub fn sse_data(resp: &OpenAIChatResponse) -> Vec<u8> {
    format!("data: {}\n\n", resp.to_go()).into_bytes()
}

/// `StreamResponse`：把上游 SSE 翻成 OpenAI chunk 流。
pub async fn stream_response(
    proxy: &Proxy,
    resp: reqwest::Response,
    request_id: &str,
    model: &str,
    created: i64,
) -> StreamOutcome {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut sent_role = false;
    let mut tool_call_index: i64 = 0;
    let mut tool_call_indexes: Vec<(String, i64)> = Vec::new();
    let mut stream_in = 0i64;
    let mut stream_out = 0i64;
    let mut stream_cr = 0i64;
    let mut stream_cw = 0i64;
    let mut recorded = false;
    let retry_tokens: i64 = 0;

    // 先把第一行抠出来判 context 超限（Go 的 streamCCEvents 契约）：
    // 只取一次字节流，首行从同一个 reader 里读，剩下的行继续从它读。
    let mut reader = LineReader::new(resp);
    let first_line = reader.first_line().await;
    if !first_line.is_empty() {
        if let Some(e) = CCStreamEvent::from_line(&first_line) {
            if e.r#type == "error" {
                if let Some(m) = &e.error_message {
                    let n = crate::build::parse_context_overflow(m);
                    if n > 0 {
                        println!(
                            "[RETRY] context overflow: {n} tokens in messages, shrinking max_tokens"
                        );
                        return StreamOutcome {
                            chunks,
                            retry_tokens: n,
                            recorded: false,
                        };
                    }
                }
            }
        }
        let mut st = StreamState {
            request_id,
            model,
            created,
            sent_role: &mut sent_role,
            tool_call_index: &mut tool_call_index,
            tool_call_indexes: &mut tool_call_indexes,
            stream_in: &mut stream_in,
            stream_out: &mut stream_out,
            stream_cr: &mut stream_cr,
            stream_cw: &mut stream_cw,
            chunks: &mut chunks,
        };
        st.process(&first_line);
    }

    let _ = reader.for_each_remaining(|line| {
        let mut st = StreamState {
            request_id,
            model,
            created,
            sent_role: &mut sent_role,
            tool_call_index: &mut tool_call_index,
            tool_call_indexes: &mut tool_call_indexes,
            stream_in: &mut stream_in,
            stream_out: &mut stream_out,
            stream_cr: &mut stream_cr,
            stream_cw: &mut stream_cw,
            chunks: &mut chunks,
        };
        st.process(line);
        Ok(())
    })
    .await;

    if stream_in > 0 || stream_out > 0 || stream_cr > 0 {
        proxy
            .stats
            .lock()
            .expect("stats 锁")
            .record(model, stream_in, stream_out, stream_cr, stream_cw);
        recorded = true;
    }
    StreamOutcome {
        chunks,
        retry_tokens,
        recorded,
    }
}

/// 流式翻译的可变状态（借用式，避免与 async 闭包打架）。
struct StreamState<'a> {
    request_id: &'a str,
    model: &'a str,
    created: i64,
    sent_role: &'a mut bool,
    tool_call_index: &'a mut i64,
    tool_call_indexes: &'a mut Vec<(String, i64)>,
    stream_in: &'a mut i64,
    stream_out: &'a mut i64,
    stream_cr: &'a mut i64,
    stream_cw: &'a mut i64,
    chunks: &'a mut Vec<Vec<u8>>,
}

impl StreamState<'_> {
    fn idx_of(&self, id: &str) -> Option<i64> {
        self.tool_call_indexes
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| *v)
    }

    fn set_idx(&mut self, id: &str, i: i64) {
        if let Some(slot) = self.tool_call_indexes.iter_mut().find(|(k, _)| k == id) {
            slot.1 = i;
        } else {
            self.tool_call_indexes.push((id.to_string(), i));
        }
    }

    fn emit(&mut self, delta: OpenAIDelta) {
        let resp = OpenAIChatResponse {
            id: self.request_id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: None,
                delta: Some(delta),
                finish_reason: None,
            }],
            usage: None,
        };
        self.chunks.push(sse_data(&resp));
    }

    fn process(&mut self, line: &str) {
        let ev = match CCStreamEvent::from_line(line) {
            Some(e) => e,
            None => {
                if std::env::var("RS_PROXY_LINE_DBG").is_ok() {
                    eprintln!("DBG dropped line: {line:?}");
                }
                return;
            }
        };
        match ev.r#type.as_str() {
            "text-delta" => {
                let mut d = OpenAIDelta {
                    content: ev.text.clone(),
                    ..Default::default()
                };
                if !*self.sent_role {
                    d.role = "assistant".to_string();
                    *self.sent_role = true;
                }
                self.emit(d);
            }
            "reasoning-delta" => {
                let mut d = OpenAIDelta {
                    reasoning_content: ev.text.clone(),
                    ..Default::default()
                };
                if !*self.sent_role {
                    d.role = "assistant".to_string();
                    *self.sent_role = true;
                }
                self.emit(d);
            }
            "reasoning-start" | "reasoning-end" => {}
            "tool-use" => {
                let tc = OpenAIDeltaToolCall {
                    index: *self.tool_call_index,
                    id: ev.tool_call_id.clone(),
                    r#type: "function".to_string(),
                    function_name: ev.tool_name.clone(),
                    function_arguments: String::new(),
                    function_present: true,
                };
                let mut d = OpenAIDelta {
                    tool_calls: vec![tc],
                    ..Default::default()
                };
                if !*self.sent_role {
                    d.role = "assistant".to_string();
                    *self.sent_role = true;
                }
                self.emit(d);
                *self.tool_call_index += 1;
            }
            "tool-delta" => {
                let tc = OpenAIDeltaToolCall {
                    index: *self.tool_call_index - 1,
                    function_arguments: ev.text.clone(),
                    function_present: true,
                    ..Default::default()
                };
                self.emit(OpenAIDelta {
                    tool_calls: vec![tc],
                    ..Default::default()
                });
            }
            "tool-input-start" => {
                // 与 Go 同：`if _, ok := toolCallIndexes[event.ID]; !ok { … }`——只有未登记过
                // 才分配新 index（已登记则沿用，否则 `tool-input-delta` 会算错 index）。
                if self.idx_of(&ev.id).is_none() {
                    self.set_idx(&ev.id, *self.tool_call_index);
                    *self.tool_call_index += 1;
                }
                let idx = self.idx_of(&ev.id).unwrap_or(0);
                let tc = OpenAIDeltaToolCall {
                    index: idx,
                    id: ev.id.clone(),
                    r#type: "function".to_string(),
                    function_name: ev.tool_name.clone(),
                    function_arguments: String::new(),
                    function_present: true,
                };
                let mut d = OpenAIDelta {
                    tool_calls: vec![tc],
                    ..Default::default()
                };
                if !*self.sent_role {
                    d.role = "assistant".to_string();
                    *self.sent_role = true;
                }
                self.emit(d);
            }
            "tool-input-delta" => {
                let idx = match self.idx_of(&ev.id) {
                    Some(i) => i,
                    None => {
                        let i = *self.tool_call_index;
                        self.set_idx(&ev.id, i);
                        *self.tool_call_index += 1;
                        i
                    }
                };
                let tc = OpenAIDeltaToolCall {
                    index: idx,
                    function_arguments: ev.delta.clone(),
                    function_present: true,
                    ..Default::default()
                };
                self.emit(OpenAIDelta {
                    tool_calls: vec![tc],
                    ..Default::default()
                });
            }
            "tool-call" => {
                if self.idx_of(&ev.tool_call_id).is_some() {
                    return; // 已经流式发过
                }
                let idx = *self.tool_call_index;
                self.set_idx(&ev.tool_call_id, idx);
                *self.tool_call_index += 1;
                let args = match &ev.input {
                    Some(v) => to_go(v),
                    None => String::new(),
                };
                let tc = OpenAIDeltaToolCall {
                    index: idx,
                    id: ev.tool_call_id.clone(),
                    r#type: "function".to_string(),
                    function_name: ev.tool_name.clone(),
                    function_arguments: args,
                    function_present: true,
                };
                let mut d = OpenAIDelta {
                    tool_calls: vec![tc],
                    ..Default::default()
                };
                if !*self.sent_role {
                    d.role = "assistant".to_string();
                    *self.sent_role = true;
                }
                self.emit(d);
            }
            "finish" => {
                let reason = normalize_finish_reason(&ev.finish_reason).to_string();
                let resp = OpenAIChatResponse {
                    id: self.request_id.to_string(),
                    object: "chat.completion.chunk".to_string(),
                    created: self.created,
                    model: self.model.to_string(),
                    choices: vec![OpenAIChoice {
                        index: 0,
                        message: None,
                        delta: Some(OpenAIDelta::default()),
                        finish_reason: Some(reason),
                    }],
                    usage: None,
                };
                self.chunks.push(sse_data(&resp));
                self.chunks.push(b"data: [DONE]\n\n".to_vec());
                if let Some(u) = &ev.total_usage {
                    *self.stream_in = u.input_tokens;
                    *self.stream_out = u.output_tokens;
                    *self.stream_cr = u.cache_read_tokens;
                    *self.stream_cw = u.cache_write_tokens;
                }
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

/// `NonStreamResponse`：把整段上游 SSE 聚合成 `chat.completion`。
pub async fn non_stream_response(
    proxy: &Proxy,
    resp: reqwest::Response,
    request_id: &str,
    model: &str,
    created: i64,
) -> Vec<u8> {
    let res = read_cc_result(resp).await;
    let mut msg = OutMessage {
        role: "assistant".to_string(),
        content: Some(res.content.clone()),
        tool_calls: Vec::new(),
        reasoning_content: String::new(),
    };
    if !res.reasoning.is_empty() {
        msg.reasoning_content = res.reasoning.clone();
    }
    let mut finish_reason = "stop".to_string();
    if res.has_tool_calls {
        msg.content = None;
        msg.tool_calls = res.tool_calls.clone();
        finish_reason = "tool_calls".to_string();
    }
    let usage = OpenAIUsage {
        prompt_tokens: res.input_tokens,
        completion_tokens: res.output_tokens,
        total_tokens: res.input_tokens + res.output_tokens,
        reasoning_tokens: if res.reasoning_tokens > 0 {
            Some(res.reasoning_tokens)
        } else {
            None
        },
    };
    let response = OpenAIChatResponse {
        id: request_id.to_string(),
        object: "chat.completion".to_string(),
        created,
        model: model.to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: Some(msg),
            delta: None,
            finish_reason: Some(finish_reason),
        }],
        usage: Some(usage),
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
    b.push(b'\n'); // json.Encoder 的末尾换行
    b
}

/// 门禁/测试用：直接从请求体算出「出站体 + 下游体」（不起 HTTP 服务）。
pub fn build_cc_from_chat_body(body: &[u8]) -> Result<String, String> {
    let req = parse_chat_request(body)?;
    Ok(build_request(&req).to_go())
}

/// 供 usage 统计的入账辅助（把 `[`CCMessage`]` 的构造暴露给门禁）。
pub fn cc_messages_of(req: &OpenAIChatRequest) -> Vec<CCMessage> {
    let (_, rest) = crate::convert::extract_system(&req.messages);
    crate::convert::convert_messages(&rest)
}

/// 供门禁：`est_tokens` 的转发（避免门禁重复实现）。
pub fn est_tokens_of(req: &OpenAIChatRequest) -> i64 {
    let (_, rest) = crate::convert::extract_system(&req.messages);
    est_tokens(&rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_request() {
        let body = br#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":1024}"#;
        let r = parse_chat_request(body).unwrap();
        assert_eq!(r.model, "deepseek-v4-flash");
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.max_tokens, Some(1024));
        assert!(!r.stream);
    }

    #[test]
    fn parse_errors_match_go_text() {
        assert_eq!(
            parse_chat_request(b"{not json").unwrap_err(),
            "invalid character 'n' looking for beginning of object key string"
        );
        assert_eq!(
            parse_chat_request(b"\"scalar\"").unwrap_err(),
            "json: cannot unmarshal string into Go value of type api.OpenAIChatRequest"
        );
        assert_eq!(
            parse_chat_request(b"[]").unwrap_err(),
            "json: cannot unmarshal array into Go value of type api.OpenAIChatRequest"
        );
        assert_eq!(
            parse_chat_request(b"{\"model\":\"m\",\"messages\":\"nope\"}").unwrap_err(),
            "json: cannot unmarshal string into Go struct field OpenAIChatRequest.messages of type []api.OpenAIMessage"
        );
        assert_eq!(
            parse_chat_request(b"{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"x\"}]} trailing")
                .unwrap_err(),
            "invalid character 't' after top-level value"
        );
    }

    #[test]
    fn error_body_shape_and_trailing_newline() {
        let b = openai_error_body("Method not allowed", "invalid_request_error");
        assert_eq!(
            b,
            "{\"error\":{\"message\":\"Method not allowed\",\"type\":\"invalid_request_error\",\
             \"param\":null,\"code\":null}}\n"
        );
    }
}

