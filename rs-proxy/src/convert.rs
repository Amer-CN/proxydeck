//! `internal/proxy/convert.go`：OpenAI 消息 → CommandCode 消息的转换管线。
//!
//! 逐条对齐 Go 的类型断言语义（`content` 是 `any`，可能为 nil / string / []any / 其它）：
//! Go 的 `switch v := content.(type)` 对未知类型落到 `default` 分支，本 crate 用
//! [`serde_json::Value`] 的形态判定等价实现。

use serde_json::{Map, Value};

use crate::api::{CCContentPart, CCMessage, CCToolOutput, OpenAIMessage};

/// `estTokens`：保守估算 messages 已占用的 token 数。
///
/// 按「字符数/2」估算（中文约 0.7 token/字、代码/英文约 0.25 token/字符，除以 2 在任何场景
/// 都不会低估），末尾 +64 余量。
///
/// ⚠ Go 用的是 `len(s)`——**字节数**，不是字符数；中文一个字符 3 字节。逐字节对齐必须
/// 用 UTF-8 字节长度。
pub fn est_tokens(msgs: &[OpenAIMessage]) -> i64 {
    let mut n: i64 = 0;
    for m in msgs {
        match m.content.as_ref() {
            Some(Value::String(s)) => n += s.len() as i64,
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Value::Object(pm) = part {
                        if let Some(Value::String(t)) = pm.get("text") {
                            n += t.len() as i64;
                        }
                    }
                }
            }
            _ => {}
        }
        // tool 调用参数也是上下文的一部分
        for tc in &m.tool_calls {
            n += tc.function_name.len() as i64 + tc.function_arguments.len() as i64;
        }
    }
    n / 2 + 64
}

/// `ConvertMessages`：OpenAI messages → CommandCode messages。
pub fn convert_messages(openai_msgs: &[OpenAIMessage]) -> Vec<CCMessage> {
    let mut cc_msgs: Vec<CCMessage> = Vec::new();
    let mut tool_names: Map<String, Value> = Map::new();

    for m in openai_msgs {
        for tc in &m.tool_calls {
            if !tc.id.is_empty() && !tc.function_name.is_empty() {
                tool_names.insert(tc.id.clone(), Value::String(tc.function_name.clone()));
            }
        }

        if m.role == "tool" {
            let mut tool_name = m.name.clone();
            if tool_name.is_empty() {
                tool_name = get_str(&tool_names, &m.tool_call_id);
            }
            if tool_name.is_empty() {
                tool_name = "unknown".to_string();
            }
            let content_str = content_to_string(m.content.as_ref());
            let output_type = if content_str.starts_with("Error:") {
                "error-text"
            } else {
                "text"
            };
            cc_msgs.push(CCMessage {
                role: "tool".to_string(),
                content: vec![CCContentPart {
                    r#type: "tool-result".to_string(),
                    tool_call_id: str_ptr(&m.tool_call_id),
                    tool_name: str_ptr(&tool_name),
                    output: Some(CCToolOutput {
                        r#type: output_type.to_string(),
                        value: content_str,
                    }),
                    ..Default::default()
                }],
            });
            continue;
        }

        if m.role == "assistant" && !m.tool_calls.is_empty() {
            let mut content_parts = parse_content(m.content.as_ref(), &mut tool_names);
            let mut added: Vec<String> = Vec::new();
            for part in &content_parts {
                if part.r#type == "tool-call" {
                    if let Some(id) = &part.tool_call_id {
                        added.push(id.clone());
                    }
                }
            }
            for tc in &m.tool_calls {
                if added.iter().any(|x| x == &tc.id) {
                    continue;
                }
                content_parts.push(CCContentPart {
                    r#type: "tool-call".to_string(),
                    tool_call_id: str_ptr(&tc.id),
                    tool_name: str_ptr(&tc.function_name),
                    input: Some(parse_tool_input(&tc.function_arguments)),
                    ..Default::default()
                });
                added.push(tc.id.clone());
            }
            cc_msgs.push(CCMessage {
                role: m.role.clone(),
                content: content_parts,
            });
            continue;
        }

        let content_parts = parse_content(m.content.as_ref(), &mut tool_names);
        if content_parts.is_empty() {
            // 跳过空消息：CommandCode 拒绝 null content（Codex 的 reasoning 摘要项就没有文本）
            continue;
        }
        // 上游只接受 user|assistant|tool；未知 role 兜底为 user，避免 400
        let role = if m.role == "user" || m.role == "assistant" || m.role == "tool" {
            m.role.clone()
        } else {
            "user".to_string()
        };
        cc_msgs.push(CCMessage {
            role,
            content: content_parts,
        });
    }
    cc_msgs
}

