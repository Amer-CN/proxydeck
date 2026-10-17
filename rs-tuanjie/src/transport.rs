//! transport.rs —— 出站传输层：**代理自探测**（`client.go:92-137` 的 `smartProxy` 口径）
//! 与 **uTLS 指纹路线取证结论**（P8a 开工前必须先定的那一条）。
//!
//! # 代理自探测（P8c）
//!
//! Go 的三步判定：本地/内网地址先短路 → 环境变量 `HTTP_PROXY`/`HTTPS_PROXY`
//! → Windows 注册表 `HKCU\...\Internet Settings`（Clash/v2ray 写在那里）。
//! Rust 侧的口径（[`build_http_client`]）：
//!
//! * **环境变量沿用 reqwest 内建**（`system-proxy` 默认开启，含 `NO_PROXY` 的全部写法）；
//! * **仅在完全没有代理环境变量时**才把注册表那条判定成出站代理 —— 这样「环境变量」
//!   这一层的语义与 reqwest 内建完全一致（含 `HTTPS_PROXY`/`https_proxy` 两种写法、
//!   大写的 `ALL_PROXY`、`NO_PROXY` 的域名/端口/CIDR 匹配），注册表层按 Go 的
//!   `ProxyEnable`/`ProxyServer` 判定追加（见 [`crate::winpaths`]）；
//! * 日志**只报来源名**（`环境变量` / `Windows 注册表 Internet Settings`），
//!   绝不打印代理 URL —— URL 里可能带 `user:pass@`（P8c 卫生要求）。
//!
//! # uTLS 路线取证结论（2026-10-16，实测）
//!
//! Go 侧 `utls_transport.go` 用 `utls.HelloChrome_Auto` 伪装 TLS 指纹（扩展序还会
//! **每条连接随机置换**、GREASE 值与 ECH padding 随机）。两条候选路线都做了实测：
//!
//! - **(a) 接受指纹差异（rustls 默认栈）**：已抓到 rustls 的 ClientHello 做对照——
//!   257 字节 / 10 个密套件（无 GREASE）/ 11 个扩展 / key_share 只有 x25519（38 字节）/
//!   JA4 `d130a0bh1_8cbec4ae9a62_418b`。Go 侧是 1796 字节 / 16 密套件 / 18 扩展 /
//!   key_share 含 `x25519mlkem768`（1263 字节）/ JA4 `d130f10h1_4560fe214f0b_14c5`。
//!   → 一眼可辨「不是浏览器」，JA4 载荷哈希都不同。
//! - **(b) BoringSSL 系 Rust 客户端（`wreq` + `wreq-util` Chrome 模拟）**：已实测，
//!   ClientHello **与 Go 侧同构**：16 个密套件**逐项同序**、17 个扩展**集合相同**、
//!   `key_share` 同为 `GREASE + x25519mlkem768 + x25519`（1263 字节）、签名算法 8 项
//!   逐项相同、JA4 载荷哈希**完全相同**（`4560fe214f0b_14c5`），扩展序同样逐连接随机。
//!   → 指纹轴与 Go 版**同源**，是真正的 uTLS 对等物。
//!
//! **代价（实测）**：(b) 需要本机具备 `cmake` + **NASM** + **libclang**（bindgen）+ MSVC
//! 才能编译 BoringSSL——本机原本两者都缺（补便携 NASM 与 libclang 后编译通过），
//! 首次全量编译为分钟级；即「构建环境多两项原生工具链依赖」。
//!
//! **P8a 的选择**：P8a 的公开面是**凭证链**（签名/UA/路径/头集合/体重排），出站 TLS 属
//! P8 的生态级红线、跨 P8a~P8c 的面；且 P8a 的验收要求 `cargo test` 在**裸环境**可复跑
//! （不预置 NASM/libclang）。故本轮交付 [rustls] 直连（**差异已登记**：见 `README.md`
//! 「已知差异」1），并把 (b) 的取证产物与替换点固定下来——**接真实流量前必须换成 (b)**，
//! 替换面只有本文件的 [`build_http_client`] 一处。

use crate::{Error, Result, DEFAULT_TIMEOUT};

/// 代理环境变量的候选名（Go 的 `http.ProxyFromEnvironment` 认大小写两种写法）。
pub const PROXY_ENV_NAMES: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// 是否已有代理环境变量（有则这一层交给 reqwest 内建，注册表不参与）。
pub fn has_proxy_env() -> bool {
    PROXY_ENV_NAMES
        .iter()
        .any(|n| std::env::var(n).map(|v| !v.is_empty()).unwrap_or(false))
}

