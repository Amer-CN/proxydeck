//! 本机登录态读取——换 key 的输入（`client.go:160-265`）。
//!
//! `~/.codely-cli/oauth_creds.json` 的 `access_token` 是换 `cli_api_key` 的前置；
//! 桌面端切换账号会重写该文件，所以缓存以「文件指纹（mtime + size）」为键，
//! 指纹变了才重读（`client.go:174-194` 的 tokenCache 口径）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use crate::{Error, Result};

/// 相对登录态路径——`client.go:164`（home 解析失败时的兜底形态）。
pub const OAUTH_CREDS_REL: &str = ".codely-cli/oauth_creds.json";

/// 登录态文件路径（对应 `oauthCredsPath`，`client.go:161-167`）。
/// Windows 上 Go `os.UserHomeDir()` 取 `%USERPROFILE%`。
pub fn oauth_creds_path() -> PathBuf {
    oauth_creds_path_in(std::env::var("USERPROFILE").ok())
}

/// 由 home 目录解析路径——把「取环境变量」与「拼路径」拆开，测试不必改进程环境。
pub fn oauth_creds_path_in(home: Option<String>) -> PathBuf {
    match home {
        Some(home) if !home.is_empty() => PathBuf::from(home)
            .join(".codely-cli")
            .join("oauth_creds.json"),
        _ => PathBuf::from(OAUTH_CREDS_REL),
    }
}

#[derive(serde::Deserialize)]
struct OauthCreds {
    #[serde(default)]
    access_token: String,
}

/// 登录态文件的轻量指纹（mtime 纳秒 + 字节数）——对应 `accessTokenFP`，`client.go:176-182`。
/// 不读内容（避免每次请求解析 JWT）；文件不存在返回 `None`。
pub fn access_token_fp() -> Option<String> {
    let meta = std::fs::metadata(oauth_creds_path()).ok()?;
    let nanos = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Some(format!("{nanos}-{}", meta.len()))
}

/// 本地登录态内容缓存（键 = 指纹）——对应 `client.go:189-194` 的包级缓存。
static TOKEN_CACHE: Mutex<Option<(String, String)>> = Mutex::new(None);

/// 读取本地登录态（文件未变时回缓存）——对应 `loadAccessToken`，`client.go:197-224`。
pub fn load_access_token() -> Result<String> {
    let Some(fp) = access_token_fp() else {
        return Err(Error::NoLoginState);
    };
    {
        let cache = TOKEN_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_fp, token)) = cache.as_ref() {
            if *cached_fp == fp {
                return Ok(token.clone());
            }
        }
    }
    let bytes = std::fs::read(oauth_creds_path()).map_err(Error::ReadLoginState)?;
    // 解 JSON 失败才算错；缺 access_token 是另一条独立错误（与 Go 逐条对应）
    let creds: OauthCreds = serde_json::from_slice(&bytes).map_err(Error::ParseLoginState)?;
    if creds.access_token.is_empty() {
        return Err(Error::EmptyAccessToken);
    }
    let mut cache = TOKEN_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = Some((fp, creds.access_token.clone()));
    Ok(creds.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reports_no_login_state() {
        // 用一个必定不存在的 home 隔离真实 HOME（不经环境变量，不读用户真登录态）
        let p = oauth_creds_path_in(Some(r"Z:\__p8a_nonexistent_home__".into()));
        assert!(!p.exists());
        assert!(std::fs::metadata(&p).is_err());
    }

    #[test]
    fn relative_path_when_home_unset() {
        assert_eq!(
            oauth_creds_path_in(None),
            PathBuf::from(OAUTH_CREDS_REL)
        );
        assert_eq!(
            oauth_creds_path_in(Some(String::new())),
            PathBuf::from(OAUTH_CREDS_REL)
        );
    }

    #[test]
    fn path_joins_dot_codely_cli() {
        let p = oauth_creds_path_in(Some(r"C:\Users\someone".into()));
        assert!(p.ends_with(r".codely-cli\oauth_creds.json"), "{p:?}");
    }
}
