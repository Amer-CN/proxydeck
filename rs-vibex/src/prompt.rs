//! prompt.rs —— messages→prompt 打平与 claude_event 事件折叠（对应 Go `internal/vibex/prompt.go`）。
//!
//! C28~C34。`OutEvent` 的 JSON 形态必须与 Go 的 `json.Marshal([]outEvent{…})` 逐字节一致：
//! Go 的 `outEvent` 是 **struct 且无 json tag**，故字段名是 `Kind`/`Text`/`Usage`/`Flags`
//! 且按**声明顺序**写出（不是 map 的排序）。
use serde_json::{Map, Value};

use crate::gjson::{push_go_string, J};
use crate::util::{str_of, to_map, truthy};

/// 一轮对话中由上游产出、供 HTTP 层消费的事件。
/// `kind` 为空 = 哨兵（本轮结束，且发送者的错误状态已就位）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutEvent {
    pub kind: String,
    pub text: String,
    pub usage: Option<Value>,
    pub flags: Option<Value>,
}

impl OutEvent {
    pub fn new(kind: &str, text: &str) -> OutEvent {
        OutEvent {
            kind: kind.to_string(),
            text: text.to_string(),
            usage: None,
            flags: None,
        }
    }

    /// 与 Go `json.Marshal(outEvent)` 同形的单行 JSON（struct 字段顺序 + map 键排序）。
    pub fn to_go(&self) -> String {
        let mut out = String::from("{");
        out.push_str("\"Kind\":");
        push_go_string(&mut out, &self.kind);
        out.push_str(",\"Text\":");
        push_go_string(&mut out, &self.text);
        out.push_str(",\"Usage\":");
        match &self.usage {
            Some(v) => out.push_str(&J::from_value(v).to_go()),
            None => out.push_str("null"),
        }
        out.push_str(",\"Flags\":");
        match &self.flags {
            Some(v) => out.push_str(&J::from_value(v).to_go()),
            None => out.push_str("null"),
        }
        out.push('}');
        out
    }
}

/// 与 Go `json.Marshal([]outEvent)` 同形的数组 JSON。
///
/// `None` = Go 侧返回的是 **nil slice**（`json.Marshal(nil slice)` 是 `null`，不是 `[]`）——
/// 这条形状差异既是门禁的断言对象，也如实反映「没有事件」在 Go 里的表示。
pub fn events_to_go(events: Option<&[OutEvent]>) -> String {
    let events = match events {
        Some(e) => e,
        None => return "null".to_string(),
    };
    let mut out = String::from("[");
    for (i, e) in events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&e.to_go());
    }
    out.push(']');
    out
}

/// `extractText`：string 原样；数组逐块；map→`text`；其它→`str(v)`。
pub fn extract_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
            for blk in blocks {
                match blk {
                    Value::String(s) => parts.push(s.clone()),
                    Value::Object(b) => {
                        let has_image = b.contains_key("image_url");
                        if str_of(b.get("type")) == "image_url" || has_image {
                            parts.push(crate::config::IMAGE_PLACEHOLDER.to_string());
                        } else if let Some(Value::String(t)) = b.get("text") {
                            parts.push(t.clone());
                        } else if let Some(Value::String(t)) = b.get("content") {
                            parts.push(t.clone());
                        }
                    }
                    _ => {}
                }
            }
            let non_empty: Vec<&String> = parts.iter().filter(|p| !p.is_empty()).collect();
            non_empty
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<&str>>()
                .join("\n")
        }
        Value::Object(c) => {
            if let Some(Value::String(t)) = c.get("text") {
                return t.clone();
            }
            crate::util::str_v(content)
        }
        other => crate::util::str_v(other),
    }
}

