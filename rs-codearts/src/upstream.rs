//! 上游客户端：华为云码道 `snap-access` 的签名请求、模型、额度、聊天。
//!
//! 端点常量来自本机 `D:\Program Files\CodeArts\resources\product.json`
//! （2026-10-26 实测：`snapEngineUrl` / `kernel.envConfig.inferhubBaseUrl`）。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::config::Account;
use crate::signer::{self, Credential};

/// 默认上游根（`product.json` 的 `snapEngineUrl`）。
pub const DEFAULT_BASE: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";
/// 聊天端点（`/api/v2/` + `chat/completions`）。
pub const CHAT_PATH: &str = "/api/v2/chat/completions";
/// 内置模型清单（需 `Agent-Type: PromptCenter`）。
pub const BUILTIN_MODELS_PATH: &str = "/v1/model/builtin";
/// 积分套餐与用量。
pub const STATISTICS_PATH: &str = "/snap-manager/v1/statistics/plugin";

/// 上游客户端。
#[derive(Clone)]
pub struct Upstream {
    base: String,
    client: reqwest::Client,
}

/// 上游错误。
#[derive(Debug, Clone)]
pub struct UpError {
    pub status: u16,
    pub code: String,
    pub message: String,
    /// 是否凭据类错误（需要用户重新登录）。
    pub credential: bool,
}

impl UpError {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let message = message.into();
        let lower = format!("{code} {message}").to_lowercase();
        let credential = lower.contains("apig.0301")
            || lower.contains("apig.0303")
            || lower.contains("sts5.")
            || lower.contains("invalid_grant")
            || lower.contains("invalid dpop")
            || lower.contains("credentials_error");
        Self {
            status,
            code,
            message,
            credential,
        }
    }
}

impl std::fmt::Display for UpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream {} {}: {}", self.status, self.code, self.message)
    }
}

impl Upstream {
    pub fn new(base: &str) -> Result<Self, String> {
        let base = base.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|e| format!("构造 HTTP client 失败: {e}"))?;
        Ok(Self { base, client })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// 带签名的通用请求。`extra` 里的键会统一小写后一起参与签名。
    async fn signed(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
        extra: BTreeMap<String, String>,
        account: &Account,
    ) -> Result<reqwest::Response, UpError> {
        let url = self.url(path);
        let mut headers = extra;
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert(
            "x-language".to_string(),
            "zh-cn".to_string(),
        );
        headers.insert(
            "x-security-token".to_string(),
            account.security_token.clone(),
        );
        let cred = Credential {
            access_key: account.access_key.clone(),
            secret_key: account.secret_key.clone(),
            security_token: account.security_token.clone(),
        };
        let auth = signer::sign_request(method.as_str(), &url, &body, &mut headers, &cred)
            .map_err(|e| UpError::new(500, "sign", e))?;
        let mut req = self.client.request(method.clone(), &url);
        for (k, v) in &headers {
            if k == "host" {
                continue; // reqwest 自己带 Host；签名里已经算进去了
            }
            if let (Ok(name), Ok(value)) =
                (reqwest::header::HeaderName::from_bytes(k.as_bytes()), reqwest::header::HeaderValue::from_str(v))
            {
                req = req.header(name, value);
            }
        }
        // authorization 由 sign_request 写回 headers；上面的循环已带出。
        let _ = auth;
        if method == reqwest::Method::POST {
            req = req.body(body);
        }
        req.send().await.map_err(|e| UpError {
            status: 502,
            code: "network".to_string(),
            message: format!("请求上游失败: {e}"),
            credential: false,
        })
    }

