//! 请求体重排——**红线 8**（`reqshape.go` 全量对位）。
//!
//! 官方 `buildCreateParams` 字段序（`reqshape.go:13-18`）：
//!
//! ```text
//! model, messages, [temperature], [max_completion_tokens|max_tokens],
//! [reasoning_effort], [top_p], [metadata], [litellm_session_id],
//! [prompt_cache_key], [tools, parallel_tool_calls:true],
//! [tool_choice], stream:true, stream_options:{include_usage:true}
//! ```
//!
//! 重排保守原则：客户端没发的字段不凭空造值——只补三个官方默认字段
//! （`parallel_tool_calls` / `litellm_session_id` / `prompt_cache_key`），其余原值透传；
//! 罕见字段（官方清单外）放 `metadata` 之前、按字母序稳定输出。非流式时
//! `stream`/`stream_options` 都删掉（官方 delete 行为）。
//!
//! 值一律按 Go `json.Marshal` 口径输出（见 [`crate::gojson`]）——客户端字段是
//! **原样字节搬运**（compact+HTML 转义），构造出来的字段走 Go 的字符串/浮点/键序规则。

use std::collections::BTreeMap;

use serde_json::value::RawValue;
use serde_json::Value;

use crate::gojson;
use crate::session::LitellmSession;

/// 官方字段顺序——`reqshape.go:30-46` 逐字。
pub const OFFICIAL_FIELD_ORDER: [&str; 15] = [
    "model",
    "messages",
    "temperature",
    "max_completion_tokens", // 与 max_tokens 互斥同位
    "max_tokens",
    "reasoning_effort",
    "top_p",
    "metadata",
    "litellm_session_id",
    "prompt_cache_key",
    "tools",
    "parallel_tool_calls",
    "tool_choice",
    "stream",
    "stream_options",
];

/// 字段在官方清单里的权重；清单外（罕见字段）返回 `len`（最大）——`reqshape.go:49-56`。
fn field_rank(name: &str) -> usize {
    OFFICIAL_FIELD_ORDER
        .iter()
        .position(|f| *f == name)
        .unwrap_or(OFFICIAL_FIELD_ORDER.len())
}

/// 输出值：Go 侧每个值最终都经 `json.Marshal` 出去，这里按来源分派到同一套口径。
enum OutValue {
    /// 客户端原值（Go `json.RawMessage`）→ compact + HTML 转义。
    Raw(Vec<u8>),
    /// 构造的字符串（Go `string`）。
    Str(String),
    /// 构造的布尔（Go `bool`）。
    Bool(bool),
    /// 解成 `any` 再吐出的值（Go `map[string]any` 等）。
    Map(Value),
}

struct Field {
    key: String,
    value: OutValue,
}

/// 按序序列化（对应 `orderedJSON`，`reqshape.go:64-82`）。
fn ordered_json(fields: &[Field]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(b'{');
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        gojson::write_go_string(&mut out, &f.key);
        out.push(b':');
        match &f.value {
            OutValue::Raw(r) => out.extend_from_slice(&gojson::compact_go(r, true)),
            OutValue::Str(s) => gojson::write_go_string(&mut out, s),
            OutValue::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            OutValue::Map(v) => out.extend_from_slice(&gojson::marshal_go_value(v)),
        }
    }
    out.push(b'}');
    out
}

/// 构造 `metadata` 字段（对应 `buildMetadataPayload`，`reqshape.go:245-253`）：
/// 四字段、字段序按官方；`cwd` 由调用方注入（Go 取 `exeDirForProgram()`）。
pub fn build_metadata_payload(sess: &LitellmSession, cwd: &str) -> Vec<u8> {
    let fields = [
        Field {
            key: "prompt_id".into(),
            value: OutValue::Str(sess.prompt_id()),
        },
        Field {
            key: "session_id".into(),
            value: OutValue::Str(sess.id.clone()),
        },
        Field {
            key: "cwd".into(),
            value: OutValue::Str(cwd.to_string()),
        },
        Field {
            key: "litellm_conversation_id".into(),
            value: OutValue::Str(sess.conversation_id.clone()),
        },
    ];
    ordered_json(&fields)
}

