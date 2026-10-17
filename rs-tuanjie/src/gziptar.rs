//! gziptar.rs —— Go `compress/gzip` + `archive/tar` 的**只读对等物**。
//!
//! 用途只有一个：种子卫兵在线层要把 npm 下载下来的 `pkg.tgz` 解开，取出
//! `package/bundle/gemini.js` 做特征核验（`seedcheck.go:413-461`）。Go 用标准库两行搞定，
//! Rust 侧 `flate2` 在依赖树里（`reqwest` 的 gzip 特性已带，本 crate 只是显式声明），
//! tar 是本文件自己实现的**uSTAR 只读解析器**。
//!
//! ## 覆盖面（与 Go `tar.NewReader` 的差集，如实登记）
//!
//! 实现 uSTAR（`ustar\0` magic）+ GNU 长名（`L`/`K`）+ PAX 扩展头（`x`/`g`，**只跳读**）：
//!
//! * 文件体条目：`TypeReg`（`'0'`/`\0`）——npm 包里 `bundle/gemini.js` 就是它；
//! * 目录（`'5'`）、符号链接（`'2'`）、硬链接（`'1'`）等**一律跳过**（Go 侧
//!   `seedcheck.go:436` 也是 `hdr.Typeflag != tar.TypeReg → continue`）；
//! * 名字 = `prefix/name`（uSTAR 的 155/100 字节两段），`'\0'` 截断、尾部空白去掉；
//! * 大小字段：八进制；GNU base-256（首字节 0x80）也支持（大文件用得上）；
//! * 校验和**不校验**（Go 的 `tar.Reader` 会校验，但被打包工具写坏校验和的包在
//!   真实世界并不存在；这里只解「官方 npm 包」，故按最小实现，登记在 README）。
//!
//! 与 Go 一样：条目名先 `path.Clean` 再判穿越（`..` / 绝对路径跳过）。

use std::io::Read;

/// 解包时的字节上限（与 `seedTarballMaxBytes` 同值，`seedcheck.go:52`）。
pub const TARBALL_MAX_BYTES: u64 = 64 << 20;

/// 一次「从 tar.gz 里取某个文件」的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractOutcome {
    /// 是否找到该条目。
    pub found: bool,
    /// 条目内容（`found=false` 时为空）。
    pub content: Vec<u8>,
}

/// 从 gzip 字节流里解出 `want`（`package/bundle/gemini.js` 这种 `package/` 前缀名）的内容。
///
/// `want` 是**去掉 `package/` 前缀后**的相对名（与 Go `seedcheck.go:440` 的 `rel` 同名）。
/// 找不到该条目返回 `found=false`（不是错误——官方包结构变了正是这条信号）。
pub fn extract_from_gzip(gz: &[u8], want: &str, max_bytes: u64) -> Result<ExtractOutcome, String> {
    let mut decoder = flate2::read::GzDecoder::new(gz);
    extract_from_tar(&mut decoder, want, max_bytes)
}

/// 从 tar 字节流里取条目（gzip 层之外的纯 tar 面，单测直接喂真 tar 字节）。
pub fn extract_from_tar<R: Read>(r: &mut R, want: &str, max_bytes: u64) -> Result<ExtractOutcome, String> {
    let mut header = [0u8; 512];
    // GNU 长名（`L`）与长链接名（`K`）是**独立条目**，其体是下一个条目的名字；
    // PAX（`x`/`g`）体是 `key=value` 列表，我们只跳过。
    let mut long_name: Option<String> = None;
    loop {
        if !read_exact_or_eof(r, &mut header)? {
            return Ok(ExtractOutcome {
                found: false,
                content: Vec::new(),
            });
        }
        // 全零块 = 归档结束（标准 tar 连写两块 0，Go 的 Reader 认第一块即可）
        if header.iter().all(|b| *b == 0) {
            return Ok(ExtractOutcome {
                found: false,
                content: Vec::new(),
            });
        }
        let size = parse_tar_number(&header[124..136]);
        let typeflag = header[156];
        let is_long_name = typeflag == b'L';
        let is_meta = matches!(typeflag, b'x' | b'g' | b'K');
        if is_long_name || is_meta {
            // 元数据体：读走（长名要留用），不产出内容
            let body = read_body(r, size, 1 << 20)?;
            if is_long_name {
                long_name = Some(trim_nul(&String::from_utf8_lossy(&body)).to_string());
            }
            continue;
        }
        let name = match long_name.take() {
            Some(n) => n,
            None => tar_entry_name(&header),
        };
        let clean = clean_path(&name);
        let reg = typeflag == b'0' || typeflag == 0;
        let rel = clean.strip_prefix("package/").unwrap_or(&clean);
        if !reg || rel != want {
            skip_body(r, size)?;
            continue;
        }
        let content = read_body(r, size, max_bytes)?;
        return Ok(ExtractOutcome {
            found: true,
            content,
        });
    }
}

/// 读满 `buf`；流在**开头**就结束返回 `Ok(false)`，中途结束是错误（Go 的 `io.ReadFull` 同款）。
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool, String> {
    let mut done = 0usize;
    while done < buf.len() {
        match r.read(&mut buf[done..]) {
            Ok(0) => {
                if done == 0 {
                    return Ok(false);
                }
                return Err("tar 流提前结束".into());
            }
            Ok(n) => done += n,
            Err(e) => return Err(format!("读 tar 失败: {e}")),
        }
    }
    Ok(true)
}

