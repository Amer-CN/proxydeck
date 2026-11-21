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

/// 活动积分投放通道：2026-11-02 起新积分体系「每日签到领100积分」投在 **PORTAL**，
/// 老体系在 IDE。两边都查，按 `campaignId` 合并（后查的 PORTAL 覆盖同 ID 的 IDE 条目），
/// 单通道失败不阻断——新体系只在 PORTAL 投放时 IDE 仍然可用。
pub const OPS_CHANNELS: [&str; 2] = ["IDE", "PORTAL"];
pub const OPS_DELIVERY_PATH: &str = "/v1/ops/delivery";
pub const OPS_CLAIM_PATH: &str = "/v1/ops/claim";
pub const OPS_CONFIRM_PATH: &str = "/v1/ops/confirm";

/// 并发排队：网关把请求排队时回 429 + `queue-status: queued` + `trace-id`，
/// 轮询这个 trace 直到 `promoted` 再重发；放弃时 DELETE。
pub const TRACE_QUEUE_PATH: &str = "/api/v1/concurrency/trace-queues";

/// 上游 `/api/v2/chat/completions` 的单请求硬上限（实测 10,484,871 B 过 /
/// 10,485,871 B 挂；超限回的是误导性的 `InferHub.001001005`「参数非法」）。
pub const CHAT_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// 限时福利（免费套餐）网关：与 snap-access 不同域，签名按该 host 计算。
pub const BENEFIT_BASE: &str = "https://opengw.developer.huaweicloud.com";
pub const BENEFIT_CLAIM_PATH: &str = "/api/v1/benefit/claim";
pub const BENEFIT_CONFIG_PATH: &str = "/api/v1/gateway/config";

/// 福利模型聊天必须追加的请求头（官方 renderer 的 `isFreeBenefit` 路径）。
pub const MAAS_TYPE_HEADER: &str = "maas_type";
pub const MAAS_BENEFIT: &str = "benefit";

/// 上游客户端。
#[derive(Clone)]
pub struct Upstream {
    base: String,
    /// 福利网关根（默认 `BENEFIT_BASE`；`CODEARTS_BENEFIT_BASE` 或测试可覆盖）。
    benefit_base: String,
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
        // 网关还有一条与凭据无关的伪装错误：请求体超限时它回 APIG.0301
        // 「calc ak sk signature fail: body too large」。别当凭据错误去冷却/标死账号，
        // 真正该说的是「上下文太大」（2026-11-05 实测：40+ 张历史截图 base64 后 18MB >
        // 上游 15 MiB 单请求上限）。
        let too_large = lower.contains("body too large");
        // APIG.0602 = 「the security token has expired」：华为 STS 临时令牌寿命比
        // `session.expiresAt` 短，到点后上游就回这个码。它必须当凭据错误处理，否则
        // 插件既不续期也不换号，会把 400 原样透给客户端（2026-11-05 实测踩过）。
        let credential = !too_large
            && (lower.contains("apig.0301")
                || lower.contains("apig.0303")
                || lower.contains("apig.0602")
                || lower.contains("security token has expired")
                || lower.contains("sts5.")
                || lower.contains("invalid_grant")
                || lower.contains("invalid dpop")
                || lower.contains("credentials_error"));
        Self {
            status,
            code,
            message,
            credential,
        }
    }
}

