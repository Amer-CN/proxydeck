//! 转发请求头——**红线 4/6/7** 的头集合部分（`client.go:482-507`）。
//!
//! 头集合 = 官方 CLI 实际发送的最小集（`client.go:485-494` 的四条实证）：
//!
//! 1. `Authorization: Bearer <cli_api_key>` 必带——2026-08-27 排查教训：漏了会得
//!    401「请升级到最新版」（响应里 `param=x-codely-signature` 具有误导性）
//! 2. `X-DashScope-*` **不带**——官方仅 `isDashScopeProvider()` 路线发，团结 LiteLLM
//!    路线官方不发（**红线 7**）
//! 3. B3 追踪头（`x-b3-*`）不带——telemetry 默认关且未注册 propagator
//! 4. `User-Agent` = [`crate::ua`] 解析值（**红线 6**），`x-litellm-session-id` 与会话同值
//!    （**红线 4**），`X-Codely-Signature` = [`crate::sign::sign_litellm`]（**红线 3**）
//!
//! **大小写**：Go 用 `http.Header.Set`，键经 `textproto.CanonicalMIMEHeaderKey` 规范化，
//! 线上实际发出的是 `X-Litellm-Session-Id`（不是小写形态）——已由 `.work/p8a/go-outbound-wire.txt`
//! 实抓确认。故本模块直接产出**线上真值大小写**。

use crate::sign::sign_litellm;

/// 认证头名——`client.go:500`。
pub const HEADER_AUTHORIZATION: &str = "Authorization";
/// 请求体类型——`client.go:501`。
pub const HEADER_CONTENT_TYPE: &str = "Content-Type";
/// 响应类型——`client.go:502`。
pub const HEADER_ACCEPT: &str = "Accept";
/// UA——`client.go:503`（**红线 6**）。
pub const HEADER_USER_AGENT: &str = "User-Agent";
/// 会话 id——`client.go:504`（**红线 4**）；线上形态为 Go 规范化后的 `X-Litellm-Session-Id`。
pub const HEADER_LITELLM_SESSION_ID: &str = "X-Litellm-Session-Id";
/// 签名——`client.go:505`（**红线 3**）。
pub const HEADER_CODELY_SIGNATURE: &str = "X-Codely-Signature";

/// 本模块**禁止**出现的头前缀/名字（红线 7 + 次一级差异）。
pub const FORBIDDEN_HEADER_PREFIXES: [&str; 2] = ["X-DashScope-", "x-b3-"];

/// 构造伪装 codely CLI 的请求头。返回顺序 = **Go 侧线上实抓的写出顺序**
/// （`.work/p8a/go-outbound-wire.txt`：UA 由 net/http 单独先写，其余按 key 排序后写出）。
///
/// `path` 参与签名（是上游路径），`now_unix` 由调用方给（Go 侧取 `time.Now()`）。
pub fn litellm_headers(
    path: &str,
    cli_api_key: &str,
    session_id: &str,
    now_unix: i64,
    user_agent: &str,
) -> Vec<(String, String)> {
    vec![
        (HEADER_USER_AGENT.into(), user_agent.to_string()),
        (HEADER_ACCEPT.into(), "application/json".into()),
        (
            HEADER_AUTHORIZATION.into(),
            format!("Bearer {cli_api_key}"),
        ),
        (HEADER_CONTENT_TYPE.into(), "application/json".into()),
        (
            HEADER_CODELY_SIGNATURE.into(),
            sign_litellm(path, cli_api_key, now_unix),
        ),
        (
            HEADER_LITELLM_SESSION_ID.into(),
            session_id.to_string(),
        ),
    ]
}

/// 头名字集合按字节序（Go `http.Header` 写成 map 后按 key 排序的形态）——用于「头集合对照」。
pub fn sorted_header_names(headers: &[(String, String)]) -> Vec<String> {
    let mut names: Vec<String> = headers.iter().map(|(k, _)| k.clone()).collect();
    names.sort();
    names
}

/// 换 key 请求的头（`client.go:238-240`）：
/// `Authorization: Bearer <access_token>` + `Accept: application/json` + `User-Agent`。
pub fn key_fetch_headers(access_token: &str, user_agent: &str) -> Vec<(String, String)> {
    vec![
        (
            HEADER_AUTHORIZATION.into(),
            format!("Bearer {access_token}"),
        ),
        (HEADER_ACCEPT.into(), "application/json".into()),
        (HEADER_USER_AGENT.into(), user_agent.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_set_matches_go_golden() {
        let h = litellm_headers(
            "/v1/chat/completions",
            "sk-test-key-1234567890",
            "11111111-2222-4333-8444-555555555555",
            1750000000,
            "codely-cli/1.0.0-rc.60 (win32; x64)",
        );
        // 期望值来自 Go 侧实抓（.work/p8a/golden-p8a-go.json 的 header_set 段）按字节序排
        assert_eq!(
            sorted_header_names(&h).join(", "),
            "Accept, Authorization, Content-Type, User-Agent, X-Codely-Signature, X-Litellm-Session-Id"
        );
        let get = |k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("Accept").unwrap(), "application/json");
        assert_eq!(
            get("Authorization").unwrap(),
            "Bearer sk-test-key-1234567890"
        );
        assert_eq!(get("Content-Type").unwrap(), "application/json");
        assert_eq!(
            get("X-Litellm-Session-Id").unwrap(),
            "11111111-2222-4333-8444-555555555555"
        );
        assert!(get("X-Codely-Signature")
            .unwrap()
            .starts_with("v1.1750000000."));
    }

    #[test]
    fn no_dashscope_or_b3_headers() {
        let h = litellm_headers("/v1/models", "k", "s", 1, "codely-cli/x (win32; x64)");
        for (name, _) in &h {
            for bad in FORBIDDEN_HEADER_PREFIXES {
                assert!(
                    !name.starts_with(bad),
                    "红线 7：不该发 {name}（前缀 {bad}）"
                );
            }
        }
        assert_eq!(h.len(), 6, "头集合恰好 6 个");
    }

    #[test]
    fn key_fetch_headers_shape() {
        let h = key_fetch_headers("AT-123", "codely-cli/1.0.0-rc.60 (win32; x64)");
        assert_eq!(sorted_header_names(&h).join(", "), "Accept, Authorization, User-Agent");
        assert_eq!(
            h.iter().find(|(n, _)| n == "Authorization").unwrap().1,
            "Bearer AT-123"
        );
    }
}
