//! winpaths.rs —— 代理自探测的 Windows 分支（`internal/tuanjie/proxy_windows.go`
//! + `client.go:117-137` 的 `smartProxy`）与桌面端安装目录（`desktop_path_windows.go`）。
//!
//! Go 的判定链：**本地/内网地址先短路**（localhost / 127.0.0.1 / ::1 / 192.168.* /
//! 10.* / 172.16.~172.3*）→ 环境变量 `HTTP_PROXY`/`HTTPS_PROXY`（`http.ProxyFromEnvironment`）
//! → Windows 注册表 `HKCU\...\Internet Settings` 的 `ProxyEnable`/`ProxyServer`。
//!
//! ## 与 Go 的差异（登记）
//!
//! 1. 注册表读取走 `reg.exe query`（见 [`crate::winreg`]）而不是 Windows API；
//! 2. `http.ProxyFromEnvironment` 的完整语义（`NO_PROXY` 的域名/端口/CIDR 匹配、
//!    `HTTP_PROXY` 只对 http 生效、`ALL_PROXY` 的优先级）**不逐位复刻**——本模块给出
//!    Go 三步判定本身，`NO_PROXY` 由调用方（reqwest 的 `NoProxy`）按同一套写法处理；
//! 3. `ProxyServer` 的多段形态（`http=…;https=…`）与 Go 一样**取第一个非空段**。

use std::path::Path;

/// 注册表路径 —— `proxy_windows.go:15`。
pub const INTERNET_SETTINGS_KEY: &str =
    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";

/// 代理探测的三步输入（收成结构体便于门禁逐值对照，不读真实环境）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProxyEnv {
    /// `HTTP_PROXY` / `http_proxy`（Go 的 `http.ProxyFromEnvironment` 认这两个大小写）。
    pub http_proxy: String,
    /// `HTTPS_PROXY` / `https_proxy`。
    pub https_proxy: String,
    /// 注册表 `ProxyEnable`（DWORD 文本，如 `0x1`）。
    pub proxy_enable: Option<String>,
    /// 注册表 `ProxyServer`。
    pub proxy_server: Option<String>,
}

/// 从进程环境与注册表读一次（生产入口）。
pub fn read_proxy_env() -> ProxyEnv {
    ProxyEnv {
        http_proxy: env_any(&["HTTP_PROXY", "http_proxy"]),
        https_proxy: env_any(&["HTTPS_PROXY", "https_proxy"]),
        proxy_enable: read_proxy_enable(),
        proxy_server: read_proxy_server(),
    }
}

fn env_any(names: &[&str]) -> String {
    for n in names {
        if let Ok(v) = std::env::var(n) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    String::new()
}

/// 注册表 `ProxyEnable`（读不到 → `None`）。
pub fn read_proxy_enable() -> Option<String> {
    read_proxy_enable_with("reg")
}

/// 测试缝：命令行可注入（假 `reg` 脚本，绝不真读注册表）。
pub fn read_proxy_enable_with(program: &str) -> Option<String> {
    crate::winreg::query_values_with(program, INTERNET_SETTINGS_KEY)
        .into_iter()
        .find(|(n, _)| n == "ProxyEnable")
        .map(|(_, v)| v.data)
}

/// 注册表 `ProxyServer`（读不到 → `None`）。
pub fn read_proxy_server() -> Option<String> {
    read_proxy_server_with("reg")
}

/// 测试缝：命令行可注入。
pub fn read_proxy_server_with(program: &str) -> Option<String> {
    crate::winreg::query_values_with(program, INTERNET_SETTINGS_KEY)
        .into_iter()
        .find(|(n, _)| n == "ProxyServer")
        .map(|(_, v)| v.data)
}

/// 目标主机是否该短路直连 —— `smartProxy` 开头的本地/内网判定（`client.go:119-126`）。
pub fn is_local_host(host: &str) -> bool {
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return true;
    }
    host.starts_with("192.168.")
        || host.starts_with("10.")
        || host.starts_with("172.16.")
        || host.starts_with("172.17.")
        || host.starts_with("172.18.")
        || host.starts_with("172.19.")
        // Go 侧写的是 `HasPrefix(host, "172.2")` / `"172.3"`——**宽匹配**
        // （172.2*.x 与 172.3*.x 全在内，含 172.200./172.399. 这类非私有段），
        // 逐字照抄，不"顺手改对"
        || host.starts_with("172.2")
        || host.starts_with("172.3")
}

