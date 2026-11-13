//! 协议适配：Anthropic `/v1/messages` 与 OpenAI Responses `/v1/responses`
//! 都翻译成内部 chat/completions 请求，回包再翻回各自协议。
//!
//! 只覆盖代理实际需要的子集：文本、工具调用、流式增量；不认识的块原样丢弃
//! （宁可少字段，也不编造）。

use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::upstream::random_hex;

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for it in items {
                if let Some(t) = it.get("text").and_then(|x| x.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn system_text(v: Option<&Value>) -> Option<String> {
    let v = v?;
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Array(items) => {
            let mut out = String::new();
            for it in items {
                if let Some(t) = it.get("text").and_then(|x| x.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            if out.trim().is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

fn image_url(source: &Value) -> Option<String> {
    let data = source.get("data").and_then(|x| x.as_str())?;
    let media = source
        .get("media_type")
        .and_then(|x| x.as_str())
        .unwrap_or("image/png");
    Some(format!("data:{media};base64,{data}"))
}

/// Anthropic messages → OpenAI chat 请求。
pub fn anthropic_to_chat(v: &Value) -> Result<Value, String> {
    let model = v
        .get("model")
        .and_then(|x| x.as_str())
        .ok_or("缺少 model")?;
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(sys) = system_text(v.get("system")) {
        msgs.push(json!({"role":"system","content":sys}));
    }
    for m in v
        .get("messages")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("user");
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        match content {
            Value::Array(blocks) => {
                let mut text = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                let mut tool_results: Vec<Value> = Vec::new();
                let mut images: Vec<Value> = Vec::new();
                for b in blocks {
                    match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                        "text" => {
                            if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                        "image" => {
                            if let Some(url) = b.get("source").and_then(image_url) {
                                images.push(json!({"type":"image_url","image_url":{"url":url}}));
                            }
                        }
                        "tool_use" => {
                            let id = b.get("id").and_then(|x| x.as_str()).unwrap_or("");
                            let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            let args = b.get("input").cloned().unwrap_or(json!({}));
                            tool_calls.push(json!({
                                "id": id, "type": "function",
                                "function": {"name": name, "arguments": args.to_string()}
                            }));
                        }
                        "tool_result" => {
                            let id = b
                                .get("tool_use_id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("");
                            let content = b.get("content").cloned().unwrap_or(Value::Null);
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": text_of(&content),
                            }));
                        }
                        _ => {}
                    }
                }
                if !tool_results.is_empty() {
                    msgs.push(json!({"role": role, "content": text}));
                    msgs.extend(tool_results);
                    continue;
                }
                if role == "assistant" {
                    let mut msg = json!({"role": "assistant", "content": text});
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = Value::Array(tool_calls);
                    }
                    msgs.push(msg);
                } else if !images.is_empty() {
                    let mut parts = vec![json!({"type":"text","text":text})];
                    parts.extend(images);
                    msgs.push(json!({"role": role, "content": parts}));
                } else {
                    msgs.push(json!({"role": role, "content": text}));
                }
            }
            other => {
                let t = text_of(&other);
                let msg = if role == "assistant" {
                    json!({"role":"assistant","content":t})
                } else {
                    json!({"role":role,"content":t})
                };
                msgs.push(msg);
            }
        }
    }
    let mut out = json!({
        "model": model,
        "messages": msgs,
        "max_tokens": v.get("max_tokens").cloned().unwrap_or(json!(4096)),
    });
    if let Some(t) = v.get("temperature") {
        out["temperature"] = t.clone();
    }
    if let Some(t) = v.get("top_p") {
        out["top_p"] = t.clone();
    }
    if let Some(t) = v.get("stream") {
        out["stream"] = t.clone();
    }
    if let Some(stop) = v.get("stop_sequences").and_then(|x| x.as_array()) {
        if !stop.is_empty() {
            out["stop"] = Value::Array(stop.clone());
        }
    }
    if let Some(tools) = v.get("tools").and_then(|x| x.as_array()) {
        let fns: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                        "description": t.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                        "parameters": t.get("input_schema").cloned().unwrap_or(json!({"type":"object"})),
                    }
                })
            })
            .collect();
        if !fns.is_empty() {
            out["tools"] = Value::Array(fns);
        }
    }
    if let Some(tc) = v.get("tool_choice") {
        let kind = tc.get("type").and_then(|x| x.as_str()).unwrap_or("");
        out["tool_choice"] = match kind {
            "any" => json!("required"),
            "none" => json!("none"),
            "tool" => tc
                .get("name")
                .and_then(|x| x.as_str())
                .map(|n| json!({"type":"function","function":{"name":n}}))
                .unwrap_or(json!("auto")),
            _ => json!("auto"),
        };
    }
    Ok(out)
}