/// 上游网关的「请求体超限」伪装错误（APIG.0301 + `body too large`）。
/// 实测上限在 15 MiB 附近（15,699,930 B 过、15,759,930 B 挂）。
pub fn is_body_too_large(e: &UpError) -> bool {
    format!("{} {}", e.code, e.message)
        .to_lowercase()
        .contains("body too large")
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
        let benefit_base = std::env::var("CODEARTS_BENEFIT_BASE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| BENEFIT_BASE.to_string());
        Ok(Self {
            base,
            benefit_base,
            client,
        })
    }

    /// 覆盖福利网关根（测试/应急；生产走 `CODEARTS_BENEFIT_BASE`）。
    pub fn set_benefit_base(&mut self, base: &str) {
        self.benefit_base = base.trim_end_matches('/').to_string();
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// 供续期模块复用的 HTTP client（同一套 TLS/代理配置）。
    pub fn client(&self) -> &reqwest::Client {
        &self.client
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
        self.signed_url(method, &self.url(path), body, extra, account)
            .await
    }

    /// 同 `signed`，但吃**绝对 URL**（福利网关 `opengw.developer.huaweicloud.com`
    /// 与 snap-access 不同域，签名按 URL 里的 host 计算）。
    async fn signed_url(
        &self,
        method: reqwest::Method,
        raw_url: &str,
        body: Vec<u8>,
        extra: BTreeMap<String, String>,
        account: &Account,
    ) -> Result<reqwest::Response, UpError> {
        let url = raw_url.to_string();
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
                benefit: false,
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

    /// 活动列表：签到（`USER_LOGIN`）、新手（`NEW_USER_REGISTER`）、邀请、学生等。
    /// 需 `Agent-Type: PromptCenter`；签名通道与推理相同（snap-access 根路径）。
    ///
    /// 轮询 [`OPS_CHANNELS`]（IDE + PORTAL）取并集：新积分体系的每日签到只在 PORTAL
    /// 投放，老体系在 IDE；按 `campaignId` 合并、PORTAL 覆盖同 ID 条目。单通道失败
    /// 不阻断，两通道全失败才把第一个错误抛出去。
    pub async fn campaigns(
        &self,
        account: &Account,
    ) -> Result<Vec<crate::activity::Campaign>, UpError> {
        let mut merged: std::collections::BTreeMap<String, crate::activity::Campaign> =
            std::collections::BTreeMap::new();
        let mut first_err: Option<UpError> = None;
        for channel in OPS_CHANNELS {
            match self.campaigns_channel(account, channel).await {
                Ok(list) => {
                    for mut c in list {
                        c.channel = channel.to_string();
                        merged.insert(c.id_text(), c);
                    }
                }
                Err(e) => {
                    // 单通道失败不阻断（另一通道仍可领），但要留痕：2026-11-21 实测
                    // 活动已全部搬到 PORTAL，IDE 可能回空列表或报错。
                    println!("[codearts] 活动通道 {channel} 拉取失败：{}", e.message);
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if merged.is_empty() {
            if let Some(e) = first_err {
                return Err(e);
            }
        }
        Ok(merged.into_values().collect())
    }

    /// 单通道活动列表。
    async fn campaigns_channel(
        &self,
        account: &Account,
        channel: &str,
    ) -> Result<Vec<crate::activity::Campaign>, UpError> {
        let mut extra = BTreeMap::new();
        extra.insert("agent-type".to_string(), "PromptCenter".to_string());
        let path = format!("{OPS_DELIVERY_PATH}?channel={channel}");
        let resp = self
            .signed(reqwest::Method::GET, &path, Vec::new(), extra, account)
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| UpError::new(502, "read", e.to_string()))?;
        if status >= 400 {
            return Err(error_from_body(status, &bytes));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| UpError::new(502, "parse", format!("活动列表不是 JSON: {e}")))?;
        if let Some(err) = embedded_error(&v) {
            return Err(err);
        }
        if let Some(code) = v.get("code").and_then(|x| x.as_i64()) {
            if code != 0 {
                let msg = v
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("活动列表失败");
                return Err(UpError::new(400, code.to_string(), msg.to_string()));
            }
        }
        Ok(crate::activity::parse_campaigns(&v))
    }

    /// 领取一个活动；返回上游 JSON（`data.id` 非空时调用方需再 confirm）。
    /// `channel` 用该活动自己的投放通道（IDE / PORTAL），空则退回 IDE。
    pub async fn claim_campaign(
        &self,
        account: &Account,
        id: &Value,
        channel: &str,
    ) -> Result<Value, UpError> {
        let id_text = match id {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            other => other.to_string(),
        };
        let ch = if channel.trim().is_empty() {
            "IDE"
        } else {
            channel.trim()
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let body = serde_json::json!({
            "campaignId": id,
            "channel": ch,
            "idempotentKey": format!("claim_{id_text}_{now_ms}"),
        });
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        self.post_ops_json(OPS_CLAIM_PATH, bytes, account).await
    }

    /// 并发排队状态（`GET /api/v1/concurrency/trace-queues/{traceId}`）。
    pub async fn trace_queue_state(&self, account: &Account, trace_id: &str) -> Result<Value, UpError> {
        let mut extra = BTreeMap::new();
        extra.insert("accept".to_string(), "application/json".to_string());
        let path = format!("{TRACE_QUEUE_PATH}/{trace_id}");
        let resp = self
            .signed(reqwest::Method::GET, &path, Vec::new(), extra, account)
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| UpError::new(502, "read", e.to_string()))?;
        if status >= 400 {
            return Err(error_from_body(status, &bytes));
        }
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| UpError::new(502, "parse", format!("排队状态不是 JSON: {e}")))
    }

    /// 放弃排队（DELETE 同一路径）。
    pub async fn trace_queue_cancel(&self, account: &Account, trace_id: &str) -> Result<(), UpError> {
        let mut extra = BTreeMap::new();
        extra.insert("accept".to_string(), "application/json".to_string());
        let path = format!("{TRACE_QUEUE_PATH}/{trace_id}");
        let resp = self
            .signed(reqwest::Method::DELETE, &path, Vec::new(), extra, account)
            .await?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let bytes = resp.bytes().await.unwrap_or_default();
            return Err(error_from_body(status, &bytes));
        }
        Ok(())
    }

    /// 确认领取（claim 返回 `data.id` 时才需要；失败不影响已到账的积分）。
    pub async fn confirm_campaign(&self, account: &Account, id: &Value) -> Result<(), UpError> {
        let body = serde_json::json!({ "campaignId": id });
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        self.post_ops_json(OPS_CONFIRM_PATH, bytes, account)
            .await
            .map(|_| ())
    }

    async fn post_ops_json(
        &self,
        path: &str,
        body: Vec<u8>,
        account: &Account,
    ) -> Result<Value, UpError> {
        let mut extra = BTreeMap::new();
        extra.insert("agent-type".to_string(), "PromptCenter".to_string());
        let resp = self
            .signed(reqwest::Method::POST, path, body, extra, account)
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| UpError::new(502, "read", e.to_string()))?;
        if status >= 400 {
            return Err(error_from_body(status, &bytes));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| UpError::new(502, "parse", format!("响应不是 JSON: {e}")))?;
        if let Some(err) = embedded_error(&v) {
            return Err(err);
        }
        if let Some(code) = v.get("code").and_then(|x| x.as_i64()) {
            if code != 0 {
                let msg = v
                    .get("message")
                    .or_else(|| v.get("msg"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("领取失败");
                return Err(UpError::new(400, code.to_string(), msg.to_string()));
            }
        }
        Ok(v)
    }

    /// 领取限时福利（幂等：已领返回成功）。官方客户端打开模型菜单即调用。
    /// 这是对账号的写操作；调用方按配置决定是否自动执行。
    pub async fn claim_benefit(&self, account: &Account) -> Result<(), UpError> {
        let url = format!("{}{BENEFIT_CLAIM_PATH}", self.benefit_base);
        let resp = self
            .signed_url(
                reqwest::Method::POST,
                &url,
                b"{}".to_vec(),
                BTreeMap::new(),
                account,
            )
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| UpError::new(502, "read", e.to_string()))?;
        if status >= 400 {
            return Err(error_from_body(status, &bytes));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| UpError::new(502, "parse", format!("福利领取响应不是 JSON: {e}")))?;
        let code = v
            .get("error_code")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if !code.is_empty() && code != "0000" {
            let msg = v
                .get("error_msg")
                .and_then(|x| x.as_str())
                .unwrap_or("福利领取失败");
            return Err(UpError::new(400, code, msg.to_string()));
        }
        Ok(())
    }

    /// 限时福利模型清单（`/api/v1/gateway/config`），返回条目 `benefit=true`。
    pub async fn benefit_models(&self, account: &Account) -> Result<Vec<ModelInfo>, UpError> {
        let url = format!("{}{BENEFIT_CONFIG_PATH}", self.benefit_base);
        let resp = self
            .signed_url(reqwest::Method::GET, &url, Vec::new(), BTreeMap::new(), account)
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| UpError::new(502, "read", e.to_string()))?;
        if status >= 400 {
            return Err(error_from_body(status, &bytes));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| UpError::new(502, "parse", format!("福利模型清单不是 JSON: {e}")))?;
        let code = v
            .get("error_code")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if !code.is_empty() && code != "0000" {
            let msg = v
                .get("error_msg")
                .and_then(|x| x.as_str())
                .unwrap_or("福利模型清单失败");
            return Err(UpError::new(400, code, msg.to_string()));
        }
        let mut out = Vec::new();
        for m in v
            .get("result")
            .and_then(|r| r.get("models"))
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let id = m
                .get("model_id")
                .and_then(|x| x.as_str())
                .or_else(|| m.get("model_name").and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                continue;
            }
            out.push(ModelInfo {
                name: m
                    .get("model_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&id)
                    .to_string(),
                desc: m
                    .get("model_desc")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                context_window: m.get("context_window").and_then(|x| x.as_i64()),
                max_tokens: m.get("max_tokens").and_then(|x| x.as_i64()),
                enable: true,
                supports_images: false,
                display_enabled: true,
                benefit: true,
                id,
            });
        }
        Ok(out)
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
        // 上游 chat 单请求硬上限实测 10 MiB，超限回的是误导性的
        // `InferHub.001001005`「参数非法」。在这里按实际字节数拦下，回可读的 413，
        // 且不轮换、不冷却账号（2026-11-21 社区实证，见 hardened v1.0.1）。
        if body.len() > CHAT_BODY_LIMIT {
            return Err(UpError::new(
                413,
                "request_too_large",
                format!(
                    "请求体 {:.1} MiB 超过码道上游单请求上限 10 MiB（历史消息/工具 schema/内联图片过大），请压缩上下文或开新会话",
                    body.len() as f64 / (1024.0 * 1024.0)
                ),
            ));
        }
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
    /// 限时福利（免费套餐）模型：聊天必须带 `maas_type: benefit` 头。
    pub benefit: bool,
}

/// 由**稳定账号身份**派生会话 id（上游按 `x-ot-session-id` 认会话，每账号并发上限 3）。
///
/// ⚠ 不能用 AK 派生：STS 临时 AK 每次续期都会变，会话 id 跟着变等于过一段时间就换一个
/// 新会话，网关那边旧槽还挂着（2026-11-21 社区实证，magpie `packages/codearts`：
/// 不带该头的请求各自算一个会话、槽位挂约一分钟；官方 agent 放的是 conversation id）。
/// 身份取 `domainId|显示名`（[`Account::identity`]），续期前后一致。
pub fn session_id_for(account: &Account) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(account.identity().as_bytes());
    h.update(b"|codearts-session-v2");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-05 线上原文：凭据过期时上游回 400，错误码在 body 顶层。
    #[test]
    fn apig_0602_security_token_expired_is_credential() {
        let body = br#"{"message":"Bad request: the security token has expired","type":"invalid_request_error","code":"APIG.0602","param":null}"#;
        let e = error_from_body(400, body);
        assert_eq!(e.status, 400);
        assert!(e.credential, "APIG.0602 必须判成凭据错误，否则不会换号：{e}");
    }

    /// 有的网关只回文本，没有 code 字段，也要认出来。
    #[test]
    fn security_token_expired_without_code_is_credential() {
        let e = error_from_body(400, br#"{"message":"the security token has expired"}"#);
        assert!(e.credential, "{e}");
    }

    #[test]
    fn apig_0301_is_still_credential() {
        let body = br#"{"error_code":"APIG.0301","error_msg":"Incorrect IAM authentication information"}"#;
        let e = error_from_body(401, body);
        assert!(e.credential, "{e}");
    }

    /// 业务错误不能误判成凭据错误（会把好账号标死）。
    #[test]
    fn model_not_found_is_not_credential() {
        let body = br#"{"error_code":"InferHub.001001044.404","error_msg":"model not found"}"#;
        let e = error_from_body(404, body);
        assert!(!e.credential, "业务错误被当成凭据错误：{e}");
        assert_eq!(e.status, 404);
    }
    /// 网关超限的伪装错误：APIG.0301 + body too large，不能当凭据问题。
    #[test]
    fn body_too_large_is_not_credential() {
        let body = br#"{"message":"Incorrect IAM authentication information: calc ak sk signature fail:body too large","code":"APIG.0301"}"#;
        let e = error_from_body(401, body);
        assert!(!e.credential, "超限不该冷却/标死账号：{e}");
        assert!(is_body_too_large(&e), "{e}");
    }

    /// 同码但没超限 → 仍然是凭据错误。
    #[test]
    fn signature_fail_without_too_large_is_credential() {
        let body = br#"{"message":"Incorrect IAM authentication information: calc ak sk signature fail","code":"APIG.0301"}"#;
        let e = error_from_body(401, body);
        assert!(e.credential, "{e}");
        assert!(!is_body_too_large(&e));
    }

    /// 会话 id 必须由稳定身份派生：STS 续期会换 AK，拿 AK 派生等于定期换会话，
    /// 上游并发槽位会按多个会话累计（2026-11-21 社区实证）。
    #[test]
    fn session_id_survives_ak_rotation() {
        let mut a = Account {
            access_key: "AK-OLD".into(),
            secret_key: "SK".into(),
            security_token: "T1".into(),
            domain_id: Some("dom1".into()),
            name: "hw123".into(),
            ..Default::default()
        };
        let before = session_id_for(&a);
        a.access_key = "AK-NEW".into();
        a.security_token = "T2".into();
        assert_eq!(before, session_id_for(&a), "续期换了 AK 也必须同会话");
        assert!(before.starts_with("ses_"));
        // 换账号 = 换会话
        a.name = "hw456".into();
        assert_ne!(before, session_id_for(&a));
    }

    /// 上游 chat 单请求上限 10 MiB：本地拦下并回可读的 413，不联网、不算凭据错误。
    #[tokio::test]
    async fn oversized_chat_body_is_rejected_locally() {
        let up = Upstream::new("http://127.0.0.1:1").unwrap();
        let e = up
            .chat(
                &Account::default(),
                "glm-5.3",
                vec![b'x'; CHAT_BODY_LIMIT + 1],
                false,
                "ses_x",
            )
            .await
            .unwrap_err();
        assert_eq!(e.status, 413);
        assert_eq!(e.code, "request_too_large");
        assert!(!e.credential, "请求体超限不是凭据错误");
    }
}
