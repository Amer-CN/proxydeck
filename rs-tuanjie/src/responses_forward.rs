//! responses_forward.rs —— Zen Responses 协议转发（`internal/tuanjie/responses_forward.go`）。
//!
//! 入站 chat 请求转成出站 `/v1/responses`，再把 Responses 结果翻回 chat 格式
//! （非流式回填 + 流式 SSE 翻译），客户端零改动。
//!
//! **逐字节口径**：出站请求体与翻译出来的下游 SSE 块都是 Go `json.Marshal` 的等价物
//! （键按字节序、数字按 Go 的 strconv 口径、字符串带 HTML 转义、`RawMessage` 先
//! compact 再原样嵌入）—— 用的是本 crate 的 [`crate::server::GJ`] 与 [`crate::gojson`]，
//! 与 `reqshape.rs` 同一条地基。
//!
//! **流式是真流式**：与 Go 一样逐行读上游 SSE、逐事件写出（不是整块读回再翻）。
//! 翻译器 [`ResponsesStreamTranslator`] 的状态机与 `streamResponsesToChat` 逐分支对应。
//!
//! 失败语义沿用 `forwardExternal`：`Ok(响应)` = 已向客户端写出（不可重试），
//! `Err(status)` = 尚未写任何字节（上游状态码 / -1 网络错），调用方可回落。

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::value::RawValue;

use futures_util::StreamExt;

use crate::gojson::compact_go;
use crate::providers::{
    external_base_url, external_headers, ExternalProvider, EXTERNAL_TIMEOUT, MAX_UPSTREAM_BODY_BYTES,
    MAX_UPSTREAM_ERR_BYTES,
};
use crate::server::{gobj, AppState, GJ};

// ---------------------------------------------------------------- 入站解析

/// 入站 chat 请求（Go `json.Unmarshal` 到匿名 struct 的等价物）。
///
/// `Option<T>` 同时承担 Go 的两种语义：字段缺失 = 零值；显式 `null` = 零值（no-op）。
/// 类型不符（`"model":123`）则整体解析失败 → `None`（Go 的 `err != nil`）。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawChat {
    pub model: Option<String>,
    pub messages: Option<Vec<Option<RawMsg>>>,
    pub tools: Option<Box<RawValue>>,
    pub tool_choice: Option<Box<RawValue>>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i64>,
    pub max_completion_tokens: Option<i64>,
}

/// 一条入站消息。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawMsg {
    pub role: Option<String>,
    pub content: Option<Box<RawValue>>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<Vec<Option<RawToolCall>>>,
}

/// 一条 tool_call。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawToolCall {
    pub id: Option<String>,
    pub r#type: Option<String>,
    pub function: Option<RawFunction>,
}

/// tool_call 的 function 段。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RawFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// 解析入站 chat 请求体 —— Go `json.Unmarshal(body, &chat)` 的等价物。
///
/// 顶层 `null` 是 **no-op（无错）**，与 Go 对 struct 的语义一致；顶层非对象
/// （数组/串/数字/布尔）与非法 JSON 都是 `None`。
pub fn parse_chat_request(body: &[u8]) -> Option<RawChat> {
    let trimmed = trim_ascii(body);
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == b"null" {
        return Some(RawChat::default());
    }
    // 顶层须是对象：Go 解 `[1,2]`/`"x"` 进 struct 都报错
    if trimmed[0] != b'{' {
        return None;
    }
    serde_json::from_slice::<RawChat>(trimmed).ok()
}

/// 把一个 JSON 对象解成结构体 —— Go `json.Unmarshal(body, &struct)` 的等价入口。
///
/// Go 的边界：顶层 `null` 是 no-op（全零值、**无错**）；顶层数组/串/数字/布尔一律报错；
/// 空输入报 `unexpected end of JSON input`。serde 的派生实现在这两点上与 Go 不同
/// （它接受**序列形式**的 struct、也接受 `null`），故显式挡一层。
pub(crate) fn parse_go_struct<T: serde::de::DeserializeOwned + Default>(body: &[u8]) -> Option<T> {
    let t = trim_ascii(body);
    if t.is_empty() {
        return None;
    }
    if t == b"null" {
        return Some(T::default());
    }
    if t[0] != b'{' {
        return None;
    }
    serde_json::from_slice::<T>(t).ok()
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end
        && (b[start] == b' ' || b[start] == b'\t' || b[start] == b'\n' || b[start] == b'\r')
    {
        start += 1;
    }
    while end > start
        && (b[end - 1] == b' ' || b[end - 1] == b'\t' || b[end - 1] == b'\n' || b[end - 1] == b'\r')
    {
        end -= 1;
    }
    &b[start..end]
}