/// 把客户端 chat 形态 `reasoning_effort` 翻成上游真正解析的
/// `reasoning:{effort,summary}`（对应 `reasoningPayloadFor`，`reqshape.go:214-223`）。
/// 非字符串或空值返回 `None`（不注入，不给上游塞垃圾）。
pub fn reasoning_payload_for(raw: &[u8]) -> Option<Vec<u8>> {
    let v: String = serde_json::from_slice(raw).ok()?;
    if v.is_empty() {
        return None;
    }
    Some(ordered_json(&[
        Field {
            key: "effort".into(),
            value: OutValue::Str(reasoning_effort_wire(&v)),
        },
        Field {
            key: "summary".into(),
            value: OutValue::Str("auto".into()),
        },
    ]))
}

/// 档位值映射（对应 `reasoningEffortWire`，`reqshape.go:230-240`，对齐官方 CLI 的 `WSi()`）：
/// `low/medium/high/xhigh` 原样、`minimal→low`、其余（含 `max`）→`xhigh`。
/// 有意偏离一处（Go 注释已写明）：官方梯子之外的值原样透传，不兜底成 `xhigh`。
pub fn reasoning_effort_wire(v: &str) -> String {
    match v.to_lowercase().as_str() {
        "low" => "low".into(),
        "medium" => "medium".into(),
        "high" => "high".into(),
        "xhigh" => "xhigh".into(),
        "minimal" => "low".into(),
        "max" => "xhigh".into(),
        _ => v.to_string(),
    }
}

/// 把 chat/completions 请求体重排为官方字段序并补默认（对应 `reshapeChatBody`，
/// `reqshape.go:88-198`）。`sess` 为 `None` 时现场造一次性会话（Go 侧同样兜底）。
/// 非法 JSON 原样返回（不拦转发）。
pub fn reshape_chat_body(body: &[u8], sess: Option<&LitellmSession>) -> Vec<u8> {
    reshape_chat_body_with_cwd(body, sess, &crate::appdir::exe_dir_string())
}

