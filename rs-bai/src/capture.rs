// capture.rs —— 出站请求校验记录（黄金回放对照用，**默认关闭**）。
//
// 打开方式：设环境变量 `RS_BAI_CAPTURE=<文件路径>`，每次出站请求追加一行 JSON：
//   {"method","url","path_query","headers":[name,value...],"body_bytes","body"}
// `authorization` 的值一律写成 `Bearer <redacted>`（脱敏要求：密钥不进记录）。
//
// 为什么需要它：验收 2 要求"同一脱敏请求打两边、出站头体逐字节 diff"。Rust 侧产出的
// 出站体就是本文件的 body 字段，与 Go 侧 golden（`.work/rust-rewrite/golden-adapt-go.json`）
// 或 Go 真实进程的捕获逐字节比对。
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::http::{HeaderMap, Method};

/// 进程级记录器（首次调用时按环境变量决定是否启用）。
static CAPTURE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dump_outbound(method: &Method, url: &str, headers: &HeaderMap, body: &[u8]) {
    let slot = CAPTURE.get_or_init(|| match std::env::var("RS_BAI_CAPTURE") {
        Ok(path) if !path.trim().is_empty() => match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => Some(Mutex::new(f)),
            Err(e) => {
                eprintln!("bai-plugin: 捕获文件打不开（{path}）: {e}");
                None
            }
        },
        _ => None,
    });
    let Some(file) = slot else { return };

    let mut hs: Vec<(String, String)> = Vec::new();
    for (name, value) in headers.iter() {
        let n = name.as_str().to_ascii_lowercase();
        let v = if n == "authorization" {
            "Bearer <redacted>".to_string()
        } else {
            value.to_str().unwrap_or("<binary>").to_string()
        };
        hs.push((n, v));
    }
    let path_query = url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string()))
        .unwrap_or_default();
    let rec = serde_json::json!({
        "seq": SEQ.fetch_add(1, Ordering::SeqCst),
        "method": method.as_str(),
        "url": url,
        "path_query": path_query,
        "headers": hs,
        "body_bytes": body.len(),
        "body": String::from_utf8_lossy(body),
    });
    let line = format!("{rec}\n");
    if let Ok(mut f) = file.lock() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}
