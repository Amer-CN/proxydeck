//! `internal/version/version.go`：CommandCode CLI 的版本号（出站头 `x-command-code-version`）。
//!
//! Go 的做法：启动时（包 `init` 的 goroutine）拉一次 npm registry 的
//! `https://registry.npmjs.org/command-code/latest`，缓存 30 分钟；拉取失败时返回
//! 上次成功的版本，从没成功过则返回 `"unknown"`。

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 版本缓存时长（Go 的 `30 * time.Minute`）。
pub const VERSION_CACHE: Duration = Duration::from_secs(30 * 60);

/// npm registry 的 latest 端点（Go 里是硬编码的 URL）。
pub const NPM_LATEST_URL: &str = "https://registry.npmjs.org/command-code/latest";

/// 拉取失败且从未成功时的兜底（Go 返回 `"unknown"`）。
pub const UNKNOWN: &str = "unknown";

struct Cache {
    version: Option<String>,
    at: Option<Instant>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn with_cache<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(Cache {
            version: None,
            at: None,
        });
    }
    f(g.as_mut().expect("刚初始化"))
}

/// 缓存里的当前版本（不发起网络请求）。
pub fn cached_version() -> Option<String> {
    with_cache(|c| {
        c.at.and_then(|at| {
            if at.elapsed() < VERSION_CACHE {
                c.version.clone()
            } else {
                None
            }
        })
    })
}

/// `GetCommandCodeVersion` 的等价物（async 版）：命中缓存直接返回；否则拉一次。
pub async fn get_command_code_version() -> String {
    if let Some(v) = cached_version() {
        return v;
    }
    match fetch_version_from_npm().await {
        Ok(v) => {
            let v2 = v.clone();
            with_cache(|c| {
                c.version = Some(v);
                c.at = Some(Instant::now());
            });
            v2
        }
        Err(_) => with_cache(|c| c.version.clone()).unwrap_or_else(|| UNKNOWN.to_string()),
    }
}

/// 启动时的后台预热（对应 Go 的 `init()` goroutine）：失败只记状态、不阻塞。
pub fn warm_up() {
    tokio::spawn(async {
        let _ = get_command_code_version().await;
    });
}

/// 测试/门禁用：注入一个固定版本（避免门禁依赖网络）。
pub fn set_version_for_test(v: &str) {
    with_cache(|c| {
        c.version = Some(v.to_string());
        c.at = Some(Instant::now());
    });
}

/// `fetchVersionFromNPM`：拉 latest 并取 `version` 字段。
pub async fn fetch_version_from_npm() -> Result<String, String> {
    let resp = reqwest::get(NPM_LATEST_URL)
        .await
        .map_err(|e| format!("failed to fetch version: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(format!(
            "npm registry returned {status}: {}",
            String::from_utf8_lossy(&body)
        ));
    }
    let v = crate::godecode::parse_value(&body)
        .map_err(|e| format!("failed to parse version: {e}"))?;
    match v.get("version").and_then(|x| x.as_str()) {
        Some(s) => Ok(s.to_string()),
        None => Err("version field not found or invalid".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_serves_injected_version() {
        set_version_for_test("9.9.9-test");
        assert_eq!(cached_version().as_deref(), Some("9.9.9-test"));
    }

    #[test]
    fn unknown_is_the_documented_fallback() {
        assert_eq!(UNKNOWN, "unknown");
    }
}