    /// 内置模型清单（积分通道）。
    pub async fn builtin_models(&self, account: &Account) -> Result<Vec<ModelInfo>, UpError> {
        let mut extra = BTreeMap::new();
        extra.insert("agent-type".to_string(), "PromptCenter".to_string());
        let resp = self
            .signed(reqwest::Method::GET, BUILTIN_MODELS_PATH, Vec::new(), extra, account)
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| UpError::new(502, "read", e.to_string()))?;
        if status >= 400 {
            return Err(error_from_body(status, &bytes));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| UpError::new(502, "parse", format!("模型清单不是 JSON: {e}")))?;
        if let Some(err) = embedded_error(&v) {
            return Err(err);
        }
        let mut out = Vec::new();
        for m in v
            .get("builtinModels")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let id = m.get("model_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            out.push(ModelInfo {
                id,
                name: m
                    .get("model_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                desc: m
                    .get("model_desc")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                context_window: m.get("context_window").and_then(|x| x.as_i64()),
                max_tokens: m.get("max_tokens").and_then(|x| x.as_i64()),
                enable: m.get("enable").and_then(|x| x.as_bool()).unwrap_or(true),
                supports_images: m
                    .get("supports_images")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                display_enabled: m
                    .get("display_enabled")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true),
            });
        }
        Ok(out)
    }

    /// 积分套餐与用量（原样 JSON）。
    pub async fn statistics(&self, account: &Account) -> Result<Value, UpError> {
        let resp = self
            .signed(
                reqwest::Method::GET,
                STATISTICS_PATH,
                Vec::new(),
                BTreeMap::new(),
                account,
            )
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| UpError::new(502, "read", e.to_string()))?;
        if status >= 400 {
            return Err(error_from_body(status, &bytes));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| UpError::new(502, "parse", format!("额度响应不是 JSON: {e}")))?;
        if let Some(err) = embedded_error(&v) {
            return Err(err);
        }
        Ok(v)
    }

    /// 发聊天请求（**一律 stream=true**：上游 `glm-5.3` 只支持流式）。
    pub async fn chat(
        &self,
        account: &Account,
        model: &str,
        body: Vec<u8>,
        benefit: bool,
        session_id: &str,
    ) -> Result<reqwest::Response, UpError> {
        let mut extra = BTreeMap::new();
        extra.insert("accept".to_string(), "text/event-stream".to_string());
        extra.insert("app-id".to_string(), "CodeAgent3.0".to_string());
        extra.insert("is_confidential".to_string(), "false".to_string());
        extra.insert("model-id".to_string(), model.to_string());
        extra.insert("model-name".to_string(), model.to_string());
        let plugin_name = account
            .plugin_name
            .clone()
            .unwrap_or_else(|| "snap_AIIDE".to_string());
        let plugin_version = account
            .plugin_version
            .clone()
            .unwrap_or_else(|| "5.1.0".to_string());
        let ide_name = account.ide_name.clone().unwrap_or_else(|| "CodeArts".to_string());
        let ide_version = account
            .ide_version
            .clone()
            .unwrap_or_else(|| plugin_version.clone());
        extra.insert("plugin-name".to_string(), plugin_name);
        extra.insert("plugin-version".to_string(), plugin_version.clone());
        extra.insert("ide-name".to_string(), ide_name);
        extra.insert("ide-version".to_string(), ide_version);
        extra.insert("x-ot-client-type".to_string(), "IDE".to_string());
        extra.insert("x-ot-client-version".to_string(), plugin_version);
        extra.insert("x-ot-function".to_string(), "agent-general".to_string());
        extra.insert("x-ot-session-id".to_string(), session_id.to_string());
        extra.insert("user-session-id".to_string(), session_id.to_string());
        extra.insert("x-ot-trace-id".to_string(), random_hex(16));
        extra.insert("x-ot-span-id".to_string(), random_hex(8));
        extra.insert("x-snap-traceid".to_string(), random_hex(16));
        extra.insert("user-msg-id".to_string(), format!("msg_{}", random_hex(8)));
        extra.insert(
            "created-time".to_string(),
            crate::config::iso8601_utc(crate::config::now_unix()),
        );
        if benefit {
            extra.insert("maas_type".to_string(), "benefit".to_string());
        }
        self.signed(reqwest::Method::POST, CHAT_PATH, body, extra, account)
            .await
    }
}

/// 上游模型条目。
#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub desc: String,
    pub context_window: Option<i64>,
    pub max_tokens: Option<i64>,
    pub enable: bool,
    pub supports_images: bool,
    pub display_enabled: bool,
}

/// 由 AK 派生稳定会话 id（上游按它统计并发，每账号上限 3）。
pub fn session_id_for(account: &Account) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(account.access_key.as_bytes());
    h.update(b"|codearts-session");
    let digest = h.finalize();
    format!("ses_{}", hex::encode(&digest[..12]))
}

/// 随机 hex（无 rand 依赖：时间 + 计数器 + SHA-256）。
pub fn random_hex(bytes: usize) -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut h = Sha256::new();
    h.update(now.to_le_bytes());
    h.update(n.to_le_bytes());
    h.update(pid.to_le_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..bytes.min(32)])
}

/// 识别「HTTP 200 里的业务错误」。
pub fn embedded_error(v: &Value) -> Option<UpError> {
    let code = v.get("error_code").and_then(|x| x.as_str()).unwrap_or("");
    if code.is_empty() || code == "0" {
        return None;
    }
    let msg = v
        .get("error_msg")
        .and_then(|x| x.as_str())
        .unwrap_or("上游返回业务错误");
    Some(error_from_code(code, msg))
}

/// 状态码提取：`InferHub.001001005.400` 取末段；纯数字 3 位直接用；否则 400。
pub fn status_from_code(code: &str) -> u16 {
    let last = code.rsplit('.').next().unwrap_or("");
    if let Ok(n) = last.parse::<u16>() {
        if (400..=599).contains(&n) {
            return n;
        }
    }
    if let Ok(n) = code.parse::<u16>() {
        if (400..=599).contains(&n) {
            return n;
        }
    }
    let lower = code.to_lowercase();
    if lower.contains("tm.00001041") || lower.contains("concurrent") {
        return 429;
    }
    400
}

/// 把上游错误映射成 `UpError`。
pub fn error_from_code(code: &str, msg: &str) -> UpError {
    let status = status_from_code(code);
    UpError::new(status, code.to_string(), truncate(msg, 600))
}

/// 从 HTTP 响应体构造错误。
pub fn error_from_body(status: u16, body: &[u8]) -> UpError {
    let text = String::from_utf8_lossy(body);
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(e) = embedded_error(&v) {
            return e;
        }
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|x| x.as_str())
                .or_else(|| err.as_str())
                .unwrap_or(&text);
            return UpError::new(status, "", truncate(msg, 600));
        }
    }
    UpError::new(status, "", truncate(&text, 600))
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// OpenAI 错误体。
pub fn openai_error(status: u16, message: &str, code: &str) -> (u16, Value) {
    let ty = match status {
        401 | 403 => "authentication_error",
        429 => "rate_limit_error",
        400 => "invalid_request_error",
        _ => "upstream_error",
    };
    (
        status,
        json!({
            "error": {
                "message": message,
                "type": ty,
                "code": code,
                "param": null
            }
        }),
    )
}