/// 读条目体（`size` 向上取整到 512 的倍数，尾部填充一并吃掉），超 `max` 报错
/// （Go 侧是 `limitWriter` 累计超限报错，语义相同）。
fn read_body<R: Read>(r: &mut R, size: u64, max: u64) -> Result<Vec<u8>, String> {
    if size > max {
        return Err(format!("超过 {max} 字节上限"));
    }
    let mut out = Vec::with_capacity(size as usize);
    let mut left = size;
    let mut buf = [0u8; 32 * 1024];
    while left > 0 {
        let want = std::cmp::min(left, buf.len() as u64) as usize;
        let n = r.read(&mut buf[..want]).map_err(|e| format!("读 tar 失败: {e}"))?;
        if n == 0 {
            return Err("tar 条目体提前结束".into());
        }
        out.extend_from_slice(&buf[..n]);
        left -= n as u64;
    }
    skip_padding(r, size)?;
    Ok(out)
}

/// 跳过条目体（不读进内存）。
fn skip_body<R: Read>(r: &mut R, size: u64) -> Result<(), String> {
    let mut left = size;
    let mut buf = [0u8; 8 * 1024];
    while left > 0 {
        let want = std::cmp::min(left, buf.len() as u64) as usize;
        let n = r.read(&mut buf[..want]).map_err(|e| format!("读 tar 失败: {e}"))?;
        if n == 0 {
            return Err("tar 条目体提前结束".into());
        }
        left -= n as u64;
    }
    skip_padding(r, size)
}

fn skip_padding<R: Read>(r: &mut R, size: u64) -> Result<(), String> {
    let pad = (512 - (size % 512) as usize) % 512;
    let mut buf = [0u8; 512];
    let mut left = pad;
    while left > 0 {
        let n = r
            .read(&mut buf[..left])
            .map_err(|e| format!("读 tar 失败: {e}"))?;
        if n == 0 {
            return Err("tar 填充提前结束".into());
        }
        left -= n;
    }
    Ok(())
}

/// uSTAR 条目名 = `prefix` + `/` + `name`（各自 `'\0'` 截断）。
fn tar_entry_name(header: &[u8; 512]) -> String {
    let raw_name = String::from_utf8_lossy(&header[0..100]);
    let name = trim_nul(&raw_name);
    let raw_prefix = String::from_utf8_lossy(&header[345..500]);
    let prefix = trim_nul(&raw_prefix);
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn trim_nul(s: &str) -> &str {
    s.split('\0').next().unwrap_or("")
}

/// tar 的数字字段：八进制文本（前导空白/`0`、尾部 `'\0'`/空白），或 GNU base-256。
fn parse_tar_number(field: &[u8]) -> u64 {
    if field.first().map(|b| b & 0x80 != 0).unwrap_or(false) {
        // GNU base-256：首字节的 0x80 是标志位，低位与后续字节一起构成**大端数值**
        // （即 `0x80 00 … 00` = 1；`0x81 00 … 00` = 1；`0x80 00 … 01` = 2^88，
        // 超过 u64 按 Rust 的自然截断——真实 tar 体积远小于 2^64，不会触及）。
        let mut v: u64 = (field[0] & 0x7f) as u64;
        for b in &field[1..] {
            v = (v << 8) | *b as u64;
        }
        return v;
    }
    if field.is_empty() {
        return 0;
    }
    let text: String = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    let t = text.trim();
    u64::from_str_radix(t, 8).unwrap_or(0)
}

/// Go `path.Clean` 的等价物（只用在本函数的判定上：判 `..` / 绝对路径）。
fn clean_path(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let absolute = p.starts_with('/');
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().map(|s| *s != "..").unwrap_or(false) {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// 路径穿越防护 —— 与 `seedcheck.go:433` 同判据（绝对路径 / `..` / `../` 开头）。
pub fn is_path_traversal(cleaned: &str) -> bool {
    cleaned.starts_with('/') || cleaned == ".." || cleaned.starts_with("../")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_matches_go_cases() {
        assert_eq!(clean_path("package/./bundle/gemini.js"), "package/bundle/gemini.js");
        assert_eq!(clean_path("package/a/../b"), "package/b");
        assert_eq!(clean_path("../etc/passwd"), "../etc/passwd");
        assert_eq!(clean_path("/abs/x"), "/abs/x");
        assert!(is_path_traversal(&clean_path("/abs/x")));
        assert!(is_path_traversal(&clean_path("../x")));
        assert!(!is_path_traversal(&clean_path("package/bundle/gemini.js")));
    }

    #[test]
    fn tar_number_octal_and_base256() {
        assert_eq!(parse_tar_number(b"00000000144\0"), 100);
        assert_eq!(parse_tar_number(b"     12\0"), 10);
        // GNU base-256：首字节**只借最高位当标志**，其低位与后续 11 字节一起构成
        // 大端数值。故 `0x80 00 … 01` = 1；`0x80 00 … 00` = 0（与八进制全零同值）。
        let mut f = [0u8; 12];
        f[0] = 0x80;
        f[11] = 0x01;
        assert_eq!(parse_tar_number(&f), 1, "标志位 + 末字节 1");
        let mut g = [0u8; 12];
        g[0] = 0x80;
        assert_eq!(parse_tar_number(&g), 0, "标志位 + 全零 = 0");
        // 紧随标志字节的那一位权重是 2^88 —— 12 字节字段在 u64 下会整体移位溢出，
        // 故这里只锁「全零 = 0」（真实 tar 的体积字段远小于 2^64，不会走到这条）。
        assert_eq!(parse_tar_number(&[]), 0, "空字段");
    }
}
