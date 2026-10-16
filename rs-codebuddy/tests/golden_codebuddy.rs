//! 黄金门禁：`rs-codebuddy` 与真 Go（`internal/codebuddy`）的逐字节对照 —— **P5a 段**。
//!
//! 两部分：
//! 1. **纯函数段**（`pure`）：`normalizeToolChoice` / `sanitizeToolRefs` / `requestFingerprint` /
//!    `summarizeToolsForDiag` / `collectStream` 的 EOF 残行合同的输入输出全部由真 Go 采收；
//! 2. **HTTP 场景段**（`scenarios`，只取 `scope == "P5a"`）：每个场景按黄金里的 `upstream_spec`
//!    起假上游、按 `auth_files` 在临时目录重建合成登录态，用**生产装配**（真凭据层 + 真池 +
//!    真 failover）起服务，然后逐步回放请求，比 **状态码 + 响应头（白名单）+ 响应体 +
//!    上游看到的请求**。
//!
//! 与 P5a 最初那版的差别（P5b 顺带修正）：那时黄金是**零值 Server** 采的，于是
//! 「生产缺省 reasoning_effort 注入」这类行为不在基准里；现在两侧都用生产装配，
//! 基准口径 = 生产口径（P5b 明确点名的坑）。
//!
//! `scope == "P5b"` 的场景由 `tests/p5b_golden.rs` 接管——本文件显式跳过并计数，不静默少覆盖。
//! 跳过的三类东西**显式计数并打印**：
//! * `scope = "P5b"` 的场景；
//! * `body_compare == "outbound-only"` 步（非流式下游聚合属 P5c，本轮只比出站请求与状态码）；
//! * `content-length` allowance（唯一的 allowance：HEAD 的体为空，那个数字是本地化前的体长）。

mod common;

use bytes::Bytes;
use common::*;
use serde_json::{json, Value};

use rs_codebuddy::gjson;
use rs_codebuddy::godecode;
use rs_codebuddy::region::Region;

fn field<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

// ---------- 纯函数段 ----------

#[tokio::test]
async fn pure_sections_match_go_golden() {
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

    // collect_eof（P5c 审查后补修新增）：`collectStream` 的**「EOF 残行」合同**——末条
    // **没有换行**的残行在流结束时同样要交给聚合（`bufio.ScanLines` 的 `if atEOF` 分支，
    // 读错误也走它），只有 `[DONE]` 触发的提前 `break` 才让它作废。
    // 基准是真 Go `collectStream` 采收的；`id`（随机 hex）与 `created`（墙钟）是易变字段，
    // 采收侧已剔除，这里同样剔除后再逐字节比。
    let mut e = 0;
    for c in field(pure, "collect_eof").as_array().expect("collect_eof 段") {
        let body = Bytes::from(field(c, "in").as_str().unwrap().as_bytes().to_vec());
        let mut v = rs_codebuddy::collect::collect_stream(
            futures_util::stream::iter(vec![Ok::<Bytes, std::io::Error>(body)]),
            "fallback-model",
        )
        .await;
        {
            let obj = v.as_object_mut().expect("聚合结果是对象");
            obj.remove("id");
            obj.remove("created");
        }
        assert_eq!(
            gjson::to_go(&v),
            field(c, "out").as_str().unwrap(),
            "[{}] collectStream 与 Go 不一致",
            field(c, "desc")
        );
        e += 1;
    }

    println!("纯函数黄金对比：tool_choice {n} 条 / tool_refs {m} 条 / fingerprint {f} 条 / summarize_tool {s} 条 / collect_eof {e} 条，全部逐字节一致");
}

