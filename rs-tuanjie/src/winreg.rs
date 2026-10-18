//! winreg.rs —— 读 Windows 注册表的最小封装（**只读**）。
//!
//! Go 侧用 `golang.org/x/sys/windows/registry`（`proxy_windows.go` 与
//! `desktop_path_windows.go` 两处）。Rust crate 目前没有 `windows-sys`/`winreg` 依赖，
//! 引入新 crate 要过锁文件与断网构建两道门，而这里只需要读几个键：
//! 走系统自带的 `reg.exe query`（`C:\Windows\System32\reg.exe`，Windows 2000+ 恒在），
//! 解析它的**标准输出**。
//!
//! ## `reg query` 输出的形状（实测，中文系统同样）
//!
//! ```text
//! HKEY_CURRENT_USER\Software\...\Internet Settings
//!     ProxyEnable    REG_DWORD    0x1
//!     ProxyServer    REG_SZ       127.0.0.1:7897
//! ```
//!
//! 表头行是 `HKEY_` 开头、无前导空白；值行是两个以上前导空格 + 名字 + 两个以上空格 +
//! 类型 + 两个以上空格 + 值。**类型与值之间也是多空格**（`REG_SZ    127.0.0.1:7897`），
//! 按「2 个以上空白」切分比按列位置切稳。
//!
//! ## 与 Go 的差异（登记）
//!
//! 1. Go 用注册表 API 取 `ProxyEnable` 的 DWORD，Rust 解析 `reg query` 的
//!    `0x1` 十六进制文本——`reg.exe` 恒以十六进制打印 DWORD，两种写法在
//!    「启用=1 / 未启用=0」这条判据上等价（其余取值 Go 侧 `enabled == 0` 才返回空，
//!    Rust 侧同样只在「解析出整数 0」时返回空）。
//! 2. `reg query` **不走 32/64 位视图差异以外的重定向**：`HKCU\Software\...\Internet Settings`
//!    与卸载表项都想看 64 位视图（Go 的 `registry.CURRENT_USER` 默认 64 位视图）。
//!    `reg.exe` 默认与自身位数一致；本进程与 `reg.exe` 同为 64 位宿主，实测一致。

use std::process::Command;

/// `reg query` 的一条返回值。
#[derive(Debug, Clone, PartialEq)]
pub struct RegValue {
    /// 注册表值类型，如 `REG_SZ` / `REG_DWORD`。
    pub kind: String,
    /// 原始值文本（`REG_DWORD` 形如 `0x1`）。
    pub data: String,
}

/// `reg query <key>` 的全部值（键不存在 / 无权 / 解析不出行 → 空表，与 Go 的
/// `registry.OpenKey` 失败返回零值同款：**不报警、不重试**）。
pub fn query_values(key: &str) -> Vec<(String, RegValue)> {
    query_values_with("reg", key)
}

/// 同上，命令行可注入（测试用假 `reg` 脚本，绝不真读注册表）。
pub fn query_values_with(program: &str, key: &str) -> Vec<(String, RegValue)> {
    query_values_cmd(program, &["query", key])
}

/// `reg query <key> /v <name>` 的单值查询（找不到 → `None`）。
pub fn query_value(key: &str, name: &str) -> Option<RegValue> {
    query_values_cmd("reg", &["query", key, "/v", name])
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
}

fn query_values_cmd(program: &str, args: &[&str]) -> Vec<(String, RegValue)> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    // CREATE_NO_WINDOW：GUI 子系统进程 spawn 控制台程序（reg.exe）会新建黑窗闪一下。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let Ok(out) = cmd.output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_query_output(&String::from_utf8_lossy(&out.stdout))
}

