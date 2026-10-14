// tests/http_contract.rs —— P3 的 HTTP 合同与边界（不依赖黄金基准的那部分）。
//
// 覆盖黄金门禁照顾不到或采不到的几条：
// * 入库材料齐不齐（缺了就显式失败，不是静默跳过）；
// * 生产默认值就是 Go 的常量（端口 8785 / host 回环 / 额度接口域名）；
// * **请求体不做隐形截断**：>2MB 的体必须真的走到 worker（axum 的 2MB 默认上限不适用）；
// * `no_worker` 503 分支（本机真装了 Qoder CN ⇒ Go 侧采不到这条，用测试缝覆盖并如实登记）。
mod common;

use common::*;
use serde_json::json;
use std::sync::Arc;

#[test]
fn golden_files_exist() {
    // 入库的四份材料：基准 JSON、假 worker、合成登录态夹具两份。
    // **读不到就失败**——「从干净检出能跑」是 P1 第 4 轮定下的口径。
    for f in [
        "golden-qoder-go.json",
        "fake-worker.js",
        "auth-fixture/Local State",
        "auth-fixture/auth.v1.dat",
    ] {
        let p = golden_path(f);
        assert!(p.is_file(), "入库材料缺失: {}（见 tests/golden/README.md）", p.display());
    }
    let g = load_golden();
    for key in [
        "flatten", "resolve", "chunks", "complete", "quota_error", "rfc3339", "floats",
        "usage_view", "credits_ledger", "health_ok", "models_get", "routes", "credits_set",
        "chat", "worker_source_sha256",
    ] {
        assert!(!g[key].is_null(), "黄金基准缺段: {key}");
    }
}

