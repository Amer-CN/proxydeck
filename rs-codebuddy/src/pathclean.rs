//! pathclean.rs —— Go `net/http.ServeMux` 的路径清洗与重定向（差异 12 的对齐）。
//!
//! Go 1.22+ 的 `ServeMux.Handler` 在匹配**之前**先清洗路径：`cleanPath(r.URL.EscapedPath())`
//! 与原始转义路径不同时，直接回 **307 Temporary Redirect**（`StatusTemporaryRedirect`，
//! Go 1.26.5 `net/http/server.go:2696`），`Location` 是清洗后的路径 + 原查询串。
//! axum 不清洗，旧实现在 `//v1/models` 上回 404——本轮按 Go 复刻（含响应字节）。
//!
//! 响应字节（`net/http.Redirect`，逐字复刻）：
//! * 头：`Location: <hexEscapeNonASCII(url)>`；GET/HEAD 加
//!   `Content-Type: text/html; charset=utf-8`；
//! * 体：**仅 GET** 写 `<a href="{htmlEscape(url)}">Temporary Redirect</a>.\n` 再经
//!   `fmt.Fprintln` 补一个 `\n`（实测 46 字节：`…</a>.\n\n`）；POST/HEAD 无体。

/// Go `net/http.cleanPath`（含 `path.Clean` 与尾斜杠保持）。
pub fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return "/".to_string();
    }
    let mut s = p.to_string();
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    let np = go_path_clean(&s);
    if s.ends_with('/') && np != "/" {
        // 快路径：p 恰好是 np + "/" 时直接用 p（与 Go 同）。
        if s.len() == np.len() + 1 && s.starts_with(&np) {
            return s;
        }
        return format!("{np}/");
    }
    np
}

/// Go `path.Clean` 的字节级等价实现（分隔符固定为 `/`，不做任何文件系统语义）。
pub fn go_path_clean(path: &str) -> String {
    let b = path.as_bytes();
    if b.is_empty() {
        return ".".to_string();
    }
    let rooted = b[0] == b'/';
    let n = b.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut r = 0usize;
    let mut dotdot = 0usize;
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }
    while r < n {
        if b[r] == b'/' {
            r += 1;
        } else if b[r] == b'.' && (r + 1 == n || b[r + 1] == b'/') {
            r += 1;
        } else if b[r] == b'.'
            && r + 1 < n
            && b[r + 1] == b'.'
            && (r + 2 == n || b[r + 2] == b'/')
        {
            r += 2;
            if out.len() > dotdot {
                out.pop();
                while out.len() > dotdot && *out.last().expect("非空") != b'/' {
                    out.pop();
                }
            } else if !rooted {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.push(b'.');
                out.push(b'.');
                dotdot = out.len();
            }
        } else {
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && b[r] != b'/' {
                out.push(b[r]);
                r += 1;
            }
        }
    }
    if out.is_empty() {
        return ".".to_string();
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string())
}

/// 清掉 `?` 之后的查询串，返回 `(path, query_opt)`。
fn split_query(u: &str) -> (&str, Option<&str>) {
    match u.find('?') {
        Some(i) => (&u[..i], Some(&u[i..])),
        None => (u, None),
    }
}

