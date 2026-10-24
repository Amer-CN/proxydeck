//! chat.rs —— `/v1/chat/completions`（**P5a 请求侧 + P5b 池/凭据/额度/failover + P5c 下游**）。
//!
//! 覆盖：方法门（非 POST → 405 + `Allow: POST`）、64MB 上限读体、Go `json.Unmarshal`→
//! `map[string]any` 的入站解析（逐字错误文案）、白名单字段透传 + `stream` 强制 true +
//! `stream_options` 缺省、`tool_choice` 归一、`$ref` 内联、`developer`→`system`、
//! **全角色脱敏**（P5c；生产 CN/INTL 都开）、INTL 自动补首条 system、出站体**逐字节重建**、
//! **账号池换号分支**（401/403 标死换活号、429 冷却换号、14018 额度耗尽换号、
//! 11128 按号封禁换号 + 1h 冷却；**通道校验子类直接透传**，不判账号不冷却）、
//! **200 体 14018 Peek**（判定窗口 64KB；不命中时 peek 到的字节**一字不丢**地交给下游聚合，
//! 对齐 Go `3d4627f` 之后的行为，见 `PEEK_WINDOW`）、**流内错误嗅探**（流式路径在发出
//! SSE 头之前 peek 8KB，见 `STREAM_PEEK_WINDOW`；命中错误码/空流只**换号重试**、
//! **绝不冷却账号**，判错只耗 attempt）、CN 缺省
//! `reasoning_effort` 注入、hy4 限流兜底改写与 trip、11133 退避重试（上限 2 次，
//! 800ms/1600ms，退避期间客户端断开 → 499）与失败现场落盘、上游非 200 的 `writeErr` 形状。
//!
//! **P5c 的下游**（此前是最小透传，现已等价）：
//! * 流式：`sseFieldCleaner` 逐行剔空键 + `usageScanner` 提取 usage 入账（逐字节同 Go）；
//! * 非流式：`collectStream` 聚合为 `chat.completion` + usage 入账。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use serde_json::{Map, Value};

use crate::conn::ConnClosed;
use crate::gjson;
use crate::sse::{SseFieldCleaner, UsageScanner};
use crate::toolrefs;
use crate::AppState;

/// Go `io.LimitReader(r.Body, 64<<20)`：**截断**读（不是拒绝）。
pub const MAX_BODY_BYTES: usize = 64 << 20;
/// Go `io.LimitReader(resp.Body, 1<<20)`：上游错误体上限。
pub const MAX_ERR_BODY: usize = 1 << 20;
/// Go `transientMaxRetry`：11133 原地重试次数上限。
pub const TRANSIENT_MAX_RETRY: usize = 2;
/// Go `flaggedCooldown`：11128 账号级封禁的冷却时长（`time.Hour`）。
///
/// 2026-10-17 实证：国际版 5 号中 2 号被上游按号封禁——干净请求也照回 400/11128，
/// 同字节换号即 200，与内容/模型无关。上游不给 reset 时点（不是 429 频控），固定
/// 1 小时兜底：`pick` 懒判定到期即自动可选；GUI「提前恢复」（`clear_cool`）可立即解冷却。
pub const FLAGGED_COOLDOWN_MS: i64 = 60 * 60 * 1000;
/// Go `br.Peek(64 * 1024)` 的**判定窗口**。
///
/// Go 写的 `bufio.NewReader(resp.Body)` 默认缓冲只有 4096，`Peek(64KB)` 实到 4096 字节就
/// 返回 `ErrBufferFull`；**原先 `br` 没回填 `resp.Body`**，那 4096 字节被丢弃，下游
/// `collectStream` 读到的是从第 4097 字节开始的流（表现为 `content:null` + usage 全 0）。
/// **Go 已于 `3d4627f` 修复**：不命中时 `resp.Body = struct{ io.Reader; io.Closer }{br, resp.Body}`，
/// 缓冲与未读部分一起被读到 ⇒ 下游拿到**完整**上游体。本实现据此改为「peek 多少都留下」，
/// 不再复刻丢头（差异 33 已改写为「Go 已修、Rust 已对齐」）。
///
/// 另：`ChunkReader::peek` 按 64KB 窗口读、Go 的 `bufio.Reader` 只读到 4096 —— 这只影响
/// 「一次从上游拉多少字节」，两边的判定都是对「已缓冲字节」做 `body != "" && contains("14018")`
/// 子串判，聚合又是逐行的（`feed_line` 与 Go `bufio.Scanner` 同语义），故下游字节完全一致。
pub const PEEK_WINDOW: usize = 64 * 1024;
/// 流式路径的嗅探窗口（**比非流式的 64KB 小**）：流式要求首字节尽快到下游，
/// 8KB 足够看清「是不是 data: 帧 / 有没有错误码」，又不至于把首字节压太久。
pub const STREAM_PEEK_WINDOW: usize = 8 * 1024;

/// 流内嗅探的**时间预算**：到点就用已攒到的字节下判断，不再等。
///
/// 上游在思考型模型的 prefill 阶段常静默 30~70 秒（实测数据见 qidi-relay 的
/// `classify_failure.py` 注释）；若按「攒满 8KB 或流结束」死等，首字节会被压到
/// prefill 结束——比不嗅探更糟。120ms 足够覆盖「上游一上来就吐错误帧」的情形，
/// 又不会让正常流明显变慢。
pub const STREAM_PEEK_BUDGET: Duration = Duration::from_millis(120);
/// Go `passthroughBodyKeys`（顺序即 Go 的切片顺序；写出时按键排序）。
pub const PASSTHROUGH_KEYS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "temperature",
    "max_tokens",
    "max_completion_tokens",
    "top_p",
    "stream",
    "stream_options",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "n",
    "response_format",
    "seed",
    "user",
    "reasoning_effort",
    "verbosity",
    "reasoning_summary",
];

