//! anthropic_forward.rs —— Anthropic Messages 协议转发（`internal/tuanjie/anthropic_forward.go`）。
//!
//! `protocol=anthropic` 的外部账号出站走原生 `/v1/messages`（`externalBaseURL` 已补
//! `/v1`，这里只拼 `/messages`）。入站 chat 请求转成 Anthropic Messages 请求，再把
//! 结果翻回 chat 格式（非流式回填 + 流式 SSE 翻译），客户端零改动。
//!
//! 形态要点（对齐 Go 与本文件头注释）：
//! * 请求：`{model, max_tokens(必填，缺省 4096), system?, messages[{role, content blocks}],
//!   tools?(name/description/input_schema), tool_choice?, stream?}`；`tool_result` 嵌在
//!   user 消息里、`tool_use` 嵌在 assistant 消息里，user/assistant **必须交替**
//!   （连续同角色上游 400，本地合并）。
//! * 出站认证：`x-api-key` + `anthropic-version` 头（不用 Bearer；`Bearer ` 前缀剥离）。
//! * 非流式响应：`content[]` 为 text/tool_use 块，`stop_reason`，usage 无 total
//!   （chat 口径 `total = in + out`）。
//! * 流式事件：`message_start` → `content_block_start/delta/stop` → `message_delta`
//!   → `message_stop`；另有 `ping`（忽略）与 `error`；没有 `data: [DONE]`，由本层补发。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::value::RawValue;

use futures_util::StreamExt;

use crate::gojson::compact_go;
use crate::providers::{
    external_base_url, external_headers, ExternalProvider, EXTERNAL_TIMEOUT, MAX_UPSTREAM_BODY_BYTES,
    MAX_UPSTREAM_ERR_BYTES,
};
use crate::responses_forward::{gj_to_bytes, now_unix, parse_chat_request, RawChat};
use crate::server::{gobj, AppState, GJ};

/// Anthropic Messages API 版本头 —— `anthropic_forward.go:35`。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` 缺省值 —— `anthropic_forward.go:38`。
pub const ANTHROPIC_DEFAULT_MAX_TOKENS: i64 = 4096;

// ---------------------------------------------------------------- 请求转换

/// 一条已合并好的 Anthropic 消息（`content` 为 `None` = Go 的 nil 切片 → JSON `null`）。
#[derive(Debug, Clone, PartialEq)]
struct AnthropicMessage {
    role: String,
    content: Option<Vec<GJ>>,
}

fn value_to_raw_json(v: &serde_json::Value) -> String {
    let raw = crate::gojson::marshal_go_value(v);
    String::from_utf8_lossy(&compact_go(&raw, true)).into_owned()
}

fn raw_msg_content(msg: &crate::responses_forward::RawMsg) -> Option<&[u8]> {
    msg.content.as_ref().map(|c| c.get().as_bytes())
}