/// chat 消息 content 是否非空 —— `hasContent`（`responses_forward.go:148-161`）。
pub fn has_content(raw: Option<&[u8]>) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    if raw.is_empty() || raw == b"null" {
        return false;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return true;
    };
    match v {
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// chat content → 字符串 —— `contentAsString`（`responses_forward.go:217-237`）。
pub fn content_as_string(raw: Option<&[u8]>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return String::from_utf8_lossy(raw).into_owned();
    };
    match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
                .collect();
            parts.join(" ")
        }
        other => value_to_raw_json(&other),
    }
}

/// 一个 JSON 值按 Go `json.Marshal(RawMessage)` 的口径嵌入（compact + HTML 转义）。
fn value_to_raw_json(v: &serde_json::Value) -> String {
    let raw = crate::gojson::marshal_go_value(v);
    String::from_utf8_lossy(&compact_go(&raw, true)).into_owned()
}

/// `GJ` → 字节（Go `json.Marshal` 等价物）。
pub fn gj_to_bytes(v: &GJ) -> Vec<u8> {
    let mut out = Vec::new();
    v.write(&mut out);
    out
}

/// 按角色产出文本 part（assistant 历史消息用 output_text）—— `textPart`。
pub fn text_part(role: &str, text: &str) -> GJ {
    let t = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    gobj(vec![
        ("type", GJ::Str(t.to_string())),
        ("text", GJ::Str(text.to_string())),
    ])
}

/// 单个 chat 内容块 → Responses 内容块 —— `contentBlockToResponses`（`:193-213`）。
fn content_block_to_responses(role: &str, b: &serde_json::Value) -> GJ {
    match b.get("type").and_then(|t| t.as_str()) {
        Some("text") => text_part(role, b.get("text").and_then(|t| t.as_str()).unwrap_or("")),
        Some("image_url") => {
            let mut pairs: Vec<(&str, GJ)> = vec![("type", GJ::Str("input_image".to_string()))];
            match b.get("image_url") {
                Some(serde_json::Value::Object(_)) => {
                    let iu = b.get("image_url").expect("已判是对象");
                    pairs.push(("image_url", value_to_raw_gj(iu.get("url"))));
                    if let Some(d) = iu.get("detail").and_then(|d| d.as_str()) {
                        if !d.is_empty() {
                            pairs.push(("detail", GJ::Str(d.to_string())));
                        }
                    }
                }
                Some(other) => pairs.push(("image_url", value_to_raw_gj(Some(other)))),
                None => pairs.push(("image_url", GJ::Null)),
            }
            gobj(pairs)
        }
        // input_text / input_image 等已是 Responses 形态，或其他类型原样透传
        _ => value_to_raw_gj(Some(b)),
    }
}

fn value_to_raw_gj(v: Option<&serde_json::Value>) -> GJ {
    match v {
        None => GJ::Null,
        Some(v) => GJ::Raw(value_to_raw_json(v)),
    }
}

/// chat 消息 → Responses message item —— `messageItem`（`:167-181`）。
pub fn message_item(role: &str, content: &serde_json::Value) -> GJ {
    let mut parts: Vec<GJ> = Vec::new();
    match content {
        serde_json::Value::String(s) => parts.push(text_part(role, s)),
        serde_json::Value::Array(blocks) => {
            for b in blocks {
                parts.push(content_block_to_responses(role, b));
            }
        }
        _ => {}
    }
    gobj(vec![
        ("type", GJ::Str("message".to_string())),
        ("role", GJ::Str(role.to_string())),
        ("content", GJ::Arr(parts)),
    ])
}

/// chat 嵌套 function tools → Responses 扁平形态 —— `chatToolsToResponses`（`:241-278`）。
pub fn chat_tools_to_responses(raw: Option<&serde_json::Value>) -> Option<GJ> {
    let raw = raw?;
    let tools = raw.as_array()?;
    let mut out: Vec<GJ> = Vec::new();
    for t in tools {
        if t.get("type").and_then(|x| x.as_str()) != Some("function") {
            continue;
        }
        let Some(f) = t.get("function").filter(|f| f.is_object()) else {
            continue;
        };
        let mut pairs: Vec<(&str, GJ)> = vec![
            ("type", GJ::Str("function".to_string())),
            (
                "name",
                GJ::Str(f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string()),
            ),
        ];
        if let Some(d) = f.get("description").and_then(|x| x.as_str()) {
            if !d.is_empty() {
                pairs.push(("description", GJ::Str(d.to_string())));
            }
        }
        if let Some(p) = f.get("parameters") {
            pairs.push(("parameters", GJ::Raw(value_to_raw_json(p))));
        }
        if let Some(s) = f.get("strict") {
            if !s.is_null() {
                pairs.push(("strict", value_to_raw_gj(Some(s))));
            }
        }
        out.push(gobj(pairs));
    }
    if out.is_empty() {
        None
    } else {
        Some(GJ::Arr(out))
    }
}

