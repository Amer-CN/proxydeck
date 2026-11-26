//! 码道响应提交边界：业务错误在客户端提交前交给账号策略，提交后只负责交付。

use std::pin::Pin;

use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::Value;

use crate::sse::{self, Aggregate};
use crate::upstream::{embedded_error, error_from_body, UpError};

pub type ByteStream = Pin<
    Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>,
>;

pub enum OutgoingBody {
    Full(Vec<u8>),
    Stream(ByteStream),
}

/// 已通过提交前校验的响应。调用方只能在账号策略确认后 `into_response()`；
/// 一旦提交，外层请求循环不得回到换号/重试路径。
pub struct PreparedResponse {
    status: u16,
    content_type: String,
    body: OutgoingBody,
    sse: bool,
    model: String,
}

impl PreparedResponse {
    pub fn for_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    pub fn into_response(self) -> Response {
        match self.body {
            OutgoingBody::Stream(stream) => (
                StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK),
                [
                    (header::CONTENT_TYPE, "text/event-stream; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-cache"),
                    (header::CONNECTION, "keep-alive"),
                ],
                Body::from_stream(stream),
            )
                .into_response(),
            OutgoingBody::Full(bytes) if self.sse => {
                let mut aggregate = Aggregate::default();
                aggregate.feed(&String::from_utf8_lossy(&bytes));
                // prepare_response 已检查同一聚合器；这里只组装成功响应。
                crate::json_response(StatusCode::OK, &aggregate.into_response(&self.model))
            }
            OutgoingBody::Full(bytes) => (
                StatusCode::from_u16(self.status).unwrap_or(StatusCode::OK),
                [(header::CONTENT_TYPE, self.content_type)],
                bytes,
            )
                .into_response(),
        }
    }
}

/// 读取提交前所需前缀/完整体，并把 HTTP200 内业务错误转成 `UpError`。
pub async fn prepare_response(
    resp: reqwest::Response,
    wants_stream: bool,
) -> Result<PreparedResponse, UpError> {
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = resp.bytes().await.unwrap_or_default();
        return Err(error_from_body(status, &body));
    }
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !content_type.contains("text/event-stream") {
        let body = resp.bytes().await.unwrap_or_default();
        if let Ok(value) = serde_json::from_slice::<Value>(&body) {
            if let Some(error) = embedded_error(&value) {
                return Err(error);
            }
        }
        let content_type = if content_type.is_empty() {
            "application/json; charset=utf-8".to_string()
        } else {
            content_type
        };
        return Ok(PreparedResponse {
            status,
            content_type,
            body: OutgoingBody::Full(body.to_vec()),
            sse: false,
            model: String::new(),
        });
    }

    if wants_stream {
        let stream = preflight_stream(resp).await?;
        Ok(PreparedResponse {
            status,
            content_type,
            body: OutgoingBody::Stream(stream),
            sse: true,
            model: String::new(),
        })
    } else {
        let text = collect_stream(resp).await?;
        let mut aggregate = Aggregate::default();
        aggregate.feed(&text);
        if let Some(error) = aggregate.error.take() {
            return Err(error);
        }
        Ok(PreparedResponse {
            status,
            content_type,
            body: OutgoingBody::Full(text.into_bytes()),
            sse: true,
            model: String::new(),
        })
    }
}

/// 读首个 SSE 事件做预检；正常时把已读前缀与剩余流接回去。
async fn preflight_stream(resp: reqwest::Response) -> Result<ByteStream, UpError> {
    let mut source = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + crate::STREAM_PRELUDE;
    loop {
        let complete = sse::first_data_event(&buf).is_some()
            && buf.windows(2).any(|w| w == &b"\n\n"[..]);
        if complete || buf.len() > 128 * 1024 {
            break;
        }
        match tokio::time::timeout_at(deadline, source.next()).await {
            Err(_) => {
                return Err(UpError::new(504, "timeout", "等待上游首个事件超时"));
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                return Err(UpError::new(502, "network", format!("读取上游流失败: {e}")))
            }
            Ok(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
        }
    }
    if let Some(error) = sse::preflight_error(&buf) {
        return Err(error);
    }
    let head = futures_util::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(buf))
    });
    let rest = source.map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    Ok(Box::pin(head.chain(rest)))
}

/// 收集整条 SSE（非流式聚合用），上限 64 MiB。
async fn collect_stream(resp: reqwest::Response) -> Result<String, UpError> {
    let mut source = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|e| UpError::new(502, "network", format!("读取上游流失败: {e}")))?;
        if buf.len() + chunk.len() > 64 << 20 {
            return Err(UpError::new(502, "too_large", "上游响应超过 64 MiB"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}