/// `ConvertTools`：OpenAI tools → CC tools。
pub fn convert_tools(openai_tools: &[Value]) -> Vec<Value> {
    if openai_tools.is_empty() {
        return Vec::new();
    }
    let mut tools: Vec<Value> = Vec::with_capacity(openai_tools.len());
    for tool in openai_tools {
        let tool_map = match tool.as_object() {
            Some(m) => m,
            None => continue, // 非对象 → Go 的 `tool.(map[string]any)` 断言失败 → 跳过
        };
        let tool_type = tool_map
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if tool_type != "function" {
            tools.push(tool.clone());
            continue;
        }
        let fn_map = match tool_map.get("function").and_then(|v| v.as_object()) {
            Some(f) => f,
            None => continue,
        };
        let name = fn_map
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let input_schema = match fn_map.get("parameters") {
            Some(Value::Object(o)) => Value::Object(o.clone()),
            _ => {
                // Go：`!ok || inputSchema == nil` → 空 object schema
                let mut props = Map::new();
                props.insert("properties".to_string(), Value::Object(Map::new()));
                props.insert("type".to_string(), Value::String("object".to_string()));
                Value::Object(props)
            }
        };
        let mut cc_tool = Map::new();
        cc_tool.insert("name".to_string(), Value::String(name.to_string()));
        cc_tool.insert("input_schema".to_string(), input_schema);
        if let Some(desc) = fn_map.get("description").and_then(|v| v.as_str()) {
            if !desc.is_empty() {
                cc_tool.insert("description".to_string(), Value::String(desc.to_string()));
            }
        }
        tools.push(Value::Object(cc_tool));
    }
    tools
}

fn get_str(m: &Map<String, Value>, k: &str) -> String {
    m.get(k)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn str_ptr(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// `parseToolInput`：把 tool 参数 JSON 解析成对象；非法 JSON →
/// `{"arguments": "<原串>"}`；空串 → `{}`。
pub fn parse_tool_input(arguments: &str) -> Value {
    if arguments.is_empty() {
        return Value::Object(Map::new());
    }
    match crate::godecode::parse_value(arguments.as_bytes()) {
        Ok(v) => v,
        Err(_) => {
            let mut m = Map::new();
            m.insert(
                "arguments".to_string(),
                Value::String(arguments.to_string()),
            );
            Value::Object(m)
        }
    }
}

/// `contentToString`。
pub fn content_to_string(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut b = String::new();
            for part in parts {
                if let Value::Object(pm) = part {
                    let text = content_part_to_string(Some(&Value::Object(pm.clone())));
                    if !text.is_empty() {
                        b.push_str(&text);
                    }
                }
            }
            b
        }
        Some(v) => crate::gjson::to_go(v),
    }
}

/// `contentPartToString`：从内容块里抽可读文本。
pub fn content_part_to_string(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut b = String::new();
            for item in items {
                if let Value::Object(m) = item {
                    b.push_str(&content_part_to_string(Some(&Value::Object(m.clone()))));
                }
            }
            b
        }
        Some(Value::Object(v)) => {
            for key in [
                "text",
                "content",
                "output_text",
                "input_text",
                "refusal",
                "thinking",
                "redacted_thinking",
            ] {
                if let Some(Value::String(text)) = v.get(key) {
                    return text.clone();
                }
            }
            if let Some(Value::Object(img)) = v.get("image_url") {
                if let Some(Value::String(url)) = img.get("url") {
                    return format!("[Image URL: {url}]");
                }
            }
            if let Some(Value::String(url)) = v.get("image_url") {
                return format!("[Image URL: {url}]");
            }
            crate::gjson::to_go(&Value::Object(v.clone()))
        }
        // Go 的 default 分支是 `fmt.Sprint(v)`（`%v` 语义，不是 json）
        Some(v) => go_sprint(v),
    }
}