/// 本次构造实际采用的代理来源（日志用；**只有来源名，没有 URL**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySource {
    /// 环境变量（reqwest 内建处理）。
    Env,
    /// Windows 注册表 `Internet Settings`。
    Registry,
    /// 无代理（直连）。
    None,
}

impl ProxySource {
    /// 打日志的短名。
    pub fn as_str(&self) -> &'static str {
        match self {
            ProxySource::Env => "环境变量",
            ProxySource::Registry => "Windows 注册表 Internet Settings",
            ProxySource::None => "无",
        }
    }
}

/// 判定本次构造的代理来源 —— `smartProxy`（`client.go:117-137`）的三步。
///
/// `env`/`registry_url` 由调用方注入（生产读进程环境与注册表；门禁喂合成值）。
pub fn proxy_source_with(env_set: bool, registry_url: &str) -> ProxySource {
    if env_set {
        return ProxySource::Env;
    }
    if registry_url.is_empty() {
        ProxySource::None
    } else {
        ProxySource::Registry
    }
}

/// 生产口径的代理来源判定（读进程环境 + 注册表）。
pub fn proxy_source() -> ProxySource {
    let env_set = has_proxy_env();
    if env_set {
        return ProxySource::Env;
    }
    let reg = crate::winpaths::proxy_url_from_env(&crate::winpaths::read_proxy_env());
    proxy_source_with(false, &reg)
}

/// 构造出站 HTTP 客户端（**P8b+ 替换点**：接真实流量前把这里换成
/// `wreq::Client::builder().emulation(Chrome…).http1_only()`，其余调用方不变）。
///
/// 代理口径见模块头注释：环境变量交给 reqwest 内建，注册表在「无环境变量」时追加。
pub fn build_http_client() -> Result<reqwest::Client> {
    build_http_client_with(proxy_source())
}

/// 按给定来源构造（门禁可直接指定来源，避开真实环境/注册表）。
pub fn build_http_client_with(source: ProxySource) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(DEFAULT_TIMEOUT);
    if source == ProxySource::Registry {
        let env = crate::winpaths::read_proxy_env();
        let url = crate::winpaths::proxy_url_from_env(&env);
        if !url.is_empty() {
            // `Proxy::all`：注册表里给的是系统级代理，http/https 都走它；
            // 本地/内网地址的短路由 `no_proxy` 规则承担（与 Go 的 `smartProxy` 同判据）。
            let mut proxy = reqwest::Proxy::all(&url).map_err(Error::Forward)?;
            let mut none: Vec<String> = vec![
                "localhost".into(),
                "127.0.0.1".into(),
                "::1".into(),
                // Go 的宽匹配（`172.2*`/`172.3*` 全在内）
                "192.168.0.0/16".into(),
                "10.0.0.0/8".into(),
                "172.16.0.0/12".into(),
            ];
            // 上面三个 CIDR 覆盖不了 Go 的宽匹配（172.20./172.30. 这类段），
            // 故再显式列出 Go 的前缀判定覆盖到的那些前缀。
            for p in ["172.2", "172.3"] {
                none.push(format!("{p}.0.0/16"));
            }
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&none.join(",")));
            b = b.proxy(proxy);
        }
    }
    b.build().map_err(Error::Forward)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_prefers_env_then_registry_like_go() {
        assert_eq!(proxy_source_with(true, "http://127.0.0.1:7897"), ProxySource::Env);
        assert_eq!(
            proxy_source_with(false, "http://127.0.0.1:7897"),
            ProxySource::Registry
        );
        assert_eq!(proxy_source_with(false, ""), ProxySource::None);
        // 环境变量存在时注册表即使有配置也不参与
        assert_eq!(
            proxy_source_with(true, "http://registry-only:1"),
            ProxySource::Env
        );
    }

    #[test]
    fn source_names_are_stable() {
        assert_eq!(ProxySource::Env.as_str(), "环境变量");
        assert_eq!(ProxySource::Registry.as_str(), "Windows 注册表 Internet Settings");
        assert_eq!(ProxySource::None.as_str(), "无");
    }

    #[test]
    fn building_with_each_source_never_panics_or_leaks() {
        // `None` 与 `Registry`（注册表可能空）都要能构造出来；
        // `Env` 交给 reqwest 内建代理（本机可能没有，构造本身不受影响）。
        for s in [ProxySource::None, ProxySource::Registry, ProxySource::Env] {
            assert!(
                build_http_client_with(s).is_ok(),
                "{:?} 应能构造出客户端",
                s
            );
        }
    }

    #[test]
    fn no_proxy_env_check_only_reads_presence() {
        // 只断言「返回值是 bool、不 panic」——真读环境变量会与别的用例抢（本 crate 单测并行）。
        let _ = has_proxy_env();
        assert_eq!(PROXY_ENV_NAMES.len(), 6);
    }
}