/// Go `hexEscapeNonASCII`：非 ASCII 字节逐字节写成 `%XX`（大写十六进制）。
pub fn hex_escape_non_ascii(s: &str) -> String {
    if s.bytes().all(|b| b < 0x80) {
        return s.to_string();
    }
    const UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let mut out: Vec<u8> = Vec::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b >= 0x80 {
            out.push(b'%');
            out.push(UPPER[(b >> 4) as usize]);
            out.push(UPPER[(b & 0x0f) as usize]);
        } else {
            out.push(b);
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Go `htmlEscape`（`htmlReplacer`：`&`→`&amp;`、`<`→`&lt;`、`>`→`&gt;`、`"`→`&#34;`）。
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

/// 清洗后的重定向目标（`net/http.Redirect` 会再跑一遍「相对路径 → 绝对 + 保留尾斜杠」）。
pub fn redirect_target(path: &str, raw_query: Option<&str>) -> String {
    // Go 传进 `Redirect` 的是 `url.URL{Path: cleaned, RawQuery: raw}.String()`；Redirect
    // 再解析它：无 scheme/host、以 `/` 开头 ⇒ 不拼前缀，只做「切查询串 + Clean + 保尾斜杠」。
    let with_query = match raw_query {
        Some(q) => format!("{path}{q}"),
        None => path.to_string(),
    };
    let (u, query) = split_query(&with_query);
    let trailing = u.ends_with('/');
    let mut cleaned = go_path_clean(u);
    if trailing && !cleaned.ends_with('/') {
        cleaned.push('/');
    }
    match query {
        Some(q) => format!("{cleaned}{q}"),
        None => cleaned,
    }
}

/// 一次路径清洗重定向的完整响应（状态码 / 头 / 体）。
pub struct Redirect {
    pub status: u16,
    pub location: String,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
}

/// 按 Go `RedirectHandler(…, StatusTemporaryRedirect)` 生成响应；`None` = 路径本来就是干净的。
pub fn clean_path_redirect(raw_path: &str, raw_query: Option<&str>, method: &str) -> Option<Redirect> {
    let cleaned = clean_path(raw_path);
    if cleaned == raw_path {
        return None;
    }
    let url = redirect_target(&cleaned, raw_query);
    let location = hex_escape_non_ascii(&url);
    let get_or_head = method == "GET" || method == "HEAD";
    let content_type = if get_or_head {
        Some("text/html; charset=utf-8")
    } else {
        None
    };
    // 体只给 GET：`fmt.Fprintln(w, body)` ⇒ 尾部再多一个 `\n`。
    let body = if method == "GET" {
        let mut v = format!(
            "<a href=\"{}\">Temporary Redirect</a>.\n",
            html_escape(&url)
        )
        .into_bytes();
        v.push(b'\n');
        v
    } else {
        Vec::new()
    };
    Some(Redirect {
        status: 307,
        location,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_matches_go_serve_mux_examples() {
        assert_eq!(clean_path(""), "/");
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path("//v1/models"), "/v1/models");
        assert_eq!(clean_path("/v1/../v1/models"), "/v1/models");
        assert_eq!(clean_path("/a/./b/"), "/a/b/");
        assert_eq!(clean_path("/a/../../b"), "/b");
        assert_eq!(clean_path("/v1/models"), "/v1/models");
        assert_eq!(clean_path("/v1/models/"), "/v1/models/");
        // 干净路径不变（调用方据此判定是否要重定向）
        assert_eq!(go_path_clean(""), ".");
        assert_eq!(go_path_clean("../x"), "../x");
        assert_eq!(go_path_clean("a//b"), "a/b");
    }

    #[test]
    fn redirect_bytes_match_go_measurement() {
        let r = clean_path_redirect("//v1/models", None, "GET").expect("应重定向");
        assert_eq!(r.status, 307);
        assert_eq!(r.location, "/v1/models");
        assert_eq!(r.content_type, Some("text/html; charset=utf-8"));
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            "<a href=\"/v1/models\">Temporary Redirect</a>.\n\n"
        );
        let r = clean_path_redirect("//v1/models", None, "POST").expect("应重定向");
        assert_eq!(r.content_type, None);
        assert!(r.body.is_empty());
        let r = clean_path_redirect("//v1/stats", Some("?a=1"), "GET").expect("应重定向");
        assert_eq!(r.location, "/v1/stats?a=1");
        assert!(clean_path_redirect("/v1/models", None, "GET").is_none());
    }

    #[test]
    fn non_ascii_is_hex_escaped_and_html_escaped() {
        assert_eq!(hex_escape_non_ascii("/a/b"), "/a/b");
        assert_eq!(hex_escape_non_ascii("/中"), "/%E4%B8%AD");
        assert_eq!(html_escape("a&b<c>\"d"), "a&amp;b&lt;c&gt;&#34;d");
    }
}
