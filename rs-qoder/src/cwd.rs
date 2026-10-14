//! `internal/clientcwd` 的 Rust 等价物：从扁平化 prompt 里抽客户端会话工作目录。
//!
//! 逐条复刻 `cwd.go` 的判定：标记列表、`strings.ToLower` 后再找、跳过冒号/空白、
//! 抓到行尾/引号为止、`TrimRight(" \t.,；;")`、必须 `len>=3` 且第 2 个**字节**是 `:`、
//! 且 `os.Stat` 必须是目录。找不到时回退 `QODER_CWD`（同样要真的是目录），再回退临时目录。

/// 客户端系统提示里常见的工作目录标记（与 Go 的 `cwdMarkers` 逐条同序）。
pub const CWD_MARKERS: [&str; 5] = [
    "working directory",
    "current directory",
    "工作目录",
    "当前工作目录",
    "cwd",
];

/// Go `strings.ToLower` 的近似：逐 rune 取单个小写 rune（`unicode.ToLower` 的简单映射）。
fn go_to_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.extend(c.to_lowercase().next());
    }
    out
}

fn trim_left_cutset(seg: &[u8]) -> &[u8] {
    let mut k = 0usize;
    while k < seg.len() {
        if seg[k] == b':' {
            k += 1;
            continue;
        }
        if seg[k..].starts_with("：".as_bytes()) {
            k += "：".len();
            continue;
        }
        if matches!(seg[k], b' ' | b'\t' | b'\r' | b'\n') {
            k += 1;
            continue;
        }
        break;
    }
    &seg[k..]
}

fn trim_right_cutset(mut s: &[u8]) -> &[u8] {
    loop {
        if s.is_empty() {
            return s;
        }
        if s.ends_with("；".as_bytes()) {
            s = &s[..s.len() - "；".len()];
            continue;
        }
        match s[s.len() - 1] {
            b' ' | b'\t' | b'.' | b',' | b';' => s = &s[..s.len() - 1],
            _ => return s,
        }
    }
}

/// Go `clientcwd.Extract`：从扁平化 prompt 中提取工作目录；提取不到返回空串。
///
/// 索引语义按 Go 的**字节偏移**照搬（Go 在 `lower` 上算偏移、回到 `prompt` 上切片）；
/// 若小写转换改变了字节长度（非 ASCII 的大小写折叠），两边行为都会漂——真实提示词里
/// 标记本身是 ASCII/常见中文，未遇到该情形（登记在 README）。
pub fn extract(prompt: &str) -> String {
    let lower = go_to_lower(prompt);
    let lb = lower.as_bytes();
    let pb = prompt.as_bytes();
    for marker in CWD_MARKERS {
        let mb = marker.as_bytes();
        let mut idx = 0usize;
        loop {
            if idx > lb.len() {
                break;
            }
            let hit = match find_subslice(&lb[idx..], mb) {
                Some(i) => idx + i,
                None => break,
            };
            let start = hit + mb.len();
            if start > pb.len() {
                break;
            }
            let seg = &pb[start..];
            let trimmed = trim_left_cutset(seg);
            let mut j = 0usize;
            while j < trimmed.len() {
                match trimmed[j] {
                    b'\r' | b'\n' | b'"' | b'`' | b'\'' => break,
                    _ => j += 1,
                }
            }
            let cand = trim_right_cutset(&trimmed[..j]);
            if cand.len() >= 3 && cand[1] == b':' {
                let path = String::from_utf8_lossy(cand).to_string();
                if std::path::Path::new(&path).is_dir() {
                    return path;
                }
            }
            let next = start + j;
            if next >= pb.len() {
                break;
            }
            idx = next;
        }
    }
    String::new()
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Go `clientcwd.Dir`：Extract 命中优先 → `env_value`（须真实存在且是目录）→ 临时目录。
pub fn dir(prompt: &str, env_value: Option<&str>) -> String {
    let p = extract(prompt);
    if !p.is_empty() {
        return p;
    }
    if let Some(v) = env_value {
        let t = v.trim_matches(char::is_whitespace);
        if !t.is_empty() && std::path::Path::new(t).is_dir() {
            return t.to_string();
        }
    }
    std::env::temp_dir().to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_markers_like_go() {
        // `C:\Windows` 是跨机可复现的真实目录（黄金基准用的就是它）
        assert_eq!(extract("Working directory: C:\\Windows\nrest"), "C:\\Windows");
        assert_eq!(extract("工作目录：C:\\Windows\nrest"), "C:\\Windows");
        assert_eq!(extract("current directory: C:\\Windows"), "C:\\Windows");
        assert_eq!(extract("cwd: C:\\Windows"), "C:\\Windows");
        // 提示词里的路径不存在 → 不算命中
        assert_eq!(extract("Working directory: F:\\AIXM\\no-such-dir-xyz"), "");
        // 长度不足/没有盘符冒号 → 不算命中
        assert_eq!(extract("cwd: ab"), "");
        assert_eq!(extract("cwd: abcd"), "");
        // 引号/反引号/单引号是**终止符**：只在路径已经起头之后才截断（Go 的 TrimLeft 不跳引号，
        // 所以路径前面直接跟引号时整段抽不到——这条按 Go 逻辑如实断言）
        assert_eq!(extract("Working directory: C:\\Windows\" quoted"), "C:\\Windows");
        assert_eq!(extract("Working directory: \"C:\\Windows\" quoted"), "");
    }

    #[test]
    fn dir_falls_back_to_env_then_temp() {
        let p = dir(
            "Working directory: F:\\AIXM\\no-such-dir-xyz",
            Some("C:\\Windows"),
        );
        assert_eq!(p, "C:\\Windows");
        let p = dir("no marker at all", None);
        assert!(std::path::Path::new(&p).is_dir(), "temp dir 应为目录: {p}");
        // env 指向不是目录的东西 → 回退临时目录
        let p = dir("no marker", Some("F:\\AIXM\\no-such-dir-xyz"));
        assert!(std::path::Path::new(&p).is_dir(), "temp dir 应为目录: {p}");
    }
}
