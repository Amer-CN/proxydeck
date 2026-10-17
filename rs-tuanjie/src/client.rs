//! 换 key / key 缓存 / 转发（`client.go:139-555`）。
//!
//! - **红线 5**：换 key 只打 `codely.tuanjie.cn/api/api-token/cli-api-key`（GET），
//!   不换源、不加 `?teamId`；成功才更新「所用 access_token 的指纹」
//! - key 缓存 24h；`access_token` 指纹变化（桌面端切号）→ 缓存作废重换
//!   （`client.go:148-152` 注释：否则官方踢号后返回 400 而非 401，`InvalidateKey` 触发不了）
//! - 转发 401 → 清缓存重换一次（`client.go:548-553`）
//!
//! **未移植**：`smartProxy` 的 Windows 注册表分支属 P8c（`proxy_windows.go`）；
//! 本轮的 reqwest 客户端按默认口径走环境变量代理（与 Go 的第一步一致）。
//! Go `ForwardWithSession` 的 `contentType` 形参全程未被使用（死参数），这里不带。

use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::headers::{key_fetch_headers, litellm_headers};
use crate::session::LitellmSession;
use crate::transport::build_http_client;
use crate::{
    credentials, Error, Result, CODEY_API_BASE, KEY_CACHE_TTL, KEY_FETCH_TIMEOUT, LITELLM_API_BASE,
};

/// **测试缝**（`main.rs` 只在 `RS_TUANJIE_TEST_BASES` 显式存在、且端口不是 8788 时写入）：
/// 把两个域一起指向本机假上游，供 P8b 第三片的差分对照用。
/// 可重复覆写（`Mutex`）：门禁全程串行（`env_lock`），各用例按需重定向；
/// 生产只在启动时写一次，行为与 `OnceLock` 无异；生产不带该变量 ⇒ 恒为两个官方常量。
static TEST_BASES: Mutex<Option<(String, String)>> = Mutex::new(None);

/// 设置测试缝的两个 base（可重复覆写；调用方须持串行锁，见各门禁的 `env_lock` 惯例）。
pub fn set_test_bases(codey_api_base: String, litellm_api_base: String) {
    *TEST_BASES.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((codey_api_base, litellm_api_base));
}

fn default_codey_base() -> String {
    TEST_BASES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .map(|(c, _)| c)
        .unwrap_or_else(|| CODEY_API_BASE.to_string())
}

fn default_litellm_base() -> String {
    TEST_BASES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .map(|(_, l)| l)
        .unwrap_or_else(|| LITELLM_API_BASE.to_string())
}

/// access_token 来源（默认读本机登录态文件；测试缝见 [`Client::with_token_loader`]）。
pub type TokenLoader = Arc<dyn Fn() -> Result<String> + Send + Sync>;

#[derive(Default)]
struct KeyState {
    api_key: String,
    key_at: Option<Instant>,
    /// 换取 key 所用 access_token 的指纹（仅成功换取时更新）——`client.go:148-152`。
    token_fp: Option<String>,
    /// 首次未采样前不做失效判断（避免误杀首次换取）——`client.go:152`。
    fp_ok: bool,
}

/// 团结 LiteLLM 转发客户端（对应 Go `Client`）。
pub struct Client {
    http: reqwest::Client,
    codey_api_base: String,
    litellm_api_base: String,
    user_agent: String,
    state: Mutex<KeyState>,
    token_loader: TokenLoader,
}

impl Client {
    /// 创建客户端（对应 `NewClient`，`client.go:156-158`）。
    pub fn new() -> Result<Self> {
        // 代理来源只报**来源名**（环境变量 / 注册表 / 无），绝不打印代理 URL ——
        // URL 可能带 `user:pass@`（P8c 卫生要求）。
        crate::logf(&format!(
            "[tuanjie] 出站代理来源：{}（本地/内网地址恒直连）",
            crate::transport::proxy_source().as_str()
        ));
        Ok(Self {
            http: build_http_client()?,
            codey_api_base: default_codey_base(),
            litellm_api_base: default_litellm_base(),
            user_agent: crate::ua::process_cli_user_agent().to_string(),
            state: Mutex::new(KeyState::default()),
            token_loader: Arc::new(credentials::load_access_token),
        })
    }

    /// 测试缝：替换出站客户端。
    #[doc(hidden)]
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// 测试缝：替换域名（本地假上游用；生产一律用两个域常量）。
    #[doc(hidden)]
    pub fn with_bases(mut self, codey_api_base: &str, litellm_api_base: &str) -> Self {
        self.codey_api_base = codey_api_base.to_string();
        self.litellm_api_base = litellm_api_base.to_string();
        self
    }