/// Go `fmt.Sprint(v)` 在标量上的等价物（`%v` 语义）。
///
/// 与 `encoding/json` 的浮点规则**不同**：`fmt` 用 `strconv.FormatFloat(f, 'g', -1, 64)`，
/// 即 `1e+20`（json 是 `100000000000000000000`）。
pub fn go_sprint(v: &Value) -> String {
    match v {
        Value::Null => "<nil>".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) => go_format_g(f),
            None => String::new(),
        },
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(go_sprint).collect();
            format!("[{}]", inner.join(" "))
        }
        Value::Object(m) => {
            // Go 的 map 打印形如 `map[a:1 b:2]`（键排序）
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .iter()
                .map(|k| format!("{k}:{}", go_sprint(&m[*k])))
                .collect();
            format!("map[{}]", body.join(" "))
        }
    }
}

/// `strconv.FormatFloat(f, 'g', -1, 64)` 的等价物（Go `%v` / `fmt.Sprint` 的浮点形态）。
///
/// 规则直接来自 Go 1.26.5 的源码 `internal/strconv/ftoa.go:225-258` 的 `formatDigits`：
///
/// ```text
/// shortest 模式（prec < 0）:  eprec = 6                     // 源码 :240-242
/// exp := digs.dp - 1                                        // 源码 :243
/// if exp < -4 || exp >= eprec { 用 'e' } else { 用 'f' }     // 源码 :244
/// ```
///
/// 即**最短表示下阈值恒为 6**（不是尾数位数）——这解释了实测：`1e5`→`100000`、
/// `1e6`→`1e+06`、`1234567`→`1.234567e+06`、`123456.789`→`123456.789`。
/// `'e'` 形态的指数至少两位、正数带 `+`（`fmtE`）。
pub fn go_format_g(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    let (neg, digits, exp) = shortest_digits_g(f);
    let use_exp = exp < -4 || exp >= 6;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if use_exp {
        out.push_str(&crate::gjson::expand_sci_padded(&digits, exp));
    } else {
        out.push_str(&crate::gjson::expand_fixed(&digits, exp));
    }
    out
}

fn shortest_digits_g(v: f64) -> (bool, Vec<u8>, i32) {
    let s = format!("{v:e}");
    let (mant, exp) = match s.split_once('e') {
        Some((m, e)) => (m.to_string(), e.parse::<i32>().unwrap_or(0)),
        None => (s.clone(), 0),
    };
    let neg = mant.starts_with('-');
    let mant = mant.trim_start_matches('-');
    let digits: Vec<u8> = mant.bytes().filter(|b| b.is_ascii_digit()).collect();
    (neg, digits, exp)
}