/// 同 [`reshape_chat_body`]，但 `metadata.cwd` 由调用方给。
///
/// Go 侧写死取 `exeDirForProgram()`；拆出这个形参是为了**已知答案门禁**——黄金基准里
/// 的 `cwd` 是采集时那台机器的 exe 目录（Go 测试二进制所在目录），不可能与 Rust 测试
/// 二进制目录相同，只有注入才能做逐字节比对。
pub fn reshape_chat_body_with_cwd(
    body: &[u8],
    sess: Option<&LitellmSession>,
    cwd: &str,
) -> Vec<u8> {
    let fallback;
    let sess = match sess {
        Some(s) => s,
        None => {
            fallback = LitellmSession {
                id: crate::session::new_uuid_v4(),
                conversation_id: crate::session::new_litellm_conversation_id(),
                prompt_seq: 1,
            };
            &fallback
        }
    };

    // Go `json.Unmarshal(body, &m)`：`null` 解进 map **不报错**、map 保持 nil
    // （`m == nil`、`len == 0`，其余读取零值），等价于「空对象」继续重排。
    let m: BTreeMap<String, Box<RawValue>> = if is_json_null_literal(body) {
        BTreeMap::new()
    } else {
        match serde_json::from_slice(body) {
            Ok(m) => m,
            Err(_) => return body.to_vec(), // 非法 JSON 原样返回
        }
    };

    // 非流式（无 stream 字段或 stream=false）：官方 delete 掉 stream/stream_options
    let streaming = m
        .get("stream")
        .map(|sv| serde_json::from_slice::<bool>(sv.get().as_bytes()).unwrap_or(false))
        .unwrap_or(false);

    // 罕见字段（官方清单外）：字母序（BTreeMap 迭代即字节序，与 Go sortStrings 同结果）
    let extras: Vec<&String> = m
        .keys()
        .filter(|k| field_rank(k) >= OFFICIAL_FIELD_ORDER.len())
        .collect();

    // 1. 官方顺序字段（客户端传了才带）
    let mut fields: Vec<Field> = Vec::with_capacity(m.len() + 4);
    let mut rare_done = false;
    for name in OFFICIAL_FIELD_ORDER {
        // 罕见字段在 metadata 之前统一插入；客户端没带 metadata 时在此补注入
        if name == "metadata" && !rare_done {
            for k in &extras {
                fields.push(Field {
                    key: (*k).clone(),
                    value: OutValue::Raw(m[*k].get().as_bytes().to_vec()),
                });
            }
            rare_done = true;
            if !m.contains_key("metadata") {
                fields.push(Field {
                    key: "metadata".into(),
                    value: OutValue::Raw(build_metadata_payload(sess, cwd)),
                });
            }
        }
        if name == "max_completion_tokens" || name == "max_tokens" {
            // 互斥同位：两个名字占同一个官方位置，带哪个放哪个
            if name == "max_tokens" {
                continue;
            }
            for k in ["max_completion_tokens", "max_tokens"] {
                if let Some(v) = m.get(k) {
                    fields.push(Field {
                        key: k.into(),
                        value: OutValue::Raw(v.get().as_bytes().to_vec()),
                    });
                }
            }
            continue;
        }
        if let Some(v) = m.get(name) {
            fields.push(Field {
                key: name.into(),
                value: OutValue::Raw(v.get().as_bytes().to_vec()),
            });
            // 客户端只发 chat 形态 reasoning_effort 时紧邻补一份 reasoning
            // （客户端自带 reasoning 时不覆盖）
            if name == "reasoning_effort" && !m.contains_key("reasoning") {
                if let Some(rp) = reasoning_payload_for(v.get().as_bytes()) {
                    fields.push(Field {
                        key: "reasoning".into(),
                        value: OutValue::Raw(rp),
                    });
                }
            }
        }
    }
    if !rare_done {
        for k in &extras {
            fields.push(Field {
                key: (*k).clone(),
                value: OutValue::Raw(m[*k].get().as_bytes().to_vec()),
            });
        }
    }

    // 2. 补官方默认字段（客户端没发才补；插在官方位置上）
    if !m.contains_key("parallel_tool_calls") && m.contains_key("tools") {
        insert_field(&mut fields, "parallel_tool_calls", OutValue::Bool(true), "tools");
    }
    if !m.contains_key("litellm_session_id") {
        insert_field(
            &mut fields,
            "litellm_session_id",
            OutValue::Str(sess.id.clone()),
            "metadata",
        );
    }
    if !m.contains_key("prompt_cache_key") {
        insert_field(
            &mut fields,
            "prompt_cache_key",
            OutValue::Str(sess.id.clone()),
            "litellm_session_id",
        );
    }

    // 3. stream 相关：流式带 stream:true + stream_options（include_usage 强制 true）；
    //    非流式两个都不带（官方 delete 行为）
    remove_fields(&mut fields, &["stream", "stream_options"]);
    if streaming {
        fields.push(Field {
            key: "stream".into(),
            value: OutValue::Bool(true),
        });
        let so = merge_stream_options(m.get("stream_options"));
        fields.push(Field {
            key: "stream_options".into(),
            value: OutValue::Map(so),
        });
    }
    ordered_json(&fields)
}