/// chat tool_choice → Responses 形态 —— `chatToolChoiceToResponses`（`:283-305`）。
pub fn chat_tool_choice_to_responses(raw: Option<&serde_json::Value>) -> Option<GJ> {
    let raw = raw?;
    if let serde_json::Value::String(s) = raw {
        return Some(GJ::Str(s.clone()));
    }
    let obj = raw.as_object()?;
    let fname = obj
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    if !fname.is_empty() {
        return Some(gobj(vec![
            ("type", GJ::Str("function".to_string())),
            ("name", GJ::Str(fname.to_string())),
        ]));
    }
    let name = obj.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let ty = obj.get("type").and_then(|n| n.as_str()).unwrap_or("");
    if !name.is_empty() && !ty.is_empty() {
        return Some(gobj(vec![
            ("type", GJ::Str(ty.to_string())),
            ("name", GJ::Str(name.to_string())),
        ]));
    }
    None
}

/// 入站 chat 请求体 → Responses 请求体 —— `chatToResponsesRequest`（`:64-145`）。
/// `None` = 转换失败（body 非法 JSON），与 Go 的 `ok=false` 同义。
pub fn chat_to_responses_request(body: &[u8], stream: bool) -> Option<Vec<u8>> {
    let chat = parse_chat_request(body)?;
    let mut input: Vec<GJ> = Vec::new();
    for msg in chat.messages.as_deref().unwrap_or(&[]) {
        let Some(msg) = msg else {
            // Go：slice 元素是 null → struct 是零值（role 空），走下面的空消息分支
            continue;
        };
        let role = msg.role.clone().unwrap_or_default();
        let content_raw = msg.content.as_ref().map(|c| c.get().as_bytes());
        let content_value = content_raw.and_then(|c| serde_json::from_slice::<serde_json::Value>(c).ok());
        if role == "tool" {
            input.push(gobj(vec![
                ("type", GJ::Str("function_call_output".to_string())),
                (
                    "call_id",
                    GJ::Str(msg.tool_call_id.clone().unwrap_or_default()),
                ),
                ("output", GJ::Str(content_as_string(content_raw))),
            ]));
            continue;
        }
        let tool_calls = msg.tool_calls.as_deref().unwrap_or(&[]);
        if role == "assistant" && !tool_calls.is_empty() {
            if has_content(content_raw) {
                let v = content_value.clone().unwrap_or(serde_json::Value::Null);
                input.push(message_item(&role, &v));
            }
            for tc in tool_calls.iter().flatten() {
                let (name, args) = tc
                    .function
                    .as_ref()
                    .map(|f| {
                        (
                            f.name.clone().unwrap_or_default(),
                            f.arguments.clone().unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default();
                input.push(gobj(vec![
                    ("type", GJ::Str("function_call".to_string())),
                    ("call_id", GJ::Str(tc.id.clone().unwrap_or_default())),
                    ("name", GJ::Str(name)),
                    ("arguments", GJ::Str(args)),
                ]));
            }
            continue;
        }
        if !has_content(content_raw) {
            continue; // 空消息（如 assistant 占位）不产出 item
        }
        let v = content_value.unwrap_or(serde_json::Value::Null);
        input.push(message_item(&role, &v));
    }

    let mut pairs: Vec<(&str, GJ)> = vec![
        (
            "model",
            GJ::Str(chat.model.clone().unwrap_or_default()),
        ),
        ("input", GJ::Arr(input)),
        ("stream", GJ::Bool(stream)),
    ];
    if let Some(t) = chat.temperature {
        pairs.push(("temperature", GJ::Float(t)));
    }
    if let Some(p) = chat.top_p {
        pairs.push(("top_p", GJ::Float(p)));
    }
    let max_tok = chat.max_completion_tokens.or(chat.max_tokens);
    if let Some(m) = max_tok {
        pairs.push(("max_output_tokens", GJ::Int(m)));
    }
    let tools_value = chat
        .tools
        .as_ref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t.get()).ok());
    if let Some(tools) = chat_tools_to_responses(tools_value.as_ref()) {
        pairs.push(("tools", tools));
    }
    let tc_value = chat
        .tool_choice
        .as_ref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t.get()).ok());
    if let Some(tc) = chat_tool_choice_to_responses(tc_value.as_ref()) {
        pairs.push(("tool_choice", tc));
    }
    Some(gj_to_bytes(&gobj(pairs)))
}

// ---------------------------------------------------------------- 响应转换

/// Responses 的 usage —— `respUsage`（`:308-312`）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
pub struct RespUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
}

fn usage_gj(u: RespUsage, with_total: bool) -> GJ {
    let total = if with_total {
        u.total_tokens
    } else {
        u.input_tokens + u.output_tokens
    };
    gobj(vec![
        ("prompt_tokens", GJ::Int(u.input_tokens)),
        ("completion_tokens", GJ::Int(u.output_tokens)),
        ("total_tokens", GJ::Int(total)),
    ])
}