/// `parseContent`：把 `content`（string / []any / 其它）转成 CC 内容块。
pub fn parse_content(content: Option<&Value>, tool_names: &mut Map<String, Value>) -> Vec<CCContentPart> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(v)) => {
            if v.is_empty() {
                return Vec::new();
            }
            vec![CCContentPart::text(v.clone())]
        }
        Some(Value::Array(parts)) => {
            let mut out: Vec<CCContentPart> = Vec::new();
            for part in parts {
                let part_map = match part.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let typ = part_map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                match typ {
                    "text" | "input_text" | "output_text" | "refusal" | "thinking"
                    | "redacted_thinking" | "reasoning" | "document" | "search_result"
                    | "image_url" | "input_image" | "image" => {
                        let text = content_part_to_string(Some(part));
                        if !text.is_empty() {
                            out.push(CCContentPart::text(text));
                        }
                    }
                    "tool_use" | "tool-call" => {
                        let str_of = |k: &str| -> String {
                            part_map
                                .get(k)
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string()
                        };
                        let mut id = str_of("id");
                        if id.is_empty() {
                            id = str_of("toolCallId");
                        }
                        if id.is_empty() {
                            id = str_of("tool_use_id");
                        }
                        let mut name = str_of("name");
                        if name.is_empty() {
                            name = str_of("toolName");
                        }
                        if !id.is_empty() && !name.is_empty() {
                            tool_names.insert(id.clone(), Value::String(name.clone()));
                        }
                        let input = match part_map.get("input") {
                            None | Some(Value::Null) => part_map.get("arguments").cloned(),
                            other => other.cloned(),
                        };
                        out.push(CCContentPart {
                            r#type: "tool-call".to_string(),
                            tool_call_id: str_ptr(&id),
                            tool_name: str_ptr(&name),
                            input,
                            ..Default::default()
                        });
                    }
                    "tool_result" | "tool-result" => {
                        let str_of = |k: &str| -> String {
                            part_map
                                .get(k)
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string()
                        };
                        let mut tool_id = str_of("tool_use_id");
                        if tool_id.is_empty() {
                            tool_id = str_of("toolCallId");
                        }
                        let mut tool_name = str_of("toolName");
                        if tool_name.is_empty() {
                            tool_name = get_str(tool_names, &tool_id);
                        }
                        if tool_name.is_empty() {
                            tool_name = "unknown".to_string();
                        }
                        let mut content_val =
                            content_part_to_string(part_map.get("content"));
                        if content_val.is_empty() {
                            content_val = content_part_to_string(part_map.get("output"));
                        }
                        let output_type = if content_val.starts_with("Error:") {
                            "error-text"
                        } else {
                            "text"
                        };
                        out.push(CCContentPart {
                            r#type: "tool-result".to_string(),
                            tool_call_id: str_ptr(&tool_id),
                            tool_name: str_ptr(&tool_name),
                            output: Some(CCToolOutput {
                                r#type: output_type.to_string(),
                                value: content_val,
                            }),
                            ..Default::default()
                        });
                    }
                    _ => {}
                }
            }
            out
        }
        Some(v) => vec![CCContentPart::text(content_to_string(Some(v)))],
    }
}