/// `stream_options` 的合并（对应 `reqshape.go:187-195`）：默认 `{"include_usage":true}`，
/// 客户端给了对象就保留其键并强制 `include_usage=true`。
///
/// **登记差异（Go 侧 bug）**：客户端发 `"stream_options": null` 且流式时，Go 的
/// `json.Unmarshal(null, &map)` 不报错但把 map 解成 nil，紧接着 `so["include_usage"]=true`
/// 会 **panic: assignment to entry in nil map**（本仓 `.work/p8a/scratch` 复现）。
/// Rust 侧按「非对象 → 退回默认 `{"include_usage":true}`」处理、不 panic。
/// 要修 Go 侧须改 `internal/tuanjie/reqshape.go`（P8 只读不改），故登记为已知差异。
fn merge_stream_options(raw: Option<&Box<RawValue>>) -> Value {
    let mut merged: BTreeMap<String, Value> = BTreeMap::new();
    merged.insert("include_usage".into(), Value::Bool(true));
    let Some(raw) = raw else {
        return Value::Object(merged.into_iter().collect());
    };
    let Ok(Value::Object(orig)) = serde_json::from_slice::<Value>(raw.get().as_bytes()) else {
        // 非对象（含 null / 字符串 / 数字 / 数组）：Go 侧 null 会 panic，其余退回默认；
        // 这里统一退回默认，不 panic。
        return Value::Object(merged.into_iter().collect());
    };
    let mut merged = orig;
    merged.insert("include_usage".into(), Value::Bool(true));
    Value::Object(merged)
}

/// 在 anchor 字段之后插入新字段（anchor 不存在则追加到末尾）——`reqshape.go:256-269`。
fn insert_field(fields: &mut Vec<Field>, key: &str, value: OutValue, anchor: &str) {
    if index_field(fields, key).is_some() {
        return;
    }
    let idx = index_field(fields, anchor);
    let f = Field {
        key: key.to_string(),
        value,
    };
    match idx {
        Some(i) => fields.insert(i + 1, f),
        None => fields.push(f),
    }
}

/// 字段下标——`reqshape.go:272-279`。
fn index_field(fields: &[Field], key: &str) -> Option<usize> {
    fields.iter().position(|f| f.key == key)
}

/// 删掉指定键——`reqshape.go:282-294`。
fn remove_fields(fields: &mut Vec<Field>, keys: &[&str]) {
    fields.retain(|f| !keys.contains(&f.key.as_str()));
}