#[test]
fn fake_worker_source_hash_matches_golden() {
    // 假 worker 是整条 argv/cwd/env/payload/stdin 证据链的**测量仪器**：它的源码一改，
    // 所有建立在它上面的观测都不再可比。黄金里记下采集时那份源码的 SHA-256
    // （`worker_source_sha256`），这里对**入库的那份**做真校验——仪器漂移必红。
    let p = golden_path("fake-worker.js");
    let body = std::fs::read(&p).unwrap_or_else(|e| panic!("读不到假 worker {}: {e}", p.display()));
    let want = load_golden()["worker_source_sha256"]
        .as_str()
        .expect("黄金缺 worker_source_sha256")
        .to_ascii_lowercase();
    assert_eq!(
        sha256_hex(&body),
        want,
        "入库假 worker 的源码哈希与黄金记录不一致（改了仪器就得连带重采基准，见 tests/golden/README.md）"
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn production_defaults_are_the_go_constants() {
    use clap::Parser;
    let args = rs_qoder::Args::try_parse_from(["rs-qoder"]).expect("默认参数应能解析");
    assert_eq!(args.port, 8785, "默认端口与 Go 侧 8785 同值");
    assert_eq!(args.host, "127.0.0.1", "默认只监听回环");
    assert_eq!(args.usage_url, "https://openapi.qoder.com.cn/api/v2/quota/usage");
    assert!(!args.plugin_qoder, "占位开关默认关");
    assert!(args.worker.is_none() && args.auth_dir.is_none() && args.credits_file.is_none());

    // 生产构造：worker 探测与账本路径都与 Go 同一套判定
    let st = rs_qoder::AppState::from_env();
    assert_eq!(st.usage_url, "https://openapi.qoder.com.cn/api/v2/quota/usage");
    // 本机真的装了 Qoder CN（`D:\Program Files\Qoder\Qoder CN\...`），探测必须命中；
    // 换机没装时是 None（调用方回 503 no_worker），两种都合法，这里只锁「命中就得是绝对路径」
    if let Some(p) = &st.worker_path {
        assert!(p.is_absolute(), "worker 路径应是绝对路径: {}", p.display());
        assert!(p.is_file());
    }
}

#[tokio::test]
async fn cors_options_is_204_and_404_is_plain_text() {
    let dir = TestDir::new("contract-cors");
    let empty = TestDir::new("contract-cors-empty");
    let srv = AppServer::start(Arc::new(rs_qoder::AppState::new(
        empty.path.clone(),
        None,
        Some(dir.join(".qoder-cli-credits.json")),
        None,
    )))
    .await;

    for path in ["/health", "/v1/models", "/v1/chat/completions", "/nope"] {
        let (status, headers, body) = raw_request(&srv.addr, "OPTIONS", path, &[], "").await;
        assert_eq!(status, 204, "OPTIONS {path} 应是 204（Go 的 corsWith 短路）");
        assert!(body.is_empty(), "OPTIONS {path} 不该有体");
        let h = headers_from_raw(&headers);
        assert_eq!(h.get("access-control-allow-origin").unwrap(), "*");
        assert_eq!(h.get("access-control-allow-methods").unwrap(), "GET, POST, OPTIONS");
        assert_eq!(
            h.get("access-control-allow-headers").unwrap(),
            "Content-Type, Authorization"
        );
    }

    let (status, headers, body) = raw_request(&srv.addr, "GET", "/nope", &[], "").await;
    assert_eq!(status, 404);
    assert_eq!(String::from_utf8_lossy(&body), "404 page not found\n");
    let h = headers_from_raw(&headers);
    assert_eq!(h.get("content-type").unwrap(), "text/plain; charset=utf-8");
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    // CORS 头对 404 也要有（corsWith 罩整个 mux）
    assert_eq!(h.get("access-control-allow-origin").unwrap(), "*");
}

#[tokio::test]
async fn no_worker_is_503_with_go_message() {
    // 本机装了 Qoder CN ⇒ Go 的 `findWorker()` 一定找得到，这条分支**在 Go 侧采不到**
    // （登记在 rs-qoder/README.md 的「未覆盖」清单）；这里用测试缝把文案与形状锁住。
    if !auth_fixture_usable() {
        println!("SKIP(dpapi-fixture): no_worker 分支要先过登录态，夹具本机解不开——未跑");
        return;
    }
    let dir = TestDir::new("contract-noworker");
    let st = Arc::new(rs_qoder::AppState::new(
        auth_fixture_dir(),
        None, // 故意不给 worker
        Some(dir.join(".qoder-cli-credits.json")),
        None,
    ));
    let srv = AppServer::start(st).await;
    let h = hop(
        &srv.base,
        "POST",
        "/v1/chat/completions",
        Some(r#"{"model":"Auto","messages":[]}"#),
    )
    .await;
    assert_eq!(h.status, 503);
    assert_eq!(
        h.body,
        "{\"error\":{\"message\":\"未找到 Qoder 官方 worker，请先安装 Qoder CN（AI IDE）\",\"type\":\"no_license\"}}\n"
    );
    assert_eq!(h.headers.get("content-type").unwrap(), "application/json");
}

#[tokio::test]
async fn large_body_reaches_the_worker() {
    // axum 的 `DefaultBodyLimit` 只对 `Bytes`/`Json` 这类**提取器**生效；本 crate 走的是
    // `req.into_body()` + 自带的 32MB 上限（对齐 Go 的 `io.LimitReader(r.Body, 32<<20)`），
    // 所以 2MB 以上的体必须原样走到 worker。这条用例就是防「悄悄退回 2MB」的回归。
    if !auth_fixture_usable() {
        println!("SKIP(dpapi-fixture): 大体积用例要先过登录态，夹具本机解不开——未跑");
        return;
    }
    let worker = FakeWorker::new("contract-bigbody");
    worker.set_scenario("normal");
    let dir = TestDir::new("contract-bigbody-home");
    let home = TestDir::new("contract-bigbody-home2");
    let st = Arc::new(rs_qoder::AppState::new(
        auth_fixture_dir(),
        Some(worker.path()),
        Some(home.join(".qoder-cli-credits.json")),
        None,
    ));
    let srv = AppServer::start(st).await;

    let filler = "测".repeat(1_200_000); // UTF-8 下 3.6 MB
    let body = json!({
        "model": "Auto",
        "messages": [{"role": "user", "content": filler}]
    })
    .to_string();
    assert!(body.len() > 2 * 1024 * 1024, "用例前提：体要超过 2MB");
    let h = hop(&srv.base, "POST", "/v1/chat/completions", Some(&body)).await;
    assert_eq!(h.status, 200, "大体积请求不该在本地被挡: {}", h.body);
    assert!(h.body.contains("\"finish_reason\":\"stop\""));

    // 假 worker 收到的提示词必须**逐字节**等于扁平化结果（说明没被截断）
    let want_prompt = format!("[User]\n{filler}");
    let raw = std::fs::read_to_string(worker.dir.join("worker-log.jsonl")).expect("worker 日志");
    let mut found: Option<String> = None;
    for line in raw.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["kind"] == "stdin" {
            if let Some(s) = v["line"].as_str() {
                if s.contains("\"type\":\"user\"") {
                    let parsed: serde_json::Value = serde_json::from_str(s).unwrap();
                    found = Some(parsed["message"]["content"][0]["text"].as_str().unwrap().to_string());
                }
            }
        }
    }
    let got = found.expect("假 worker 没收到用户消息");
    assert_eq!(got.len(), want_prompt.len(), "提示词长度被改动了（截断？）");
    assert_eq!(got, want_prompt, "提示词内容与扁平化结果不一致");
    let _ = dir;
}

#[tokio::test]
async fn method_not_allowed_paths_match_go() {
    let dir = TestDir::new("contract-405");
    let empty = TestDir::new("contract-405-empty");
    let srv = AppServer::start(Arc::new(rs_qoder::AppState::new(
        empty.path.clone(),
        None,
        Some(dir.join(".qoder-cli-credits.json")),
        None,
    )))
    .await;
    // chat：405 且**没有 Allow 头**（Go 的 writeError 只写 Content-Type）
    let (status, headers, body) =
        raw_request(&srv.addr, "GET", "/v1/chat/completions", &[], "").await;
    assert_eq!(status, 405);
    assert_eq!(
        String::from_utf8_lossy(&body),
        "{\"error\":{\"message\":\"仅支持 POST\",\"type\":\"invalid_request\"}}\n"
    );
    assert!(headers_from_raw(&headers).get("allow").is_none(), "chat 的 405 没有 Allow");

    // credits/set：405 且带 `Allow: POST`
    let (status, headers, body) = raw_request(&srv.addr, "GET", "/credits/set", &[], "").await;
    assert_eq!(status, 405);
    assert_eq!(
        String::from_utf8_lossy(&body),
        "{\"error\":\"仅支持 POST\",\"ok\":false}\n"
    );
    assert_eq!(headers_from_raw(&headers).get("allow").unwrap(), "POST");
}
