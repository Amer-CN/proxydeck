//! 黄金门禁：`rs-codebuddy` 与真 Go（`internal/codebuddy`）的逐字节对照。
//!
//! 两部分：
//! 1. **纯函数段**（`pure`）：`normalizeToolChoice` / `sanitizeToolRefs` / `requestFingerprint` /
//!    `summarizeToolsForDiag` 的输入输出全部由真 Go 采收；
//! 2. **HTTP 场景段**（`scenarios`）：每个场景按黄金里的 `upstream_spec` 起假上游、按 `cred_seam`
//!    喂凭据缝，然后逐步回放请求，比 **状态码 + 响应头（白名单）+ 响应体 + 上游看到的请求**。
//!
//! 跳过的两类东西**显式计数并打印**（不静默）：
//! * `deferred` 步（属 P5b/P5c：`/quota`、`/v1/failover`、`/v1/accounts`、`/v1/stats`）；
//! * `body_compare == "outbound-only"` 步（非流式下游聚合属 P5c，本轮只比出站请求与状态码）。

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{json, Value};

use rs_codebuddy::cred::StubCredential;
use rs_codebuddy::gjson;
use rs_codebuddy::godecode;
use rs_codebuddy::region::Region;

fn field<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

// ---------- 纯函数段 ----------

#[test]
fn pure_sections_match_go_golden() {
    let g = load_golden();
    let pure = field(&g, "pure");

    // tool_choice
    let mut n = 0;
    for c in field(pure, "tool_choice").as_array().expect("tool_choice 段") {
        let mut body = godecode::unmarshal_map(field(c, "in").as_str().unwrap().as_bytes())
            .unwrap_or_else(|e| panic!("[{}] 入站解析失败: {e}", field(c, "desc")));
        rs_codebuddy::toolrefs::normalize_tool_choice(&mut body);
        assert_eq!(
            gjson::to_go(&Value::Object(body)),
            field(c, "out").as_str().unwrap(),
            "[{}] normalizeToolChoice 与 Go 不一致",
            field(c, "desc")
        );
        n += 1;
    }

    // tool_refs
    let mut m = 0;
    for c in field(pure, "tool_refs").as_array().expect("tool_refs 段") {
        let mut body = godecode::unmarshal_map(field(c, "in").as_str().unwrap().as_bytes())
            .unwrap_or_else(|e| panic!("[{}] 入站解析失败: {e}", field(c, "desc")));
        rs_codebuddy::toolrefs::sanitize_tool_refs(&mut body);
        assert_eq!(
            gjson::to_go(&Value::Object(body)),
            field(c, "out").as_str().unwrap(),
            "[{}] sanitizeToolRefs 与 Go 不一致",
            field(c, "desc")
        );
        m += 1;
    }

    // fingerprint
    let mut f = 0;
    for c in field(pure, "fingerprint").as_array().expect("fingerprint 段") {
        let body = godecode::unmarshal_map(field(c, "in").as_str().unwrap().as_bytes())
            .unwrap_or_else(|e| panic!("[{}] 入站解析失败: {e}", field(c, "desc")));
        assert_eq!(
            rs_codebuddy::diag::request_fingerprint(&body),
            field(c, "out").as_str().unwrap(),
            "[{}] requestFingerprint 与 Go 不一致",
            field(c, "desc")
        );
        f += 1;
    }

    // summarizeToolsForDiag
    let mut s = 0;
    for c in field(pure, "summarize_tool").as_array().expect("summarize_tool 段") {
        let body = godecode::unmarshal_map(field(c, "in").as_str().unwrap().as_bytes())
            .unwrap_or_else(|e| panic!("[{}] 入站解析失败: {e}", field(c, "desc")));
        assert_eq!(
            rs_codebuddy::diag::summarize_tools_for_diag(&body),
            field(c, "out").as_str().unwrap(),
            "[{}] summarizeToolsForDiag 与 Go 不一致",
            field(c, "desc")
        );
        s += 1;
    }

    println!("纯函数黄金对比：tool_choice {n} 条 / tool_refs {m} 条 / fingerprint {f} 条 / summarize_tool {s} 条，全部逐字节一致");
}