/// `body` 是否就是 JSON 字面量 `null`（允许两侧空白）——Go 把它解进 map 时无错、留空表。
fn is_json_null_literal(body: &[u8]) -> bool {
    let s = std::str::from_utf8(body).unwrap_or("");
    matches!(s.trim_matches(|c: char| c.is_ascii_whitespace()), "null")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = r"C:\pd\exe";

    fn sess() -> LitellmSession {
        LitellmSession {
            id: "sess-123".into(),
            conversation_id: "litellm_conversation_1700000000000_abc123".into(),
            prompt_seq: 1,
        }
    }

    /// 顶层键序（serde_json 的 `Value` 是 BTreeMap、会丢序，故直接扫原文）。
    /// 只用于测试里的 ASCII 键——这里不做转义还原。
    fn top_order(raw: &[u8]) -> Vec<String> {
        let mut keys = Vec::new();
        let mut i = 0usize;
        let mut depth = 0i32;
        let mut expect_key = false;
        while i < raw.len() {
            match raw[i] {
                b'"' => {
                    let start = i + 1;
                    let mut j = start;
                    while j < raw.len() {
                        if raw[j] == b'\\' {
                            j += 2;
                            continue;
                        }
                        if raw[j] == b'"' {
                            break;
                        }
                        j += 1;
                    }
                    if depth == 1 && expect_key {
                        keys.push(String::from_utf8_lossy(&raw[start..j]).into_owned());
                        expect_key = false;
                    }
                    i = j + 1;
                }
                b'{' => {
                    depth += 1;
                    if depth == 1 {
                        expect_key = true;
                    }
                    i += 1;
                }
                b'[' => {
                    depth += 1;
                    i += 1;
                }
                b'}' | b']' => {
                    depth -= 1;
                    i += 1;
                }
                b',' => {
                    if depth == 1 {
                        expect_key = true;
                    }
                    i += 1;
                }
                b':' => {
                    if depth == 1 {
                        expect_key = false;
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        keys
    }

    fn reshape(body: &[u8]) -> Vec<u8> {
        reshape_chat_body_with_cwd(body, Some(&sess()), CWD)
    }

    #[test]
    fn stream_official_order() {
        let im = br#"{"stream_options":{"include_usage":true},"model":"codely-flash","max_tokens":20,"reasoning_effort":"low","messages":[{"role":"user","content":"OK"}],"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":"auto","stream":true}"#;
        let out = reshape(im);
        assert_eq!(
            top_order(&out).join(","),
            "model,messages,max_tokens,reasoning_effort,reasoning,metadata,litellm_session_id,prompt_cache_key,tools,parallel_tool_calls,tool_choice,stream,stream_options"
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["parallel_tool_calls"], Value::Bool(true));
        assert_eq!(v["litellm_session_id"], Value::String("sess-123".into()));
        assert_eq!(v["prompt_cache_key"], Value::String("sess-123".into()));
        assert_eq!(v["stream_options"]["include_usage"], Value::Bool(true));
        assert_eq!(
            v["metadata"],
            serde_json::json!({
                "prompt_id": "sess-123########1",
                "session_id": "sess-123",
                "cwd": "C:\\pd\\exe",
                "litellm_conversation_id": "litellm_conversation_1700000000000_abc123"
            })
        );
    }

    #[test]
    fn non_stream_drops_stream_fields() {
        let im = br#"{"model":"codely-flash","stream_options":{"include_usage":true},"messages":[{"role":"user","content":"OK"}],"max_tokens":20,"stream":false}"#;
        let out = reshape(im);
        assert_eq!(
            top_order(&out).join(","),
            "model,messages,max_tokens,metadata,litellm_session_id,prompt_cache_key"
        );
    }

    #[test]
    fn invalid_json_passthrough() {
        assert_eq!(
            reshape_chat_body_with_cwd(b"not json", Some(&sess()), CWD),
            b"not json"
        );
    }

    #[test]
    fn null_body_behaves_like_empty_object() {
        // Go：`json.Unmarshal("null", &map)` 无错、map 留 nil → 继续重排
        let out = reshape_chat_body_with_cwd(b"null", Some(&sess()), CWD);
        assert_eq!(
            top_order(&out).join(","),
            "metadata,litellm_session_id,prompt_cache_key"
        );
    }

    #[test]
    fn null_stream_options_does_not_panic() {
        // 登记差异：Go 在此输入上 panic（nil map 赋值），Rust 退回默认值
        let im = br#"{"model":"m","messages":[],"stream":true,"stream_options":null}"#;
        let out = reshape(im);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], Value::Bool(true));
    }

    #[test]
    fn rare_fields_sorted_before_metadata() {
        let im = br#"{"frequency_penalty":0.5,"model":"m","messages":[{"role":"user","content":"OK"}],"presence_penalty":0.1,"stream":true}"#;
        let out = reshape(im);
        assert_eq!(
            top_order(&out).join(","),
            "model,messages,frequency_penalty,presence_penalty,metadata,litellm_session_id,prompt_cache_key,stream,stream_options"
        );
    }

    #[test]
    fn effort_wire_mapping() {
        for (input, want) in [
            ("minimal", "low"),
            ("Minimal", "low"),
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("xhigh", "xhigh"),
            ("max", "xhigh"),
            ("MAX", "xhigh"),
            ("off", "off"),
            ("none", "none"),
            ("banana", "banana"),
        ] {
            assert_eq!(reasoning_effort_wire(input), want, "输入 {input}");
        }
    }
}