/// 当前 Unix 秒（Go `time.Now().Unix()`）。
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Responses 非流式响应 → chat completion —— `responsesToChatCompletion`（`:318-403`）。
/// 返回 `(chat body, usage)`；非法 JSON → `None`。
pub fn responses_to_chat_completion(
    resp_body: &[u8],
    model: &str,
) -> Option<(Vec<u8>, Option<RespUsage>)> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Resp {
        id: String,
        status: String,
        created_at: i64,
        model: String,
        output: Vec<Item>,
        usage: Option<RespUsage>,
        incomplete_details: Option<Incomplete>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Item {
        r#type: String,
        content: Vec<Part>,
        call_id: String,
        id: String,
        name: String,
        arguments: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Part {
        r#type: String,
        text: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Incomplete {
        reason: String,
    }
    let rr: Resp = parse_go_struct(resp_body)?;

    let mut content = String::new();
    let mut tool_calls: Vec<GJ> = Vec::new();
    for item in &rr.output {
        match item.r#type.as_str() {
            "message" => {
                let mut texts: Vec<String> = Vec::new();
                for p in &item.content {
                    if p.r#type == "output_text" && !p.text.is_empty() {
                        texts.push(p.text.clone());
                    }
                }
                content = texts.join("");
            }
            "function_call" => {
                let cid = if item.call_id.is_empty() {
                    item.id.clone()
                } else {
                    item.call_id.clone()
                };
                tool_calls.push(gobj(vec![
                    ("id", GJ::Str(cid)),
                    ("type", GJ::Str("function".to_string())),
                    (
                        "function",
                        gobj(vec![
                            ("name", GJ::Str(item.name.clone())),
                            ("arguments", GJ::Str(item.arguments.clone())),
                        ]),
                    ),
                ]));
            }
            _ => {}
        }
    }
    let created = if rr.created_at == 0 {
        now_unix()
    } else {
        rr.created_at
    };
    let mdl = if rr.model.is_empty() {
        model.to_string()
    } else {
        rr.model.clone()
    };
    let mut finish = "stop".to_string();
    if !tool_calls.is_empty() {
        finish = "tool_calls".to_string();
    } else if rr.status == "incomplete"
        && rr
            .incomplete_details
            .as_ref()
            .map(|d| d.reason == "max_output_tokens")
            .unwrap_or(false)
    {
        finish = "length".to_string();
    }
    let mut message: Vec<(&str, GJ)> = vec![
        ("role", GJ::Str("assistant".to_string())),
        ("content", GJ::Str(content)),
    ];
    if !tool_calls.is_empty() {
        message.push(("tool_calls", GJ::Arr(tool_calls)));
    }
    let mut pairs: Vec<(&str, GJ)> = vec![
        ("id", GJ::Str(rr.id.clone())),
        ("object", GJ::Str("chat.completion".to_string())),
        ("created", GJ::Int(created)),
        ("model", GJ::Str(mdl)),
        (
            "choices",
            GJ::Arr(vec![gobj(vec![
                ("index", GJ::Int(0)),
                ("message", gobj(message)),
                ("finish_reason", GJ::Str(finish)),
                ("logprobs", GJ::Null),
            ])]),
        ),
    ];
    if let Some(u) = rr.usage {
        pairs.push(("usage", usage_gj(u, true)));
    }
    Some((gj_to_bytes(&gobj(pairs)), rr.usage))
}

// ---------------------------------------------------------------- 流式翻译

/// 一次 `feed` 的产物：下游字节 + 「计入 inflight 的字节数」。
///
/// Go 的 `touch(n)` 只对 `data: <chunk>\n\n` 计（`fmt.Fprintf` 的返回值），
/// 补发的 `data: [DONE]\n\n` **不计**——两者分开带出来，别混。
#[derive(Debug, Default, Clone)]
pub struct StreamOut {
    pub bytes: Vec<u8>,
    pub touched: i64,
}

/// Responses SSE → chat SSE 的**增量**翻译器 —— `streamResponsesToChat`（`:417-546`）。
///
/// 状态机逐分支对应：`output_item.added(message)` → 首块 delta{role}；
/// `output_text.delta` → delta{content}；`output_item.done(function_call)` →
/// delta{tool_calls}；`completed`/`incomplete` → 终块 finish + usage + `[DONE]`；
/// `failed`/`error` → 错误帧 + `[DONE]`。上游中断时 `finish()` 也补 `[DONE]`。
pub struct ResponsesStreamTranslator {
    chunk_id: String,
    created: i64,
    model: String,
    buf: Vec<u8>,
    role_sent: bool,
    tool_count: usize,
    done: bool,
    /// 收尾用：completed/incomplete 里的 usage（Go 把它交给 onUsage 回调）。
    pub usage: Option<RespUsage>,
}

