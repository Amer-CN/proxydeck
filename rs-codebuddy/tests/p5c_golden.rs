//! P5c 黄金门禁：`rs-codebuddy` 的**脱敏 / SSE 清理 / 非流式聚合 / 统计 / 路径清洗 /
//! 积分失败措辞**与真 Go 的对照。
//!
//! 口径：
//! * 状态码、白名单响应头、**上游请求体与头**逐字节比；
//! * 下游响应体逐字节比（非 JSON 体）或「键序无关一致」比（JSON 体，见差异 22）；
//! * 非流式聚合体里的 `created` / `chatcmpl-<hex>` 两处墙钟/随机字段归一成标记
//!   （`normalize_volatile_json`），其余字节照比；
//! * `codebuddy-stats(-intl).json` 落盘内容按键序无关比；
//! * **脱敏是本轮的硬门禁**：`p5c-desensitize-cn/-intl` 两条场景在 `desensitize=true`
//!   的**生产装配**下采收，出站体逐字节比 ⇒ 与 Go 的 `--desensitize` 口径等价。
//!
//! 场景与再生成链见 `tests/golden/README.md`。

mod common;

use common::*;
use serde_json::Value;

use rs_codebuddy::gjson;
use rs_codebuddy::region::Region;

fn field<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

fn region_of(sc: &Value) -> Region {
    match field(sc, "region").as_str() {
        Some("INTL") => Region::Intl,
        _ => Region::Cn,
    }
}

/// 键序无关一致（Go 的结构体序列化 vs Rust 的键序序列化，登记在 README 差异 22）。
fn canon_json(s: &str) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(v) => gjson::to_go(&v),
        Err(e) => panic!("待归一的内容不是合法 JSON: {e}\n{s}"),
    }
}

fn assert_body_equal(name: &str, desc: &str, got: &str, want: &str) {
    if serde_json::from_str::<Value>(got).is_ok() && serde_json::from_str::<Value>(want).is_ok() {
        assert_eq!(
            canon_json(got),
            canon_json(want),
            "[{name} / {desc}] 响应体（键序无关、数组顺序敏感）与 Go 不一致"
        );
        return;
    }
    assert_eq!(got, want, "[{name} / {desc}] 响应体与 Go 不一致（非 JSON，逐字节比）");
}

