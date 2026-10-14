//! chat.rs —— `/v1/chat/completions` 的**请求侧**（P5a 范围）。
//!
//! 覆盖：方法门（非 POST → 405 + `Allow: POST`）、64MB 上限读体、Go `json.Unmarshal`→
//! `map[string]any` 的入站解析（逐字错误文案）、白名单字段透传 + `stream` 强制 true +
//! `stream_options` 缺省、`tool_choice` 归一、`$ref` 内联、`developer`→`system`、
//! INTL 自动补首条 system、出站体**逐字节重建**（Go `mustJSONReader` → `json.Marshal`）、
//! 11133 退避重试（上限 2 次，800ms/1600ms）与失败现场落盘、上游非 200 的 `writeErr` 形状。
//!
//! **不在本轮**（各自的小轮接手，见 crate README 交接清单）：
//! * 账号池（选号 / 401 标死换号 / 429 冷却换号 / 14018 额度耗尽换号）= P5b；
//! * hy4 限流兜底与缺省 `reasoning_effort` 注入（依赖 failover 配置）= P5b；
//! * 全角色脱敏 `DesensitizeBody` = P5c；
//! * **下游**：流式 SSE 字段清洗（`sseFieldCleaner`）、非流式聚合（`collectStream`）、
//!   usage 入账与统计（`codebuddy-stats.json`）= P5c。
//!   本轮的 200 响应是**最小透传**（流式按原字节转发、非流式原样转发上游体），
//!   与 Go 的下游行为**不等价**——这一点在 README 差异清单里显著登记，不静默。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
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
pub async fn read_capped(body: Body, max: usize) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;
    let mut out: Vec<u8> = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
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

/// Go `handleChat` 的请求侧主体。
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
    // Go 这里取 `modelName` 供 hy4 兜底改写与统计用——两者分别属 P5b/P5c，本轮不使用；
    // 出站模型名一律以重建后的 `backend["model"]` 为准（与 Go 的 `sentModel` 同源）。
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
    // INTL：客户端消息里没有 system 时自动在最前补一条最小中性 system（11128 结构校验）
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

    let hdr = match st.cred.headers() {
        Ok(h) => h,
        Err(e) => return write_err(502, &e),
    };
    let sent_model = backend
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();

    let mut transient_resent = 0usize;
    let resp = loop {
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
            break resp;
        }
        let status = resp.status().as_u16();
        let err_body = read_err_body(resp).await;
        println!(
            "[codebuddy] chat model={} status={} err={}",
            sent_model,
            status,
            truncate(&err_body, 200)
        );
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
            if transient_resent < TRANSIENT_MAX_RETRY {
                transient_resent += 1;
                let backoff = Duration::from_millis(800 * transient_resent as u64);
                println!(
                    "[codebuddy] 11133 属上游抖动，第 {}/{} 次退避 {:?} 后重试",
                    transient_resent, TRANSIENT_MAX_RETRY, backoff
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
            println!(
                "[codebuddy] 11133 连续 {} 次仍被拒，如实透传",
                TRANSIENT_MAX_RETRY
            );
        }
        return write_err(status, &err_body);
    };

    // —— 下游（P5c 范围）：本轮最小透传 ——
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
    let mut out = Response::new(Body::from_stream(resp.bytes_stream()));
    *out.status_mut() = status;
    out.headers_mut().insert(header::CONTENT_TYPE, ct);
    out
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
