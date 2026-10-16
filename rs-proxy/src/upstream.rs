//! 出站 HTTP 客户端（对应 Go 的 `http.Client{Timeout: 300s}` 与 `http.DefaultTransport`）。
//!
//! 与 Go 的对齐口径：
//! * **总超时 300s**（`defaultTimeout`）——含连接、等响应头与读体（Go 的
//!   `http.Client.Timeout` 确实罩住整次请求，含读 body）；
//! * **不设连接超时以外的分项超时**：Go 的 `DefaultTransport` 只有 `DialContext` 的 30s
//!   连接超时 + `IdleConnTimeout` 90s，本 crate 逐条对应；
//! * 代理：Go 的 `http.DefaultTransport` 默认 `ProxyFromEnvironment`（本地回环豁免）。
//!   reqwest 的 `ClientBuilder` 同样默认读环境变量并豁免回环，故**不显式设 proxy**，
//!   只在启动时按 Go `internal/bai/server.go:202` 的同款口径**报存在性，不打印值**。

use std::time::Duration;

use crate::build::DEFAULT_TIMEOUT;

/// 出站客户端。
pub struct UpstreamClient {
    client: reqwest::Client,
    /// 是否检测到代理环境变量（启动时探测一次，只用于日志/`/health` 展示）。
    pub proxy_env_detected: bool,
    /// 生效的总超时（供展示与测试断言）。
    pub total_timeout: Duration,
}

impl Default for UpstreamClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UpstreamClient {
    pub fn new() -> UpstreamClient {
        let proxy_env_detected = proxy_env_present();
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("构造 reqwest 客户端失败");
        UpstreamClient {
            client,
            proxy_env_detected,
            total_timeout: DEFAULT_TIMEOUT,
        }
    }

    /// 用给定超时构造（测试用；生产走 [`UpstreamClient::new`] 的 Go 口径）。
    pub fn with_timeout(total: Duration) -> UpstreamClient {
        let proxy_env_detected = proxy_env_present();
        let client = reqwest::Client::builder()
            .timeout(total)
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("构造 reqwest 客户端失败");
        UpstreamClient {
            client,
            proxy_env_detected,
            total_timeout: total,
        }
    }

    /// 指向本机假上游（测试用；不做 TLS）。
    pub fn for_base_url(base: &str) -> UpstreamClient {
        let _ = base;
        UpstreamClient::new()
    }

    pub async fn send(&self, req: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        self.client.execute(req).await
    }

    pub fn raw(&self) -> &reqwest::Client {
        &self.client
    }
}

/// 检测代理环境变量（**只报存在性，绝不返回值**——与 Go `internal/bai/server.go:202` 同款口径，
/// 变量值里可能带凭据）。
pub fn proxy_env_present() -> bool {
    for k in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

/// 代理探测的日志文案（与 Go 同款：只说「检测到」，不说值）。
pub fn proxy_log_line(detected: bool) -> String {
    if detected {
        "检测到 HTTPS_PROXY/https_proxy 环境变量，使用 http.DefaultTransport（按环境变量走代理）".to_string()
    } else {
        "未检测到代理环境变量，直连上游".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_300s_like_go() {
        let c = UpstreamClient::new();
        assert_eq!(c.total_timeout, Duration::from_secs(300));
    }

    #[test]
    fn proxy_log_never_contains_the_value() {
        let line = proxy_log_line(true);
        assert!(line.contains("检测到"));
        assert!(!line.contains("http://"), "日志不得出现代理地址：{line}");
    }
}