fn compare_hop(scenario: &str, i: usize, want: &Value, got: &CapturedHop) {
    assert_eq!(
        field(want, "method").as_str().unwrap(),
        got.method,
        "[{scenario} # {i}] 上游方法不一致"
    );
    assert_eq!(
        url_path(field(want, "url").as_str().unwrap()),
        got.path,
        "[{scenario} # {i}] 上游路径不一致"
    );
    assert_eq!(
        norm_upstream_body(field(want, "body").as_str().unwrap()),
        norm_upstream_body(&String::from_utf8_lossy(&got.body)),
        "[{scenario} # {i}] 上游请求体与 Go 不一致（**脱敏硬门禁**）"
    );
    if let Some(wh) = field(want, "headers").as_object() {
        for (k, v) in wh {
            let wv = v.as_str().unwrap_or("");
            match got.header(k) {
                Some(g) => assert_eq!(g, wv, "[{scenario} # {i}] 上游请求头 {k} 与 Go 不一致"),
                None => panic!("[{scenario} # {i}] 上游请求头 {k} 缺失（Go 侧为 {wv}）"),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p5c_scenarios_match_go_golden() {
    let env = EnvGuard::set(&[]);
    let g = load_golden();
    let all = field(&g, "scenarios").as_array().expect("scenarios 段");
    let scenarios: Vec<&Value> = all
        .iter()
        .filter(|s| field(s, "scope").as_str() == Some("P5c"))
        .collect();
    assert_eq!(
        scenarios.len(),
        8,
        "P5c 场景数不该变（实得 {}）：脱敏 CN/INTL、流式清洗、非流式聚合、大流 Peek、统计、路径清洗、积分失败",
        scenarios.len()
    );

    let mut steps_done = 0usize;
    let mut desensitize_steps = 0usize;
    for sc in &scenarios {
        let name = field(sc, "name").as_str().unwrap_or("?").to_string();
        let desc_sc = field(sc, "desc").as_str().unwrap_or("");
        let rp = start_real_replay(&name, region_of(sc), sc, None, &env).await;
        for step in field(sc, "steps").as_array().expect("steps 段") {
            let desc = field(step, "desc").as_str().unwrap_or("");
            let method = field(step, "method").as_str().unwrap();
            let path = field(step, "path").as_str().unwrap();
            let body = field(step, "req_body").as_str().unwrap_or("");
            let resp = if field(step, "raw_request").as_bool().unwrap_or(false) {
                // 点段路径：reqwest/url 会在客户端规范化，只有裸 socket 能送到原始路径。
                send_raw(&rp.app.base, method, path).await
            } else {
                send(
                    &rp.app.base,
                    method,
                    path,
                    if body.is_empty() {
                        None
                    } else {
                        Some(body.as_bytes())
                    },
                )
                .await
            };
            assert_eq!(
                resp.status,
                field(step, "resp_status").as_u64().unwrap() as u16,
                "[{name} / {desc}] 状态码与 Go 不一致（实得 body: {}）",
                resp.text()
            );
            let want_norm = rp.normalize_json_lib_error(&normalize_volatile_json(
                &rp.normalize_body(field(step, "resp_body").as_str().unwrap_or("")),
            ));
            let got_norm =
                rp.normalize_json_lib_error(&normalize_volatile_json(&rp.normalize_body(&resp.text())));
            assert_body_equal(&name, desc, &got_norm, &want_norm);
            if let Some(want) = field(step, "resp_headers").as_object() {
                for (k, v) in want {
                    if k == "content-length" {
                        // 派生头部：黄金里的数字含采集机绝对路径长度，体本身已比过。
                        continue;
                    }
                    let wv = v.as_str().unwrap_or("");
                    match resp.header(k) {
                        Some(got) => {
                            assert_eq!(got, wv, "[{name} / {desc}] 响应头 {k} 与 Go 不一致")
                        }
                        None => panic!("[{name} / {desc}] 响应头 {k} 缺失（Go 侧为 {wv}）"),
                    }
                }
            }
            steps_done += 1;
            if field(sc, "desensitize").as_bool().unwrap_or(false) {
                desensitize_steps += 1;
            }
            let _ = desc_sc;
        }

        // 上游看到的请求（含脱敏后的出站体）
        let want: Vec<Value> = field(sc, "upstream_seen")
            .as_array()
            .cloned()
            .unwrap_or_default();
        let got = rp.upstream.hits();
        assert_eq!(
            got.len(),
            want.len(),
            "[{name}] 上游命中数与 Go 不一致（Go {} 次 / Rust {} 次）",
            want.len(),
            got.len()
        );
        for (i, w) in want.iter().enumerate() {
            compare_hop(&name, i, w, &got[i]);
        }

        // 统计落盘（按键序无关比）
        if field(sc, "stats_file_exists").as_bool().unwrap_or(false) {
            let suffix = if region_of(sc) == Region::Intl { "-intl" } else { "" };
            let p = rp.data.path().join(format!("codebuddy-stats{suffix}.json"));
            let got = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("[{name}] 统计文件应存在 {}: {e}", p.display()));
            assert_eq!(
                canon_json(&got),
                canon_json(field(sc, "stats_file").as_str().unwrap_or("")),
                "[{name}] codebuddy-stats 落盘内容与 Go 不一致"
            );
        }

        rp.finish();
    }

    assert_eq!(
        desensitize_steps, 3,
        "脱敏场景的步骤数不该变（硬门禁不能空转）"
    );
    println!(
        "P5c 黄金对比：{} 个场景 / {steps_done} 步（其中脱敏硬门禁 {desensitize_steps} 步：出站体逐字节比）",
        scenarios.len()
    );
}