/// OpenAI chat 响应 → Anthropic message。
pub fn chat_to_anthropic(v: &Value, model: &str) -> Value {
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content: Vec<Value> = Vec::new();
    if let Some(t) = msg.get("content").and_then(|x| x.as_str()) {
        if !t.is_empty() {
            content.push(json!({"type":"text","text":t}));
        }
    }
    let mut stop = "end_turn";
    if let Some(calls) = msg.get("tool_calls").and_then(|x| x.as_array()) {
        for c in calls {
            let f = c.get("function").cloned().unwrap_or(json!({}));
            let args = f
                .get("arguments")
                .and_then(|x| x.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": c.get("id").cloned().unwrap_or(json!("")),
                "name": f.get("name").cloned().unwrap_or(json!("")),
                "input": args,
            }));
            stop = "tool_use";
        }
    }
    if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
        stop = match fr {
            "tool_calls" => "tool_use",
            "length" => "max_tokens",
            "content_filter" => "refusal",
            _ => stop,
        };
    }
    let usage = v.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": format!("msg_{}", random_hex(12)),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
        },
    })
}

fn sse(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// OpenAI chat SSE → Anthropic SSE。
pub fn anthropic_stream(body: Body, model: String) -> Body {
    let stream = body.into_data_stream().scan(
        (Vec::<u8>::new(), false, 0usize, 0i64),
        move |(buf, started, index, out_tokens), chunk| {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => return futures_util::future::ready(Some(vec![])),
            };
            buf.extend_from_slice(&chunk);
            let mut out: Vec<Bytes> = Vec::new();
            // 按行切（SSE 的 data 行以 \n 结尾）
            let mut consumed = 0;
            while let Some(pos) = buf[consumed..].iter().position(|&b| b == b'\n') {
                let line_end = consumed + pos;
                let line = String::from_utf8_lossy(&buf[consumed..line_end])
                    .trim()
                    .to_string();
                consumed = line_end + 1;
                if !line.starts_with("data:") {
                    continue;
                }
                let payload = line[5..].trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let v: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !*started {
                    *started = true;
                    out.push(sse(
                        "message_start",
                        &json!({"type":"message_start","message":{
                            "id": format!("msg_{}", random_hex(12)),
                            "type": "message", "role": "assistant", "model": model,
                            "content": [], "stop_reason": null, "usage": {"input_tokens": 0, "output_tokens": 0}
                        }}),
                    ));
                }
                if let Some(delta) = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                {
                    if let Some(t) = delta.get("content").and_then(|x| x.as_str()) {
                        if !t.is_empty() {
                            if *index == 0 {
                                out.push(sse(
                                    "content_block_start",
                                    &json!({"type":"content_block_start","index":0,
                                            "content_block":{"type":"text","text":""}}),
                                ));
                            }
                            out.push(sse(
                                "content_block_delta",
                                &json!({"type":"content_block_delta","index":0,
                                        "delta":{"type":"text_delta","text":t}}),
                            ));
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                        for c in calls {
                            let i = c.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize + 1;
                            if i > *index {
                                *index = i;
                                let f = c.get("function").cloned().unwrap_or(json!({}));
                                out.push(sse(
                                    "content_block_start",
                                    &json!({"type":"content_block_start","index":i,
                                        "content_block":{"type":"tool_use",
                                            "id": c.get("id").cloned().unwrap_or(json!("")),
                                            "name": f.get("name").cloned().unwrap_or(json!("")),
                                            "input":{}}}),
                                ));
                            }
                            if let Some(args) = c
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|x| x.as_str())
                            {
                                out.push(sse(
                                    "content_block_delta",
                                    &json!({"type":"content_block_delta","index":i,
                                        "delta":{"type":"input_json_delta","partial_json":args}}),
                                ));
                            }
                        }
                    }
                }
                if let Some(u) = v.get("usage").and_then(|u| u.get("completion_tokens")).and_then(|x| x.as_i64()) {
                    *out_tokens = u;
                }
            }
            if consumed > 0 {
                buf.drain(0..consumed);
            }
            futures_util::future::ready(Some(out))
        },
    );
    let finish = futures_util::stream::once(async move {
        vec![
            sse(
                "content_block_stop",
                &json!({"type":"content_block_stop","index":0}),
            ),
            sse(
                "message_delta",
                &json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
                        "usage":{"output_tokens":0}}),
            ),
            sse("message_stop", &json!({"type":"message_stop"})),
        ]
    });
    let merged = stream
        .chain(finish)
        .flat_map(|v| futures_util::stream::iter(v))
        .map(|b| Ok::<Bytes, std::io::Error>(b));
    Body::from_stream(merged)
}