/// 解析 `reg query` 的标准输出为 (名字, 值) 列表。
///
/// 只认「前导空白 + 名字 + **2 个以上空白** + `REG_<类型>` + **2 个以上空白** + 值」这种行；
/// 表头行（`HKEY_...`）与空行跳过。**值按原文保留**（只砍掉分隔用的那段空白）——
/// 值里可能自带单个空格（`REG_SZ` 的路径/UA），按「全部空白切分再拼回」会把单空格也
/// 变成双空格（实测踩过）。**纯函数**，门禁直接用真 `reg.exe` 的输出文本喂它。
pub fn parse_query_output(text: &str) -> Vec<(String, RegValue)> {
    let mut out = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            continue; // 表头行 / 空行
        }
        let body = line.trim_start();
        // 第一段分隔：名字 | 类型
        let Some((name, rest)) = split_once_ws(body) else {
            continue;
        };
        // 第二段分隔：类型 | 值
        let Some((kind, data)) = split_once_ws(rest) else {
            continue;
        };
        if !kind.starts_with("REG_") {
            continue;
        }
        out.push((
            name.to_string(),
            RegValue {
                kind: kind.to_string(),
                data: data.trim_end().to_string(),
            },
        ));
    }
    out
}

/// 在**第一个 2 个以上空白**处切成两段（返回的右段已去掉紧邻的那段空白）。
///
/// `reg query` 的列分隔是「≥2 个空白」；值内部的单空格不是分隔符。
fn split_once_ws(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b' ' || b[i] == b'\t' {
            let start = i;
            while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
                i += 1;
            }
            if i - start >= 2 {
                return Some((&s[..start], &s[i..]));
            }
            continue;
        }
        i += 1;
    }
    None
}

/// `REG_DWORD` 的 `0x...` 文本 → 整数；非法形态返回 `None`。
pub fn parse_dword(s: &str) -> Option<u32> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    t.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真 `reg.exe` 的输出文本（2026-10-17 本机 `reg query "HKCU\Software\Microsoft\
    /// Windows\CurrentVersion\Internet Settings"` 的输出逐字抄录：表头 / 缩进 /
    /// `REG_BINARY` 十六进制 / 带空格的 `REG_SZ` 都在里面）。
    const REAL_DUMP: &str = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\r\n    CertificateRevocation    REG_DWORD    0x1\r\n    IE5_UA_Backup_Flag    REG_SZ    5.0\r\n    SecureProtocols    REG_DWORD    0xa80\r\n    User Agent    REG_SZ    Mozilla/4.0 (compatible; MSIE 8.0; Win32)\r\n    ZonesSecurityUpgrade    REG_BINARY    28EFE607C5ACDC01\r\n    ProxyEnable    REG_DWORD    0x1\r\n\r\n";

    #[test]
    fn parses_real_reg_query_dump() {
        let v = parse_query_output(REAL_DUMP);
        assert_eq!(v.len(), 6);
        assert_eq!(
            v[0],
            (
                "CertificateRevocation".to_string(),
                RegValue {
                    kind: "REG_DWORD".into(),
                    data: "0x1".into()
                }
            )
        );
        // 带空格的 REG_SZ：值整段保留
        assert_eq!(v[3].0, "User Agent");
        assert_eq!(v[3].1.data, "Mozilla/4.0 (compatible; MSIE 8.0; Win32)");
        // REG_BINARY 也照原样带出（消费方按类型自行处置）
        assert_eq!(v[4].1.kind, "REG_BINARY");
        assert_eq!(parse_dword(&v[2].1.data), Some(0xa80));
        assert_eq!(parse_dword(&v[5].1.data), Some(1));
    }

    #[test]
    fn value_with_spaces_survives() {
        let dump = "HKEY_X\r\n    InstallLocation    REG_SZ    D:\\Program Files (x86)\\Tuanjie Cowork\r\n";
        let v = parse_query_output(dump);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1.data, "D:\\Program Files (x86)\\Tuanjie Cowork");
    }

    #[test]
    fn header_and_garbage_lines_are_skipped() {
        assert!(parse_query_output("HKEY_X\r\n\r\n").is_empty());
        assert!(parse_query_output("    not a value line\r\n").is_empty());
        assert!(parse_query_output("").is_empty());
    }

    #[test]
    fn dword_parsing_matches_go_semantics() {
        assert_eq!(parse_dword("0x1"), Some(1));
        assert_eq!(parse_dword("0x0"), Some(0));
        assert_eq!(parse_dword(""), None);
        assert_eq!(parse_dword("abc"), None);
    }
}