/// Go `isTransientReject`：只认字段级 `"code":11133`（词边界）。
pub fn is_transient_reject(err_body: &str) -> bool {
    let b = err_body.as_bytes();
    let needle = b"\"code\"";
    let mut i = 0usize;
    while i + needle.len() <= b.len() {
        if &b[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < b.len() && b[j] == b':' {
                j += 1;
                while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n' | b'\r') {
                    j += 1;
                }
                if b[j..].starts_with(b"11133") {
                    let after = b.get(j + 5).copied();
                    // Go 的 `\b`：后面不能是 [0-9A-Za-z_]
                    let word = after
                        .map(|c| c.is_ascii_alphanumeric() || c == b'_')
                        .unwrap_or(false);
                    if !word {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// Go `isFlaggedBody`：上游 11128（`Illegal API invocation from an unapproved channel`，
/// 2026-10-17 实证既可能是内容拦截也可能是账号级封禁）。
///
/// Go 的正则是 `"code"\s*:\s*"?11128"?\b`：只认**字段级** code（数字或字符串两种形态），
/// 裸子串 `11128` 会把 requestId/正文里的同数字串误判成封号。尾随 `"?` 是可选的，
/// 故是否命中只取决于「11128 之后那个字节不是词字符」——与 Go 的 `\b` 逐条等价
/// （数字是词字符，其后是 `,`/`}`/`"`/行尾都成立；`111280`/`"11128x"` 都不成立）。
pub fn is_flagged_body(err_body: &str) -> bool {
    let b = err_body.as_bytes();
    let needle = b"\"code\"";
    let mut i = 0usize;
    while i + needle.len() <= b.len() {
        if &b[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if b.get(j) == Some(&b':') {
                j += 1;
                while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n' | b'\r') {
                    j += 1;
                }
                // 正则里的 `"?`：数字形态与字符串形态都认（只吃一个引号）。
                if b.get(j) == Some(&b'"') {
                    j += 1;
                }
                if b[j..].starts_with(b"11128") {
                    let after = b.get(j + 5).copied();
                    // Go 的 `\b`：后面不能是 [0-9A-Za-z_]
                    let word = after
                        .map(|c| c.is_ascii_alphanumeric() || c == b'_')
                        .unwrap_or(false);
                    if !word {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// 11128 治理的冷却到期时点（epoch 毫秒）：Go `time.Now().Add(flaggedCooldown)`。
pub fn flagged_until(now_ms: i64) -> i64 {
    now_ms.saturating_add(FLAGGED_COOLDOWN_MS)
}

/// INTL 会话关联头（2026-10-19 上游网关新增校验）：`www.codebuddy.ai` 的 chat
/// 缺这三个头即回 400/11128（`request illegal` + channel verification）。
/// 证据：mmqz/cpa-multi-plugins#43（Global，补头后 200）/#44（Intl，补头后
/// 200/1.4s）、9router#4944（七行矩阵：三头必须同时在，值只验形态不校验）。
pub const CONV_HEADERS: &[&str] = &[
    "x-conversation-id",
    "x-conversation-request-id",
    "x-conversation-message-id",
];

/// 生成一组会话关联 ID（三个 v4 UUID 字符串）。
///
/// 用 crate 自带的 `atrest::getrandom_fill`（Windows 走 RtlGenRandom = Go 的
/// `crypto/rand`），不新增依赖；随机源不可用时如实报 502，不编可预测 ID。
pub fn new_uuid_v4() -> Result<String, String> {
    let mut b = [0u8; 16];
    crate::atrest::getrandom_fill(&mut b)?;
    // RFC 4122 v4：版本 nibble 置 4，variant 置 10xx。
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
         {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10],
        b[11], b[12], b[13], b[14], b[15]
    ))
}

pub fn new_conversation_ids() -> Result<[String; 3], String> {
    Ok([new_uuid_v4()?, new_uuid_v4()?, new_uuid_v4()?])
}

/// `n` 字节随机数的 hex 串（traceparent / b3 用：trace 16B→32 hex，span 8B→16 hex）。
pub fn new_hex(n: usize) -> Result<String, String> {
    let mut b = vec![0u8; n];
    crate::atrest::getrandom_fill(&mut b)?;
    let mut s = String::with_capacity(n * 2);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    Ok(s)
}

/// 官方网关身份头的一组值（2026-10-24 取自 CN WorkBuddy 5.8.3 `app.asar` 的
/// `applyGatewayHeaders` / `applyTraceHeaders`）。
///
/// 上游 11128 的判定不只看三个 conversation 头：官方桌面端每条 chat 请求还带
/// `X-Request-ID`、`traceparent` / `b3` / `X-B3-*` / `X-Trace-ID`、`x-codebuddy-request`、
/// `X-Agent-Intent/Purpose`、`X-IDE-*`、`X-Private-Data`、`X-Product`。
/// 这些不是“越多越好”的装饰——它们是官方客户端本来就有的固定形态，缺了才像第三方。
///
/// 三组会话 ID 每请求生成一次，换号/退避重试复用同一组（上游按会话分桶，
/// 轮次换 ID 会被当新会话，见 9router#4944）。
pub struct IdentityHeaders {
    conv: [String; 3],
    request_id: String,
    trace_id: String,
    span_id: String,
    parent_span_id: String,
}

impl IdentityHeaders {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            conv: new_conversation_ids()?,
            request_id: new_uuid_v4()?,
            trace_id: new_hex(16)?,
            span_id: new_hex(8)?,
            parent_span_id: new_hex(8)?,
        })
    }

    /// 应用到出站头。INTL 暂只发结构头，品牌头（X-IDE-*/X-Private-Data）等端点复核后再加。
    pub fn apply(&self, h: &mut HeaderMap, region: crate::region::Region) -> Result<(), String> {
        let mut put = |k: &'static str, v: &str| -> Result<(), String> {
            let name = HeaderName::from_static(k);
            let val = HeaderValue::from_str(v)
                .map_err(|e| format!("身份头 {k} 非法: {e}"))?;
            h.insert(name, val);
            Ok(())
        };
        put("x-conversation-id", &self.conv[0])?;
        put("x-conversation-request-id", &self.conv[1])?;
        put("x-conversation-message-id", &self.conv[2])?;
        put("x-request-id", &self.request_id)?;
        put("x-requested-with", "XMLHttpRequest")?;
        put("x-agent-intent", "craft")?;
        put("x-agent-purpose", "conversation")?;
        put("x-codebuddy-request", "1")?;
        put("x-product", "SaaS")?;
        put(
            "traceparent",
            &format!("00-{}-{}-01", self.trace_id, self.span_id),
        )?;
        put(
            "b3",
            &format!(
                "{}-{}-1-{}",
                self.trace_id, self.span_id, self.parent_span_id
            ),
        )?;
        put("x-b3-traceid", &self.trace_id)?;
        put("x-b3-spanid", &self.span_id)?;
        put("x-b3-parentspanid", &self.parent_span_id)?;
        put("x-b3-sampled", "1")?;
        put("x-trace-id", &self.trace_id)?;
        if region == crate::region::Region::Cn {
            put("x-ide-type", "WorkBuddy")?;
            put("x-ide-name", "WorkBuddy")?;
            put("x-ide-version", "0.0.0")?;
            put("x-private-data", "true")?;
        }
        Ok(())
    }
}

/// Go `writeErr`：`Content-Type: application/json` + `{"error":{"message":…,"type":"upstream_error"}}`
/// + `json.Encoder` 的末尾换行。
pub fn write_err(status: u16, msg: &str) -> Response {
    let body = gjson::obj(vec![(
        "error",
        gjson::obj(vec![
            ("message", gjson::s(msg)),
            ("type", gjson::s("upstream_error")),
        ]),
    )]);
    let mut text = gjson::to_go(&body);
    text.push('\n');
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        )],
        text,
    )
        .into_response()
}

/// Go `http.Error` 形状（405 / 404 用）：`text/plain; charset=utf-8` + `nosniff` + 末尾换行。
pub fn http_error(status: u16, msg: &str) -> Response {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        h,
        format!("{}\n", msg),
    )
        .into_response()
}

/// 读请求体：**最多** `max` 字节，多余部分丢弃（Go `io.LimitReader` 的语义）。
///
/// 已知差异（登记在 README）：Go 在 `handleChat` 里写的是 `body, _ := io.ReadAll(...)`——
/// **丢弃**读体错误（含客户端半途断开），随后照常走 `json.Unmarshal`，大概率回
/// `bad json: unexpected end of JSON input`。本实现把流错误折叠成空体（等价于 Go 截断到
/// 已收字节），只在完全取不到字节时才回错误文案——两者最终都是 400 `bad json:` 族。
pub async fn read_capped(body: Body, max: usize) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let mut out: Vec<u8> = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            // Go 丢弃读体错误 → 保留已读到的字节继续（不中断请求）。
            Err(_) => break,
        };
        if out.len() >= max {
            break;
        }
        let remain = max - out.len();
        if chunk.len() <= remain {
            out.extend_from_slice(&chunk);
        } else {
            out.extend_from_slice(&chunk[..remain]);
            break;
        }
    }
    Ok(out)
}