/// OpenAI Responses 请求 → chat 请求。
pub fn responses_to_chat(v: &Value) -> Result<Value, String> {
    let model = v
        .get("model")
        .and_then(|x| x.as_str())
        .ok_or("缺少 model")?;
    let mut msgs: Vec<Value> = Vec::new();
    if let Some(ins) = system_text(v.get("instructions")) {
        msgs.push(json!({"role":"system","content":ins}));
    }
    match v.get("input") {
        Some(Value::String(s)) => msgs.push(json!({"role":"user","content":s})),
        Some(Value::Array(items)) => {
            for it in items {
                let kind = it.get("type").and_then(|x| x.as_str()).unwrap_or("message");
                match kind {
                    "message" => {
                        let role = it.get("role").and_then(|x| x.as_str()).unwrap_or("user");
                        let content = it.get("content").cloned().unwrap_or(Value::Null);
                        let text = match &content {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => {
                                let mut out = String::new();
                                for p in parts {
                                    if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                                        if !out.is_empty() {
                                            out.push('\n');
                                        }
                                        out.push_str(t);
                                    }
                                }
                                out
                            }
                            _ => String::new(),
                        };
                        msgs.push(json!({"role": role, "content": text}));
                    }
                    "function_call" => {
                        let call_id = it
                            .get("call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        let args = it
                            .get("arguments")
                            .cloned()
                            .unwrap_or(json!("{}"));
                        msgs.push(json!({"role":"assistant","content":null,"tool_calls":[{
                            "id": call_id, "type":"function",
                            "function": {"name": name, "arguments": args}
                        }]}));
                    }
                    "function_call_output" => {
                        let call_id = it
                            .get("call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let output = it.get("output").cloned().unwrap_or(Value::Null);
                        msgs.push(json!({
                            "role": "tool", "tool_call_id": call_id,
                            "content": text_of(&output),
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let mut out = json!({
        "model": model,
        "messages": msgs,
        "max_tokens": v.get("max_output_tokens").cloned().unwrap_or(json!(4096)),
    });
    if let Some(t) = v.get("temperature") {
        out["temperature"] = t.clone();
    }
    if let Some(t) = v.get("stream") {
        out["stream"] = t.clone();
    }
    if let Some(tools) = v.get("tools").and_then(|x| x.as_array()) {
        let fns: Vec<Value> = tools
            .iter()
            .filter(|t| t.get("type").and_then(|x| x.as_str()) == Some("function"))
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").and_then(|x| x.as_str()).unwrap_or(""),
                        "description": t.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                        "parameters": t.get("parameters").cloned().unwrap_or(json!({"type":"object"})),
                    }
                })
            })
            .collect();
        if !fns.is_empty() {
            out["tools"] = Value::Array(fns);
        }
    }
    Ok(out)
}

/// OpenAI chat 响应 → Responses 对象。
pub fn chat_to_responses(v: &Value, model: &str) -> Value {
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let text = msg
        .get("content")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let mut output = vec![json!({
        "id": format!("msg_{}", random_hex(12)),
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type":"output_text","text":text,"annotations":[]}],
    })];
    if let Some(calls) = msg.get("tool_calls").and_then(|x| x.as_array()) {
        for c in calls {
            let f = c.get("function").cloned().unwrap_or(json!({}));
            output.push(json!({
                "id": format!("fc_{}", random_hex(8)),
                "type": "function_call",
                "status": "completed",
                "call_id": c.get("id").cloned().unwrap_or(json!("")),
                "name": f.get("name").cloned().unwrap_or(json!("")),
                "arguments": f.get("arguments").cloned().unwrap_or(json!("{}")),
            }));
        }
    }
    let usage = v.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": format!("resp_{}", random_hex(12)),
        "object": "response",
        "created_at": crate::config::now_unix(),
        "model": model,
        "status": "completed",
        "output": output,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").cloned().unwrap_or(json!(0)),
            "output_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
        },
    })
}

/// OpenAI chat SSE → Responses 事件流（只发文本增量与完成事件）。
pub fn responses_stream(body: Body, model: String) -> Body {
    let resp_id = format!("resp_{}", random_hex(12));
    let item_id = format!("msg_{}", random_hex(12));
    let resp_id2 = resp_id.clone();
    let item_id2 = item_id.clone();
    let model2 = model.clone();
    let stream = body.into_data_stream().scan(
        (Vec::<u8>::new(), false, String::new()),
        move |(buf, started, acc), chunk| {
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => return futures_util::future::ready(Some(vec![])),
            };
            buf.extend_from_slice(&chunk);
            let mut out: Vec<Bytes> = Vec::new();
            let mut consumed = 0;
            while let Some(pos) = buf[consumed..].iter().position(|&b| b == b'\n') {
                let line_end = consumed + pos;
                let line = String::from_utf8_lossy(&buf[consumed..line_end])
                    .trim()
                    .to_string();
                consumed = line_end + 1;
                if !line.starts_with("data:") {
                    continue;
                }
                let payload = line[5..].trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let v: Value = match serde_json::from_str(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !*started {
                    *started = true;
                    out.push(sse(
                        "response.created",
                        &json!({"type":"response.created","response":{
                            "id": resp_id, "object": "response", "created_at": crate::config::now_unix(),
                            "model": model, "status": "in_progress", "output": []
                        }}),
                    ));
                    out.push(sse(
                        "response.output_item.added",
                        &json!({"type":"response.output_item.added","output_index":0,
                            "item":{"id": item_id, "type":"message", "role":"assistant",
                                    "status":"in_progress", "content":[]}}),
                    ));
                    out.push(sse(
                        "response.content_part.added",
                        &json!({"type":"response.content_part.added","item_id":item_id,
                            "output_index":0,"content_index":0,
                            "part":{"type":"output_text","text":"","annotations":[]}}),
                    ));
                }
                if let Some(t) = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| d.get("content"))
                    .and_then(|x| x.as_str())
                {
                    if !t.is_empty() {
                        acc.push_str(t);
                        out.push(sse(
                            "response.output_text.delta",
                            &json!({"type":"response.output_text.delta","item_id":item_id,
                                "output_index":0,"content_index":0,"delta":t}),
                        ));
                    }
                }
            }
            if consumed > 0 {
                buf.drain(0..consumed);
            }
            futures_util::future::ready(Some(out))
        },
    );
    let finish = futures_util::stream::once(async move {
        vec![
            sse(
                "response.output_text.done",
                &json!({"type":"response.output_text.done","item_id":item_id2,
                    "output_index":0,"content_index":0,"text":""}),
            ),
            sse(
                "response.completed",
                &json!({"type":"response.completed","response":{
                    "id": resp_id2, "object": "response", "created_at": crate::config::now_unix(),
                    "model": model2, "status": "completed", "output": []
                }}),
            ),
        ]
    });
    let merged = stream
        .chain(finish)
        .flat_map(|v| futures_util::stream::iter(v))
        .map(|b| Ok::<Bytes, std::io::Error>(b));
    Body::from_stream(merged)
}

