//! 出站传输层 —— 以及 **uTLS 指纹路线取证结论**（P8a 开工前必须先定的那一条）。
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
//!   ClientHello **与 Go 侧同构**：16 个密套件**逐项同序**（前导 GREASE + TLS1.3 三件套 +
//!   9 个 ECDHE 系 + 4 个 RSA 系）、17 个扩展**集合相同**（含 `0x44cd` ALPS、`0xfe0d` ECH
//!   padding、`0xff01`、`0x12` SCT、`0x1b` compress_certificate）、`key_share` 同为
//!   `GREASE + x25519mlkem768 + x25519`（1263 字节）、签名算法 8 项逐项相同、
//!   JA4 载荷哈希**完全相同**（`4560fe214f0b_14c5`），扩展序同样是逐连接随机置换。
//!   → 指纹轴与 Go 版**同源**，是真正的 uTLS 对等物。
//!
//! **代价（实测）**：(b) 需要本机具备 `cmake` + **NASM** + **libclang**（bindgen）+ MSVC
//! 才能编译 BoringSSL——本机原本两者都缺（`No CMAKE_ASM_NASM_COMPILER` → 补便携 NASM 后
//! 推进到 bindgen 报 `Unable to find libclang` → 补 libclang 后编译通过），首次全量编译
//! 为分钟级；即「构建环境多两项原生工具链依赖」，且新增 `wreq`/`btls` 依赖树。
//!
//! **本小轮的选择**：P8a 的公开面是**凭证链**（签名/UA/路径/头集合/体重排），出站 TLS 属
//! P8 的生态级红线、跨 P8a~P8c 的面；且 P8a 的验收要求 `cargo test` 在**裸环境**可复跑
//! （不预置 NASM/libclang）。故本轮交付 [rustls] 直连（**差异已登记**：见 `README.md`
//! 「已知差异」1），并把 (b) 的取证产物与替换点固定下来——**P8b 接真实流量前必须换成 (b)**，
//! 替换面只有本文件的 [`build_http_client`] 一处（约 30 行）。

use crate::{Error, Result, DEFAULT_TIMEOUT};

/// 构造出站 HTTP 客户端。
///
/// **P8b 替换点**：接真实流量前把这里换成 `wreq::Client::builder().emulation(Chrome…)
/// .http1_only()`（对应 Go 的「HelloChrome_Auto + ALPN 只留 http/1.1」），其余调用方不变。
pub fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .map_err(Error::Forward)
}