/// chat content → Anthropic 内容块数组 —— `contentBlocksToAnthropic`（`:157-171`）。
/// 返回 `None` = Go 的 nil（content 既不是字符串也不是块数组）。
fn content_blocks_to_anthropic(content: Option<&[u8]>) -> Option<Vec<GJ>> {
    let Some(content) = content else {
        return None;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(content) else {
        return None;
    };
    match v {
        serde_json::Value::String(s) => Some(vec![gobj(vec![
            ("type", GJ::Str("text".to_string())),
            ("text", GJ::Str(s)),
        ])]),
        serde_json::Value::Array(blocks) => Some(
            blocks
                .iter()
                .map(chat_block_to_anthropic)
                .collect::<Vec<GJ>>(),
        ),
        _ => None,
    }
}

/// 单个 chat 内容块 → Anthropic 内容块 —— `chatBlockToAnthropic`（`:175-191`）。
fn chat_block_to_anthropic(b: &serde_json::Value) -> GJ {
    match b.get("type").and_then(|t| t.as_str()) {
        Some("text") => gobj(vec![
            ("type", GJ::Str("text".to_string())),
            (
                "text",
                GJ::Str(b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
            ),
        ]),
        Some("image_url") => {
            let url = match b.get("image_url") {
                Some(serde_json::Value::Object(_)) => b
                    .get("image_url")
                    .and_then(|iu| iu.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or(""),
                Some(serde_json::Value::String(s)) => s.as_str(),
                _ => "",
            };
            anthropic_image_block(url)
        }
        // 未识别的块原样透传（上游不支持会 400，走 4xx 透传语义）
        _ => value_to_raw_gj(b),
    }
}

/// 一个 JSON 值按 Go `json.Marshal(RawMessage)` 口径嵌入（compact + HTML 转义）。
fn value_to_raw_gj(v: &serde_json::Value) -> GJ {
    GJ::Raw(value_to_raw_json(v))
}

/// chat image_url → Anthropic image 块 —— `anthropicImageBlock`（`:196-207`）。
pub fn anthropic_image_block(url: &str) -> GJ {
    if let Some((meta, data)) = url.split_once(',') {
        if meta.starts_with("data:") && meta.contains(";base64") {
            let mt = meta.trim_start_matches("data:");
            let media_type = mt.split(';').next().unwrap_or("");
            let media_type = if media_type.is_empty() {
                "image/png"
            } else {
                media_type
            };
            return gobj(vec![
                ("type", GJ::Str("image".to_string())),
                (
                    "source",
                    gobj(vec![
                        ("type", GJ::Str("base64".to_string())),
                        ("media_type", GJ::Str(media_type.to_string())),
                        ("data", GJ::Str(data.to_string())),
                    ]),
                ),
            ]);
        }
    }
    gobj(vec![
        ("type", GJ::Str("image".to_string())),
        (
            "source",
            gobj(vec![
                ("type", GJ::Str("url".to_string())),
                ("url", GJ::Str(url.to_string())),
            ]),
        ),
    ])
}

/// chat `arguments` 字符串 → Anthropic `input` 对象 —— `anthropicToolInput`（`:211-219`）。
pub fn anthropic_tool_input(arguments: &str) -> GJ {
    if !arguments.is_empty() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) {
            if v.is_object() {
                return value_to_raw_gj(&v);
            }
        }
    }
    GJ::Obj(BTreeMap::new())
}

/// chat 嵌套 function tools → Anthropic 形态 —— `chatToolsToAnthropic`（`:224-259`）。
pub fn chat_tools_to_anthropic(raw: Option<&serde_json::Value>) -> Option<GJ> {
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
        let mut pairs: Vec<(&str, GJ)> = vec![(
            "name",
            GJ::Str(f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string()),
        )];
        if let Some(d) = f.get("description").and_then(|x| x.as_str()) {
            if !d.is_empty() {
                pairs.push(("description", GJ::Str(d.to_string())));
            }
        }
        match f.get("parameters") {
            Some(p) => pairs.push(("input_schema", value_to_raw_gj(p))),
            None => pairs.push((
                "input_schema",
                gobj(vec![("type", GJ::Str("object".to_string()))]),
            )),
        }
        out.push(gobj(pairs));
    }
    if out.is_empty() {
        None
    } else {
        Some(GJ::Arr(out))
    }
}

/// chat tool_choice → Anthropic 形态 —— `chatToolChoiceToAnthropic`（`:264-288`）。
pub fn chat_tool_choice_to_anthropic(raw: Option<&serde_json::Value>) -> Option<GJ> {
    let raw = raw?;
    if let serde_json::Value::String(s) = raw {
        return match s.as_str() {
            "auto" => Some(gobj(vec![("type", GJ::Str("auto".to_string()))])),
            "required" => Some(gobj(vec![("type", GJ::Str("any".to_string()))])),
            _ => None,
        };
    }
    let obj = raw.as_object()?;
    let fname = obj
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("");
    if !fname.is_empty() {
        return Some(gobj(vec![
            ("type", GJ::Str("tool".to_string())),
            ("name", GJ::Str(fname.to_string())),
        ]));
    }
    None
}