    /// 测试缝：替换 access_token 来源（**不读本机真登录态**）。
    #[doc(hidden)]
    pub fn with_token_loader(mut self, loader: TokenLoader) -> Self {
        self.token_loader = loader;
        self
    }

    /// 测试缝：替换 UA（生产用启动时解析的进程级值）。
    #[doc(hidden)]
    pub fn with_user_agent_for_test(mut self, user_agent: &str) -> Self {
        self.user_agent = user_agent.to_string();
        self
    }

    /// 当前进程 UA（红线 6）。
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// 换 key 端点（默认 = [`CLI_API_KEY_URL`]）。
    pub fn cli_api_key_url(&self) -> String {
        format!("{}/api/api-token/cli-api-key", self.codey_api_base)
    }

    /// 账号域 base（生产 = [`crate::CODEY_API_BASE`]）——`/health`、积分端点的形态用。
    pub fn codey_base(&self) -> &str {
        &self.codey_api_base
    }

    /// 推理域 base（生产 = [`crate::LITELLM_API_BASE`]）——`/health` 的 `backend` 字段用。
    pub fn litellm_base(&self) -> &str {
        &self.litellm_api_base
    }

    /// 出站 HTTP 客户端（积分拉取 `fetch_quota_cached` 与转发共用同一个栈）。
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 转发一次 GET（对应 Go 的 `Forward(ctx, http.MethodGet, path, nil, "")`）——
    /// `/v1/models`、`/model/info` 这类读路径用。上游 401 清缓存重换一次。
    ///
    /// 会话 id 走 [`crate::session::new_uuid_v4`]——Go 的 `Forward(…, sess=nil)` 经
    /// `litellmHeaders` 内部 `newLitellmSession()` 现生成一次性 UUID，口径相同。
    pub async fn forward_get(&self, path: &str) -> Result<reqwest::Response> {
        let resp = self.forward_get_once(path).await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            drop(resp);
            self.invalidate_key();
            return self.forward_get_once(path).await;
        }
        Ok(resp)
    }

    async fn forward_get_once(&self, path: &str) -> Result<reqwest::Response> {
        let key = self.api_key_cached().await?;
        let url = format!("{}{}", self.litellm_api_base, path);
        let mut req = self.http.get(url);
        let sess = crate::session::new_uuid_v4();
        for (k, v) in litellm_headers(path, &key, &sess, now_unix(), &self.user_agent) {
            req = req.header(k, v);
        }
        req.send().await.map_err(Error::Forward)
    }

    /// 用 access_token 换 cli_api_key（对应 `fetchKey`，`client.go:227-265`）。
    pub async fn fetch_key(&self) -> Result<String> {
        let at = (self.token_loader)()?;
        let mut req = self
            .http
            .get(self.cli_api_key_url())
            .timeout(KEY_FETCH_TIMEOUT);
        for (k, v) in key_fetch_headers(&at, &self.user_agent) {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(Error::FetchKeyNetwork)?;
        let status = resp.status();
        let body = read_body_limited(resp, 1 << 20).await;
        if status != reqwest::StatusCode::OK {
            return Err(Error::FetchKeyStatus {
                status: status.as_u16(),
                body: truncate_bytes(&body, 200),
            });
        }
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            cli_api_key: String,
        }
        match serde_json::from_slice::<Out>(&body) {
            Ok(out) if !out.cli_api_key.is_empty() => {
                // 记录本次换取所用 access_token 的指纹（换 key 成功才更新）
                if let Some(fp) = credentials::access_token_fp() {
                    let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.token_fp = Some(fp);
                    st.fp_ok = true;
                }
                Ok(out.cli_api_key)
            }
            _ => Err(Error::FetchKeyBody(truncate_bytes(&body, 200))),
        }
    }

    /// 用指定 access_token 换 cli_api_key —— `fetchKeyWithToken`（`handlers_extra.go` 里
    /// `ForwardDirect` 调的那个包级函数）。与 [`Client::fetch_key`] 的差别只有 token 来源：
    /// 池路径**按账号**换 key，**不共享**单账号的 key 缓存。
    pub async fn fetch_key_with_token(&self, access_token: &str) -> Result<String> {
        let mut req = self
            .http
            .get(self.cli_api_key_url())
            .timeout(KEY_FETCH_TIMEOUT);
        for (k, v) in key_fetch_headers(access_token, &self.user_agent) {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(Error::Forward)?;
        let status = resp.status();
        let body = read_body_limited(resp, 1 << 20).await;
        if status != reqwest::StatusCode::OK {
            return Err(Error::FetchKeyStatus {
                status: status.as_u16(),
                body: truncate_bytes(&body, 200),
            });
        }
        #[derive(serde::Deserialize)]
        struct Out {
            #[serde(default)]
            cli_api_key: String,
        }
        match serde_json::from_slice::<Out>(&body) {
            Ok(out) if !out.cli_api_key.is_empty() => Ok(out.cli_api_key),
            _ => Err(Error::FetchKeyBody(truncate_bytes(&body, 200))),
        }
    }

    /// 返回有效的 cli_api_key（过期/换号自动换取）——对应 `apiKeyCached`，`client.go:268-294`。
    pub async fn api_key_cached(&self) -> Result<String> {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // 账号指纹变化（桌面端切换了登录账号）→ 缓存 key 作废
            if !st.api_key.is_empty() && st.fp_ok {
                if let Some(fp) = credentials::access_token_fp() {
                    if Some(&fp) != st.token_fp.as_ref() {
                        st.api_key.clear();
                        st.key_at = None;
                    }
                }
            }
            if !st.api_key.is_empty() {
                if let Some(at) = st.key_at {
                    if at.elapsed() < KEY_CACHE_TTL {
                        return Ok(st.api_key.clone());
                    }
                }
            }
        }
        let k = self.fetch_key().await?;
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.api_key = k.clone();
            st.key_at = Some(Instant::now());
        }
        Ok(k)
    }

    /// 清除缓存的 key（401 时调用，下次重新换取）——`client.go:297-302`。
    pub fn invalidate_key(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.api_key.clear();
        st.key_at = None;
    }

    /// 转发一次（不发 401 重试）——对应 `ForwardWithSession` 的 `doForward`。
    async fn forward_once(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
        sess: &LitellmSession,
    ) -> Result<reqwest::Response> {
        let key = self.api_key_cached().await?;
        let url = format!("{}{}", self.litellm_api_base, path);
        let mut req = self.http.request(method, url).body(body);
        for (k, v) in litellm_headers(path, &key, &sess.id, now_unix(), &self.user_agent) {
            req = req.header(k, v);
        }
        req.send().await.map_err(Error::Forward)
    }

    /// 转发到 LiteLLM；上游 401 时清缓存重换一次（对应 `Forward`/`ForwardWithSession`，
    /// `client.go:512-555`）。请求体先进内存，保证重试能完整重放。
    pub async fn forward(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
        sess: &LitellmSession,
    ) -> Result<reqwest::Response> {
        let resp = self
            .forward_once(method.clone(), path, body.clone(), sess)
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            drop(resp);
            self.invalidate_key();
            return self.forward_once(method, path, body, sess).await;
        }
        Ok(resp)
    }
}