/// `ProxyServer` 的多段形态取第一个有效地址 —— `proxy_windows.go:31-45`。
///
/// 段内 `key=value` 取 `value`；空段跳过；无 scheme 补 `http://`。
pub fn parse_proxy_server(server: &str) -> String {
    for part in server.split(';') {
        let mut addr = part.trim();
        if let Some(eq) = addr.find('=') {
            addr = &addr[eq + 1..];
        }
        if addr.is_empty() {
            continue;
        }
        if !addr.starts_with("http://") && !addr.starts_with("https://") {
            return format!("http://{addr}");
        }
        return addr.to_string();
    }
    String::new()
}

/// 注册表形态的代理 URL —— `readWindowsProxy` + `regReadProxy`（`client.go:567-574`
/// / `proxy_windows.go:13-45`）：`ProxyEnable` 缺失或解析为 **0** 时返回空串。
pub fn proxy_url_from_env(env: &ProxyEnv) -> String {
    match env
        .proxy_enable
        .as_deref()
        .and_then(crate::winreg::parse_dword)
    {
        None | Some(0) => String::new(),
        Some(_) => match env.proxy_server.as_deref() {
            Some(s) if !s.is_empty() => parse_proxy_server(s),
            _ => String::new(),
        },
    }
}

/// 三步判定 —— `smartProxy`（`client.go:117-137`）：本地短路 → 环境变量 → 注册表。
///
/// `https` = 目标是否 TLS（Go 的 `http.ProxyFromEnvironment` 按 scheme 选
/// `HTTPS_PROXY` 还是 `HTTP_PROXY`；另一侧非空时兜底，真实环境两者通常同设）。
pub fn proxy_decision(host: &str, https: bool, env: &ProxyEnv) -> Option<String> {
    if is_local_host(host) {
        return None;
    }
    let (first, second) = if https {
        (&env.https_proxy, &env.http_proxy)
    } else {
        (&env.http_proxy, &env.https_proxy)
    };
    if !first.is_empty() {
        return Some(first.clone());
    }
    if !second.is_empty() {
        return Some(second.clone());
    }
    let reg = proxy_url_from_env(env);
    if reg.is_empty() {
        None
    } else {
        Some(reg)
    }
}

/// 桌面端内置 CLI 的发行件路径（`seedcheck.go:147`）——安装目录 + 固定相对路径。
pub fn cowork_cli_exe(install_location: &str) -> String {
    let base = install_location.trim_end_matches(['\\', '/']);
    if base.is_empty() {
        return String::new();
    }
    format!("{base}\\cli\\bin\\win32-x64\\codely.exe")
}

/// `%LOCALAPPDATA%\Programs\Tuanjie Cowork` 的兜底安装目录（`seedcheck.go:150`）。
pub fn cowork_fallback_dir(local_app_data: &str) -> String {
    let base = local_app_data.trim_end_matches(['\\', '/']);
    if base.is_empty() {
        return String::new();
    }
    format!("{base}\\Programs\\Tuanjie Cowork")
}

