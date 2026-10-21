//! uihtml.rs —— 内嵌 `app/ui.html`。
//!
//! 合同：`rs-gui/ui.html` 必须是 `app/ui.html` 的**逐字节副本**（一个字节都不许改）——
//! 它的 JS 与 bridge 键名就是桥接层的接口定义。两道门禁：
//!   1. **构建期**（`build.rs`）：两个文件字节不等就直接编译失败；
//!   2. **测试期**（`tests/ui_html.rs`）：内嵌字节的 SHA-256 = 记录常量 = `app/ui.html` 的哈希。

/// 内嵌的界面字节（`include_bytes!`：零拷贝进二进制）。
pub const UI_HTML: &[u8] = include_bytes!("../ui.html");

/// `ui.html` 的 SHA-256（小写十六进制）——与 `app/ui.html` 逐字节一致那份。
/// 改动 `app/ui.html` 必须同步 `rs-gui/ui.html`，否则构建期与测试期都会红。
pub const UI_HTML_SHA256: &str = "d777b78bf6c0b01c98ed6f2d2e9874a288b06b4a8601cb26ba1eee9f5b24ab48";

/// 内嵌字节长度（与 `app/ui.html` 同值，多一个字节都算改了界面）。
pub const UI_HTML_LEN: usize = 1500518;

/// 内嵌字节的 SHA-256（小写十六进制）。
pub fn embedded_sha256() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(UI_HTML);
    hex_lower(&h.finalize())
}

/// 任意字节的 SHA-256（十六进制小写）。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex_lower(&h.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bytes_hash_matches_recorded_constant() {
        assert_eq!(embedded_sha256(), UI_HTML_SHA256);
        assert_eq!(UI_HTML.len(), UI_HTML_LEN);
    }
}