/// 当前时间戳（秒）——对应 Go `time.Now().Unix()`。
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 读满上限字节的响应体（对应 Go `io.ReadAll(io.LimitReader(body, n))`）。
async fn read_body_limited(resp: reqwest::Response, limit: usize) -> Vec<u8> {
    match resp.bytes().await {
        Ok(b) => {
            let mut v = b.to_vec();
            v.truncate(limit);
            v
        }
        Err(_) => Vec::new(),
    }
}

/// 错误文案里的截断（对应 Go `truncate`，`client.go:557-562`：按**字节**截，超长加 `...`）。
pub fn truncate_bytes(b: &[u8], n: usize) -> String {
    if b.len() <= n {
        return String::from_utf8_lossy(b).into_owned();
    }
    format!("{}...", String::from_utf8_lossy(&b[..n]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CLI_API_KEY_URL;

    #[test]
    fn default_endpoints_match_red_line_five() {
        assert_eq!(
            CLI_API_KEY_URL,
            "https://codely.tuanjie.cn/api/api-token/cli-api-key"
        );
        assert!(!CLI_API_KEY_URL.contains('?'), "红线 5：不加 ?teamId");
        assert_eq!(CODEY_API_BASE, "https://codely.tuanjie.cn");
        assert_eq!(LITELLM_API_BASE, "https://codely-litellm.tuanjie.cn");
    }

    #[test]
    fn truncate_is_byte_based_like_go() {
        assert_eq!(truncate_bytes(b"abc", 5), "abc");
        assert_eq!(truncate_bytes("路径".as_bytes(), 3), "路...");
    }
}
