//! `x-opencode-session` 会话头 —— `internal/tuanjie/opencode_session.go` 的对应物。
//!
//! 依据 opencode.ai 官方文档（"Where can I use it"）：客户端须为每段会话发送**稳定**的
//! `x-opencode-session` 头，供服务端做路由优化、prompt 缓存与防滥用监测。缺头会被
//! Console 按出口分片以 `400 MissingSessionID` 拒绝（CLAUDE.md 坑 18）。
//!
//! Go 侧事实（`opencode_session.go`，只读核对）：
//! - `opencodeSession()`（`:21-24`）= `sync.Once` + `uuid.NewString()`，**进程级稳定**；
//! - `applyOpenCodeSession(req, baseURL)`（`:29-34`）= `strings.Contains(baseURL, "opencode.ai")`
//!   —— **大小写敏感**，命中才 `req.Header.Set("x-opencode-session", …)`；
//! - `req == nil` 时直接返回（Rust 侧由 `&mut` 引用天然保证非空）。

use std::sync::OnceLock;

use reqwest::header::HeaderMap;

/// 会话头名 —— `opencode_session.go:33`。
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// 进程级稳定的会话标识（UUID v4，懒生成一次）—— `opencode_session.go:21-24`。
pub fn opencode_session() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

/// baseURL 是否指向 opencode.ai（**大小写敏感**，对齐 Go 的 `strings.Contains`）。
pub fn is_opencode_base(base_url: &str) -> bool {
    base_url.contains("opencode.ai")
}

/// 出站前套用会话头 —— `applyOpenCodeSession`（`opencode_session.go:29-34`）。
/// 非 opencode 渠道原样不动。
pub fn apply_opencode_session(base_url: &str, headers: &mut HeaderMap) {
    if !is_opencode_base(base_url) {
        return;
    }
    if let Ok(v) = reqwest::header::HeaderValue::from_str(opencode_session()) {
        headers.insert(
            reqwest::header::HeaderName::from_static(OPENCODE_SESSION_HEADER),
            v,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_is_stable_uuid_v4() {
        let a = opencode_session();
        let b = opencode_session();
        assert_eq!(a, b, "进程级稳定");
        let u = uuid::Uuid::parse_str(a).expect("是合法 UUID");
        assert_eq!(u.get_version_num(), 4, "v4");
    }

    #[test]
    fn apply_only_for_opencode_base_case_sensitive() {
        for (base, want) in [
            ("https://opencode.ai/zen", true),
            ("https://api.opencode.ai/v1", true),
            ("opencode.ai", true),
            ("https://xopencode.ai", true),
            ("https://OPENCODE.AI", false),
            ("https://example.com", false),
            ("", false),
        ] {
            let mut h = HeaderMap::new();
            apply_opencode_session(base, &mut h);
            assert_eq!(
                h.contains_key(OPENCODE_SESSION_HEADER),
                want,
                "base={base:?}"
            );
        }
    }
}
