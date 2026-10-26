//! SSE 处理：首事件预检 + 非流式聚合。
//!
//! 上游是原生 OpenAI SSE（`chat.completion.chunk`），所以流式路径**原样透传**；
//! 需要本地做的只有两件事：
//!
//! 1. **首事件预检**：上游常用 HTTP 200 表达业务错误（`{"error_code":...}` 直接放在
//!    `data:` 里）。必须在把响应头发给客户端之前识别出来，否则客户端会拿到 200 再看到
//!    一个空的假回复。
//! 2. **非流式聚合**：客户端要 `stream:false` 时，把 SSE 拼回一个 `chat.completion`
//!    （上游 `glm-5.3` 只支持流式，所以本插件对上游**一律**发 stream=true）。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::upstream::{embedded_error, UpError};

/// 从缓冲区里取第一个完成的 `data:` 事件。
pub fn first_data_event(buf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(buf);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.trim();
            if !payload.is_empty() {
                return Some(payload.to_string());
            }
        }
    }
    None
}

/// 预检：首事件是业务错误就返回 `UpError`。
pub fn preflight_error(buf: &[u8]) -> Option<UpError> {
    let payload = first_data_event(buf)?;
    if payload == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(&payload).ok()?;
    embedded_error(&v)
}

/// 非流式聚合结果。
#[derive(Default)]
pub struct Aggregate {
    pub id: String,
    pub model: String,
    pub created: i64,
    pub content: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Value>,
    tool_calls: BTreeMap<usize, ToolAcc>,
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    arguments: String,
    kind: String,
}

impl Aggregate {
    /// 消费整段 SSE 文本。
    pub fn feed(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            if self.id.is_empty() {
                self.id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default().to_string();
            }
            if self.model.is_empty() {
                self.model = v
                    .get("model")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
            }
            if self.created == 0 {
                self.created = v.get("created").and_then(|x| x.as_i64()).unwrap_or(0);
            }
            if let Some(u) = v.get("usage") {
                if !u.is_null() {
                    self.usage = Some(u.clone());
                }
            }
            let Some(choice) = v
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
            else {
                continue;
            };
            if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
                if !fr.is_empty() {
                    self.finish_reason = Some(fr.to_string());
                }
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(c) = delta.get("content").and_then(|x| x.as_str()) {
                self.content.push_str(c);
            }
            if let Some(r) = delta.get("reasoning_content").and_then(|x| x.as_str()) {
                self.reasoning.push_str(r);
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    let slot = self.tool_calls.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                        slot.id = id.to_string();
                    }
                    if let Some(k) = tc.get("type").and_then(|x| x.as_str()) {
                        slot.kind = k.to_string();
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(|x| x.as_str()) {
                            slot.name = n.to_string();
                        }
                        if let Some(a) = f.get("arguments").and_then(|x| x.as_str()) {
                            slot.arguments.push_str(a);
                        }
                    }
                }
            }
        }
    }

    /// 拼成 `chat.completion`。
    pub fn into_response(self, fallback_model: &str) -> Value {
        let mut message = json!({
            "role": "assistant",
            "content": self.content,
        });
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(self.reasoning);
        }
        if !self.tool_calls.is_empty() {
            let calls: Vec<Value> = self
                .tool_calls
                .into_values()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "type": if t.kind.is_empty() { "function" } else { &t.kind },
                        "function": { "name": t.name, "arguments": t.arguments }
                    })
                })
                .collect();
            message["tool_calls"] = Value::Array(calls);
        }
        let finish = self.finish_reason.unwrap_or_else(|| {
            if message.get("tool_calls").is_some() {
                "tool_calls".to_string()
            } else {
                "stop".to_string()
            }
        });
        let model = if self.model.is_empty() {
            fallback_model.to_string()
        } else {
            self.model
        };
        let mut out = json!({
            "id": if self.id.is_empty() { format!("chatcmpl-codearts-{}", crate::upstream::random_hex(8)) } else { self.id },
            "object": "chat.completion",
            "created": if self.created == 0 { crate::config::now_unix() } else { self.created },
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish
            }]
        });
        if let Some(u) = self.usage {
            out["usage"] = u;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_content_and_reasoning() {
        let sse = concat!(
            "data:{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"想\"}}]}\n\n",
            "data:{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data:{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",",
            "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
            "data:[DONE]\n\n"
        );
        let mut agg = Aggregate::default();
        agg.feed(sse);
        let v = agg.into_response("fallback");
        assert_eq!(v["choices"][0]["message"]["content"], "你好");
        assert_eq!(v["choices"][0]["message"]["reasoning_content"], "想");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["total_tokens"], 3);
    }

    #[test]
    fn detects_embedded_error() {
        let buf = b"data:{\"error_code\":\"InferHub.002002009.404\",\"error_msg\":\"not registered\"}\n\n";
        let err = preflight_error(buf).expect("应识别为错误");
        assert_eq!(err.status, 404);
    }
}
