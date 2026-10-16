//! HTTP 装配：`internal/server/server.go` 的路由表 + axum。
//!
//! 路由逐条对应 Go 的 `http.NewServeMux()` 注册（含别名路径）：
//!
//! ```text
//! /v1/chat/completions  /chat/completions   → HandleChatCompletions
//! /v1/responses                             → HandleResponses
//! /v1/messages          /messages           → HandleMessages
//! /v1/models                                → HandleModels
//! /v1/usage                                 → HandleUsage
//! /v1/stats                                 → HandleStats
//! /v1/calibration                           → HandleCalibration
//! /health                                   → 常量体
//! ```
//!
//! ⚠ 与 axum 默认行为的差异（已登记进 README）：
//! * Go 的 `ServeMux` 对**含 `.`/`..` 段或重复斜杠**的路径会 301/307 重定向；`/v1/models//x`
//!   经 `cleanPath` 后是 `/v1/models/x`，不匹配任何模式 → **404**（黄金已锁定）。本 crate
//!   不做路径清洗，直接按 axum 的匹配走，行为一致（见 `tests/http_contract.rs` 的对照）。
//! * Go 的 `ServeMux` 把 `GET` 模式自动也接受 `HEAD`；本 crate 用 `any`，效果相同。

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;

use crate::Proxy;

/// 取客户端 `Authorization` 头（大小写不敏感，axum 的 `HeaderMap` 已是规范的）。
fn auth_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// 读整份请求体（Go 的 `io.ReadAll(r.Body)`）。
async fn read_body(req: axum::extract::Request) -> Result<Vec<u8>, ()> {
    use http_body_util::BodyExt;
    let (_parts, body) = req.into_parts();
    match body.collect().await {
        Ok(c) => Ok(c.to_bytes().to_vec()),
        Err(_) => Err(()),
    }
}

fn json_response(status: u16, body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("构造响应失败")
}

fn typed_response(status: u16, content_type: &str, body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("构造响应失败")
}

/// 把多块响应体写成 `Body::from_stream`（`Vec<u8>` 的 `Stream` 已由 `futures_util` 提供）。
fn stream_response(content_type: &str, chunks: Vec<Vec<u8>>) -> Response {
    let s = futures_util::stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok::<Vec<u8>, std::io::Error>(c))
            .collect::<Vec<_>>(),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(s))
        .expect("构造流式响应失败")
}

/// 按当前的版本号取上游版本（门禁可注入；生产走 npm 拉取 + 缓存）。
async fn cc_version() -> String {
    crate::version::get_command_code_version().await
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

async fn h_chat(State(p): State<std::sync::Arc<Proxy>>, req: axum::extract::Request) -> Response {
    let method = req.method().clone();
    let auth = auth_of(req.headers());
    if method != Method::POST {
        return json_response(
            405,
            crate::chat::openai_error_body("Method not allowed", "invalid_request_error")
                .into_bytes(),
        );
    }
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(()) => {
            return json_response(
                400,
                crate::chat::openai_error_body("Failed to read body", "invalid_request_error")
                    .into_bytes(),
            )
        }
    };
    let v = cc_version().await;
    let out = crate::chat::handle_chat(&p, &body, auth.as_deref(), &v).await;
    if out.status == 200 && out.content_type == "text/event-stream" {
        stream_response(out.content_type, out.chunks)
    } else {
        typed_response(
            out.status,
            out.content_type,
            out.chunks.into_iter().next().unwrap_or_default(),
        )
    }
}

async fn h_responses(State(p): State<std::sync::Arc<Proxy>>, req: axum::extract::Request) -> Response {
    let method = req.method().clone();
    let auth = auth_of(req.headers());
    if method != Method::POST {
        return json_response(
            405,
            crate::chat::openai_error_body("Method not allowed", "invalid_request_error")
                .into_bytes(),
        );
    }
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(()) => {
            return json_response(
                400,
                crate::chat::openai_error_body("Failed to read body", "invalid_request_error")
                    .into_bytes(),
            )
        }
    };
    let v = cc_version().await;
    let out = crate::responses::handle_responses(&p, &body, auth.as_deref(), &v).await;
    if out.status == 200 && out.content_type == "text/event-stream" {
        stream_response(out.content_type, out.chunks)
    } else {
        typed_response(
            out.status,
            out.content_type,
            out.chunks.into_iter().next().unwrap_or_default(),
        )
    }
}