/// 入站 chat 请求体 → Anthropic 请求体 —— `chatToAnthropicRequest`（`:43-154`）。
pub fn chat_to_anthropic_request(body: &[u8], stream: bool) -> Option<Vec<u8>> {
    let chat: RawChat = parse_chat_request(body)?;
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    let append_msg = |messages: &mut Vec<AnthropicMessage>, role: &str, blocks: Vec<GJ>| {
        if let Some(last) = messages.last_mut() {
            if last.role == role {
                match &mut last.content {
                    Some(v) => {
                        v.extend(blocks);
                        return;
                    }
                    None => {
                        if !blocks.is_empty() {
                            last.content = Some(blocks);
                        }
                        return;
                    }
                }
            }
        }
        messages.push(AnthropicMessage {
            role: role.to_string(),
            content: if blocks.is_empty() { None } else { Some(blocks) },
        });
    };

    for msg in chat.messages.as_deref().unwrap_or(&[]) {
        let Some(msg) = msg else {
            continue;
        };
        let role = msg.role.clone().unwrap_or_default();
        let content_raw = raw_msg_content(msg);
        match role.as_str() {
            "system" => {
                // system 不进 messages：提取文本归一为顶层 system 字符串
                let t = crate::responses_forward::content_as_string(content_raw);
                if !t.is_empty() {
                    system_parts.push(t);
                }
            }
            "tool" => {
                let block = gobj(vec![
                    ("type", GJ::Str("tool_result".to_string())),
                    (
                        "tool_use_id",
                        GJ::Str(msg.tool_call_id.clone().unwrap_or_default()),
                    ),
                    (
                        "content",
                        GJ::Str(crate::responses_forward::content_as_string(content_raw)),
                    ),
                ]);
                append_msg(&mut messages, "user", vec![block]);
            }
            _ => {
                let tool_calls = msg.tool_calls.as_deref().unwrap_or(&[]);
                if role == "assistant" && !tool_calls.is_empty() {
                    let mut blocks: Vec<GJ> = Vec::new();
                    let s = crate::responses_forward::content_as_string(content_raw);
                    if !s.is_empty() {
                        blocks.push(gobj(vec![
                            ("type", GJ::Str("text".to_string())),
                            ("text", GJ::Str(s)),
                        ]));
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
                        blocks.push(gobj(vec![
                            ("type", GJ::Str("tool_use".to_string())),
                            ("id", GJ::Str(tc.id.clone().unwrap_or_default())),
                            ("name", GJ::Str(name)),
                            ("input", anthropic_tool_input(&args)),
                        ]));
                    }
                    append_msg(&mut messages, &role, blocks);
                    continue;
                }
                if !crate::responses_forward::has_content(content_raw) {
                    continue;
                }
                match content_blocks_to_anthropic(content_raw) {
                    Some(blocks) => append_msg(&mut messages, &role, blocks),
                    None => append_msg(&mut messages, &role, Vec::new()),
                }
            }
        }
    }

    let max_tokens = chat
        .max_completion_tokens
        .or(chat.max_tokens)
        .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
    let messages_gj: Vec<GJ> = messages.iter().map(anthropic_message_gj).collect();
    let mut pairs: Vec<(&str, GJ)> = vec![
        ("model", GJ::Str(chat.model.clone().unwrap_or_default())),
        ("max_tokens", GJ::Int(max_tokens)),
        ("messages", GJ::Arr(messages_gj)),
        ("stream", GJ::Bool(stream)),
    ];
    let sys = system_parts.join("\n\n");
    if !sys.is_empty() {
        pairs.push(("system", GJ::Str(sys)));
    }
    if let Some(t) = chat.temperature {
        pairs.push(("temperature", GJ::Float(t)));
    }
    if let Some(p) = chat.top_p {
        pairs.push(("top_p", GJ::Float(p)));
    }
    let tools_value = chat
        .tools
        .as_ref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t.get()).ok());
    if let Some(tools) = chat_tools_to_anthropic(tools_value.as_ref()) {
        pairs.push(("tools", tools));
    }
    let tc_value = chat
        .tool_choice
        .as_ref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t.get()).ok());
    if let Some(tc) = chat_tool_choice_to_anthropic(tc_value.as_ref()) {
        pairs.push(("tool_choice", tc));
    }
    Some(gj_to_bytes(&gobj(pairs)))
}