/// `buildPrompt`：把 OpenAI messages 拍平成单条 VibeX prompt。
pub fn build_prompt(messages: &[Value], chat_instruction: &str) -> String {
    let mut sys_texts: Vec<String> = Vec::new();
    // (label, text)
    let mut history: Vec<(String, String)> = Vec::new();
    for raw in messages {
        let m = match raw.as_object() {
            Some(m) => m,
            None => continue,
        };
        let mut role = str_of(m.get("role")).to_lowercase();
        if role.is_empty() {
            role = "user".to_string();
        }
        let text = extract_text(m.get("content").unwrap_or(&Value::Null));
        match role.as_str() {
            "system" | "developer" => sys_texts.push(text),
            "assistant" => history.push(("助手".into(), text)),
            "tool" => history.push(("工具输出".into(), text)),
            _ => history.push(("用户".into(), text)),
        }
    }
    let default_instruction = crate::config::DEFAULT_INSTRUCTION;
    let instruction = if chat_instruction.is_empty() {
        default_instruction
    } else {
        chat_instruction
    };
    if sys_texts.is_empty() && messages.len() == 1 {
        if let Some(m) = messages[0].as_object() {
            if str_of(m.get("role")).to_lowercase() == "user" {
                let base = extract_text(m.get("content").unwrap_or(&Value::Null));
                if chat_instruction.is_empty() {
                    return base;
                }
                return format!("{base}\n\n【任务】\n{instruction}");
            }
        }
    }
    let mut blocks: Vec<String> = Vec::new();
    if !sys_texts.is_empty() {
        blocks.push(format!("【系统设定】\n{}", sys_texts.join("\n")));
    }
    if !history.is_empty() {
        let lines: Vec<String> = history
            .iter()
            .map(|(label, text)| format!("【{label}】\n{text}"))
            .collect();
        blocks.push(format!("【对话记录】\n{}", lines.join("\n\n")));
    }
    blocks.push(format!("【任务】\n{instruction}"));
    blocks.join("\n\n")
}