/// `ExtractSystem`：抽出 system/developer 消息并拼接，返回 (system, 其余消息)。
pub fn extract_system(msgs: &[OpenAIMessage]) -> (String, Vec<OpenAIMessage>) {
    let mut system = String::new();
    let mut rest: Vec<OpenAIMessage> = Vec::new();
    for m in msgs {
        // `developer` 是 OpenAI/DeepSeek 对 system 的新命名（DSH 等框架用它做系统提示），
        // 必须与 system 同等提取——否则会原样转发给上游，而 CommandChain 只认 user|assistant|tool。
        if m.role == "system" || m.role == "developer" {
            if !system.is_empty() {
                system.push('\n');
            }
            system.push_str(&content_to_string(m.content.as_ref()));
        } else {
            rest.push(m.clone());
        }
    }
    (system, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, content: Option<Value>) -> OpenAIMessage {
        OpenAIMessage {
            role: role.to_string(),
            content,
            ..Default::default()
        }
    }

    #[test]
    fn est_tokens_uses_byte_length_and_rounds() {
        // "hi" = 2 字节 → 2/2 + 64 = 65
        assert_eq!(est_tokens(&[msg("user", Some(json!("hi")))]), 65);
        // 中文 3 字节/字：「你好」= 6 字节 → 6/2 + 64 = 67
        assert_eq!(est_tokens(&[msg("user", Some(json!("你好")))]), 67);
        // 空 → 0/2 + 64
        assert_eq!(est_tokens(&[]), 64);
        // 数组内容取 text 字段
        assert_eq!(
            est_tokens(&[msg("user", Some(json!([{"type":"text","text":"hello"}])))]),
            5 / 2 + 64
        );
    }

    #[test]
    fn unknown_role_falls_back_to_user() {
        let cc = convert_messages(&[msg("weird", Some(json!("x")))]);
        assert_eq!(cc.len(), 1);
        assert_eq!(cc[0].role, "user");
    }

    #[test]
    fn empty_content_is_skipped() {
        let cc = convert_messages(&[
            msg("user", None),
            msg("user", Some(json!(""))),
            msg("user", Some(json!([]))),
            msg("assistant", Some(json!("reply"))),
        ]);
        assert_eq!(cc.len(), 1);
        assert_eq!(cc[0].role, "assistant");
    }

    #[test]
    fn developer_is_extracted_as_system() {
        let (sys, rest) = extract_system(&[
            msg("developer", Some(json!("You are an AI agent."))),
            msg("user", Some(json!("hi"))),
        ]);
        assert_eq!(sys, "You are an AI agent.");
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].role, "user");
    }

    #[test]
    fn multiple_systems_join_with_newline() {
        let (sys, _) = extract_system(&[
            msg("system", Some(json!("a"))),
            msg("developer", Some(json!("b"))),
        ]);
        assert_eq!(sys, "a\nb");
    }

    #[test]
    fn tool_output_prefix_selects_error_text() {
        let mut m = OpenAIMessage {
            role: "tool".to_string(),
            tool_call_id: "c1".to_string(),
            name: "bash".to_string(),
            content: Some(json!("Error: boom")),
            ..Default::default()
        };
        let cc = convert_messages(&[m.clone()]);
        assert_eq!(cc[0].content[0].output.as_ref().unwrap().r#type, "error-text");
        m.content = Some(json!("ok"));
        let cc = convert_messages(&[m]);
        assert_eq!(cc[0].content[0].output.as_ref().unwrap().r#type, "text");
    }

    #[test]
    fn tool_input_invalid_json_wraps_arguments() {
        // 注意：Go 的 `map[string]any` 把数字落成 float64，故比较也用浮点字面量
        assert_eq!(parse_tool_input(""), json!({}));
        assert_eq!(parse_tool_input("{\"a\":1}"), json!({"a": 1.0}));
        assert_eq!(parse_tool_input("nope"), json!({"arguments": "nope"}));
    }

    #[test]
    fn convert_tools_drops_unnamed_and_fills_missing_schema() {
        let tools = vec![
            json!({"type":"function","function":{"name":"f"}}),
            json!({"type":"function","function":{"description":"d"}}),
            json!({"type":"web_search"}),
            json!(1),
        ];
        let out = convert_tools(&tools);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            json!({"name":"f","input_schema":{"properties":{},"type":"object"}})
        );
        assert_eq!(out[1], json!({"type":"web_search"}));
    }

    // 期望值由真 Go 采收（`.work/cli/gofloat`：`fmt.Sprint(float64)` / `FormatFloat(f,'g',-1,64)`）
    #[test]
    fn go_format_g_matches_go() {
        let cases: &[(f64, &str)] = &[
            (42.0, "42"),
            (42.5, "42.5"),
            (1e5, "100000"),
            (1e6, "1e+06"),
            (1234567.0, "1.234567e+06"),
            (1e15, "1e+15"),
            (1e16, "1e+16"),
            (1e17, "1e+17"),
            (1e20, "1e+20"),
            (1e21, "1e+21"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (123456.789, "123456.789"),
            (0.3, "0.3"),
            (1e-7, "1e-07"),
        ];
        for (v, want) in cases {
            assert_eq!(&go_format_g(*v), want, "go_format_g({v:?})");
        }
    }

    #[test]
    fn go_sprint_matches_fmt_v_semantics() {
        assert_eq!(go_sprint(&json!(42)), "42");
        assert_eq!(go_sprint(&json!(42.5)), "42.5");
        assert_eq!(go_sprint(&json!(1e20)), "1e+20");
        assert_eq!(go_sprint(&json!("s")), "s");
        assert_eq!(go_sprint(&json!(true)), "true");
    }

    #[test]
    fn content_shapes() {
        assert_eq!(content_to_string(None), "");
        assert_eq!(content_to_string(Some(&json!("x"))), "x");
        assert_eq!(
            content_to_string(Some(&json!([{"type":"text","text":"a"},{"type":"text","text":"b"}]))),
            "ab"
        );
        assert_eq!(
            content_part_to_string(Some(&json!({"image_url":{"url":"http://a/b.png"}}))),
            "[Image URL: http://a/b.png]"
        );
    }
}

