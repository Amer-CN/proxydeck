//! httpc.rs —— 桥接层的同步 HTTP（对应 Go `bridge.go` 里的 `httpClient` /
//! `httpClientSlow` / `httpClientLocal` 三个共用客户端）。
//!
//! 为什么是同步：Go 侧这些调用发生在 WebView2 的串行 bind 队列（UI 线程）上，
//! 超时都是硬性的（1.5s / 3s / 10s），慢接口一律另开 goroutine。
//! Rust 侧保持同一执行形态：同步 reqwest blocking + 同样的超时值，
//! 调用方（bridge）负责把慢操作挪到后台线程。

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::blocking::Client;

/// `httpClient` 的 1.5s 超时（Go `httpClient`）。
pub const FAST_TIMEOUT: Duration = Duration::from_millis(1500);
/// `httpClientSlow` 的 10s 超时（Go `httpClientSlow`，官网 4 接口要 3s 左右）。
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(10);
/// `httpClientLocal` 的 3s 超时（Go `httpClientLocal`，打本地核心写接口）。
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(3);
/// 响应体读取上限（Go `io.LimitReader(resp.Body, 4<<20)`）。
pub const BODY_LIMIT: usize = 4 << 20;

fn client_with(timeout: Duration) -> Client {
    Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| Client::new())
}

fn fast_client() -> &'static Client {
    static C: OnceLock<Client> = OnceLock::new();
    C.get_or_init(|| client_with(FAST_TIMEOUT))
}

fn slow_client() -> &'static Client {
    static C: OnceLock<Client> = OnceLock::new();
    C.get_or_init(|| client_with(SLOW_TIMEOUT))
}

fn local_client() -> &'static Client {
    static C: OnceLock<Client> = OnceLock::new();
    C.get_or_init(|| client_with(LOCAL_TIMEOUT))
}

/// 建一个自定义超时的客户端（一键更新下载用：镜像 120s / 直连 30 分钟）。
pub fn client_with_timeout(timeout: Duration) -> Client {
    client_with(timeout)
}

fn body_of(resp: reqwest::blocking::Response, limit: usize) -> Option<String> {
    let mut buf = Vec::new();
    let mut resp = resp;
    let mut chunk = [0u8; 64 * 1024];
    use std::io::Read;
    loop {
        if buf.len() >= limit {
            break;
        }
        match resp.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = limit - buf.len();
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(_) => return None,
        }
    }
    Some(String::from_utf8_lossy(&buf).to_string())
}

/// GET 并要求 200（对应 Go `httpGet`：非 200 / 网络错误 / 读失败一律 false）。
pub fn get(url: &str) -> Option<String> {
    get_limited(url, BODY_LIMIT)
}

fn get_limited(url: &str, limit: usize) -> Option<String> {
    let resp = fast_client().get(url).send().ok()?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    body_of(resp, limit)
}

/// 慢速 GET（对应 Go `httpGetSlow`：10s 超时，官网统计专用）。
pub fn get_slow(url: &str) -> Option<String> {
    get_slow_status(url).ok().and_then(|(code, body)| {
        if code == 200 {
            Some(body)
        } else {
            None
        }
    })
}

/// 慢速 GET 并回状态码（更新检查要区分 403 限额与其它失败）。
pub fn get_slow_status(url: &str) -> Result<(u16, String), String> {
    get_slow_status_limited(url, 8 << 20)
}

fn get_slow_status_limited(url: &str, limit: usize) -> Result<(u16, String), String> {
    let resp = slow_client().get(url).send().map_err(|e| e.to_string())?;
    let code = resp.status().as_u16();
    let body = body_of(resp, limit).ok_or_else(|| "读取响应失败".to_string())?;
    Ok((code, body))
}

/// 端口上有健康服务即 true（对应 Go `httpOK`：只看 200，不关心内容）。
pub fn ok(url: &str) -> bool {
    get(url).is_some()
}

/// 表单 POST（对应 Go `httpClientLocal.PostForm`：`application/x-www-form-urlencoded`）。
pub fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<(u16, String), String> {
    let resp = local_client()
        .post(url)
        .form(fields)
        .send()
        .map_err(|e| e.to_string())?;
    let code = resp.status().as_u16();
    let body = body_of(resp, BODY_LIMIT).ok_or_else(|| "读取响应失败".to_string())?;
    Ok((code, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 起一个最小 HTTP 服务：固定状态码 + 固定响应体。
    fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ln.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            for stream in ln.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), h)
    }

    #[test]
    fn get_requires_200() {
        let (base, _h) = serve_once(200, "{\"ok\":true}");
        assert_eq!(get(&format!("{base}/health")).as_deref(), Some("{\"ok\":true}"));
        assert!(ok(&format!("{base}/health")));
    }

    #[test]
    fn non_200_is_none_like_go() {
        let (base, _h) = serve_once(500, "boom");
        assert_eq!(get(&format!("{base}/health")), None);
        assert!(!ok(&format!("{base}/health")));
        assert_eq!(get_slow_status(&format!("{base}/health")).unwrap().0, 500);
    }

    #[test]
    fn connection_refused_is_none() {
        // 127.0.0.1:1 必定连不上（保留端口）
        assert_eq!(get("http://127.0.0.1:1/health"), None);
    }
}