impl ResponsesStreamTranslator {
    pub fn new(model: &str, chunk_id: &str, created: i64) -> Self {
        ResponsesStreamTranslator {
            chunk_id: chunk_id.to_string(),
            created,
            model: model.to_string(),
            buf: Vec::new(),
            role_sent: false,
            tool_count: 0,
            done: false,
            usage: None,
        }
    }

    /// `onUsage` 是否已被调用过（等价物：usage 已记）。
    /// 一次性消费：上报后取空，后续 chunk 不再触发（Go 只在终态回调一次；
    /// 此前非消费式读取致每块都 `add` 一次——P8b 第五片审查 #1）。
    pub fn on_usage(&mut self) -> Option<RespUsage> {
        self.usage.take()
    }

    /// 喂一块上游字节，返回本块翻译出的下游字节。
    pub fn feed(&mut self, chunk: &[u8]) -> StreamOut {
        let mut out = StreamOut::default();
        self.buf.extend_from_slice(chunk);
        loop {
            let Some(i) = self.buf.iter().position(|b| *b == b'\n') else {
                break;
            };
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            self.handle_line(&line, &mut out);
        }
        out
    }

    /// 上游流结束：处理尾行（Go 的 `ReadString` 在 EOF 时也返回未带换行的那一段），
    /// 没走到终态就补 `[DONE]`。
    pub fn finish(&mut self) -> StreamOut {
        let mut out = StreamOut::default();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.handle_line(&line, &mut out);
        }
        if !self.done {
            out.bytes.extend_from_slice(b"data: [DONE]\n\n");
        }
        out
    }

    fn handle_line(&mut self, line: &[u8], out: &mut StreamOut) {
        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim();
        let Some(payload) = trimmed.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "response.output_item.added" => {
                if ev.get("item").and_then(|i| i.get("type")).and_then(|t| t.as_str())
                    == Some("message")
                {
                    self.ensure_role(out);
                }
            }
            "response.output_text.delta" => {
                self.ensure_role(out);
                let delta = ev.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                self.send_chunk(out, vec![("content", GJ::Str(delta.to_string()))], "", None);
            }
            "response.output_item.done" => {
                let item = ev.get("item");
                if item.and_then(|i| i.get("type")).and_then(|t| t.as_str()) == Some("function_call") {
                    let get = |k: &str| {
                        item.and_then(|i| i.get(k))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    let idx = self.tool_count as i64;
                    self.send_chunk(
                        out,
                        vec![(
                            "tool_calls",
                            GJ::Arr(vec![gobj(vec![
                                ("index", GJ::Int(idx)),
                                ("id", GJ::Str(get("call_id"))),
                                ("type", GJ::Str("function".to_string())),
                                (
                                    "function",
                                    gobj(vec![
                                        ("name", GJ::Str(get("name"))),
                                        ("arguments", GJ::Str(get("arguments"))),
                                    ]),
                                ),
                            ])]),
                        )],
                        "",
                        None,
                    );
                    self.tool_count += 1;
                }
            }
            "response.completed" | "response.incomplete" => {
                let resp = ev.get("response");
                let usage = resp.and_then(|r| r.get("usage")).and_then(|u| {
                    serde_json::from_value::<RespUsage>(u.clone()).ok()
                });
                self.usage = usage;
                let mut finish = "stop";
                if self.tool_count > 0 {
                    finish = "tool_calls";
                } else if ty == "response.incomplete"
                    && resp
                        .and_then(|r| r.get("incomplete_details"))
                        .and_then(|d| d.get("reason"))
                        .and_then(|r| r.as_str())
                        == Some("max_output_tokens")
                {
                    finish = "length";
                }
                self.send_chunk(out, vec![], finish, usage);
                out.bytes.extend_from_slice(b"data: [DONE]\n\n");
                self.done = true;
            }
            "response.failed" | "response.error" => {
                let mut msg = "upstream stream failed".to_string();
                if let Some(m) = ev
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                {
                    if !m.is_empty() {
                        msg = m.to_string();
                    }
                }
                let err = gobj(vec![(
                    "error",
                    gobj(vec![
                        ("message", GJ::Str(msg)),
                        ("type", GJ::Str("server_error".to_string())),
                    ]),
                )]);
                out.bytes.extend_from_slice(b"data: ");
                out.bytes.extend_from_slice(&gj_to_bytes(&err));
                out.bytes.extend_from_slice(b"\n\n");
                out.bytes.extend_from_slice(b"data: [DONE]\n\n");
                self.done = true;
            }
            _ => {}
        }
    }

    fn ensure_role(&mut self, out: &mut StreamOut) {
        if self.role_sent {
            return;
        }
        self.role_sent = true;
        self.send_chunk(
            out,
            vec![
                ("role", GJ::Str("assistant".to_string())),
                ("content", GJ::Str(String::new())),
            ],
            "",
            None,
        );
    }

    fn send_chunk(
        &self,
        out: &mut StreamOut,
        delta: Vec<(&str, GJ)>,
        finish: &str,
        usage: Option<RespUsage>,
    ) {
        let choice: Vec<(&str, GJ)> = vec![
            ("index", GJ::Int(0)),
            ("delta", gobj(delta)),
            ("finish_reason", GJ::Str(finish.to_string())),
            ("logprobs", GJ::Null),
        ];
        let mut pairs: Vec<(&str, GJ)> = vec![
            ("id", GJ::Str(self.chunk_id.clone())),
            ("object", GJ::Str("chat.completion.chunk".to_string())),
            ("created", GJ::Int(self.created)),
            ("model", GJ::Str(self.model.clone())),
            ("choices", GJ::Arr(vec![gobj(choice)])),
        ];
        if let Some(u) = usage {
            pairs.push(("usage", usage_gj(u, true)));
        }
        let bytes = gj_to_bytes(&gobj(pairs));
        out.bytes.extend_from_slice(b"data: ");
        out.bytes.extend_from_slice(&bytes);
        out.bytes.extend_from_slice(b"\n\n");
        out.touched += (bytes.len() + b"data: \n\n".len()) as i64;
    }
}

