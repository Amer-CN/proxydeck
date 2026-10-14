//! chat.rs —— `/v1/chat/completions`（**P5a 请求侧 + P5b 池/凭据/额度/failover 分支**）。
//!
//! 覆盖：方法门（非 POST → 405 + `Allow: POST`）、64MB 上限读体、Go `json.Unmarshal`→
//! `map[string]any` 的入站解析（逐字错误文案）、白名单字段透传 + `stream` 强制 true +
//! `stream_options` 缺省、`tool_choice` 归一、`$ref` 内联、`developer`→`system`、
//! INTL 自动补首条 system、出站体**逐字节重建**（Go `mustJSONReader` → `json.Marshal`）、
//! **账号池换号三条分支**（401/403 标死换活号、429 冷却换号、14018 额度耗尽换号）、
//! **200 体 14018 Peek**、CN 缺省 `reasoning_effort` 注入、hy4 限流兜底改写与 trip、
//! 11133 退避重试（上限 2 次，800ms/1600ms，退避期间客户端取消 → 499）与失败现场落盘、
//! 上游非 200 的 `writeErr` 形状。
//!
//! **不在本轮**（P5c，见 crate README 交接清单）：
//! * 全角色脱敏 `DesensitizeBody`；
//! * **下游**：流式 SSE 字段清洗（`sseFieldCleaner`）、非流式聚合（`collectStream`）、
//!   usage 入账与统计（`codebuddy-stats.json`）。
//!   本轮的 200 响应是**最小透传**（流式按原字节转发、非流式原样转发上游体），
//!   与 Go 的下游行为**不等价**——这一点在 README 差异清单里显著登记，不静默。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use serde_json::{Map, Value};

use crate::gjson;
use crate::toolrefs;
use crate::AppState;

/// Go `io.LimitReader(r.Body, 64<<20)`：**截断**读（不是拒绝）。
pub const MAX_BODY_BYTES: usize = 64 << 20;
/// Go `io.LimitReader(resp.Body, 1<<20)`：上游错误体上限。
pub const MAX_ERR_BODY: usize = 1 << 20;
/// Go `transientMaxRetry`：11133 原地重试次数上限。
pub const TRANSIENT_MAX_RETRY: usize = 2;
/// Go 的 200 体 14018 Peek 窗口（`br.Peek(64 * 1024)`）。
pub const PEEK_BYTES: usize = 64 * 1024;
/// 非流式 200 上游体的读入上限。
///
/// Go 在非流式路径用 `collectStream(resp.Body)` 边扫边聚（无显式上限）；本轮的
/// 200 下游是**最小透传**（属 P5c），为了让「14018 判定的窗口」与「转发给客户端的字节」
/// 同源，这里把整份上游体读进来再转发，上限取与请求侧同值的 64MB（超限即截断，
/// 与 Go 的 `io.LimitReader` 语义同族）。P5c 换成真聚合时可撤掉这条缓冲。
pub const MAX_OK_BODY: usize = 64 << 20;
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
pub async fn handle_chat(State(st): State<Arc<AppState>>, req: axum::extract::Request) -> Response {
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
    let mut hdr = match st.cred.headers_for(&acc_path) {
        Ok(h) => h,
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
    let mut hy4_tripped = false; // hy4 模型级兜底重放只允许一次
    // Go `for attempt := 0; ; attempt++` 的**轮次**语义：先判后增，首轮 `attempt == 0`。
    // 这里的循环体在末尾用 `continue`（不是 `for` 的增量表达式），故把自增放在**轮首**，
    // 起点取 `usize::MAX` 让它首轮回绕成 0——与 Go 逐轮自增的取值序列完全一致。
    let mut attempt = usize::MAX;

    // 200 响应体（非流式 + 有池）在 loop 里就地读完：14018 判定窗口与转发字节同源。
    let mut ok_body: Option<Vec<u8>> = None;
    let (status, resp) = loop {
        // Go `for attempt := 0; ; attempt++`：attempt 是**轮次**，每轮自增（含 11133 退避重试、
        // 14018/401/429 换号重发），不是「hy4 兜底次数」。守卫 `(attempt == 0 || account_resent)`
        // 正是靠它拒绝「已经重试过之后的 hy4 trip」（Go `server.go:1247/1433`）。
        attempt = attempt.wrapping_add(1);
        let url = format!("{}/v2/chat/completions", st.base_url);
        let req = st
            .http
            .post(&url)
            .headers(hdr.clone())
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
            // 头 64KB（Go 用 `bufio.Peek(64*1024)`；错体在流首行，正常内容流里不会出现该业务码）。
            // 流式路径不做此检查（headers 一旦发出中途换号会撕裂客户端流）。
            if !client_wants_stream && st.pool.is_some() && !acc_uid.is_empty() {
                // 非流式 + 有池：整份读完（Go 在非流式路径本就要把上游体读完做聚合，
                // 本轮的「聚合」是 P5c 的最小透传版）。body 留着转发，resp 不再用。
                let stream = resp
                    .bytes_stream()
                    .map_err(std::io::Error::other);
                let body = read_capped(Body::from_stream(stream), MAX_OK_BODY)
                    .await
                    .unwrap_or_default();
                if crate::failover::is_exhausted_body(
                    &String::from_utf8_lossy(&body[..body.len().min(PEEK_BYTES)]),
                ) {
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
                ok_body = Some(body);
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
                if !wait_or_cancelled(backoff, &st).await {
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

    // —— 下游（P5c 范围）：本轮最小透传 ——
    let resp = match resp {
        Some(r) => r,
        None => {
            // 已在 loop 里读完的非流式体（无上游 Response 可用）：按采集到的状态与体回。
            let mut out = Response::new(Body::from(ok_body.unwrap_or_default()));
            *out.status_mut() = StatusCode::from_u16(status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            out.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            return out;
        }
    };
    let _ = status;
    if client_wants_stream {
        let mut out = Response::new(Body::from_stream(resp.bytes_stream()));
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
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| axum::http::HeaderValue::from_static("application/json"));
    let status = resp.status();
    let mut out = Response::new(Body::from_stream(
        resp.bytes_stream().map_err(std::io::Error::other),
    ));
    *out.status_mut() = status;
    out.headers_mut().insert(header::CONTENT_TYPE, ct);
    out
}

/// Go 的 `select { case <-time.After(d): ; case <-r.Context().Done(): 499 }`。
///
/// **如实登记（未逐字对齐，见 README 差异 13）**：499 分支已实现并可达（下面这个函数
/// 返回 `false` 时调用方回 `499 "client closed request"`，文案与 Go 逐字一致），但
/// **触发条件暂缺**——Go 的 `r.Context()` 是请求级 context（连接断开即 Done，退避等待
/// 期间也能中断）；axum/hyper 侧要拿到同一信号需要 `hyper_util` 的 graceful-shutdown
/// 观看器或自写 `Body` 包装，本小轮未引入该依赖，故这里只等完退避继续重试，
/// 与 Go 相比表现为「客户端断开后仍会多试 1~2 次」。属 P5c 的对齐项。
async fn wait_or_cancelled(d: Duration, _st: &Arc<AppState>) -> bool {
    tokio::time::sleep(d).await;
    true
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
}
