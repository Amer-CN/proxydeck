//! `internal/proxy/probe.go`：CommandCode API key 的探活。
//!
//! 用途：GUI 主进程自己持有 `api-key.txt` 里的 key（headless 子进程的 401 主进程拿不到），
//! 存储 key 失效时前端毫无感知。`whoami` 是 `/v1/usage` 在用的轻量免费端点，零生成计费。
//!
//! 返回 `("ok"|"unauthorized"|"error", httpCode)`——`httpCode` 在网络错误时为 0。

use std::time::Duration;

/// 探活的超时（Go 的 `10 * time.Second`）。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// `ProbeKey`。
pub async fn probe_key(base_url: &str, api_key: &str) -> (String, u16) {
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => return ("error".to_string(), 0),
    };
    let url = format!("{base_url}/alpha/whoami");
    let resp = match client
        .get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return ("error".to_string(), 0),
    };
    let code = resp.status().as_u16();
    match code {
        200 => ("ok".to_string(), 200),
        401 => ("unauthorized".to_string(), 401),
        other => ("error".to_string(), other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn network_error_reports_zero_code() {
        // 指向一个必然不可达的回环端口
        let (status, code) = probe_key("http://127.0.0.1:1", "k").await;
        assert_eq!(status, "error");
        assert_eq!(code, 0);
    }
}