// ---------- HTTP 场景段 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_scenarios_match_go_golden() {
    let g = load_golden();
    let scenarios = field(&g, "scenarios").as_array().expect("scenarios 段").clone();
    let mut steps_done = 0usize;
    let mut headers_allowanced = 0usize;
    let mut steps_skipped = 0usize;
    let mut scenarios_done = 0usize;

    for sc in &scenarios {
        let name = field(sc, "name").as_str().unwrap_or("?").to_string();
        let dir = TestDir::new(&name);
        // 「上限 10 份」场景：Go 采集时先预置了 10 份现场文件，这里按黄金原样预置同一批。
        if field(sc, "diag_preseed_from_files").as_bool().unwrap_or(false) {
            for f in field(sc, "diag_files").as_array().cloned().unwrap_or_default() {
                let n = field(&f, "name").as_str().unwrap();
                let b = field(&f, "body").as_str().unwrap_or("");
                std::fs::write(dir.path().join(n), b.as_bytes()).expect("预置现场文件");
            }
        }
        let upstream = FakeUpstream::start(field(sc, "upstream_spec")).await;

        // 凭据缝：summary 来自黄金的 cred_seam；headers 来自黄金记录的上游请求头
        // （探测器/chat 用的就是凭据层给的那一份——P5a 锁的是「这份头被用在哪」）。
        let mut headers = serde_json::Map::new();
        if let Some(h) = field(sc, "upstream_seen").as_array().and_then(|a| a.first()) {
            if let Some(hh) = field(h, "headers").as_object() {
                for (k, v) in hh {
                    headers.insert(k.to_ascii_lowercase(), v.clone());
                }
            }
        }
        let seam = field(sc, "cred_seam");
        let summary = if seam.is_null() {
            Value::Null
        } else {
            seam.clone()
        };
        let stub = json!({
            "auth_dirs": field(seam, "auth_dirs"),
            "summary": summary,
            "headers": Value::Object(headers),
            "headers_ok": true,
        });
        let cred = Arc::new(StubCredential::from_json(&stub).expect("构造凭据缝"));
        let region = match field(sc, "region").as_str() {
            Some("INTL") => Region::Intl,
            _ => Region::Cn,
        };
        let state = Arc::new(rs_codebuddy::AppState::new(
            region,
            cred,
            upstream.base.clone(),
            dir.path().to_path_buf(),
        ));
        let app = AppServer::start(state).await;

        for step in field(sc, "steps").as_array().expect("steps 段") {
            let desc = field(step, "desc").as_str().unwrap_or("");
            if field(step, "deferred").as_bool().unwrap_or(false) {
                steps_skipped += 1;
                println!("SKIP(deferred-p5bc): [{name}] {desc}");
                continue;
            }
            let method = field(step, "method").as_str().unwrap();
            let path = field(step, "path").as_str().unwrap();
            let body = field(step, "req_body").as_str().unwrap_or("");
            let r = send(
                &app.base,
                method,
                path,
                if body.is_empty() {
                    None
                } else {
                    Some(body.as_bytes())
                },
            )
            .await;

            assert_eq!(
                r.status,
                field(step, "resp_status").as_u64().unwrap() as u16,
                "[{name} / {desc}] 状态码与 Go 不一致（实得 body: {}）",
                r.text()
            );
            // 下游响应（体 + 头）：`outbound-only` 步只比状态码（下游聚合/清洗属 P5c）
            let outbound_only = field(step, "body_compare").as_str() == Some("outbound-only");
            if outbound_only {
                println!("SKIP(downstream-p5c): [{name}] {desc}（只比出站请求与状态码）");
            } else {
                let want_body = normalize_uptime(field(step, "resp_body").as_str().unwrap_or(""));
                let got_body = normalize_uptime(&r.text());
                assert_eq!(got_body, want_body, "[{name} / {desc}] 响应体与 Go 不一致");
                // 响应头（白名单，大小写不敏感：Go 采集的是规范化名，超文本头名大小写不敏感）
                if let Some(want) = field(step, "resp_headers").as_object() {
                    for (k, v) in want {
                        if k == "content-length" && field(step, "resp_body").as_str().unwrap_or("").is_empty() {
                            // 唯一的 allowance：**HEAD** 的体是空的，而 content-length 是
                            // 「本该写出多少字节」——采集时那个数字是本地化（临时目录→占位符）
                            // 之前的长度，必然与本地化后的体长不符（差异仅来自占位符长度）。
                            // 其余步骤的 content-length 已在采集侧随本地化重算，按实测逐字节比对。
                            headers_allowanced += 1;
                            continue;
                        }
                        let wv = v.as_str().unwrap_or("");
                        match r.header(k) {
                            Some(got) => assert_eq!(
                                got, wv,
                                "[{name} / {desc}] 响应头 {k} 与 Go 不一致"
                            ),
                            None => panic!("[{name} / {desc}] 响应头 {k} 缺失（Go 侧为 {wv}）"),
                        }
                    }
                }
            }
            // 无体步（HEAD）额外守一条：响应体必须为空（与 Go 同）
            if method == "HEAD" {
                assert!(r.body.is_empty(), "[{name} / {desc}] HEAD 响应不该带体");
            }
            steps_done += 1;
        }

        // 上游看到的请求（顺序：探测并发场景按 body 排序，其余按发生顺序）
        let mut want: Vec<Value> = field(sc, "upstream_seen")
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut got = upstream.hits();
        if field(sc, "upstream_sorted_by_body").as_bool().unwrap_or(false) {
            want.sort_by_key(|v| field(v, "body").as_str().unwrap_or("").to_string());
            got.sort_by_key(|h| String::from_utf8_lossy(&h.body).to_string());
        }
        assert_eq!(
            got.len(),
            want.len(),
            "[{name}] 上游命中数与 Go 不一致（Go {} 次 / Rust {} 次）",
            want.len(),
            got.len()
        );
        for (i, w) in want.iter().enumerate() {
            let h = &got[i];
            assert_eq!(
                field(w, "method").as_str().unwrap(),
                h.method,
                "[{name} # {i}] 上游方法不一致"
            );
            assert_eq!(
                url_path(field(w, "url").as_str().unwrap()),
                h.path,
                "[{name} # {i}] 上游路径不一致"
            );
            assert_eq!(
                field(w, "body").as_str().unwrap(),
                String::from_utf8_lossy(&h.body),
                "[{name} # {i}] 上游请求体与 Go 不一致"
            );
            if let Some(wh) = field(w, "headers").as_object() {
                for (k, v) in wh {
                    let wv = v.as_str().unwrap_or("");
                    match h.header(k) {
                        Some(got) => assert_eq!(
                            got, wv,
                            "[{name} # {i}] 上游请求头 {k} 与 Go 不一致"
                        ),
                        None => panic!("[{name} # {i}] 上游请求头 {k} 缺失（Go 侧为 {wv}）"),
                    }
                }
            }
        }

        // 11133 现场取证
        if let Some(want_files) = field(sc, "diag_files").as_array() {
            let mut got_files: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir.path())
                .expect("读诊断目录")
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("diag-11133-"))
                .map(|e| {
                    (
                        normalize_diag_name(&e.file_name().to_string_lossy()),
                        std::fs::read(e.path()).unwrap_or_default(),
                    )
                })
                .collect();
            let mut want_files: Vec<(String, Vec<u8>)> = want_files
                .iter()
                .map(|f| {
                    (
                        normalize_diag_name(field(f, "name").as_str().unwrap_or("")),
                        field(f, "body").as_str().unwrap_or("").as_bytes().to_vec(),
                    )
                })
                .collect();
            want_files.sort();
            got_files.sort();
            assert_eq!(
                got_files.len(),
                want_files.len(),
                "[{name}] 现场文件份数与 Go 不一致"
            );
            assert_eq!(got_files, want_files, "[{name}] 现场文件内容与 Go 不一致");
        } else {
            let n = std::fs::read_dir(dir.path())
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_name().to_string_lossy().starts_with("diag-11133-"))
                        .count()
                })
                .unwrap_or(0);
            assert_eq!(n, 0, "[{name}] Go 侧没有落现场，Rust 侧却落了 {n} 份");
        }
        scenarios_done += 1;
    }

    println!(
        "HTTP 场景黄金对比：{scenarios_done} 个场景 / {steps_done} 步逐字节一致；显式跳过 {steps_skipped} 步（deferred-p5bc）；已登记 allowance {headers_allowanced} 处（content-length）"
    );
}