/// 路径是否是文件（门禁断言路径形态用）。
pub fn is_file(p: &str) -> bool {
    Path::new(p).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts_short_circuit_like_go() {
        for h in [
            "localhost",
            "127.0.0.1",
            "::1",
            "192.168.1.5",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.1",
            // Go 的宽匹配：172.2*/172.3* 全算
            "172.20.0.1",
            "172.30.9.9",
        ] {
            assert!(is_local_host(h), "{h} 应短路");
        }
        for h in ["codely-litellm.tuanjie.cn", "api.b.ai", "8.8.8.8", "172.15.0.1"] {
            assert!(!is_local_host(h), "{h} 不该短路");
        }
    }

    #[test]
    fn proxy_server_forms() {
        assert_eq!(parse_proxy_server("127.0.0.1:7897"), "http://127.0.0.1:7897");
        assert_eq!(parse_proxy_server("http://127.0.0.1:7897"), "http://127.0.0.1:7897");
        assert_eq!(parse_proxy_server("https=127.0.0.1:7897"), "http://127.0.0.1:7897");
        assert_eq!(
            parse_proxy_server("http=127.0.0.1:7890;https=127.0.0.1:7891"),
            "http://127.0.0.1:7890",
            "第一个非空段（Go 取第一个）"
        );
        assert_eq!(parse_proxy_server("; ;"), "");
        assert_eq!(parse_proxy_server(""), "");
    }

    #[test]
    fn enable_gate_matches_go() {
        let env = ProxyEnv {
            proxy_enable: Some("0x0".into()),
            proxy_server: Some("127.0.0.1:7897".into()),
            ..Default::default()
        };
        assert_eq!(proxy_url_from_env(&env), "", "ProxyEnable=0 → 空");
        let env = ProxyEnv {
            proxy_enable: None,
            proxy_server: Some("127.0.0.1:7897".into()),
            ..Default::default()
        };
        assert_eq!(proxy_url_from_env(&env), "", "缺失 → 空");
        let env = ProxyEnv {
            proxy_enable: Some("0x1".into()),
            proxy_server: Some("127.0.0.1:7897".into()),
            ..Default::default()
        };
        assert_eq!(proxy_url_from_env(&env), "http://127.0.0.1:7897");
        let env = ProxyEnv {
            proxy_enable: Some("0x1".into()),
            proxy_server: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(proxy_url_from_env(&env), "", "启用但 server 空 → 空");
    }

    #[test]
    fn decision_prefers_env_over_registry() {
        let env = ProxyEnv {
            https_proxy: "http://env-proxy:1111".into(),
            proxy_enable: Some("0x1".into()),
            proxy_server: Some("127.0.0.1:7897".into()),
            ..Default::default()
        };
        assert_eq!(
            proxy_decision("upstream.example", true, &env),
            Some("http://env-proxy:1111".to_string()),
            "环境变量优先于注册表"
        );
        assert_eq!(proxy_decision("127.0.0.1", true, &env), None, "本地哪怕有代理也直连");
        let env2 = ProxyEnv {
            proxy_enable: Some("0x1".into()),
            proxy_server: Some("127.0.0.1:7897".into()),
            ..Default::default()
        };
        assert_eq!(
            proxy_decision("upstream.example", true, &env2),
            Some("http://127.0.0.1:7897".to_string())
        );
        assert_eq!(
            proxy_decision("upstream.example", true, &ProxyEnv::default()),
            None,
            "都没有 → 直连"
        );
        let env3 = ProxyEnv {
            http_proxy: "http://p1:1".into(),
            https_proxy: "http://p2:2".into(),
            ..Default::default()
        };
        assert_eq!(
            proxy_decision("x.example", false, &env3),
            Some("http://p1:1".to_string()),
            "非 https 走 HTTP_PROXY"
        );
    }

    #[test]
    fn cowork_paths() {
        assert_eq!(
            cowork_cli_exe(r"D:\Program Files (x86)"),
            r"D:\Program Files (x86)\cli\bin\win32-x64\codely.exe"
        );
        assert_eq!(cowork_cli_exe(""), "");
        assert_eq!(
            cowork_fallback_dir(r"C:\Users\u\AppData\Local"),
            r"C:\Users\u\AppData\Local\Programs\Tuanjie Cowork"
        );
        assert_eq!(cowork_fallback_dir(""), "");
    }
}
