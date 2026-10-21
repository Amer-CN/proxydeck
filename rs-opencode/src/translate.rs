//! translate.rs —— OpenAI ↔ opencode 会话/事件 的纯翻译层（不碰网络）。
//!
//! 两件事：
//! 1. 把 OpenAI `messages` 压平成 opencode `prompt.text` 需要的整段文本
//!    （照 `zen-local-proxy.ts` 的 `flattenMessages` 行为，不移植其代码）；
//! 2. 增量解析 `/api/event` 的 SSE 帧，并构造 `chat.completion[.chunk]` 的 JSON。
//!
//! SSE 解析用字节缓冲而非整块 `String`，避免 UTF-8 多字节被 chunk 边界劈开。
use serde_json::{json, Value};

/// 单条 OpenAI message 的文本：
/// * `content` 是字符串 → 直接用
/// * `content` 是数组 → 只取 `type == "text"` 的 `text`，用 `\n` 连接
/// * 其它（null/对象/数字）→ 空串
pub fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(Value::as_str) == Some("text") {
                    p.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 把 OpenAI `messages` 压成 opencode 的整段 prompt 文本。
///
/// ```text
/// [system]
/// <内容>
///
/// [user]
/// <内容>
///
/// [tool result for <id>]
/// <内容>
/// ```
///
/// 空内容的消息跳过；全部为空时返回空串（调用方回 400）。
pub fn flatten_messages(messages: &[Value]) -> String {
    let mut sections: Vec<String> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let text = message_text(m.get("content").unwrap_or(&Value::Null));
        // 空白-only 也按空内容跳过：否则会拼出只有 `[user]` 标签的伪 prompt。
        if text.trim().is_empty() {
            continue;
        }
        let label = if role == "tool" {
            let id = m
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("call");
            format!("[tool result for {id}]")
        } else {
            format!("[{role}]")
        };
        sections.push(format!("{label}\n{text}"));
    }
    sections.join("\n\n")
}

/// OpenAI `usage` 的三字段（`completion_tokens = output + reasoning`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    pub fn to_json(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
        })
    }
}

/// 从 `session.step.ended.data.tokens` 取 usage。
///
/// `tokens` 不是对象时返回 None（保持"未知"而不是编 0）。
pub fn usage_from(tokens: &Value) -> Option<Usage> {
    if !tokens.is_object() {
        return None;
    }
    let input = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
    let output = tokens.get("output").and_then(Value::as_u64).unwrap_or(0);
    let reasoning = tokens.get("reasoning").and_then(Value::as_u64).unwrap_or(0);
    let completion = output.saturating_add(reasoning);
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: completion,
        total_tokens: input.saturating_add(completion),
    })
}

/// 构造一个 `chat.completion.chunk` 帧。`finish` 为 None 时序列化成 `null`。
pub fn completion_chunk(
    id: &str,
    model: &str,
    created: i64,
    delta: Value,
    finish: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }],
    })
}

/// 一条 SSE 事件：`type` 是顶层事件名，`data` 是 `data` 字段（缺省时退回整帧）。
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub ty: String,
    pub data: Value,
}

/// 增量 SSE 解码器：`push(chunk)` 返回本次能拼出的完整帧。
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 喂入一段原始字节，返回所有完整帧解析出的事件。
    ///
    /// 支持 `\n\n` 与 `\r\n\r\n` 两种分帧；帧内 `data:` 多行按 SSE 规范拼接。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let sep = match find_separator(&self.buf) {
                Some(v) => v,
                None => break,
            };
            let frame = self.buf[..sep.0].to_vec();
            self.buf.drain(..sep.0 + sep.1);
            if let Some(ev) = parse_frame(&frame) {
                out.push(ev);
            }
        }
        // 防御：若上游一直不发分帧符，别让缓冲无限涨。
        if self.buf.len() > 16 * 1024 * 1024 {
            self.buf.clear();
        }
        out
    }
}

/// 找最早出现的分帧符，返回 (下标, 分隔符长度)。
fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = find_subslice(buf, b"\r\n\r\n").map(|i| (i, 4));
    let lf = find_subslice(buf, b"\n\n").map(|i| (i, 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 解析单个 SSE 帧。没有 `data:`、`[DONE]`、坏 JSON 都返回 None（忽略该帧）。
fn parse_frame(frame: &[u8]) -> Option<Event> {
    let text = String::from_utf8_lossy(frame);
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim());
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("");
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let parsed: Value = serde_json::from_str(&data).ok()?;
    let ty = parsed.get("type").and_then(Value::as_str)?.to_string();
    let payload = match parsed.get("data") {
        Some(Value::Null) | None => parsed,
        Some(v) => v.clone(),
    };
    Some(Event { ty, data: payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_string_array_empty_and_multi_turn() {
        let messages = vec![
            json!({"role": "system", "content": "S"}),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "A"},
                    {"type": "image_url", "image_url": {"url": "x"}},
                    {"type": "text", "text": "B"}
                ]
            }),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": [{"type": "image_url"}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "R"}),
        ];
        assert_eq!(
            flatten_messages(&messages),
            "[system]\nS\n\n[user]\nA\nB\n\n[tool result for call_1]\nR"
        );
    }

    #[test]
    fn flatten_empty_is_empty() {
        assert_eq!(flatten_messages(&[]), "");
        assert_eq!(
            flatten_messages(&[json!({"role": "user", "content": null})]),
            ""
        );
        assert_eq!(
            flatten_messages(&[json!({"role": "user", "content": [{"type": "image_url"}]})]),
            ""
        );
        assert_eq!(
            flatten_messages(&[json!({"role": "user", "content": "   "})]),
            "",
            "纯空白消息不应留下只有 [user] 标签的伪 prompt"
        );
    }

    #[test]
    fn tool_without_id_uses_call_placeholder() {
        assert_eq!(
            flatten_messages(&[json!({"role": "tool", "content": "R"})]),
            "[tool result for call]\nR"
        );
    }

    #[test]
    fn usage_adds_output_and_reasoning() {
        let u = usage_from(&json!({"input": 10, "output": 3, "reasoning": 2})).unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
        assert_eq!(u.total_tokens, 15);
        assert!(usage_from(&Value::Null).is_none());
    }

    #[test]
    fn sse_decoder_handles_split_frames_and_crlf() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: {\"type\":\"a\",\"da").is_empty());
        let evs = d.push(b"ta\":{\"x\":1}}\n\ndata: {\"type\":\"b\",\"data\":{\"y\":2}}\r\n\r\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].ty, "a");
        assert_eq!(evs[0].data["x"], 1);
        assert_eq!(evs[1].ty, "b");
        assert_eq!(evs[1].data["y"], 2);
    }

    #[test]
    fn sse_decoder_ignores_heartbeat_and_done() {
        let mut d = SseDecoder::new();
        let evs = d.push(b": heartbeat\n\ndata: [DONE]\n\ndata: {bad json}\n\n");
        assert!(evs.is_empty());
    }

    #[test]
    fn sse_decoder_joins_multi_line_data() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: {\"type\":\"x\",\ndata: \"data\":{\"ok\":true}}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].ty, "x");
        assert_eq!(evs[0].data["ok"], true);
    }

    #[test]
    fn chunk_object_is_chat_completion_chunk() {
        let c = completion_chunk("chatcmpl-1", "m", 1, json!({"content": "hi"}), None);
        assert_eq!(c["object"], "chat.completion.chunk");
        assert_eq!(c["choices"][0]["delta"]["content"], "hi");
        assert!(c["choices"][0]["finish_reason"].is_null());
    }
}