async fn h_messages(State(p): State<std::sync::Arc<Proxy>>, req: axum::extract::Request) -> Response {
    let method = req.method().clone();
    let auth = auth_of(req.headers());
    if method != Method::POST {
        return json_response(
            405,
            crate::anthropic::anthropic_error_body("Method not allowed", "invalid_request_error")
                .into_bytes(),
        );
    }
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(()) => {
            return json_response(
                400,
                crate::anthropic::anthropic_error_body("Failed to read body", "invalid_request_error")
                    .into_bytes(),
            )
        }
    };
    let v = cc_version().await;
    let out = crate::anthropic::handle_messages(&p, &body, auth.as_deref(), &v).await;
    if out.status == 200 && out.content_type == "text/event-stream" {
        stream_response(out.content_type, out.chunks)
    } else {
        typed_response(
            out.status,
            out.content_type,
            out.chunks.into_iter().next().unwrap_or_default(),
        )
    }
}

async fn h_models(State(_p): State<std::sync::Arc<Proxy>>, uri: axum::http::Uri) -> Response {
    let plan = query_of(uri.query()).get("plan").cloned().unwrap_or_default();
    let items = if plan == "go" {
        crate::models::go_plan_models()
    } else {
        crate::models::all_models()
    };
    let list = crate::api::OpenAIModelList {
        object: "list".to_string(),
        data: items
            .into_iter()
            .map(|m| crate::api::OpenAIModel {
                id: m.id.to_string(),
                object: "model".to_string(),
                created: 0,
                owned_by: m.owned_by.to_string(),
            })
            .collect(),
    };
    // Go 的 `json.NewEncoder(w).Encode(models)` 会在末尾写一个换行（`models.go:139`），
    // 体因此比 `json.Marshal` 多一个字节 —— 门禁是逐字节比，必须补上。
    let mut body = list.to_go().into_bytes();
    body.push(b'\n');
    typed_response(200, "application/json", body)
}

async fn h_usage(State(p): State<std::sync::Arc<Proxy>>, req: axum::extract::Request) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let auth = auth_of(req.headers());
    let q = query_of(uri.query());
    let out =
        crate::usage::handle_usage(&p, method.as_str(), auth.as_deref(), &q).await;
    typed_response(out.status, out.content_type, out.body)
}

async fn h_stats(State(p): State<std::sync::Arc<Proxy>>, req: axum::extract::Request) -> Response {
    if req.method() != Method::GET {
        return json_response(
            405,
            crate::chat::openai_error_body("Method not allowed", "invalid_request_error")
                .into_bytes(),
        );
    }
    let (started, models, today, file_base, dir) = {
        let mut st = p.stats.lock().expect("stats 锁");
        let snap = st.snapshot();
        let today = st.today();
        let file_base = st.file_base();
        let dir = st.file_dir();
        (snap.started, snap.models, today, file_base, dir)
    };
    // 覆盖式校准：有校准的模型，其本地总 token 被官网值覆盖
    let calib = crate::stats::read_calibration(&dir);
    let mut models = models;
    let mut calibrated = std::collections::BTreeMap::new();
    for (name, official) in &calib {
        let ms = match models.get_mut(name) {
            Some(m) => m,
            None => continue,
        };
        if *official <= 0 {
            continue;
        }
        let sum = ms.input_tokens + ms.output_tokens + ms.cache_read_tokens + ms.cache_write_tokens;
        if sum > 0 {
            let scale = *official as f64 / sum as f64;
            ms.input_tokens = (ms.input_tokens as f64 * scale) as i64;
            ms.output_tokens = (ms.output_tokens as f64 * scale) as i64;
            ms.cache_read_tokens = (ms.cache_read_tokens as f64 * scale) as i64;
            ms.cache_write_tokens = official - ms.input_tokens - ms.output_tokens - ms.cache_read_tokens;
        } else {
            ms.input_tokens = *official;
        }
        calibrated.insert(name.clone(), true);
    }
    let cost = crate::stats::estimate_cost(&models);
    // Go `stats.go:235` 走 `json.NewEncoder(w).Encode(...)`，末尾有一个换行；门禁逐字节比。
    let mut body = crate::stats::encode_stats_response(
        started,
        &models,
        &calibrated,
        today,
        &file_base,
        cost,
    )
    .into_bytes();
    body.push(b'\n');
    typed_response(200, "application/json", body)
}