// ---------------------------------------------------------------- 转发主体

/// `forwardExternalResponses`（`responses_forward.go:550-629`）。
pub async fn forward_external_responses(
    st: &Arc<AppState>,
    body: Vec<u8>,
    model: &str,
    wants_stream: bool,
    prov: &ExternalProvider,
    started: Instant,
) -> Result<Response, i32> {
    let pname = prov.name.clone();
    let rid = st.registry.register(model, &pname, wants_stream);

    let Some(req_body) = chat_to_responses_request(&body, wants_stream) else {
        st.registry.finish(&rid);
        return Err(-1);
    };
    let url = format!("{}/responses", external_base_url(&prov.base_url));
    let accept = if wants_stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let headers = external_headers(
        prov,
        ("Authorization", format!("Bearer {}", prov.api_key)),
        accept,
    );
    let cl = reqwest::Client::builder()
        .timeout(EXTERNAL_TIMEOUT)
        .build()
        .map_err(|_| -1)?;
    let resp = match cl.post(&url).headers(headers).body(req_body).send().await {
        Ok(r) => r,
        Err(e) => {
            let latency = started.elapsed().as_millis() as i64;
            st.activity.add(
                "error",
                &format!("{model} · {pname} 异常: {e}"),
                model,
                &pname,
                latency,
                0,
                0,
            );
            crate::logf(&format!(
                "[tuanjie] external responses model={model} provider={pname} err={e}"
            ));
            st.registry.finish(&rid);
            return Err(-1);
        }
    };
    let status = resp.status();
    let latency = started.elapsed().as_millis() as i64;
    if status != reqwest::StatusCode::OK {
        let err_headers = resp.headers().clone();
        let err_body = crate::chat::read_body_bounded(resp, MAX_UPSTREAM_ERR_BYTES).await;
        st.activity.add(
            "error",
            &format!("{model} · 外部源 {pname} 上游返回 {}", status.as_u16()),
            model,
            &pname,
            latency,
            0,
            status.as_u16() as i32,
        );
        crate::logf(&format!(
            "[tuanjie] external responses model={model} provider={pname} status={}",
            status.as_u16()
        ));
        st.registry.finish(&rid);
        if crate::media::should_fallback(status.as_u16() as i32) {
            return Err(status.as_u16() as i32);
        }
        let mut out_headers = HeaderMap::new();
        if let Some(ct) = err_headers.get(reqwest::header::CONTENT_TYPE) {
            if let Ok(v) = HeaderValue::from_bytes(ct.as_bytes()) {
                out_headers.insert(axum::http::header::CONTENT_TYPE, v);
            }
        }
        return Ok((status, out_headers, Body::from(err_body)).into_response());
    }
    crate::logf(&format!(
        "[tuanjie] external responses model={model} provider={pname} status=200"
    ));

    if wants_stream {
        let chunk_id = format!("chatcmpl-{}", crate::inflight::next_rid());
        let created = now_unix();
        let translator = ResponsesStreamTranslator::new(model, &chunk_id, created);
        let stc = st.clone();
        let pname2 = pname.clone();
        let model2 = model.to_string();
        let model3 = model.to_string();
        let rid2 = rid.clone();
        let guard = crate::chat::StreamGuard::with_callback(st.registry.clone(), rid.clone(), move || {
            stc.activity.add(
                "ok",
                &format!("外部源 {pname2} · {model2}（Responses）（流式）"),
                &model2,
                &pname2,
                started.elapsed().as_millis() as i64,
                0,
                200,
            );
        });
        let stc2 = st.clone();
        let stream = futures_util::stream::unfold(
            Some((resp.bytes_stream(), translator, guard)),
            move |state| {
                let stc2 = stc2.clone();
                let rid3 = rid2.clone();
                let model3 = model3.clone();
                async move {
                    let (mut inner, mut tr, mut g) = state?;
                    match inner.next().await {
                        Some(Ok(chunk)) => {
                            let out = tr.feed(&chunk);
                            if let Some(u) = tr.on_usage() {
                                stc2.stats.add(
                                    &model3,
                                    u.input_tokens,
                                    u.output_tokens,
                                    u.total_tokens,
                                );
                            }
                            stc2.registry.touch(&rid3, out.touched);
                            let bytes: Result<Vec<u8>, std::io::Error> = Ok(out.bytes);
                            Some((bytes, Some((inner, tr, g))))
                        }
                        _ => {
                            // 上游读到尽头（或读错）：处理尾行 + 补 [DONE]，再收尾一次
                            let out = tr.finish();
                            stc2.registry.touch(&rid3, out.touched);
                            let bytes: Result<Vec<u8>, std::io::Error> = Ok(out.bytes);
                            g.done();
                            Some((bytes, None))
                        }
                    }
                }
            },
        );
        let mut out_headers = HeaderMap::new();
        out_headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        out_headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
        out_headers.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
        return Ok((StatusCode::OK, out_headers, Body::from_stream(stream)).into_response());
    }

    let resp_body = crate::chat::read_body_bounded(resp, MAX_UPSTREAM_BODY_BYTES).await;
    st.registry.touch(&rid, resp_body.len() as i64);
    st.registry.finish(&rid);
    st.activity.add(
        "ok",
        &format!("外部源 {pname} · {model}（Responses）"),
        model,
        &pname,
        started.elapsed().as_millis() as i64,
        0,
        200,
    );
    let mut out_headers = HeaderMap::new();
    out_headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    out_headers.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
    match responses_to_chat_completion(&resp_body, model) {
        Some((cb, usage)) => {
            if let Some(u) = usage {
                st.stats
                    .add(model, u.input_tokens, u.output_tokens, u.total_tokens);
            }
            Ok((StatusCode::OK, out_headers, Body::from(cb)).into_response())
        }
        None => {
            crate::logf(&format!(
                "[tuanjie] external responses model={model} provider={pname} 非预期响应形态，原样透传"
            ));
            Ok((StatusCode::OK, out_headers, Body::from(resp_body)).into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 金标 `responses-nonstream` 的入站体（与 Go 采集器逐字节同源）。
    const ZEN_REQ: &str = r#"{"model":"muse-spark-1.3-contributor-free","messages":[{"role":"system","content":"sys"},{"role":"user","content":[{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA","detail":"high"}},{"type":"image_url","image_url":"https://x/y.png"}]},{"role":"assistant","content":"prev","tool_calls":[{"id":"call_9","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]},{"role":"tool","tool_call_id":"call_9","content":[{"type":"text","text":"result"}]}],"tools":[{"type":"function","function":{"name":"f","description":"d","parameters":{"type":"object","properties":{"a":{"type":"number"}}},"strict":true}},{"type":"other"}],"tool_choice":{"type":"function","function":{"name":"f"}},"temperature":0.5,"top_p":0.25,"max_completion_tokens":123,"stream":false}"#;

    #[test]
    fn chat_to_responses_request_matches_go_bytes() {
        let out = chat_to_responses_request(ZEN_REQ.as_bytes(), false).expect("转换成功");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"input":[{"content":[{"text":"sys","type":"input_text"}],"role":"system","type":"message"},{"content":[{"text":"hello","type":"input_text"},{"detail":"high","image_url":"data:image/png;base64,AAAA","type":"input_image"},{"image_url":"https://x/y.png","type":"input_image"}],"role":"user","type":"message"},{"content":[{"text":"prev","type":"output_text"}],"role":"assistant","type":"message"},{"arguments":"{\"a\":1}","call_id":"call_9","name":"f","type":"function_call"},{"call_id":"call_9","output":"result","type":"function_call_output"}],"max_output_tokens":123,"model":"muse-spark-1.3-contributor-free","stream":false,"temperature":0.5,"tool_choice":{"name":"f","type":"function"},"tools":[{"description":"d","name":"f","parameters":{"properties":{"a":{"type":"number"}},"type":"object"},"strict":true,"type":"function"}],"top_p":0.25}"#
        );
    }

    #[test]
    fn chat_to_responses_request_rejects_non_object_and_accepts_null() {
        assert!(chat_to_responses_request(b"{not json", false).is_none());
        assert!(chat_to_responses_request(b"[1,2]", false).is_none());
        // Go：`null` 解进 struct 是 no-op（无错）⇒ 全零值出站体
        assert_eq!(
            String::from_utf8(chat_to_responses_request(b"null", true).unwrap()).unwrap(),
            r#"{"input":[],"model":"","stream":true}"#
        );
        // 类型不符 ⇒ 整体失败
        assert!(chat_to_responses_request(br#"{"max_tokens":1.5}"#, false).is_none());
        assert!(chat_to_responses_request(br#"{"messages":123}"#, false).is_none());
        assert!(chat_to_responses_request(br#"{"temperature":"x"}"#, false).is_none());
    }

    #[test]
    fn responses_to_chat_completion_maps_items_and_usage() {
        let resp = r#"{"id":"resp_1","status":"completed","created_at":1700000000,"model":"m1","output":[{"type":"reasoning","id":"rs_1"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"你好"},{"type":"output_text","text":"，世界"}]},{"type":"function_call","call_id":"call_1","name":"get_time","arguments":"{\"tz\":\"UTC\"}"}],"usage":{"input_tokens":11,"output_tokens":5,"total_tokens":16}}"# .as_bytes();
        let (body, usage) = responses_to_chat_completion(resp, "fallback").expect("合法响应");
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains(r#""object":"chat.completion""#), "{text}");
        assert!(text.contains(r#""finish_reason":"tool_calls""#), "{text}");
        assert!(text.contains(r#""content":"你好，世界""#), "{text}");
        assert!(text.contains(r#""total_tokens":16"#), "{text}");
        assert_eq!(usage.unwrap().input_tokens, 11);
        // 非法 JSON ⇒ None（调用方原样透传）
        assert!(responses_to_chat_completion(b"[1,2,3]", "m").is_none());
        // incomplete + max_output_tokens ⇒ finish=length
        let inc = br#"{"id":"r","status":"incomplete","created_at":5,"model":"m","output":[],"incomplete_details":{"reason":"max_output_tokens"}}"#;
        let (body, _) = responses_to_chat_completion(inc, "m").unwrap();
        assert!(String::from_utf8(body).unwrap().contains(r#""finish_reason":"length""#));
    }

    const SSE: &str = "data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":5,\"total_tokens\":16}}}\n\n";

    fn translate_all(fragments: &[&str], chunk_id: &str) -> String {
        let mut tr = ResponsesStreamTranslator::new("m", chunk_id, 1700000000);
        let mut out: Vec<u8> = Vec::new();
        for f in fragments {
            out.extend_from_slice(&tr.feed(f.as_bytes()).bytes);
        }
        out.extend_from_slice(&tr.finish().bytes);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn responses_stream_is_fragment_invariant_and_chunk_shaped() {
        let whole = translate_all(&[SSE], "chatcmpl-X");
        // 逐字节切分（含把一行切成两半）必须得到同样的字节
        // 按 **char 边界** 切（按字节切会把多字节字符劈成无效 UTF-8，那是测试自己的损耗）
        let frags: Vec<&str> = {
            let mut out: Vec<&str> = Vec::new();
            let mut start = 0usize;
            for (i, _) in SSE.char_indices().skip(7).step_by(7) {
                out.push(&SSE[start..i]);
                start = i;
            }
            out.push(&SSE[start..]);
            out
        };
        let split = translate_all(&frags, "chatcmpl-X");
        assert_eq!(whole, split);
        // 首块是 role delta、中间块 finish_reason 是**空串**（不是 null）。
        // 块是 `map[string]any` ⇒ 键按字节序（choices/created/id/model/object）——
        // 与 Go 的 `json.Marshal(map)` 同形（金标 `responses-stream` 锁重组字节）。
        assert!(
            whole.starts_with(
                r#"data: {"choices":[{"delta":{"content":"","role":"assistant"},"finish_reason":"","index":0,"logprobs":null}],"created":1700000000,"id":"chatcmpl-X","model":"m","object":"chat.completion.chunk"}"#
            ),
            "{whole}"
        );
        assert!(whole.ends_with("data: [DONE]\n\n"), "{whole}");
        assert!(whole.contains(r#""finish_reason":"tool_calls""#), "{whole}");
        assert!(whole.contains(r#""usage":{"completion_tokens":5,"prompt_tokens":11,"total_tokens":16}"#), "{whole}");
    }

    #[test]
    fn responses_stream_error_and_lost_completion_still_send_done() {
        let err = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n\n";
        let out = translate_all(&[err], "chatcmpl-E");
        assert!(out.contains(r#"{"error":{"message":"boom","type":"server_error"}}"#), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
        // 上游没走完就断：仍然补 [DONE]（客户端不悬挂）
        let out = translate_all(&["data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n"], "chatcmpl-F");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
        assert!(!out.contains("\"usage\""), "{out}");
    }
}