fn anthropic_message_gj(m: &AnthropicMessage) -> GJ {
    gobj(vec![
        ("role", GJ::Str(m.role.clone())),
        (
            "content",
            match &m.content {
                Some(v) => GJ::Arr(v.clone()),
                None => GJ::Null,
            },
        ),
    ])
}

// ---------------------------------------------------------------- 响应转换

/// Anthropic usage —— `anthropicUsage`（`:291-294`，无 total）。
#[derive(Debug, Default, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
}

fn anthropic_usage_gj(u: AnthropicUsage) -> GJ {
    gobj(vec![
        ("prompt_tokens", GJ::Int(u.input_tokens)),
        ("completion_tokens", GJ::Int(u.output_tokens)),
        ("total_tokens", GJ::Int(u.input_tokens + u.output_tokens)),
    ])
}

/// Anthropic stop_reason → chat finish_reason —— `anthropicStopToFinish`（`:298-307`）。
pub fn anthropic_stop_to_finish(stop: &str) -> &'static str {
    match stop {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

/// Anthropic 非流式响应 → chat completion —— `anthropicToChatCompletion`（`:313-375`）。
pub fn anthropic_to_chat_completion(
    resp_body: &[u8],
    model: &str,
) -> Option<(Vec<u8>, Option<AnthropicUsage>)> {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Res {
        id: String,
        model: String,
        content: Vec<Block>,
        stop_reason: String,
        usage: Option<AnthropicUsage>,
    }
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Block {
        r#type: String,
        text: String,
        id: String,
        name: String,
        input: Option<Box<RawValue>>,
    }
    let ar: Res = crate::responses_forward::parse_go_struct(resp_body)?;

    let mut content = String::new();
    let mut tool_calls: Vec<GJ> = Vec::new();
    for b in &ar.content {
        match b.r#type.as_str() {
            "text" => content.push_str(&b.text),
            "tool_use" => {
                let args = match &b.input {
                    Some(v) => raw_bytes_to_raw_json(v.get().as_bytes()),
                    None => "{}".to_string(),
                };
                tool_calls.push(gobj(vec![
                    ("id", GJ::Str(b.id.clone())),
                    ("type", GJ::Str("function".to_string())),
                    (
                        "function",
                        gobj(vec![
                            ("name", GJ::Str(b.name.clone())),
                            ("arguments", GJ::Str(args)),
                        ]),
                    ),
                ]));
            }
            _ => {}
        }
    }
    let mdl = if ar.model.is_empty() {
        model.to_string()
    } else {
        ar.model.clone()
    };
    let mut finish = anthropic_stop_to_finish(&ar.stop_reason).to_string();
    if !tool_calls.is_empty() {
        finish = "tool_calls".to_string();
    }
    let mut message: Vec<(&str, GJ)> = vec![
        ("role", GJ::Str("assistant".to_string())),
        ("content", GJ::Str(content)),
    ];
    if !tool_calls.is_empty() {
        message.push(("tool_calls", GJ::Arr(tool_calls)));
    }
    let mut pairs: Vec<(&str, GJ)> = vec![
        ("id", GJ::Str(ar.id.clone())),
        ("object", GJ::Str("chat.completion".to_string())),
        ("created", GJ::Int(now_unix())),
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
    if let Some(u) = ar.usage {
        pairs.push(("usage", anthropic_usage_gj(u)));
    }
    Some((gj_to_bytes(&gobj(pairs)), ar.usage))
}

/// 原始 JSON 字节按 Go `json.Marshal(RawMessage)` 口径嵌入（compact + HTML 转义）。
fn raw_bytes_to_raw_json(raw: &[u8]) -> String {
    String::from_utf8_lossy(&compact_go(raw, true)).into_owned()
}

// ---------------------------------------------------------------- 流式翻译

/// 一次 `feed` 的产物（字节 + 计入 inflight 的字节数）。
pub use crate::responses_forward::StreamOut;

/// Anthropic Messages SSE → chat SSE 的**增量**翻译器 —— `streamAnthropicToChat`（`:386-542`）。
pub struct AnthropicStreamTranslator {
    chunk_id: String,
    created: i64,
    model: String,
    buf: Vec<u8>,
    role_sent: bool,
    tool_count: usize,
    tool_idx: BTreeMap<i64, i64>,
    in_tokens: i64,
    out_tokens: i64,
    /// 终态（`message_stop`）已到（Go 的 onUsage 只在终态回调一次；
    /// `done` 还会在 error 路径置位，不能拿它当门——error 不上报）。
    terminal: bool,
    /// usage 是否已上报过（与 `terminal` 合起来实现"终态一次"）。
    usage_reported: bool,
    stop_reason: String,
    done: bool,
}

impl AnthropicStreamTranslator {
    pub fn new(model: &str, chunk_id: &str, created: i64) -> Self {
        AnthropicStreamTranslator {
            chunk_id: chunk_id.to_string(),
            created,
            model: model.to_string(),
            buf: Vec::new(),
            role_sent: false,
            tool_count: 0,
            tool_idx: BTreeMap::new(),
            in_tokens: 0,
            out_tokens: 0,
            terminal: false,
            usage_reported: false,
            stop_reason: String::new(),
            done: false,
        }
    }

    /// 已收到的 usage（`in_tokens>0 || out_tokens>0` 时才有值）—— Go 的 onUsage 触发条件。
    /// 注意：这只是**读取**（供 `message_stop` 事件带 in-band usage 用，不限次）；
    /// 统计记账走 [`Self::take_usage`]（一次性，Go 只在终态回调一次）。
    pub fn usage(&self) -> Option<(i64, i64)> {
        if self.in_tokens > 0 || self.out_tokens > 0 {
            Some((self.in_tokens, self.out_tokens))
        } else {
            None
        }
    }

    /// 取出 usage 并记为已上报（供流收尾做 `stats.add` 用）——只在终态（`message_stop`）
    /// 到达后才有值，且只给一次（Go 的 onUsage 只在终态回调一次；此前非消费式读取致
    /// 每块都 `add` 一次——P8b 第五片审查 #1）。
    pub fn take_usage(&mut self) -> Option<(i64, i64)> {
        if !self.terminal || self.usage_reported {
            return None;
        }
        let u = self.usage()?;
        self.usage_reported = true;
        Some(u)
    }

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
            "message_start" => {
                if let Some(u) = ev
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| serde_json::from_value::<AnthropicUsage>(u.clone()).ok())
                {
                    self.in_tokens = u.input_tokens;
                }
                self.ensure_role(out);
            }
            "content_block_start" => {
                let Some(cb) = ev.get("content_block") else {
                    return;
                };
                match cb.get("type").and_then(|t| t.as_str()) {
                    Some("text") => self.ensure_role(out),
                    Some("tool_use") => {
                        self.ensure_role(out);
                        let index = ev.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                        let idx = self.tool_count as i64;
                        self.tool_idx.insert(index, idx);
                        let id = cb.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        let name = cb.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        self.send_chunk(
                            out,
                            vec![(
                                "tool_calls",
                                GJ::Arr(vec![gobj(vec![
                                    ("index", GJ::Int(idx)),
                                    ("id", GJ::Str(id.to_string())),
                                    ("type", GJ::Str("function".to_string())),
                                    (
                                        "function",
                                        gobj(vec![
                                            ("name", GJ::Str(name.to_string())),
                                            ("arguments", GJ::Str(String::new())),
                                        ]),
                                    ),
                                ])]),
                            )],
                            "",
                            None,
                        );
                        self.tool_count += 1;
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(delta) = ev.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        self.ensure_role(out);
                        let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        self.send_chunk(
                            out,
                            vec![("content", GJ::Str(text.to_string()))],
                            "",
                            None,
                        );
                    }
                    Some("input_json_delta") => {
                        let index = ev.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                        if let Some(ti) = self.tool_idx.get(&index).copied() {
                            let pj = delta
                                .get("partial_json")
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            self.send_chunk(
                                out,
                                vec![(
                                    "tool_calls",
                                    GJ::Arr(vec![gobj(vec![
                                        ("index", GJ::Int(ti)),
                                        (
                                            "function",
                                            gobj(vec![("arguments", GJ::Str(pj.to_string()))]),
                                        ),
                                    ])]),
                                )],
                                "",
                                None,
                            );
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = ev
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    if !sr.is_empty() {
                        self.stop_reason = sr.to_string();
                    }
                }
                if let Some(u) = ev
                    .get("usage")
                    .and_then(|u| serde_json::from_value::<AnthropicUsage>(u.clone()).ok())
                {
                    if u.output_tokens > 0 {
                        self.out_tokens = u.output_tokens;
                    }
                }
            }
            "message_stop" => {
                let mut finish = anthropic_stop_to_finish(&self.stop_reason).to_string();
                if self.stop_reason.is_empty() && self.tool_count > 0 {
                    finish = "tool_calls".to_string();
                }
                // 终态到达：统计记账从此可取一次（`take_usage` 的门）。
                self.terminal = true;
                let usage = self.usage().map(|(i, o)| AnthropicUsage {
                    input_tokens: i,
                    output_tokens: o,
                });
                self.send_chunk(out, vec![], &finish, usage);
                out.bytes.extend_from_slice(b"data: [DONE]\n\n");
                self.done = true;
            }
            "error" => {
                let mut msg = "upstream stream failed".to_string();
                if let Some(m) = ev
                    .get("error")
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

    /// `finish` 传 `""` 表示 Go 的 `finish=""` 字符串（不是 null）—— 见
    /// `sendChunk(map[string]any{…}, "", nil)`。
    fn send_chunk(
        &self,
        out: &mut StreamOut,
        delta: Vec<(&str, GJ)>,
        finish: &str,
        usage: Option<AnthropicUsage>,
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
            pairs.push(("usage", anthropic_usage_gj(u)));
        }
        let bytes = gj_to_bytes(&gobj(pairs));
        out.bytes.extend_from_slice(b"data: ");
        out.bytes.extend_from_slice(&bytes);
        out.bytes.extend_from_slice(b"\n\n");
        out.touched += (bytes.len() + b"data: \n\n".len()) as i64;
    }
}

// ---------------------------------------------------------------- 转发主体

/// `forwardExternalAnthropic`（`anthropic_forward.go:549-629`）。
pub async fn forward_external_anthropic(
    st: &Arc<AppState>,
    body: Vec<u8>,
    model: &str,
    wants_stream: bool,
    prov: &ExternalProvider,
    started: Instant,
) -> Result<Response, i32> {
    let pname = prov.name.clone();
    let rid = st.registry.register(model, &pname, wants_stream);

    let Some(req_body) = chat_to_anthropic_request(&body, wants_stream) else {
        st.registry.finish(&rid);
        return Err(-1);
    };
    let url = format!("{}/messages", external_base_url(&prov.base_url));
    let accept = if wants_stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let key = prov.api_key.strip_prefix("Bearer ").unwrap_or(&prov.api_key);
    let mut headers = external_headers(prov, ("x-api-key", key.to_string()), accept);
    if let Ok(v) = HeaderValue::from_str(ANTHROPIC_VERSION) {
        headers.insert("anthropic-version", v);
    }
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
                "[tuanjie] external anthropic model={model} provider={pname} err={e}"
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
            "[tuanjie] external anthropic model={model} provider={pname} status={}",
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
        "[tuanjie] external anthropic model={model} provider={pname} status=200"
    ));

    if wants_stream {
        let chunk_id = format!("chatcmpl-{}", crate::inflight::next_rid());
        let created = now_unix();
        let translator = AnthropicStreamTranslator::new(model, &chunk_id, created);
        let stc = st.clone();
        let pname2 = pname.clone();
        let model2 = model.to_string();
        let model3 = model.to_string();
        let rid2 = rid.clone();
        let guard = crate::chat::StreamGuard::with_callback(st.registry.clone(), rid.clone(), move || {
            stc.activity.add(
                "ok",
                &format!("外部源 {pname2} · {model2}（Anthropic）（流式）"),
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
                            if let Some((i, o)) = tr.take_usage() {
                                stc2.stats.add(&model3, i, o, i + o);
                            }
                            stc2.registry.touch(&rid3, out.touched);
                            let bytes: Result<Vec<u8>, std::io::Error> = Ok(out.bytes);
                            Some((bytes, Some((inner, tr, g))))
                        }
                        _ => {
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
        &format!("外部源 {pname} · {model}（Anthropic）"),
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
    match anthropic_to_chat_completion(&resp_body, model) {
        Some((cb, usage)) => {
            if let Some(u) = usage {
                st.stats.add(
                    model,
                    u.input_tokens,
                    u.output_tokens,
                    u.input_tokens + u.output_tokens,
                );
            }
            Ok((StatusCode::OK, out_headers, Body::from(cb)).into_response())
        }
        None => {
            crate::logf(&format!(
                "[tuanjie] external anthropic model={model} provider={pname} 非预期响应形态，原样透传"
            ));
            Ok((StatusCode::OK, out_headers, Body::from(resp_body)).into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 金标 `anthropic-nonstream` 的入站体（与 Go 采集器逐字节同源）。
    const REQ: &str = r#"{"model":"claude-x","messages":[{"role":"system","content":"sys1"},{"role":"system","content":"sys2"},{"role":"user","content":"hi"},{"role":"assistant","content":"prev","tool_calls":[{"id":"call_9","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}},{"id":"call_10","type":"function","function":{"name":"g","arguments":"not json"}}]},{"role":"tool","tool_call_id":"call_9","content":"result"},{"role":"user","content":[{"type":"text","text":"more"},{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,BBBB"}},{"type":"image_url","image_url":{"url":"https://x/y.png"}}]}],"tools":[{"type":"function","function":{"name":"f","description":"d","parameters":{"type":"object"}}},{"type":"function","function":{"name":"noname"}}],"tool_choice":"required","temperature":0.5,"max_tokens":77,"stream":false}"#;

    #[test]
    fn chat_to_anthropic_request_matches_go_bytes() {
        let out = chat_to_anthropic_request(REQ.as_bytes(), false).expect("转换成功");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"max_tokens":77,"messages":[{"content":[{"text":"hi","type":"text"}],"role":"user"},{"content":[{"text":"prev","type":"text"},{"id":"call_9","input":{"a":1},"name":"f","type":"tool_use"},{"id":"call_10","input":{},"name":"g","type":"tool_use"}],"role":"assistant"},{"content":[{"content":"result","tool_use_id":"call_9","type":"tool_result"},{"text":"more","type":"text"},{"source":{"data":"BBBB","media_type":"image/jpeg","type":"base64"},"type":"image"},{"source":{"type":"url","url":"https://x/y.png"},"type":"image"}],"role":"user"}],"model":"claude-x","stream":false,"system":"sys1\n\nsys2","temperature":0.5,"tool_choice":{"type":"any"},"tools":[{"description":"d","input_schema":{"type":"object"},"name":"f"},{"input_schema":{"type":"object"},"name":"noname"}]}"#
        );
    }

    #[test]
    fn anthropic_defaults_and_type_errors() {
        // max_tokens 必填：缺省给 4096
        let out = chat_to_anthropic_request(br#"{"model":"m","messages":[]}"#, false).unwrap();
        assert!(String::from_utf8(out).unwrap().contains(r#""max_tokens":4096"#));
        assert!(chat_to_anthropic_request(b"null", false).is_some());
        assert!(chat_to_anthropic_request(br#"{"messages":{}}"#, false).is_none());
    }

    #[test]
    fn anthropic_consecutive_same_role_is_merged() {
        // 连续 user（文本 + tool_result）合并成一条（Go 的 appendMsg）
        let out = chat_to_anthropic_request(
            br#"{"model":"m","messages":[{"role":"user","content":"a"},{"role":"tool","tool_call_id":"t","content":"r"}]}"#,
            false,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches(r#""role":"user""#).count(), 1, "{text}");
        assert!(text.contains(r#"[{"text":"a","type":"text"},{"content":"r","tool_use_id":"t","type":"tool_result"}]"#), "{text}");
    }

    #[test]
    fn anthropic_to_chat_completion_maps_blocks_and_stop_reason() {
        let resp = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-3","content":[{"type":"text","text":"你好"},{"type":"tool_use","id":"tu_1","name":"get_time","input":{"tz":"UTC"}}],"stop_reason":"tool_use","usage":{"input_tokens":11,"output_tokens":5}}"# .as_bytes();
        let (body, usage) = anthropic_to_chat_completion(resp, "fb").unwrap();
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains(r#""content":"你好""#), "{text}");
        assert!(text.contains(r#""arguments":"{\"tz\":\"UTC\"}""#), "{text}");
        assert!(text.contains(r#""finish_reason":"tool_calls""#), "{text}");
        // Anthropic 无 total ⇒ chat 口径 total = in + out
        assert!(text.contains(r#""total_tokens":16"#), "{text}");
        assert_eq!(usage.unwrap().output_tokens, 5);
        assert!(anthropic_to_chat_completion(b"[]", "m").is_none());
        // stop_reason 映射
        assert_eq!(anthropic_stop_to_finish("max_tokens"), "length");
        assert_eq!(anthropic_stop_to_finish("end_turn"), "stop");
        assert_eq!(anthropic_stop_to_finish(""), "stop");
    }

    const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"get_time\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"tz\\\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\":\\\"UTC\\\"}\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    fn translate_all(fragments: &[&str], chunk_id: &str) -> String {
        let mut tr = AnthropicStreamTranslator::new("m", chunk_id, 1700000000);
        let mut out: Vec<u8> = Vec::new();
        for f in fragments {
            out.extend_from_slice(&tr.feed(f.as_bytes()).bytes);
        }
        out.extend_from_slice(&tr.finish().bytes);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn anthropic_stream_is_fragment_invariant() {
        let whole = translate_all(&[SSE], "chatcmpl-A");
        // 按 **char 边界** 切（按字节切会劈开多字节字符，那是测试自己的损耗）
        let frags: Vec<&str> = {
            let mut out: Vec<&str> = Vec::new();
            let mut start = 0usize;
            for (i, _) in SSE.char_indices().skip(5).step_by(5) {
                out.push(&SSE[start..i]);
                start = i;
            }
            out.push(&SSE[start..]);
            out
        };
        let split = translate_all(&frags, "chatcmpl-A");
        assert_eq!(whole, split);
        assert!(whole.ends_with("data: [DONE]\n\n"), "{whole}");
        assert!(whole.contains(r#""finish_reason":"tool_calls""#), "{whole}");
        // input_json_delta 落到 tool_calls 的 arguments 增量（客户端按 OpenAI 语义累积）
        assert!(whole.contains(r#""arguments":"{\"tz\""#), "{whole}");
        assert!(whole.contains(r#""usage":{"completion_tokens":5,"prompt_tokens":11,"total_tokens":16}"#), "{whole}");
    }

    #[test]
    fn anthropic_stream_error_event_and_truncation() {
        let out = translate_all(
            &["data: {\"type\":\"error\",\"error\":{\"message\":\"nope\"}}\n\n"],
            "chatcmpl-B",
        );
        assert!(out.contains(r#"{"error":{"message":"nope","type":"server_error"}}"#), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
        // 上游没走到 message_stop：补 [DONE]，且**不**发 usage
        let out = translate_all(&["data: {\"type\":\"message_start\"}\n"], "chatcmpl-C");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
        assert!(!out.contains("usage"), "{out}");
    }

    /// P8b 第五片审查 #1：`take_usage` 只在终态（`message_stop`）给一次终值——
    /// 首块 `message_start` 带 usage 也不提前上报（Go 的 onUsage 只在终态回调），
    /// 终态后再次调用不再上报（多 chunk 真实流不再重复 `stats.add`）。
    #[test]
    fn take_usage_is_one_shot_like_go_onusage() {
        let mut tr = AnthropicStreamTranslator::new("m", "c", 1);
        assert_eq!(tr.take_usage(), None, "无 usage 时无上报");
        tr.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n");
        assert_eq!(tr.take_usage(), None, "终态前不提前上报（即使已有部分值）");
        tr.feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n");
        tr.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(tr.take_usage(), Some((11, 5)), "终态上报终值一次");
        assert_eq!(tr.take_usage(), None, "再次调用不再上报");
        // in-band 读取不受影响（message_stop 事件仍带 usage）。
        assert_eq!(tr.usage(), Some((11, 5)));
    }
}