async fn h_calibration(State(p): State<std::sync::Arc<Proxy>>, req: axum::extract::Request) -> Response {
    if req.method() != Method::POST {
        return json_response(
            405,
            crate::chat::openai_error_body("Method not allowed", "invalid_request_error")
                .into_bytes(),
        );
    }
    let body = read_body(req).await.unwrap_or_default();
    let form = parse_form(&String::from_utf8_lossy(&body));
    let model = form.get("model").cloned().unwrap_or_default();
    if model.is_empty() {
        return json_response(
            400,
            crate::chat::openai_error_body("missing model", "invalid_request_error").into_bytes(),
        );
    }
    let tokens = match form.get("tokens") {
        None => 0i64,
        Some(s) if s.is_empty() => 0i64,
        Some(s) => match s.parse::<i64>() {
            Ok(n) => n,
            Err(_) => {
                return json_response(
                    400,
                    crate::chat::openai_error_body("tokens must be a number", "invalid_request_error")
                        .into_bytes(),
                )
            }
        },
    };
    let dir = p.stats.lock().expect("stats 锁").file_dir();
    if crate::stats::write_calibration(&dir, &model, tokens).is_err() {
        let msg = "写入校准失败";
        return json_response(
            500,
            crate::chat::openai_error_body(msg, "server_error").into_bytes(),
        );
    }
    // Go 直接写死这段（**不**经 json.Encoder，故没有末尾换行）
    typed_response(
        200,
        "application/json",
        "{\"ok\":true,\"msg\":\"校准已保存\"}".as_bytes().to_vec(),
    )
}

async fn h_health() -> Response {
    typed_response(200, crate::HEALTH_CONTENT_TYPE, crate::HEALTH_BODY.as_bytes().to_vec())
}

/// Go `ServeMux` 未命中任何模式时的兜底：`http.NotFound` → `404 page not found\n`。
///
/// Go 写的是裸字节、没设 Content-Type，由 `net/http` 的 `DetectContentType` 判成
/// `text/plain; charset=utf-8`，并补上 `X-Content-Type-Options: nosniff`
/// （`net/http/server.go` 的 `writeHeader`：`ct := w.handlerHeader[Content-Type]` 为空
/// 时先嗅探，随后 `setHeader.contentType` 非空即写 nosniff）。
/// 黄金 `route-boundaries` 的 7 条 404 步（`/health/`、`/v1/models/`、`/v1/models//x`、
/// `/nope`、`/v1`、`/`）逐字节锁的就是这三点：状态、Content-Type、体。
async fn h_not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, crate::HEALTH_CONTENT_TYPE)
        .header("x-content-type-options", "nosniff")
        .body(Body::from(NOT_FOUND_BODY.as_bytes().to_vec()))
        .expect("构造 404 响应失败")
}

/// `http.NotFound` 的体（`net/http` 源码里的字面量，含末尾换行）。
pub const NOT_FOUND_BODY: &str = "404 page not found\n";

/// Go 的 `/health` 写的是裸字节、没设 Content-Type，由 `net/http` 的 `DetectContentType`
/// 判成 `text/plain; charset=utf-8`；本 crate 显式设成同一个值（见 [`crate::HEALTH_CONTENT_TYPE`]）。
fn query_of(q: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = q {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            out.insert(url_decode(k), url_decode(v));
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// `application/x-www-form-urlencoded` 的解析（`r.FormValue` 的等价物）。
fn parse_form(body: &str) -> HashMap<String, String> {
    query_of(Some(body))
}

/// `internal/server/server.go` 的 `NewServer`：搭出路由表。
pub fn router(proxy: std::sync::Arc<Proxy>) -> Router {
    Router::new()
        .route("/v1/chat/completions", any(h_chat))
        .route("/chat/completions", any(h_chat))
        .route("/v1/responses", any(h_responses))
        .route("/v1/messages", any(h_messages))
        .route("/messages", any(h_messages))
        .route("/v1/models", any(h_models))
        .route("/v1/usage", any(h_usage))
        .route("/v1/stats", any(h_stats))
        .route("/v1/calibration", any(h_calibration))
        .route("/health", any(h_health))
        .fallback(h_not_found)
        .with_state(proxy)
}

/// `Server.StartWithListener` 的等价物：在给定地址上起服务。
pub async fn serve(proxy: std::sync::Arc<Proxy>, host: &str, port: u16) -> std::io::Result<()> {
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("主代理监听 http://{addr}");
    axum::serve(listener, router(proxy)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decoding_matches_go_url_values() {
        let q = query_of(Some("plan=go&x=1"));
        assert_eq!(q.get("plan").unwrap(), "go");
        assert_eq!(q.get("x").unwrap(), "1");
        let q2 = query_of(Some("cursor=a%2Bb&empty="));
        assert_eq!(q2.get("cursor").unwrap(), "a+b");
        assert_eq!(q2.get("empty").unwrap(), "");
        let q3 = query_of(Some("space=a+b"));
        assert_eq!(q3.get("space").unwrap(), "a b");
        let q4 = query_of(Some("flag"));
        assert_eq!(q4.get("flag").unwrap(), "");
    }

    #[test]
    fn form_parsing() {
        let f = parse_form("model=deepseek-v4-flash&tokens=1000000");
        assert_eq!(f.get("model").unwrap(), "deepseek-v4-flash");
        assert_eq!(f.get("tokens").unwrap(), "1000000");
    }
}