/// `claudeEvents`：把一条 `claude_event` 的 `event` 折叠成 `OutEvent` 序列。
///
/// 返回 `None` = Go 侧返回 **nil slice**（该分支 `return nil`，或 `var out []outEvent`
/// 一个都没追加），与「空切片」在 JSON 上不同：`null` ≠ `[]`。
pub fn claude_events(ev: &Value, expose_tools: bool) -> Option<Vec<OutEvent>> {
    let m = match ev.as_object() {
        Some(m) => m,
        None => return None,
    };
    match str_of(m.get("type")).as_str() {
        "stream_event" => {
            let inner = match m.get("event").and_then(to_map) {
                Some(i) => i,
                None => return None,
            };
            if str_of(inner.get("type")) != "content_block_delta" {
                return None;
            }
            let delta = match inner.get("delta").and_then(to_map) {
                Some(d) => d,
                None => return None,
            };
            match str_of(delta.get("type")).as_str() {
                "text_delta" => Some(vec![OutEvent::new("text", &str_of(delta.get("text")))]),
                "thinking_delta" => {
                    Some(vec![OutEvent::new("thinking", &str_of(delta.get("thinking")))])
                }
                _ => None,
            }
        }
        "assistant" => {
            let msg = m.get("message").and_then(to_map);
            let blocks = msg
                .and_then(|mm| mm.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            let is_err = truthy(m.get("isApiErrorMessage").unwrap_or(&Value::Null))
                || truthy(m.get("balance_insufficient").unwrap_or(&Value::Null));
            let mut out: Vec<OutEvent> = Vec::new();
            for raw in blocks {
                let blk = match raw.as_object() {
                    Some(b) => b,
                    None => continue,
                };
                match str_of(blk.get("type")).as_str() {
                    "text" => {
                        let txt = str_of(blk.get("text"));
                        out.push(OutEvent::new("assistant_text", &txt));
                        if is_err {
                            out.push(OutEvent::new("error_text", &txt));
                        }
                    }
                    "tool_use" => {
                        if expose_tools {
                            out.push(OutEvent::new(
                                "tool",
                                &format!("[tool: {}]", str_of(blk.get("name"))),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        "result" => {
            let res_text = match m.get("result") {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            Some(vec![OutEvent {
                kind: "result".into(),
                text: res_text,
                usage: m.get("usage").and_then(to_map).map(|u| Value::Object(u.clone())),
                flags: None,
            }])
        }
        _ => None,
    }
}

/// `intOf`：float64 → `int(t)` 截断；int/int64 原样；其它 0。
pub fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f as i64).unwrap_or(0),
        _ => 0,
    }
}

/// OpenAI 口径的 usage 三元组。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageOut {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// `oaiUsage`：Claude usage → OpenAI usage（`total = in + out`）。
pub fn oai_usage(usage: Option<&Map<String, Value>>) -> UsageOut {
    let (it, ot) = match usage {
        Some(u) => (
            int_of(u.get("input_tokens")),
            int_of(u.get("output_tokens")),
        ),
        None => (0, 0),
    };
    UsageOut {
        prompt_tokens: it,
        completion_tokens: ot,
        total_tokens: it + ot,
    }
}

/// `oaiUsage` 的 JSON 形态（**struct 字段顺序**：prompt_tokens/completion_tokens/total_tokens）。
/// 注意不能走 `J::Obj`——那是 Go `map` 的排序语义，而 Go 的 `usageOut` 是 struct。
pub fn usage_to_j(u: UsageOut) -> crate::gjson::J {
    crate::gjson::st(vec![
        ("prompt_tokens", crate::gjson::J::Int(u.prompt_tokens)),
        ("completion_tokens", crate::gjson::J::Int(u.completion_tokens)),
        ("total_tokens", crate::gjson::J::Int(u.total_tokens)),
    ])
}

/// `oaiUsage` 的 JSON 形态（struct 字段顺序：prompt_tokens/completion_tokens/total_tokens）。
pub fn usage_to_go(u: UsageOut) -> String {
    format!(
        "{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}",
        u.prompt_tokens, u.completion_tokens, u.total_tokens
    )
}

/// `foldSync`：事件序列 → (content, usage, doneFlags, lastError)。
pub fn fold_sync(events: &[OutEvent]) -> (String, Option<Value>, Option<Value>, String) {
    let mut parts: Vec<String> = Vec::new();
    let mut assistant_buf: Vec<String> = Vec::new();
    let mut usage: Option<Value> = None;
    let mut flags: Option<Value> = None;
    let mut last_err = String::new();
    for ev in events {
        match ev.kind.as_str() {
            "text" | "error_text" | "tool" | "note" => {
                if !ev.text.is_empty() {
                    parts.push(ev.text.clone());
                }
            }
            "assistant_text" => {
                if !ev.text.is_empty() {
                    assistant_buf.push(ev.text.clone());
                }
            }
            "result" => usage = ev.usage.clone(),
            "done" => flags = ev.flags.clone(),
            "error" => {
                if last_err.is_empty() {
                    last_err = ev.text.clone();
                }
            }
            _ => {}
        }
    }
    let mut content = parts.join("");
    if content.is_empty() {
        content = assistant_buf.join("");
    }
    (content, usage, flags, last_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_text_shapes() {
        assert_eq!(extract_text(&json!(null)), "");
        assert_eq!(extract_text(&json!("hello")), "hello");
        assert_eq!(extract_text(&json!([])), "");
        assert_eq!(extract_text(&json!(["a", "", "b"])), "a\nb");
        assert_eq!(
            extract_text(&json!([{"type": "text", "text": "t1"}, {"type": "image_url", "image_url": {"url": "u"}}])),
            "t1\n[图片内容不支持, 已忽略]"
        );
        assert_eq!(
            extract_text(&json!([{"image_url": {"url": "u"}}])),
            "[图片内容不支持, 已忽略]"
        );
        assert_eq!(extract_text(&json!({"text": "mt"})), "mt");
        assert_eq!(extract_text(&json!(123)), "123");
        assert_eq!(extract_text(&json!(true)), "true");
    }

    #[test]
    fn build_prompt_single_user_passthrough() {
        let msgs = vec![json!({"role": "user", "content": "打个招呼"})];
        assert_eq!(build_prompt(&msgs, ""), "打个招呼");
        assert_eq!(
            build_prompt(&msgs, "自定义任务"),
            "打个招呼\n\n【任务】\n自定义任务"
        );
    }

    #[test]
    fn build_prompt_with_history_and_system() {
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "u"}),
        ];
        let got = build_prompt(&msgs, "");
        assert!(got.starts_with("【系统设定】\nsys\n\n【对话记录】\n【用户】\nu\n\n【任务】\n"), "{got}");

        let msgs = vec![
            json!({"role": "user", "content": "a"}),
            json!({"role": "assistant", "content": "b"}),
            json!({"role": "tool", "content": "t"}),
            json!({"role": "user", "content": "c"}),
        ];
        let got = build_prompt(&msgs, "");
        assert!(got.contains("【助手】\nb"), "{got}");
        assert!(got.contains("【工具输出】\nt"), "{got}");
    }

    #[test]
    fn out_event_json_matches_go_struct() {
        let e = OutEvent::new("text", "hi");
        assert_eq!(e.to_go(), "{\"Kind\":\"text\",\"Text\":\"hi\",\"Usage\":null,\"Flags\":null}");
        assert_eq!(events_to_go(Some(&[])), "[]");
        assert_eq!(events_to_go(None), "null", "Go 的 nil slice 是 null");
    }
}