// ---------- HTTP 场景段 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_scenarios_match_go_golden() {
    // 环境变量是进程级的：与 p5b_golden.rs 共用一把串行锁。
    // 门禁启动时取一次进程环境锁（整场门禁串行），之后每个场景只换目录、不重复取锁。
    let env = EnvGuard::set(&[]);
    let g = load_golden();
    let all = field(&g, "scenarios").as_array().expect("scenarios 段");
    let scenarios: Vec<Value> = all
        .iter()
        .filter(|s| field(s, "scope").as_str() == Some("P5a"))
        .cloned()
        .collect();
    let mut steps_done = 0usize;
    let mut headers_allowanced = 0usize;
    let mut steps_skipped = 0usize;
    let mut scenarios_done = 0usize;
    let scenarios_skipped_p5b = all.len() - scenarios.len();

    for sc in &scenarios {
        let name = field(sc, "name").as_str().unwrap_or("?").to_string();
        let region = match field(sc, "region").as_str() {
            Some("INTL") => Region::Intl,
            _ => Region::Cn,
        };
        let rp = start_real_replay(&name, region, sc, None, &env).await;

        for step in field(sc, "steps").as_array().expect("steps 段") {
            let desc = field(step, "desc").as_str().unwrap_or("");
            let method = field(step, "method").as_str().unwrap();
            let path = field(step, "path").as_str().unwrap();
            let body = field(step, "req_body").as_str().unwrap_or("");
            let r = send(
                &rp.app.base,
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
            let outbound_only = field(step, "body_compare").as_str() == Some("outbound-only");
            if outbound_only {
                steps_skipped += 1;
                println!("SKIP(downstream-p5c): [{name}] {desc}（只比出站请求与状态码）");
            } else {
                // P5c 起非流式 200 步的响应体也逐字节比：聚合体里的 `created` /
                // `chatcmpl-<hex>` 是墙钟/随机字段，归一成标记（其余字节照比）。
                let want_body = normalize_volatile_json(&normalize_uptime(
                    &rp.normalize_json_lib_error(&rp.normalize_body(
                        field(step, "resp_body").as_str().unwrap_or(""),
                    )),
                ));
                let got_body = normalize_volatile_json(&normalize_uptime(
                    &rp.normalize_json_lib_error(&rp.normalize_body(&r.text())),
                ));
                assert_eq!(got_body, want_body, "[{name} / {desc}] 响应体与 Go 不一致");
                // 响应头（白名单，大小写不敏感：Go 采集的是规范化名，超文本头名大小写不敏感）
                if let Some(want) = field(step, "resp_headers").as_object() {
                    for (k, v) in want {
                        if k == "content-length" {
                            // content-length 是**派生的**头部，逐字节比对没有意义：
                            // ① **HEAD**：体为空，黄金里的数字是「本该写出的体长」（本地化前）；
                            // ② 含绝对路径的体（`/health` 的 auth_search_dirs / auth_file）：
                            //    采集机目录与本机临时目录长度不同，数字必然不同。
                            // 这两处都是「同一份语义、不同长度」，体本身已在上面逐字节比过；
                            // 计数并跳过（`HEAD` 的 content-length 另有
                            // `http_contract::head_content_length_matches_get_body` 正面证据）。
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
        let mut got = rp.upstream.hits();
        if field(sc, "upstream_sorted_by_body").as_bool().unwrap_or(false) {
            want.sort_by_key(|v| {
                let b = field(v, "body").as_str().unwrap_or("").to_string();
                (
                    b.chars().take(200).collect::<String>(),
                    field(v, "url").as_str().unwrap_or("").to_string(),
                )
            });
            got.sort_by_key(hop_sort_key);
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
            let mut got_files: Vec<(String, Vec<u8>)> = std::fs::read_dir(rp.data.path())
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
            let n = std::fs::read_dir(rp.data.path())
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_name().to_string_lossy().starts_with("diag-11133-"))
                        .count()
                })
                .unwrap_or(0);
            assert_eq!(n, 0, "[{name}] Go 侧没有落现场，Rust 侧却落了 {n} 份");
        }

        // 凭据只读：回放结束后 auth 目录的字节必须与黄金记录的「跑完形态」一致
        // （默认 = `auth_files`；场景自己改写过登录文件时看 `auth_files_after`）。
        let after = if field(sc, "auth_files_after").is_array() {
            field(sc, "auth_files_after")
        } else {
            field(sc, "auth_files")
        };
        if let Some(arr) = after.as_array() {
            for f in arr {
                let fname = field(f, "name").as_str().unwrap_or("");
                let got = std::fs::read_to_string(rp.auth.path().join(fname)).unwrap_or_default();
                assert_eq!(
                    got,
                    field(f, "body").as_str().unwrap_or(""),
                    "[{name}] auth 文件 {fname} 的字节与黄金记录不一致（凭据层必须只读）"
                );
            }
        }
        rp.finish();
        scenarios_done += 1;
    }

    println!(
        "HTTP 场景黄金对比：{scenarios_done} 个场景 / {steps_done} 步逐字节一致；\
显式跳过 {steps_skipped} 步（downstream-only，P5c）+ {scenarios_skipped_p5b} 个 P5b 场景（由 \
p5b_golden.rs 的生产装配门禁接管）；已登记 allowance {headers_allowanced} 处（content-length）"
    );
}

/// 路由边界与黄金同源：`/health` 的 credential 块来自真凭据层（不再是手写的缝）。
#[test]
fn golden_meta_records_production_config() {
    let g = load_golden();
    let meta = field(&g, "meta");
    assert_eq!(
        field(field(meta, "p5b"), "atrest_secret_fixture").as_str(),
        Some("c2VjcmV0LWdvbGRlbi1zZWNyZXQ="),
        "黄金必须记录 at-rest 合成 secret（回放侧靠它重建夹具）"
    );
    assert!(
        field(meta, "scope_note").as_str().unwrap_or("").contains("cred_mode=real"),
        "黄金必须声明所有场景走生产凭据层"
    );
    let _ = json!({});
}
