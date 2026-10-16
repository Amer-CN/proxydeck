//! CLI User-Agent 解析——**红线 6**（`client.go:46-90`）。
//!
//! 官方 HTTP UA 真值 = `codely-cli/<本机 npm 版> (win32; x64)`；读不到本机 npm 安装的
//! `package.json` 才用兜底常量（rc.58）。`Codely-CLI - OSS/...` 是 telemetry 埋点字段、
//! **不是** HTTP UA——2026-08-27 曾误用并已修正。
//!
//! 本产品仅跑 Windows，`win32/x64` 写死（`client.go:53` 注释）。

use std::path::PathBuf;

/// 探测不到本机 CLI 版本时的兜底 UA——`client.go:48` 逐字。
pub const CLI_USER_AGENT_FALLBACK: &str = "codely-cli/1.0.0-rc.58 (win32; x64)";

/// Node 的 platform/arch 枚举串（写死，见模块注释）。
pub const UA_PLATFORM: &str = "win32";
/// 见 [`UA_PLATFORM`]。
pub const UA_ARCH: &str = "x64";

/// 读本机 npm 全局 codely-cli 的 `package.json` 版本号（对应 `detectLocalCliVersion`，
/// `client.go:66-87`）。候选路径顺序与 Go 一致：`%APPDATA%` → `%USERPROFILE%\AppData\Roaming`。
/// 返回空串语义在 Rust 里是 `None`。
pub fn detect_local_cli_version() -> Option<String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.is_empty() {
            candidates.push(package_json_under(PathBuf::from(appdata).join("npm")));
        }
    }
    // Go `os.UserHomeDir()` 在 Windows 上取 %USERPROFILE%。
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.is_empty() {
            candidates.push(package_json_under(
                PathBuf::from(home).join("AppData").join("Roaming").join("npm"),
            ));
        }
    }
    for p in candidates {
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let Ok(v) = serde_json::from_slice::<PkgVersion>(&bytes) else {
            continue;
        };
        if is_valid_cli_version(&v.version) {
            return Some(v.version);
        }
    }
    None
}

fn package_json_under(npm_dir: PathBuf) -> PathBuf {
    npm_dir
        .join("node_modules")
        .join("@unity-china")
        .join("codely-cli")
        .join("package.json")
}

/// `{"version": "..."}`。Go 侧版本字段不是字符串时 `json.Unmarshal` 失败 → 跳过该候选。
#[derive(serde::Deserialize)]
struct PkgVersion {
    version: String,
}

/// 启动时解析 UA（对应 `resolveCLIUserAgent`，`client.go:56-61`）。
pub fn resolve_cli_user_agent() -> String {
    cli_user_agent_from(detect_local_cli_version())
}

/// 由（可选的）版本号拼 UA——把「探测」与「拼装」拆开便于已知答案测试。
pub fn cli_user_agent_from(version: Option<String>) -> String {
    match version {
        Some(v) => format!("codely-cli/{v} ({UA_PLATFORM}; {UA_ARCH})"),
        None => CLI_USER_AGENT_FALLBACK.to_string(),
    }
}

/// 合法 semver（含 prerelease 后缀，如 `1.0.0-rc.55`）——Go `cliVersionRe` 的逐字等价物
/// （`client.go:90`：`^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$`，RE2 `\d` 为 ASCII 数字、
/// `$` 为文本末尾而非行尾）。
pub fn is_valid_cli_version(v: &str) -> bool {
    let b = v.as_bytes();
    let mut i = 0usize;
    for part in 0..3 {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false; // `\d+` 至少要一位
        }
        if part < 2 {
            if i >= b.len() || b[i] != b'.' {
                return false;
            }
            i += 1;
        }
    }
    if i < b.len() && b[i] == b'-' {
        i += 1;
        let start = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
            i += 1;
        }
        if i == start {
            return false; // 连字符后至少要一个字符
        }
    }
    i == b.len()
}

/// 进程级 UA（Go 是包级 `var cliUserAgent`，`client.go:54`）——启动解析一次。
pub fn process_cli_user_agent() -> &'static str {
    use std::sync::OnceLock;
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(resolve_cli_user_agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_regex_matches_go_semantics() {
        // 期望值由 Go 侧采集（golden-p8a-go.json 的 version_regex 段）逐字照搬。
        for v in ["1.0.0-rc.58", "1.0.0", "1.0.0-release.57"] {
            assert!(is_valid_cli_version(v), "{v} 应合法");
        }
        for v in [
            "", "1.0", "v1.0.0", "1.0.0-", "1.0.0-rc.58 ", "1.0.0+build",
            // Go 的 `$` 只匹配文本末尾，所以带换行的版本串同样不匹配
            "1.0.0\n",
        ] {
            assert!(!is_valid_cli_version(v), "{v:?} 应非法");
        }
    }

    #[test]
    fn fallback_is_byte_exact() {
        assert_eq!(
            CLI_USER_AGENT_FALLBACK,
            "codely-cli/1.0.0-rc.58 (win32; x64)"
        );
        assert_eq!(cli_user_agent_from(None), CLI_USER_AGENT_FALLBACK);
        assert_eq!(
            cli_user_agent_from(Some("1.0.0-rc.60".into())),
            "codely-cli/1.0.0-rc.60 (win32; x64)"
        );
    }

    #[test]
    fn telemetry_field_is_not_the_http_ua() {
        // 红线 6 的反向断言：UA 模板只有 `codely-cli/<ver> (win32; x64)` 一种形态
        let ua = cli_user_agent_from(Some("1.0.0-rc.58".into()));
        assert!(ua.starts_with("codely-cli/"));
        assert!(!ua.contains("OSS"));
        assert!(!ua.contains("Codely-CLI -"));
    }
}