/// Go `handleChat`。
pub async fn handle_chat(
    State(st): State<Arc<AppState>>,
    guard: Option<axum::extract::Extension<ConnClosed>>,
    req: axum::extract::Request,
) -> Response {
    let guard = guard.map(|axum::extract::Extension(g)| g);
    if req.method() != Method::POST {
        let mut resp = http_error(405, "method not allowed");
        resp.headers_mut()
            .insert(header::ALLOW, axum::http::HeaderValue::from_static("POST"));
        return resp;
    }
    let raw = match read_capped(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return write_err(400, &format!("bad json: {e}")),
    };
    let payload = match crate::godecode::unmarshal_map(&raw) {
        Ok(m) => m,
        Err(e) => return write_err(400, &format!("bad json: {e}")),
    };
    let msgs_empty = payload
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if msgs_empty {
        return write_err(400, "messages is required");
    }
    let model_name = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let model_name = if model_name.is_empty() {
        "auto".to_string()
    } else {
        model_name
    };
    let client_wants_stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut backend: Map<String, Value> = Map::new();
    for k in PASSTHROUGH_KEYS {
        if let Some(v) = payload.get(*k) {
            backend.insert(k.to_string(), v.clone());
        }
    }
    if !backend.contains_key("model") {
        backend.insert("model".to_string(), gjson::s("auto"));
    }
    backend.insert("stream".to_string(), Value::Bool(true));
    if !backend.contains_key("stream_options") {
        backend.insert(
            "stream_options".to_string(),
            gjson::obj(vec![("include_usage", Value::Bool(true))]),
        );
    }
    toolrefs::normalize_tool_choice(&mut backend);
    toolrefs::sanitize_tool_refs(&mut backend);
    if let Some(Value::Array(msgs)) = backend.get_mut("messages") {
        toolrefs::normalize_roles(msgs);
    }
    // 全角色脱敏（P5c；生产 CN/INTL 都开）：11128 是「内容指纹」拦截，指纹词出现在
    // user/assistant/tool 里同样触发（2026-09-12 实证：ZCode 会话历史里引用了报错原文与
    // Claude Code 模板句，64 条消息、零 system，每条请求都被拦）。零宽插入对模型阅读无影响；
    // 黑名单模板句的括号注解按 fingerprintRules 删除。位置与 Go 同：在 `$ref` 内联与
    // `developer`→`system` **之后**、INTL 补 system **之前**。
    if st.desensitize {
        let roles: Vec<String> = ["system", "developer", "user", "assistant", "tool"]
            .iter()
            .map(|r| r.to_string())
            .collect();
        backend = crate::desensitize::desensitize_body(&backend, &roles, true, true, true);
    }
    // INTL 特化：国际版 11128 为「首条消息必须是 system」的结构校验。
    if st.region.cfg().force_system {
        if let Some(Value::Array(msgs)) = backend.get("messages") {
            if !toolrefs::msgs_have_system(msgs) {
                let mut with_sys = vec![gjson::obj(vec![
                    ("role", gjson::s("system")),
                    ("content", gjson::s(crate::region::INTL_DEFAULT_SYSTEM)),
                ])];
                with_sys.extend(msgs.iter().cloned());
                backend.insert("messages".to_string(), Value::Array(with_sys));
                println!(
                    "[codebuddy] region={} 客户端未带 system，自动补最小 system",
                    st.region.as_str()
                );
            }
        }
    }

    // 账号池选号（双区各建各的池）：跳过冷却/停用/移除/凭据失效/额度耗尽的号、轮询推进；
    // 全不可用 → Pick 返回 None，沿用当前凭据原样请求（上游 429 将如实透传，不编造响应）。
    // 零值 Server（无池）行为不变。
    let mut acc_uid = String::new();
    let mut acc_path = String::new();
    if let Some(p) = st.pool.as_ref() {
        if let Some(a) = p.pick() {
            acc_uid = a.uid.clone();
            acc_path = a.auth_path.clone();
            p.touch(&a.uid);
            p.inc_load(&a.uid);
            println!(
                "[codebuddy] 账号池选号 uid={}({})",
                crate::pool::uid_short(&a.uid),
                a.nickname
            );
        }
    }
    // Go 的 `defer func() { if acc != nil { s.pool.DecLoad(acc.UID) } }()`：请求结束（含所有
    // 错误/换号路径）释放**最终**那个号的进行中计数。Rust 没有 defer，用 Drop 守卫等价实现：
    // * 换号分支照旧先 `dec_load` 旧号（Go 同款），并把守卫的 uid 换成新号；
    // * 流式响应把守卫**随流带走**——流结束（或被丢弃）时才释放，与 Go「handler 返回即释放」
    //   的时点一致（响应体写完才算完）。
    let mut load_guard = if acc_uid.is_empty() {
        None
    } else {
        Some(LoadRelease {
            st: st.clone(),
            uid: acc_uid.clone(),
        })
    };
    let mut hdr = match st.cred.headers_for(&acc_path) {
        Ok(h) => h,
        Err(e) => return write_err(502, &e),
    };
    // 官方网关身份头（2026-10-24 取 CN 5.8.3 app.asar `applyGatewayHeaders`）：
    // 三组 conversation ID + X-Request-ID + traceparent/b3 + x-codebuddy-request +
    // X-Agent-* + X-Product（CN 另加 X-IDE-*/X-Private-Data）。每请求生成一次，
    // 换号/退避重试复用同一组——上游按会话分桶，轮次换 ID 会被当新会话（9router#4944）。
    // `hdr` 只来自凭据层、从不含这些头，逐轮插入即等价于 GitHub 修复的 blank-only 填充。
    let identity = match IdentityHeaders::new() {
        Ok(v) => v,
        Err(e) => return write_err(502, &e),
    };

    // 限流窗口内 hy4-preview 请求体层改写 fallback（agent 零感知）；兜底关闭时不改写、不 trip。
    // hy4 兜底与缺省 reasoning 注入均为国内专属逻辑，INTL 一律跳过。
    let cfg = st.failover.config();
    let mut sent_model = model_name.clone();
    let limited = sent_model == crate::failover::HY4_PRIMARY
        && st.region == crate::region::Region::Cn
        && st.failover.hy4_limited();
    if crate::failover::failover_decide(&cfg, &sent_model, limited, 0, "").rewrite {
        backend.insert("model".to_string(), gjson::s(&cfg.fallback));
        sent_model = cfg.fallback.clone();
        println!(
            "[codebuddy] hy4-preview 限流中，本次切换 {}",
            cfg.fallback
        );
    }
    // 思考缺省档：客户端未带 reasoning_effort 时按配置注入（已带一律不动，透传优先）
    if st.region == crate::region::Region::Cn {
        if !backend.contains_key("reasoning_effort") {
            if let Some(eff) = crate::failover::reasoning_default_for(&sent_model, &cfg.default_reasoning)
            {
                backend.insert("reasoning_effort".to_string(), gjson::s(&eff));
                println!(
                    "[codebuddy] model={} 未带 reasoning_effort，注入缺省 {}",
                    sent_model, eff
                );
            }
        }
    }

    let mut account_resent = false; // 账号级换号重发只允许一次（免费池一般就 2 个号）
    let mut dead_resent = 0usize; // 凭据失效换号重发计数，上限 = 池大小
    let mut exhausted_resent = 0usize; // 额度耗尽换号重发计数，上限 = 池大小
    let mut transient_resent = 0usize; // 11133 原地重试计数，上限 TRANSIENT_MAX_RETRY
    let mut flagged_resent = 0usize; // 11128 按号封禁换号计数，上限 = 池大小（全封防循环）
    let mut stream_head_resent = 0usize; // 流内嗅探判错换号计数，上限 = 池大小（**不冷却账号**）
    let mut hy4_tripped = false; // hy4 模型级兜底重放只允许一次
    // Go `for attempt := 0; ; attempt++` 的**轮次**语义：先判后增，首轮 `attempt == 0`。
    // 这里的循环体在末尾用 `continue`（不是 `for` 的增量表达式），故把自增放在**轮首**，
    // 起点取 `usize::MAX` 让它首轮回绕成 0——与 Go 逐轮自增的取值序列完全一致。
    let mut attempt = usize::MAX;

    // 200 响应体（非流式 + 有池）在 loop 里就地把 Peek 做完：判定窗口与交给聚合的字节
    // **同源且不丢**（`ok_reader` 是本轮新建的读取器，peek 到的字节留在它里面，见 PEEK_WINDOW）。
    let mut ok_reader: Option<ChunkReader> = None;
    // 流式路径同理：嗅探用的读取器（peek 到的字节在 `pending` 里）直接交给下游，
    // 与 Go 把 `br` 回填 `resp.Body` 同义（一字不丢）。
    let mut stream_reader: Option<ChunkReader> = None;
    let (status, resp) = loop {
        // Go `for attempt := 0; ; attempt++`：attempt 是**轮次**，每轮自增（含 11133 退避重试、
        // 14018/401/429 换号重发），不是「hy4 兜底次数」。守卫 `(attempt == 0 || account_resent)`
        // 正是靠它拒绝「已经重试过之后的 hy4 trip」（Go `server.go:1247/1433`）。
        attempt = attempt.wrapping_add(1);
        let url = format!("{}/v2/chat/completions", st.base_url);
        let mut out_hdr = hdr.clone();
        if let Err(e) = identity.apply(&mut out_hdr, st.region) {
            return write_err(502, &e);
        }
        let req = st
            .http
            .post(&url)
            .headers(out_hdr)
            .body(gjson::to_go_bytes(&Value::Object(backend.clone())));
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                println!("[codebuddy] chat model={} err={}", sent_model, e);
                return write_err(502, &format!("upstream error: {}", e));
            }
        };
        if resp.status().as_u16() == 200 {
            // 200 体里的 14018（腾讯 chat 系额度耗尽常以 200 业务错体回来）：判定窗口是
            // Go 的 `br := bufio.NewReader(resp.Body); br.Peek(64*1024)`。
            // **不命中时 peek 到的字节必须一字不丢**——Go `3d4627f` 把 `br` 接回了
            // `resp.Body`（此前 4096 字节被丢弃，表现为下游 `content:null` + usage 全 0）。
            // 本实现的 `peek` 天然把已读字节留在 `ChunkReader::pending` 里，后续聚合照读。
            if !client_wants_stream && st.pool.is_some() && !acc_uid.is_empty() {
                let mut reader = ChunkReader::new(resp);
                let peeked = reader.peek(PEEK_WINDOW).await;
                if crate::failover::is_exhausted_body(&String::from_utf8_lossy(&peeked)) {
                    let p = st.pool.as_ref().expect("上面已判 Some");
                    p.mark_exhausted(&acc_uid);
                    if exhausted_resent < p.size() {
                        if let Some(next) = p.pick() {
                            let nhdr = match st.cred.headers_for(&next.auth_path) {
                                Ok(h) => h,
                                Err(e) => return write_err(502, &e),
                            };
                            println!(
                                "[codebuddy] 账号 {} 额度耗尽(14018@200)，换 {} 重发",
                                crate::pool::uid_short(&acc_uid),
                                crate::pool::uid_short(&next.uid)
                            );
                            p.dec_load(&acc_uid);
                            p.touch(&next.uid);
                            p.inc_load(&next.uid);
                            hdr = nhdr;
                            acc_uid = next.uid.clone();
                            acc_path = next.auth_path.clone();
                            if let Some(g) = load_guard.as_mut() {
                                g.uid = acc_uid.clone();
                            }
                            exhausted_resent += 1;
                            continue;
                        }
                    }
                    println!(
                        "[codebuddy] 全池额度耗尽（{} 个号），如实透传 14018",
                        p.size()
                    );
                    // Go 的注释直言这里回的是**固定错体**（原始 body 已被 Peek 过、重建太贵）。
                    return write_err(
                        200,
                        r#"{"code":14018,"msg":"Credits exhausted (all accounts in pool)"}"#,
                    );
                }
                ok_reader = Some(reader);
                break (200u16, None);
            }
            // 流式路径的流内嗅探：拿到 200、**发出 SSE 响应头之前** peek 一小段
            //（`STREAM_PEEK_WINDOW`），判上游是不是在 200 里塞了错误帧/空流。
            //
            // 关键区别（与 11128 账号级封禁治理相比）：这里**判错只换号，绝不冷却账号**
            //——不动 `mark_limited` / `mark_exhausted`。200 里的错误帧常是上游抖动或
            // 单号瞬时问题，按账号级封禁处理会级联耗竭整池（慢账号误判 → 全池冷却）。
            // 换号有界（`stream_head_resent < p.size()`）；到顶或 `pick()` 拿不到号
            // ⇒ 不 continue，落到下方把上游响应**如实透传**（peek 到的字节留在
            // `ChunkReader::pending` 里，一字不丢地交给下游）。
            if client_wants_stream && st.pool.is_some() && !acc_uid.is_empty() {
                let mut reader = ChunkReader::new(resp);
                // 带时间预算：思考型模型 prefill 静默期不会把首字节压住（见 STREAM_PEEK_BUDGET）。
                let (peeked, truncated) = reader
                    .peek_within(STREAM_PEEK_WINDOW, STREAM_PEEK_BUDGET)
                    .await;
                let verdict = crate::failover::classify_stream_head(
                    &String::from_utf8_lossy(&peeked),
                    truncated,
                );
                if verdict != crate::failover::StreamVerdict::Ok {
                    let p = st.pool.as_ref().expect("上面已判 Some");
                    let label = match verdict {
                        crate::failover::StreamVerdict::ErrCode(c) => format!("{c}@200"),
                        _ => "空流@200".to_string(),
                    };
                    if stream_head_resent < p.size() {
                        if let Some(next) = p.pick() {
                            let nhdr = match st.cred.headers_for(&next.auth_path) {
                                Ok(h) => h,
                                Err(e) => return write_err(502, &e),
                            };
                            println!(
                                "[codebuddy] 账号 {} 流内嗅探命中 {}，换 {} 重发（不冷却）",
                                crate::pool::uid_short(&acc_uid),
                                label,
                                crate::pool::uid_short(&next.uid)
                            );
                            p.dec_load(&acc_uid);
                            p.touch(&next.uid);
                            p.inc_load(&next.uid);
                            hdr = nhdr;
                            acc_uid = next.uid.clone();
                            acc_path = next.auth_path.clone();
                            if let Some(g) = load_guard.as_mut() {
                                g.uid = acc_uid.clone();
                            }
                            stream_head_resent += 1;
                            continue;
                        }
                    }
                    println!(
                        "[codebuddy] 全池 {} 个号流内嗅探均异常（{}），如实透传",
                        p.size(),
                        label
                    );
                }
                // 判定为 Ok，或已到换号上限：把 peek 过的读取器原样交给下游。
                stream_reader = Some(reader);
                break (200u16, None);
            }
            break (200u16, Some(resp));
        }
        let status = resp.status().as_u16();
        let err_body = read_err_body(resp).await;
        println!(
            "[codebuddy] chat model={} status={} err={}",
            sent_model,
            status,
            truncate(&err_body, 200)
        );
        // 11128 专项诊断（内容指纹拦截，固定前缀便于 grep）
        if err_body.contains("11128") {
            println!(
                "[codebuddy] 11128 为内容指纹拦截（第三方 Agent system 模板句/品牌词常见），对照 desensitize.go 指纹清洗表排查，body 预览: {}",
                truncate(&err_body, 200)
            );
        }
        // 11128 账号级封禁治理（2026-10-17，对齐 Go `0415ec1`）：国际版实证存在「按号
        // 封禁」——同一条干净请求在该号上必回 400/11128，换号即 200，与内容/模型无关。
        // 分两支：
        //   ① 内容嫌疑（messages/tools 命中指纹表，见 `desensitize::body_has_11128_trigger`）
        //      → 上游是按内容拦的，该号无过：原样透传，不冷却任何号；
        //   ② 干净请求 → 判账号级封禁：当前号记 `FLAGGED_COOLDOWN_MS`（1 小时）冷却
        //      （走既有 `mark_limited` 冷却机制，`pick` 自动跳过；到期自恢复 / GUI 提前
        //      恢复），换下一个可用号重发本次请求（客户端零感知）。
        // 换号有界：最多换池大小次（计数到顶或 `pick` 拿不到可用号即停，落到下方
        // `write_err` 如实透传上游报文），防恒 11128 下无限 continue。计数与 14018/401
        // 各自独立：封号不是额度耗尽也不是凭据失效。
        if is_flagged_body(&err_body) && st.pool.is_some() && !acc_uid.is_empty() {
            if err_body.contains("channel verification") {
                // 通道校验子类（2026-10-19 起 INTL 网关新增）：缺会话头时整池同错，
                // 与号无关、与内容无关——冷却任何号都是误伤，直接透传（不换号不冷却）。
                println!(
                    "[codebuddy] 11128 通道校验（缺会话头/非官方通道），不判账号、不冷却，原样透传，body 预览: {}",
                    truncate(&err_body, 200)
                );
            } else if crate::desensitize::body_has_11128_trigger(&backend) {
                println!(
                    "[codebuddy] 11128 命中内容指纹（{}），不判账号、不冷却，原样透传",
                    crate::pool::uid_short(&acc_uid)
                );
            } else {
                let p = st.pool.as_ref().expect("上面已判 Some");
                p.mark_limited(&acc_uid, flagged_until(crate::gotime::now_millis()));
                if flagged_resent < p.size() {
                    if let Some(next) = p.pick() {
                        let nhdr = match st.cred.headers_for(&next.auth_path) {
                            Ok(h) => h,
                            Err(e) => return write_err(502, &e),
                        };
                        println!(
                            "[codebuddy] 账号 {} 被封(11128)，冷却 {} 分钟后换 {} 重发",
                            crate::pool::uid_short(&acc_uid),
                            FLAGGED_COOLDOWN_MS / 60_000,
                            crate::pool::uid_short(&next.uid)
                        );
                        p.dec_load(&acc_uid); // 旧号释放进行中计数
                        p.touch(&next.uid);
                        p.inc_load(&next.uid);
                        hdr = nhdr;
                        acc_uid = next.uid.clone();
                        acc_path = next.auth_path.clone();
                        if let Some(g) = load_guard.as_mut() {
                            g.uid = acc_uid.clone();
                        }
                        flagged_resent += 1;
                        continue;
                    }
                }
                println!(
                    "[codebuddy] 全池 {} 个号 11128 封禁（冷却中），如实透传",
                    p.size()
                );
            }
        }

        // 11129 结构诊断（GPT-6 工具定义校验失败）：只记出站 tools 定义与 tool_choice +
        // 完整上游错误，不记 messages 正文、不记认证头。
        if err_body.contains("11129") || err_body.contains("invalid_function_parameters") {
            println!(
                "[codebuddy] 11129 工具定义校验失败 model={} tools={} err={}",
                sent_model,
                truncate(&crate::diag::summarize_tools_for_diag(&backend), 8000),
                truncate(&err_body, 4000)
            );
        }
        // 11133 现场取证：整份出站体落盘（上限 10 份、0600）+ 不含正文的可 grep 指纹。
        if is_transient_reject(&err_body) {
            println!(
                "[codebuddy] 11133 内容级拒绝 model={} 指纹={}",
                sent_model,
                crate::diag::request_fingerprint(&backend)
            );
            let p = crate::diag::dump_request_body(&backend, &st.diag_dir);
            if !p.is_empty() {
                println!("[codebuddy] 11133 现场已落盘：{}", p);
            }
            // 上游瞬时拒绝 → 原地退避重试（上限 2 次）。**退避期间客户端取消**：
            // Go 用 `select { <-time.After(backoff) ; <-r.Context().Done() }` 回
            // 499 "client closed request"，本实现对齐（wait_or_cancel）。
            if transient_resent < TRANSIENT_MAX_RETRY {
                transient_resent += 1;
                let backoff = Duration::from_millis(800 * transient_resent as u64);
                println!(
                    "[codebuddy] 11133 属上游抖动，第 {}/{} 次退避 {:?} 后重试",
                    transient_resent, TRANSIENT_MAX_RETRY, backoff
                );
                if !wait_or_cancelled(backoff, guard.as_ref()).await {
                    return write_err(499, "client closed request");
                }
                continue;
            }
            println!(
                "[codebuddy] 11133 连续 {} 次仍被拒，如实透传",
                TRANSIENT_MAX_RETRY
            );
        }
        // 账号额度耗尽轮换（上游 14018）：判定用业务码，状态码不限。
        if st.pool.is_some() && !acc_uid.is_empty() && crate::failover::is_exhausted_body(&err_body)
        {
            let p = st.pool.as_ref().expect("上面已判 Some");
            p.mark_exhausted(&acc_uid);
            if exhausted_resent < p.size() {
                if let Some(next) = p.pick() {
                    let nhdr = match st.cred.headers_for(&next.auth_path) {
                        Ok(h) => h,
                        Err(e) => return write_err(502, &e),
                    };
                    println!(
                        "[codebuddy] 账号 {} 额度耗尽(14018)，换 {} 重发",
                        crate::pool::uid_short(&acc_uid),
                        crate::pool::uid_short(&next.uid)
                    );
                    p.dec_load(&acc_uid);
                    p.touch(&next.uid);
                    p.inc_load(&next.uid);
                    hdr = nhdr;
                    acc_uid = next.uid.clone();
                    acc_path = next.auth_path.clone();
                    if let Some(g) = load_guard.as_mut() {
                        g.uid = acc_uid.clone();
                    }
                    exhausted_resent += 1;
                    continue;
                } else {
                    println!(
                        "[codebuddy] 全池额度耗尽（{} 个号全标耗尽），如实透传 14018",
                        p.size()
                    );
                }
            }
        }
        // 账号凭据失效轮换（401/403）：以上游 401 为准 → MarkDead 当前号，Pick 换下一个
        // 非死号、重建头后重发。换号有界：最多换池大小次；Pick 返回 None（全死）→ 不 continue，
        // 落到下方 writeErr 把 401/403 如实透传。
        if (status == 401 || status == 403) && st.pool.is_some() && !acc_uid.is_empty() {
            let p = st.pool.as_ref().expect("上面已判 Some");
            p.mark_dead(&acc_uid);
            if dead_resent < p.size() {
                if let Some(next) = p.pick() {
                    let nhdr = match st.cred.headers_for(&next.auth_path) {
                        Ok(h) => h,
                        Err(e) => return write_err(502, &e),
                    };
                    println!(
                        "[codebuddy] 账号 {} 凭据失效(401)，换 {} 重发",
                        crate::pool::uid_short(&acc_uid),
                        crate::pool::uid_short(&next.uid)
                    );
                    p.dec_load(&acc_uid);
                    p.touch(&next.uid);
                    p.inc_load(&next.uid);
                    hdr = nhdr;
                    acc_uid = next.uid.clone();
                    acc_path = next.auth_path.clone();
                    if let Some(g) = load_guard.as_mut() {
                        g.uid = acc_uid.clone();
                    }
                    dead_resent += 1;
                    continue;
                }
            }
        }
        // 账号级轮换：429 且报文是配额/频控族 → 当前号记冷却，同请求内换下一个可用号
        // 重发一次（最多 1 次；重发再 429 落到下方 hy4 兜底或如实透传，不循环）。
        if status == 429 && st.pool.is_some() && !acc_uid.is_empty() {
            if let Some(until) = crate::failover::parse_quota_reset(&err_body) {
                let p = st.pool.as_ref().expect("上面已判 Some");
                p.mark_limited(&acc_uid, until);
                if !account_resent {
                    if let Some(next) = p.pick() {
                        let nhdr = match st.cred.headers_for(&next.auth_path) {
                            Ok(h) => h,
                            Err(e) => return write_err(502, &e),
                        };
                        println!(
                            "[codebuddy] 账号 {} 限流至 {}，换号 {}({}) 重发本次请求",
                            crate::pool::uid_short(&acc_uid),
                            crate::failover::format_reset_display(until),
                            crate::pool::uid_short(&next.uid),
                            next.nickname
                        );
                        p.dec_load(&acc_uid);
                        p.touch(&next.uid);
                        p.inc_load(&next.uid);
                        hdr = nhdr;
                        acc_uid = next.uid.clone();
                        acc_path = next.auth_path.clone();
                        if let Some(g) = load_guard.as_mut() {
                            g.uid = acc_uid.clone();
                        }
                        account_resent = true;
                        continue;
                    }
                }
            }
        }
        // hy4-preview 撞配额 429：兜底开启时记限流窗口并立刻用 fallback 重放本次请求
        // （最多 1 次）。双保险防死循环：① hy4Tripped；② fallback 自指时拒绝 trip。
        // 守卫的 `attempt == 0` 是「只有**首轮**才允许 trip」——`attempt` 在每轮开头自增，
        // 故经 11133/14018/401 重试后的 429 一律被拒（与 Go 同口径）。
        if !hy4_tripped
            && cfg.fallback != crate::failover::HY4_PRIMARY
            && (attempt == 0 || account_resent)
            && status == 429
            && st.region == crate::region::Region::Cn
        {
            let d = crate::failover::failover_decide(&cfg, &sent_model, false, status, &err_body);
            if d.trip {
                st.failover.hy4_trip(d.until_ms);
                backend.insert("model".to_string(), gjson::s(&cfg.fallback));
                sent_model = cfg.fallback.clone();
                hy4_tripped = true;
                println!(
                    "[codebuddy] hy4-preview 限流至 {}，切换 {} 并重放本次请求",
                    crate::failover::format_reset_display_minute(d.until_ms),
                    cfg.fallback
                );
                continue;
            }
        }
        return write_err(status, &err_body);
    };
    let _ = acc_path;

    // —— 下游（P5c 已等价） ——
    if client_wants_stream {
        // 流式：SSE 逐行清洗后转发——CodeBuddy 后端每个 delta 同时携带 content 与
        // reasoning_content 两个键（未活动侧为空字符串），ZCode 的流解析按字段出现切分
        // part，双键并存会把一次思考阶段撕成几十个「思考」条目（团结上游只发活动侧键，
        // 无此现象）。剔掉空字符串键即对齐团结流形；同时行扫描 data: 行提取 usage 入账。
        // 有池时响应来自 `stream_reader`（已 peek 过，`pending` 里留着嗅探用的字节）。
        let reader = match stream_reader {
            Some(r) => r,
            None => match resp {
                Some(r) => ChunkReader::new(r),
                None => return write_err(500, "internal: streaming response missing"),
            },
        };
        let mut out = Response::new(Body::from_stream(clean_and_scan_stream(
            reader,
            sent_model.clone(),
            st.stats.clone(),
            // 守卫随流带走：流结束（或被丢弃）时才 DecLoad，等价 Go 的 defer 时点。
            load_guard.take(),
        )));
        let h = out.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        h.insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        );
        h.insert(
            axum::http::HeaderName::from_static("x-accel-buffering"),
            axum::http::HeaderValue::from_static("no"),
        );
        return out;
    }

    // 非流式：聚合后端 SSE 为单个 chat.completion（usage 顺带入账）。
    let reader = match resp {
        Some(r) => ChunkReader::new(r),
        None => match ok_reader {
            Some(r) => r,
            None => return write_err(500, "internal: aggregated response missing"),
        },
    };
    let _ = status;
    let stream = reader.into_rest_stream();
    let started = std::time::Instant::now();
    let result = crate::collect::collect_stream(stream, &sent_model).await;
    println!(
        "[codebuddy] chat model={} status=200 dur={:?} finish={}",
        sent_model,
        started.elapsed().as_millis(),
        result
            .get("finish_reason_debug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    );
    if let Some(usage) = result.get("usage").and_then(|u| u.as_object()) {
        st.stats.add_stat(
            &sent_model,
            to_int(usage.get("prompt_tokens")),
            to_int(usage.get("completion_tokens")),
            to_int(usage.get("total_tokens")),
        );
    }
    crate::models::json_response(result)
}

/// Go `toInt`：宽松取数（JSON 数字是 float64）。
pub fn to_int(v: Option<&Value>) -> i64 {
    match v.and_then(|x| x.as_f64()) {
        Some(f) => f as i64,
        None => 0,
    }
}

/// 上游响应体的分块读取器：把 `reqwest::Response` 的字节流包装成「按需取字节」，
/// 并且能把「上一次取多了的余量」留在 `pending` 里（Go `bufio.Reader.fill` 只按剩余
/// 缓冲空间读，从不越读——这里等价：`take(n)` 至多消费 n 字节）。
pub struct ChunkReader {
    stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
    >,
    pending: Bytes,
    eof: bool,
}

impl ChunkReader {
    pub fn new(resp: reqwest::Response) -> ChunkReader {
        ChunkReader::from_stream(Box::pin(resp.bytes_stream().map_err(std::io::Error::other)))
    }

    /// 从任意字节流构造（reqwest 之外只给单测用：用来复查 `peek` 不丢字节）。
    pub fn from_stream(
        stream: std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
        >,
    ) -> ChunkReader {
        ChunkReader {
            stream,
            pending: Bytes::new(),
            eof: false,
        }
    }

    /// 取下一个上游分块：`pending` 里的缓冲优先，空了才读上游。
    ///
    /// ⚠ **只在 `pending` 为空时才真正读源**（drain 语义）——`peek` 必须走
    /// `read_source`，否则会把同一个 `pending` 反复读回来（空转，实测过）。
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        if !self.pending.is_empty() {
            return Some(Ok(std::mem::take(&mut self.pending)));
        }
        self.read_source().await
    }

    /// 直接向上游读一个分块（不看 `pending`）。`eof` 后恒 `None`。
    async fn read_source(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        if self.eof {
            return None;
        }
        match self.stream.next().await {
            Some(Ok(b)) => Some(Ok(b)),
            Some(Err(e)) => {
                self.eof = true;
                Some(Err(e))
            }
            None => {
                self.eof = true;
                None
            }
        }
    }

    /// 把上游**读进缓冲**至多 `max` 字节并返回缓冲里的这部分（Go `bufio.Reader.Peek` 的语义）：
    /// 字节**留在 `pending` 里**，后续 `next_chunk` / `into_rest_stream` 会照发，一个都不丢
    /// （Go `3d4627f` 之后 `br` 回填 `resp.Body` 与此等价）。返回值只用于 14018 判定，
    /// **不是**消费。注意：这里**直接读源**（不经 `next_chunk`，否则 `pending` 会被自读空转）。
    pub async fn peek(&mut self, max: usize) -> Vec<u8> {
        while self.pending.len() < max {
            match self.read_source().await {
                Some(Ok(b)) => {
                    let mut merged = Vec::with_capacity(self.pending.len() + b.len());
                    merged.extend_from_slice(&self.pending);
                    merged.extend_from_slice(&b);
                    self.pending = Bytes::from(merged);
                }
                Some(Err(_)) | None => break,
            }
        }
        self.pending[..self.pending.len().min(max)].to_vec()
    }

    /// 带**时间上限**的 peek：给流式路径用。返回 `(已攒字节, 是否被截断)`。
    ///
    /// 为什么需要它：`peek` 的循环条件是「攒满 `max` 或流结束」，而上游在思考型模型的
    /// prefill 阶段可能静默几十秒——那样首字节会被压到 prefill 结束才吐，比「不嗅探」
    /// 还差。这里在 `budget` 到点时**用已攒到的字节就下判断**（可能不足 `max`），
    /// 攒到的字节仍然留在 `pending` 里、后续原样交给下游，一字不丢。
    ///
    /// **健康流不付这份时间**：只要第一块数据到了就立刻返回（首块足以判错误帧/`data:`
    /// 形态），预算只在「上游完全没吐字节」时才真正生效。这样正常请求的首字节延迟
    /// 与不嗅探时一致，只有真正可疑的慢上游才等到预算到点。
    ///
    /// 返回值第二项 `truncated`：head 是否**没读全**（预算到点 / 窗口未满）。
    /// `classify_stream_head` 据此避免把半截 `"dat"` 误判成「上游没发 SSE」。
    pub async fn peek_within(&mut self, max: usize, budget: Duration) -> (Vec<u8>, bool) {
        let deadline = tokio::time::Instant::now() + budget;
        let mut truncated = true;
        loop {
            if self.pending.len() >= max {
                truncated = false;
                break;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, self.read_source()).await {
                Ok(Some(Ok(b))) => {
                    let mut merged = Vec::with_capacity(self.pending.len() + b.len());
                    merged.extend_from_slice(&self.pending);
                    merged.extend_from_slice(&b);
                    self.pending = Bytes::from(merged);
                    // 拿到第一块就够判了：错误帧/JSON 错体/HTML 页都是首块即见，
                    // 正常 SSE 的首块也足以看到 `data:`。不再等满窗口/预算。
                    truncated = false;
                    break;
                }
                // 流结束 / 读错误：读全了（EOF 不是截断）。
                Ok(Some(Err(_))) | Ok(None) => {
                    truncated = false;
                    break;
                }
                // 预算到点：用已攒到的字节下判断，并标记「可能没读全」。
                Err(_) => break,
            }
        }
        (
            self.pending[..self.pending.len().min(max)].to_vec(),
            truncated,
        )
    }

    /// 把「剩下的全部」包成聚合用的流（`pending` 先发，然后透传上游）。
    pub fn into_rest_stream(
        self,
    ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>>
    {
        let pending = self.pending;
        let stream = self.stream;
        let head = futures_util::stream::iter(if pending.is_empty() {
            vec![]
        } else {
            vec![Ok(pending)]
        });
        Box::pin(head.chain(stream))
    }
}

/// 流式下游：清洗 + usage 入账（逐块 yield，与 Go 的「读一块→写一块→Flush」同形）。
fn clean_and_scan_stream(
    reader: ChunkReader,
    model: String,
    stats: Arc<crate::stats::Stats>,
    release: Option<LoadRelease>,
) -> std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
> {
    let cleaner = SseFieldCleaner::new();
    let scan = UsageScanner::new(&model, stats);
    Box::pin(futures_util::stream::unfold(
        (reader, cleaner, scan, false, release),
        |(mut reader, mut cleaner, mut scan, done, release)| async move {
            if done {
                return None;
            }
            loop {
                match reader.next_chunk().await {
                    Some(Ok(chunk)) => {
                        scan.feed(&chunk);
                        let out = cleaner.feed(&chunk);
                        if !out.is_empty() {
                            return Some((
                                Ok::<Bytes, std::io::Error>(Bytes::from(out)),
                                (reader, cleaner, scan, false, release),
                            ));
                        }
                    }
                    Some(Err(_)) | None => {
                        // Go 在 `rerr != nil` 时先冲尾再 return。
                        let out = cleaner.flush();
                        if !out.is_empty() {
                            return Some((
                                Ok(Bytes::from(out)),
                                (reader, cleaner, scan, true, release),
                            ));
                        }
                        // 流结束 → `release`（LoadRelease）在此 Drop，等价 Go 的 defer DecLoad。
                        drop(release);
                        return None;
                    }
                }
            }
        },
    ))
}

/// Go `handleChat` 里那个 `defer func() { if acc != nil { s.pool.DecLoad(acc.UID) } }()`。
///
/// 为什么用 Drop 而不是在每个 return 前手写 `dec_load`：handler 有十几条退出路径
/// （502/499/各换号分支/正常结束），手写必漏——**P5c 的行为级接线测试当场抓到过这个漏**：
/// 原先只在换号分支里 `dec_load`，正常路径一个都没释放，`inflight` 永久 +1。
pub struct LoadRelease {
    st: Arc<AppState>,
    uid: String,
}

impl Drop for LoadRelease {
    fn drop(&mut self) {
        if let Some(p) = self.st.pool.as_ref() {
            p.dec_load(&self.uid);
        }
    }
}

/// Go 的 `select { case <-time.After(d): ; case <-r.Context().Done(): 499 }`。
///
/// Go 的 `r.Context()` 是**请求级** context：HTTP/1.1 上客户端断开连接时立刻 Done，
/// 所以退避等待期间就能中断并回 `499 "client closed request"`（文案与 Go 逐字一致）。
/// 本实现的同一信号由 `conn::ConnClosed` 提供（自定义 `Listener` 在 `poll_read` 读到
/// EOF/错误时点亮，经 `ConnectInfo` 塞进本请求的扩展）——见 `src/conn.rs`。
async fn wait_or_cancelled(d: Duration, guard: Option<&ConnClosed>) -> bool {
    match guard {
        Some(g) => tokio::select! {
            _ = tokio::time::sleep(d) => true,
            _ = g.closed() => false,
        },
        // 没有连接信息的调用路径（理论上只有非生产装配）：退化成纯等待。
        None => {
            tokio::time::sleep(d).await;
            true
        }
    }
}

async fn read_err_body(resp: reqwest::Response) -> String {
    use futures_util::StreamExt;
    let mut out: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        if out.len() >= MAX_ERR_BODY {
            break;
        }
        let remain = MAX_ERR_BODY - out.len();
        if chunk.len() <= remain {
            out.extend_from_slice(&chunk);
        } else {
            out.extend_from_slice(&chunk[..remain]);
            break;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Go `truncate(s, n)`：超长截断并补 `...`。
pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `3d4627f` 的 Rust 侧等价物：peek 到的字节**一个都不能丢**，必须原样交给下游聚合。
    /// 反向验证：把 `peek` 改回「读进 out 且清空 `pending`」⇒ 本用例判红（下游只收到 40 字节）。
    #[tokio::test]
    async fn peek_keeps_every_byte_for_the_aggregation() {
        let parts: Vec<&[u8]> = vec![b"data: {\"choices\"", b":[]}\n\ndata: [DONE]\n\n"];
        let stream = futures_util::stream::iter(
            parts
                .iter()
                .map(|p| Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(p)))
                .collect::<Vec<_>>(),
        );
        let mut reader = ChunkReader::from_stream(Box::pin(stream));
        let peeked = reader.peek(64 * 1024).await;
        assert_eq!(
            peeked,
            parts.concat(),
            "peek 窗口应看到上游全量（{} 字节）",
            parts.concat().len()
        );
        assert!(!crate::failover::is_exhausted_body(&String::from_utf8_lossy(&peeked)));
        // peek 之后下游必须仍能读回**完整**上游体
        let mut got: Vec<u8> = Vec::new();
        while let Some(chunk) = reader.next_chunk().await {
            got.extend_from_slice(&chunk.expect("上游流不该报错"));
        }
        assert_eq!(
            got,
            parts.concat(),
            "peek 之后的字节必须与上游逐字节相同（丢头就是 Go 修复前那个缺陷）"
        );
    }

    /// `peek_within` 的**时间预算**语义：上游静默时按已攒字节返回，不把首字节压住；
    /// 且已攒到的字节仍原样留给下游（一字不丢）。
    ///
    /// 反向验证：把 `peek_within` 换成无超时的 `peek`（即死等攒满/EOF）⇒ 本用例会卡在
    /// 600ms 的 deadline 上判红（超时 panic）。
    #[tokio::test]
    async fn peek_within_returns_on_budget_and_keeps_bytes() {
        // 一个「先吐 3 字节，然后永久静默」的流：模拟思考型模型的 prefill 静默期。
        let head = futures_util::stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from_static(
            b"dat",
        ))]);
        let stall = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let mut reader = ChunkReader::from_stream(Box::pin(head.chain(stall)));

        let t0 = std::time::Instant::now();
        let (peeked, truncated) = tokio::time::timeout(
            Duration::from_millis(600),
            reader.peek_within(STREAM_PEEK_WINDOW, Duration::from_millis(60)),
        )
        .await
        .expect("peek_within 必须在预算内返回（不许死等攒满 8KB）");
        let elapsed = t0.elapsed();

        assert_eq!(peeked, b"dat", "预算到点应返回已攒到的字节");
        assert!(
            elapsed < Duration::from_millis(500),
            "应在预算附近返回（实耗 {elapsed:?}），不许等满 8KB"
        );
        // 拿到第一块就返回 ⇒ 这 3 字节已读全，不算截断
        assert!(!truncated, "拿到首块即返回，不算截断");
        // 已攒字节必须仍留在 pending 里，下游能原样读回
        let mut got: Vec<u8> = Vec::new();
        if let Some(Ok(b)) = reader.next_chunk().await {
            got.extend_from_slice(&b);
        }
        assert_eq!(got, b"dat", "peek_within 之后字节必须原样交给下游");
    }

    /// **健康流不付预算代价**：上游一吐字节就返回，不等满预算。
    /// 反向验证：去掉「拿到首块就 break」⇒ 本用例实耗会撞到 300ms 预算并判红。
    #[tokio::test]
    async fn peek_within_returns_immediately_when_data_arrives() {
        let first = futures_util::stream::iter(vec![Ok::<Bytes, std::io::Error>(
            Bytes::from_static(b"data: {\"choices\":[]}\n\n"),
        )]);
        let stall = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
        let mut reader = ChunkReader::from_stream(Box::pin(first.chain(stall)));

        let t0 = std::time::Instant::now();
        let (peeked, _truncated) = reader
            .peek_within(STREAM_PEEK_WINDOW, Duration::from_millis(300))
            .await;
        let elapsed = t0.elapsed();

        assert!(!peeked.is_empty(), "应拿到首块内容");
        assert!(
            elapsed < Duration::from_millis(120),
            "健康流拿到首块必须立刻返回，不许等满 300ms 预算（实耗 {elapsed:?}）"
        );
    }

    /// 预算截断时，半截 `"dat"` 不能被判成「上游没发 SSE」（否则误换号）。
    #[test]
    fn truncated_head_does_not_become_empty_stream() {
        use crate::failover::{classify_stream_head, StreamVerdict};
        // 没读全：半截前缀 → 按 Ok 透传
        assert_eq!(
            classify_stream_head("dat", true),
            StreamVerdict::Ok,
            "截断的半截 data: 前缀不能判空流"
        );
        // 读全了但没有 data: 行 → 才是空流
        assert_eq!(
            classify_stream_head("dat", false),
            StreamVerdict::EmptyStream,
            "读全后仍无 data: 行才算空流"
        );
        // 明确的业务码即使截断也要判出来（错误帧首块即见）
        assert_eq!(
            classify_stream_head(r#"{"code":11128,"msg":"x"}"#, true),
            StreamVerdict::ErrCode(11128),
            "截断也要能判出业务码"
        );
    }

    /// 常量本身必须落在「够快」的量级：`peek_within_returns_on_budget_and_keeps_bytes`
    /// 用的是自己传入的 60ms，测不到这个常量的取值——这里单独锁一次，
    /// 防止有人把它改成秒级（那等于把首字节压到 prefill 结束）。
    #[test]
    fn stream_peek_budget_stays_subsecond() {
        assert!(
            STREAM_PEEK_BUDGET <= Duration::from_millis(500),
            "流内嗅探预算必须小于 500ms（实得 {STREAM_PEEK_BUDGET:?}）；\
             改到秒级会把思考型模型的首字节压到 prefill 结束"
        );
        assert!(
            !STREAM_PEEK_BUDGET.is_zero(),
            "预算为 0 等于不嗅探（错误帧会被直接透传给下游）"
        );
        assert!(
            STREAM_PEEK_WINDOW <= 64 * 1024,
            "流式嗅探窗口不该超过非流式的 64KB（实得 {STREAM_PEEK_WINDOW}）"
        );
    }

    #[test]
    fn transient_reject_matches_go_regex() {
        assert!(is_transient_reject(r#"{"code":11133}"#));
        assert!(is_transient_reject(r#"{"code" : 11133 ,"msg":"x"}"#));
        assert!(is_transient_reject("prefix\n  \"code\":\n\t11133"));
        // 词边界：111330 / 11133x 不算（Go 的 `\b`）
        assert!(!is_transient_reject(r#"{"code":111330}"#));
        assert!(!is_transient_reject(r#"{"code":"11133x"}"#));
        assert!(!is_transient_reject(r#"{"code":11132}"#));
        assert!(!is_transient_reject(""));
        // 非字段级（裸子串）不算
        assert!(!is_transient_reject("error 11133 happened"));
    }

    /// Go `flaggedBodyRe`（`"code"\s*:\s*"?11128"?\b`）的等价判定：数字/字符串两种 code
    /// 形态都认，裸子串与 `111280`/`"11128x"` 不认。
    #[test]
    fn flagged_body_matches_go_regex() {
        assert!(is_flagged_body(
            r#"{"error":{"data":{"code":11128,"msg":"Illegal API invocation from an unapproved channel","requestId":"x"}}}"#
        ));
        assert!(is_flagged_body(r#"{"code" : 11128 ,"msg":"x"}"#));
        assert!(is_flagged_body(r#"{"code":"11128"}"#));
        assert!(is_flagged_body("prefix\n  \"code\":\n\t11128"));
        // 词边界：尾随字符仍是词字符时不算（Go 的 `\b`）
        assert!(!is_flagged_body(r#"{"code":111280}"#));
        assert!(!is_flagged_body(r#"{"code":"11128x"}"#));
        assert!(!is_flagged_body(r#"{"code":11129}"#));
        assert!(!is_flagged_body(""));
        // 非字段级（requestId 里同数字串、正文提及）不算
        assert!(!is_flagged_body(r#"{"requestId":"11128"}"#));
        assert!(!is_flagged_body("error 11128 happened"));
        // 冷却时长与 Go `flaggedCooldown = time.Hour` 同值
        assert_eq!(FLAGGED_COOLDOWN_MS, 3_600_000);
        assert_eq!(flagged_until(1_000), 1_000 + 3_600_000);
        assert_eq!(flagged_until(i64::MAX), i64::MAX, "溢出饱和而非回绕");
    }

    /// 身份头形态守门：值必须是官方客户端同款（UUIDv4 / 32hex trace / traceparent 00-…-01），
    /// 且 CN 带品牌头、INTL 暂不带。防止后来者“顺手删掉”或把三个 ID 写成同一个。
    #[test]
    fn identity_headers_match_official_gateway_shape() {
        let id = IdentityHeaders::new().expect("随机源应可用");
        let mut h = HeaderMap::new();
        id.apply(&mut h, crate::region::Region::Cn).unwrap();
        for k in [
            "x-conversation-id",
            "x-conversation-request-id",
            "x-conversation-message-id",
            "x-request-id",
        ] {
            let v = h
                .get(k)
                .unwrap_or_else(|| panic!("{k} 缺失"))
                .to_str()
                .unwrap();
            assert_eq!(v.len(), 36, "{k}={v} 长度不对");
            assert_eq!(v.chars().nth(14), Some('4'), "{k}={v} 不是 UUIDv4");
            let variant = v.chars().nth(19).unwrap();
            assert!(
                matches!(variant, '8' | '9' | 'a' | 'b'),
                "{k}={v} variant 不对"
            );
        }
        let trace = h.get("x-trace-id").unwrap().to_str().unwrap();
        assert_eq!(trace.len(), 32);
        assert!(trace.chars().all(|c| c.is_ascii_hexdigit()));
        let tp = h.get("traceparent").unwrap().to_str().unwrap();
        assert!(tp.starts_with("00-") && tp.ends_with("-01"), "traceparent={tp}");
        assert_eq!(h.get("x-ide-type").unwrap(), "WorkBuddy");
        assert_eq!(h.get("x-product").unwrap(), "SaaS");
        assert_eq!(h.get("x-codebuddy-request").unwrap(), "1");

        let mut hi = HeaderMap::new();
        id.apply(&mut hi, crate::region::Region::Intl).unwrap();
        assert!(hi.get("x-ide-type").is_none(), "INTL 暂不发品牌头");
        assert!(hi.get("x-trace-id").is_some());
    }
}
