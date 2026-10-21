//! upstream.rs —— opencode `serve` 的 HTTP 客户端（Basic auth `opencode:<password>`）。
//!
//! 只封装任务简报 §4.4 列出的接口。错误统一成 [`UpError`]：调用方按 `kind` 映射
//! 429 / 400（国家限制）/ 504 / 502，不在各处重复判定字符串。
use std::time::Duration;

use reqwest::Method;
use serde_json::{json, Value};

/// 单次设置类请求的超时（建会话 / 设模型 / 发 prompt / 取最终消息）。
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(20);
/// 回复权限询问的超时（默认拒绝，不能因为上游慢把整轮拖死）。
pub const PERMISSION_TIMEOUT: Duration = Duration::from_secs(10);
/// 清理会话的超时。
pub const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

/// 上游错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 免费档滚动限流（`status == 429` / `FreeUsageLimitError` / `Rate limit exceeded`）。
    RateLimit,
    /// `not available in your country`（中国大陆不可用）。
    Country,
    /// 其它 HTTP 非 2xx。
    Http,
    /// 连接失败 / 断流 / 请求超时。
    Network,
    /// 单轮总超时（由调用方的 deadline 产生）。
    Timeout,
}

/// 可读的上游错误。`message` 已截断到 600 字符，避免把整页 HTML 灌进客户端。
#[derive(Debug, Clone)]
pub struct UpError {
    pub kind: ErrorKind,
    pub status: Option<u16>,
    pub message: String,
}

impl UpError {
    pub fn rate_limit(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::RateLimit,
            status: Some(429),
            message: truncate(message.into()),
        }
    }

    pub fn country(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Country,
            status: Some(400),
            message: truncate(message.into()),
        }
    }

    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Http,
            status: Some(status),
            message: truncate(message.into()),
        }
    }

    pub fn network(error: reqwest::Error) -> Self {
        Self {
            kind: ErrorKind::Network,
            status: None,
            message: truncate(format!("连接 opencode 失败: {error}")),
        }
    }

    pub fn timeout() -> Self {
        Self {
            kind: ErrorKind::Timeout,
            status: None,
            message: "opencode 会话执行超时（已杀会话）".to_string(),
        }
    }

    pub fn stream_ended() -> Self {
        Self {
            kind: ErrorKind::Network,
            status: None,
            message: "opencode 事件流在 execution 结束前断开".to_string(),
        }
    }

    pub fn client_disconnected() -> Self {
        Self {
            kind: ErrorKind::Network,
            status: None,
            message: "客户端已断开，停止转发".to_string(),
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self.kind {
            ErrorKind::RateLimit => "rate_limit_error",
            ErrorKind::Country => "invalid_request_error",
            ErrorKind::Timeout => "timeout_error",
            ErrorKind::Http | ErrorKind::Network => "upstream_error",
        }
    }

    /// 从 `session.execution.failed.data.error` 这类错误对象构造。
    ///
    /// 形态（实测）：
    /// `{"type":"provider.quota","message":"Rate limit exceeded...","status":429,
    ///   "response":{"body":"{\"type\":\"error\",\"error\":{\"type\":\"FreeUsageLimitError\"...}}"}}`
    pub fn from_event_error(err: &Value) -> Self {
        let status = err
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok());
        let message =
            extract_message(err).unwrap_or_else(|| "opencode execution failed".to_string());
        classify(status, err, &message)
    }
}