/// 供测试：从 JSON 里取 usage（两协议共用）。
pub fn usage_pair(v: &Value) -> (i64, i64) {
    let u = v.get("usage").cloned().unwrap_or(json!({}));
    (
        u.get("prompt_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
        u.get("completion_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_request_converts_system_and_tools() {
        let v = json!({
            "model": "glm-5.3", "max_tokens": 100, "stream": true,
            "system": [{"type":"text","text":"你是助手"}],
            "tools": [{"name":"read","description":"读文件","input_schema":{"type":"object","properties":{"p":{"type":"string"}}}}],
            "tool_choice": {"type":"auto"},
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":[{"type":"text","text":"看下"},{"type":"tool_use","id":"t1","name":"read","input":{"p":"a"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"OK"}]}
            ]
        });
        let out = anthropic_to_chat(&v).unwrap();
        assert_eq!(out["model"], "glm-5.3");
        assert_eq!(out["stream"], true);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["tools"][0]["function"]["name"], "read");
        assert_eq!(out["tool_choice"], "auto");
        // 工具结果翻成 role=tool
        assert!(out["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "t1"));
    }

    #[test]
    fn anthropic_response_converts_text_and_tool_calls() {
        let chat = json!({
            "choices": [{"message": {"content": "答案",
                "tool_calls": [{"id":"c1","type":"function","function":{"name":"read","arguments":"{\"p\":\"a\"}"}}]},
                "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let out = chat_to_anthropic(&chat, "glm-5.3");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "答案");
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["content"][1]["input"]["p"], "a");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"]["input_tokens"], 10);
    }

    #[test]
    fn responses_request_converts_input_items() {
        let v = json!({
            "model": "glm-5.3", "instructions": "sys", "stream": false,
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"你好"}]},
                {"type":"function_call_output","call_id":"c1","output":"42"}
            ]
        });
        let out = responses_to_chat(&v).unwrap();
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["content"], "你好");
        assert_eq!(out["messages"][2]["role"], "tool");
        assert_eq!(out["messages"][2]["tool_call_id"], "c1");
    }

    #[test]
    fn responses_response_has_message_item() {
        let chat = json!({"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}});
        let out = chat_to_responses(&chat, "glm-5.3");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output"][0]["type"], "message");
        assert_eq!(out["output"][0]["content"][0]["text"], "hi");
        assert_eq!(out["usage"]["total_tokens"], 3);
    }
}