impl std::fmt::Display for UpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// 从上游错误对象里拿人话：优先 `error.message`，其次 `response.body` 里嵌的
/// `error.message`（opencode 的 429 把真错误放在这一层），最后 `error.type`。
fn extract_message(err: &Value) -> Option<String> {
    if let Some(m) = err.get("message").and_then(Value::as_str) {
        if !m.trim().is_empty() {
            return Some(m.trim().to_string());
        }
    }
    if let Some(body) = err
        .get("response")
        .and_then(|r| r.get("body"))
        .and_then(Value::as_str)
    {
        if let Ok(v) = serde_json::from_str::<Value>(body) {
            if let Some(m) = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
            {
                if !m.trim().is_empty() {
                    return Some(m.trim().to_string());
                }
            }
            if let Some(m) = v.get("message").and_then(Value::as_str) {
                if !m.trim().is_empty() {
                    return Some(m.trim().to_string());
                }
            }
        }
        if !body.trim().is_empty() {
            return Some(body.trim().to_string());
        }
    }
    err.get("type")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

/// 按 status + 整段文本判定分类。
fn classify(status: Option<u16>, value: &Value, message: &str) -> UpError {
    let hay = format!(
        "{} {}",
        message,
        serde_json::to_string(value).unwrap_or_default()
    )
    .to_lowercase();
    if status == Some(429)
        || hay.contains("freeusagelimiterror")
        || hay.contains("rate limit exceeded")
        || value.get("type").and_then(Value::as_str) == Some("provider.quota")
    {
        return UpError::rate_limit(message);
    }
    if hay.contains("not available in your country") {
        return UpError::country(message);
    }
    UpError::http(status.unwrap_or(502), message)
}

fn truncate(mut s: String) -> String {
    if s.chars().count() > 600 {
        s = s.chars().take(600).collect();
    }
    s
}

/// opencode HTTP 客户端（clone 便宜：内部是 reqwest::Client）。
#[derive(Clone)]
pub struct Upstream {
    client: reqwest::Client,
    pub base: String,
    pub password: String,
}

impl Upstream {
    pub fn new(client: reqwest::Client, base: &str, password: &str) -> Self {
        Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            password: password.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.password.is_empty() {
            req
        } else {
            req.basic_auth("opencode", Some(&self.password))
        }
    }

    /// 发一个 JSON 请求并解析响应。非 2xx 统一转成 [`UpError`]。
    async fn send_json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, UpError> {
        let mut req = self
            .client
            .request(method, self.url(path))
            .timeout(timeout)
            .header(reqwest::header::ACCEPT, "application/json");
        req = self.auth(req);
        if let Some(b) = body {
            let bytes = serde_json::to_vec(b).unwrap_or_else(|_| b"{}".to_vec());
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes);
        }
        let resp = req.send().await.map_err(UpError::network)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(UpError::network)?;
        let value: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}))
        };
        if !(200..300).contains(&status) {
            let error_obj = value.get("error").unwrap_or(&value);
            let message = extract_message(error_obj)
                .or_else(|| extract_message(&value))
                .unwrap_or_else(|| format!("opencode 返回 HTTP {status}"));
            return Err(classify(Some(status), &value, &message));
        }
        Ok(value)
    }

    /// `GET /api/info`（健康检查）。
    pub async fn info(&self, timeout: Duration) -> Result<Value, UpError> {
        self.send_json(Method::GET, "/api/info", None, timeout)
            .await
    }

    /// `GET /api/model`。
    pub async fn models(&self) -> Result<Value, UpError> {
        self.send_json(Method::GET, "/api/model", None, SETUP_TIMEOUT)
            .await
    }

    /// `POST /api/session`（body `{}`），返回 session id。
    pub async fn create_session(&self) -> Result<String, UpError> {
        let v = self
            .send_json(
                Method::POST,
                "/api/session",
                Some(&json!({})),
                SETUP_TIMEOUT,
            )
            .await?;
        v.get("data")
            .and_then(|d| d.get("id"))
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| UpError::http(502, "opencode 建会话响应缺少 data.id"))
    }

    /// `POST /api/session/{id}/model`（实测返回 204）。
    pub async fn set_model(
        &self,
        session_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<(), UpError> {
        let body = json!({"model": {"providerID": provider_id, "id": model_id}});
        self.send_json(
            Method::POST,
            &format!("/api/session/{session_id}/model"),
            Some(&body),
            SETUP_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// `POST /api/session/{id}/prompt`（200 后异步执行，事件从 `/api/event` 来）。
    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<(), UpError> {
        let body = json!({"text": text});
        self.send_json(
            Method::POST,
            &format!("/api/session/{session_id}/prompt"),
            Some(&body),
            SETUP_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// `GET /api/event`：长连接 SSE。**不设总超时**，由调用方的 turn deadline 约束。
    pub async fn event_stream(&self) -> Result<reqwest::Response, UpError> {
        let req = self
            .client
            .get(self.url("/api/event"))
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let resp = self.auth(req).send().await.map_err(UpError::network)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
            let message = extract_message(&value)
                .unwrap_or_else(|| format!("opencode /api/event 返回 HTTP {status}"));
            return Err(classify(Some(status), &value, &message));
        }
        Ok(resp)
    }

    /// `GET /api/session/{id}/message`：取会话消息（最终消息 / 错误兜底）。
    pub async fn final_messages(&self, session_id: &str) -> Result<Value, UpError> {
        self.send_json(
            Method::GET,
            &format!("/api/session/{session_id}/message"),
            None,
            SETUP_TIMEOUT,
        )
        .await
    }

    /// `DELETE /api/session/{id}`：清理。失败只记日志，不影响已拿到的回复。
    pub async fn delete_session(&self, session_id: &str) {
        let req = self
            .client
            .delete(self.url(&format!("/api/session/{session_id}")))
            .timeout(CLEANUP_TIMEOUT);
        if let Err(e) = self.auth(req).send().await {
            crate::logf(&format!("清理 opencode 会话 {session_id} 失败：{e}"));
        }
    }

    /// `POST /api/session/{sid}/permission/{id}/reply`，**固定 reject**。
    ///
    /// 安全要求：不收权限 = 不让模型动用户机器。这里没有 "once/approve" 分支。
    pub async fn reply_permission(
        &self,
        session_id: &str,
        permission_id: &str,
    ) -> Result<(), UpError> {
        let body = json!({"decision": "reject"});
        self.send_json(
            Method::POST,
            &format!("/api/session/{session_id}/permission/{permission_id}/reply"),
            Some(&body),
            PERMISSION_TIMEOUT,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota_error() -> Value {
        json!({
            "type": "provider.quota",
            "message": "Rate limit exceeded. Please try again later.",
            "status": 429,
            "response": {
                "body": "{\"type\":\"error\",\"error\":{\"type\":\"FreeUsageLimitError\",\"message\":\"Rate limit exceeded. Please try again later.\"}}"
            }
        })
    }

    #[test]
    fn quota_error_is_rate_limit() {
        let e = UpError::from_event_error(&quota_error());
        assert_eq!(e.kind, ErrorKind::RateLimit);
        assert_eq!(e.error_type(), "rate_limit_error");
        assert!(e.message.contains("Rate limit exceeded"));
    }

    #[test]
    fn country_error_is_country() {
        let v = json!({
            "type": "provider.error",
            "message": "This model is not available in your country",
            "status": 400
        });
        let e = UpError::from_event_error(&v);
        assert_eq!(e.kind, ErrorKind::Country);
        assert_eq!(e.error_type(), "invalid_request_error");
    }

    #[test]
    fn rate_limit_detected_from_nested_body_even_without_status() {
        let v = json!({
            "response": {"body": "{\"error\":{\"type\":\"FreeUsageLimitError\"}}"}
        });
        let e = UpError::from_event_error(&v);
        assert_eq!(e.kind, ErrorKind::RateLimit);
    }

    #[test]
    fn plain_http_error_is_http() {
        let e = UpError::from_event_error(&json!({"message": "boom", "status": 500}));
        assert_eq!(e.kind, ErrorKind::Http);
        assert_eq!(e.error_type(), "upstream_error");
    }

    #[test]
    fn long_message_is_truncated() {
        let e = UpError::http(500, "x".repeat(2000));
        assert_eq!(e.message.chars().count(), 600);
    }
}
