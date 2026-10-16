//! P6 黄金门禁：用真 Go 采收的基准（`tests/golden/golden-proxy-go.json`）逐条对照。
//!
//! 两段：
//! * **纯函数段**：别名表 / 模型目录 / 套餐门槛 / 消息转换 / system 提取 / token 估算 /
//!   溢出解析 / finishReason / planLabel / trimBearer / 金额估算 / JSON 编码边角 —— 逐字节；
//! * **场景段**：起假上游 + 真 HTTP 打被测服务，比
//!   ① 上游命中的**方法 / 路径 / 体**（逐字节，体含 `[BIG:…]` 规格串已展开）
//!   ② 下游**状态码 / Content-Type / 体**（逐字节，已归一化易变字段）。
//!
//! 归一化在门禁侧统一实现（`tests/common/mod.rs` 的 `norm_volatile`，**期望侧与实际侧都过它**）：
//! 生成型 id、`created`、`threadId`、`config.date`、`uptimeSec`、`started`、`Content-Length`、
//! 假上游端口、统计日键，以及 **`daysLeft`**（每天递减；自检见
//! `tests/http_contract.rs::golden_clock_discipline_days_left_is_normalized`）。
//! 采集器侧（`.work/rust-rewrite/collect-proxy_test.go` 的 `p6Placeholders`）**没有** daysLeft 规则
//! ——该字段靠门禁双侧归一兜住，重采时黄金里是采集日原值（`tests/golden/README.md` 归一口径表已注明）。

mod common;

use std::collections::HashMap;

use common::*;
use serde_json::Value;

/// `?plan` 之外的模型目录排序（与 Go 的 SliceStable 同）。
fn scenario_labels(g: &Value) -> Vec<String> {
    g["scenarios"]
        .as_array()
        .expect("scenarios 必是数组")
        .iter()
        .map(|s| s["label"].as_str().unwrap_or("").to_string())
        .collect()
}

// ===========================================================================
// 纯函数段
// ===========================================================================

#[test]
fn pure_functions_match_go_golden() {
    let g = load_golden();
    let pure = &g["pure"];
    let mut checks = 0usize;

    // 1) 别名表：逐键逐值（含全部 92 条）
    let table = pure["alias_table"].as_object().expect("alias_table");
    for (k, v) in table {
        let got = rs_proxy::model::MODEL_ALIASES
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, vv)| *vv);
        assert_eq!(got, Some(v.as_str().unwrap()), "别名键 {k} 与 Go 不一致");
        checks += 1;
    }
    // 键数也要一致（Rust 侧多一条或少一条都要炸）
    assert_eq!(
        rs_proxy::model::MODEL_ALIASES.len(),
        table.len(),
        "别名表条数不一致：Rust {} vs Go {}",
        rs_proxy::model::MODEL_ALIASES.len(),
        table.len()
    );

    // 2) 别名解析（入参 → 出参）
    for c in pure["aliases"].as_array().unwrap() {
        let input = c["in"].as_str().unwrap();
        let want = c["out"].as_str().unwrap();
        assert_eq!(rs_proxy::map_model(input), want, "map_model({input:?})");
        checks += 1;
    }

    // 3) 模型目录（逐条 + 顺序）
    let cat = pure["catalog"].as_array().unwrap();
    assert_eq!(
        rs_proxy::CATALOG_MODELS.len(),
        cat.len(),
        "目录条数不一致"
    );
    for (i, c) in cat.iter().enumerate() {
        let m = rs_proxy::CATALOG_MODELS[i];
        assert_eq!(m.id, c["id"].as_str().unwrap(), "第 {i} 条 id");
        assert_eq!(m.owned_by, c["owned_by"].as_str().unwrap(), "第 {i} 条 owned_by");
        assert_eq!(m.on_go, c["on_go"].as_bool().unwrap(), "第 {i} 条 on_go");
        checks += 1;
    }

    // 4) 套餐门槛
    for c in pure["plan_gate"].as_array().unwrap() {
        let m = c["in"].as_str().unwrap();
        let want_err = c["err"].as_str().unwrap();
        let got = rs_proxy::anthropic::check_plan_model(m);
        if want_err.is_empty() {
            assert!(got.is_ok(), "check_plan_model({m:?}) 应放行，得到 {got:?}");
        } else {
            assert_eq!(got.unwrap_err(), want_err, "check_plan_model({m:?})");
        }
        checks += 1;
    }

    // 5) ConvertMessages（逐字节 JSON）
    for c in pure["convert"].as_array().unwrap() {
        let desc = c["desc"].as_str().unwrap();
        let input: Vec<rs_proxy::api::OpenAIMessage> =
            parse_go_messages(c["in"].as_str().unwrap());
        let out = rs_proxy::convert::convert_messages(&input);
        let got = messages_to_go(&out);
        assert_eq!(got, c["out"].as_str().unwrap(), "convert {desc}");
        checks += 1;
    }

    // 6) ExtractSystem
    for c in pure["extract_system"].as_array().unwrap() {
        let input = parse_go_messages(c["in"].as_str().unwrap());
        let (sys, rest) = rs_proxy::convert::extract_system(&input);
        assert_eq!(sys, c["system"].as_str().unwrap(), "system of {}", c["in"]);
        let rest_go = if rest.is_empty() {
            // Go 的 nil 切片编成 `null`（ExtractSystem 的 rest 全程 append，为空时是 nil）
            "null".to_string()
        } else {
            let mut s = String::from("[");
            for (i, m) in rest.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&message_to_go(m));
            }
            s.push(']');
            s
        };
        assert_eq!(rest_go, c["rest"].as_str().unwrap(), "rest of {}", c["in"]);
        checks += 1;
    }

    // 7) estTokens
    for c in pure["est_tokens"].as_array().unwrap() {
        let input = parse_go_messages(c["in"].as_str().unwrap());
        let (_, rest) = rs_proxy::convert::extract_system(&input);
        assert_eq!(
            rs_proxy::convert::est_tokens(&rest),
            c["n"].as_i64().unwrap(),
            "est_tokens of {}",
            c["in"]
        );
        checks += 1;
    }

    // 8) parseContextOverflow
    for c in pure["parse_overflow"].as_array().unwrap() {
        assert_eq!(
            rs_proxy::build::parse_context_overflow(c["in"].as_str().unwrap()),
            c["n"].as_i64().unwrap(),
            "parseContextOverflow"
        );
        checks += 1;
    }

    // 9) normalizeFinishReason
    for c in pure["finish_reason"].as_array().unwrap() {
        assert_eq!(
            rs_proxy::stream::normalize_finish_reason(c["in"].as_str().unwrap()),
            c["out"].as_str().unwrap()
        );
        checks += 1;
    }

    // 10) planLabel
    for c in pure["plan_label"].as_array().unwrap() {
        assert_eq!(
            rs_proxy::stats::plan_label(c["in"].as_str().unwrap()),
            c["out"].as_str().unwrap()
        );
        checks += 1;
    }

    // 11) trimBearer
    for c in pure["trim_bearer"].as_array().unwrap() {
        assert_eq!(
            rs_proxy::usage::trim_bearer(c["in"].as_str().unwrap()),
            c["out"].as_str().unwrap(),
            "trimBearer({:?})",
            c["in"]
        );
        checks += 1;
    }

    // 12) estimateCost
    for c in pure["estimate_cost"].as_array().unwrap() {
        let raw = model_stats_of(c["models"].as_str().unwrap());
        let got = rs_proxy::stats::estimate_cost(&raw);
        let want = c["cost"].as_f64().unwrap();
        assert!(
            (got - want).abs() <= want.abs().max(1.0) * 1e-12,
            "estimateCost {}: got {got} want {want}",
            c["desc"]
        );
        checks += 1;
    }

    // 13) JSON 编码边角（Go 的字符串/浮点/键序规则）
    for c in pure["encoding"].as_array().unwrap() {
        let v: Value = serde_json::from_str(c["in"].as_str().unwrap()).expect("编码输入");
        assert_eq!(
            rs_proxy::gjson::to_go(&v),
            c["out"].as_str().unwrap(),
            "编码 {}",
            c["desc"]
        );
        checks += 1;
    }

    println!("纯函数黄金对比：{checks} 项与 Go 一致");
}

/// 把黄金里的模型统计表解成 `BTreeMap<String, ModelStat>`（数字容忍整/浮点两种写法）。
fn model_stats_of(raw: &str) -> std::collections::BTreeMap<String, rs_proxy::stats::ModelStat> {
    let v: Value = serde_json::from_str(raw).expect("模型表输入");
    let mut out = std::collections::BTreeMap::new();
    if let Some(m) = v.as_object() {
        for (name, ms) in m {
            let n = |k: &str| -> i64 {
                ms.get(k)
                    .and_then(|x| x.as_f64())
                    .map(|f| f as i64)
                    .unwrap_or(0)
            };
            out.insert(
                name.clone(),
                rs_proxy::stats::ModelStat {
                    runs: n("Runs"),
                    input_tokens: n("InputTokens"),
                    output_tokens: n("OutputTokens"),
                    cache_read_tokens: n("CacheReadTokens"),
                    cache_write_tokens: n("CacheWriteTokens"),
                    days: None,
                },
            );
        }
    }
    out
}

/// 把 Go 采样的 messages JSON 解成 Rust 结构（数字容忍整数/浮点两种写法）。
fn parse_go_messages(raw: &str) -> Vec<rs_proxy::api::OpenAIMessage> {
    let v: Value = serde_json::from_str(raw).expect("messages 输入");
    let arr = v.as_array().cloned().unwrap_or_default();
    arr.into_iter().map(message_of_value).collect()
}

fn message_of_value(v: Value) -> rs_proxy::api::OpenAIMessage {
    let m = v.as_object().cloned().unwrap_or_default();
    let mut out = rs_proxy::api::OpenAIMessage {
        role: m.get("role").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        content: match m.get("content") {
            None => None,
            Some(x) => Some(x.clone()),
        },
        name: m
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tool_call_id: m
            .get("tool_call_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        reasoning_content: m
            .get("reasoning_content")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tool_calls: Vec::new(),
    };
    if let Some(tcs) = m.get("tool_calls").and_then(|x| x.as_array()) {
        for tc in tcs {
            let f = tc.get("function").cloned().unwrap_or(Value::Null);
            out.tool_calls.push(rs_proxy::api::ToolCall {
                id: tc.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                r#type: tc.get("type").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                function_name: f.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                function_arguments: f
                    .get("arguments")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    out
}

fn message_to_go(m: &rs_proxy::api::OpenAIMessage) -> String {
    let v = serde_json::json!({
        "role": m.role,
        "content": m.content,
        "name": if m.name.is_empty() { Value::Null } else { Value::String(m.name.clone()) },
        "tool_calls": if m.tool_calls.is_empty() { Value::Null } else { Value::Null },
        "tool_call_id": if m.tool_call_id.is_empty() { Value::Null } else { Value::String(m.tool_call_id.clone()) },
        "reasoning_content": if m.reasoning_content.is_empty() { Value::Null } else { Value::String(m.reasoning_content.clone()) },
    });
    // 只保留非空键，形态与 Go 的 omitempty 近似（门禁比的是 system/rest 的**内容**）
    let mut obj = serde_json::Map::new();
    if let Some(o) = v.as_object() {
        for (k, val) in o {
            if !val.is_null() {
                obj.insert(k.clone(), val.clone());
            }
        }
    }
    serde_json::to_string(&Value::Object(obj)).unwrap_or_default()
}

fn messages_to_go(msgs: &[rs_proxy::api::CCMessage]) -> String {
    let mut s = String::from("[");
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let mut parts = String::from("[");
        for (j, p) in m.content.iter().enumerate() {
            if j > 0 {
                parts.push(',');
            }
            parts.push_str(&p.to_go());
        }
        parts.push(']');
        let mut o = rs_proxy::gjson::GoObj::new();
        o.field_str("role", &m.role).raw("content", &parts);
        s.push_str(&o.finish());
    }
    s.push(']');
    s
}

// ===========================================================================
// 场景段
// ===========================================================================

/// 从黄金场景里取：上游规格 + 下游期望。
struct ScenarioRunner {
    hits: Vec<CapturedHop>,
    downstream: Downstream,
}

/// 该场景被采集时用的服务端默认 key。
///
/// 基准里 `proxy_key` 缺失 = `k`（段 C/D/E/F/G/H 的 `NewProxy("k")`）；
/// 段 A 显式记 `server-default-key`，三个 nokey 场景显式记空串。
/// 缺字段时给 `k` 只是为了让门禁在旧基准上也能跑出「可读的差异」，
/// 而不是在构造 Proxy 时就 panic（场景标签会随断言一起打出来）。
fn recorded_proxy_key(scenario: &Value) -> &str {
    match scenario.get("proxy_key") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Null) | None => "k",
        Some(other) => panic!("proxy_key 不是字符串：{other}"),
    }
}

/// 该场景被采集时是否调过 `SetStatsFile`（基准里显式 `"stats_file": true`）。
fn recorded_stats_file(scenario: &Value) -> bool {
    matches!(scenario.get("stats_file"), Some(Value::Bool(true)))
}

async fn run_scenario(scenario: &Value, index: usize) -> Vec<ScenarioRunner> {
    let label = scenario["label"].as_str().unwrap_or("");
    let spec = &scenario["upstream_spec"];
    let status = spec["status"].as_u64().unwrap_or(0) as u16;
    let ct = spec["content_type"].as_str().unwrap_or("text/event-stream");
    let body = spec["body"].as_str().unwrap_or("").to_string();

    // `/v1/usage` 的两个场景打的是 base_url 上的四个「官网用量」端点（不是 /alpha/generate），
    // 采集器为它们另起了 `usageUp` / `badUp`。这里必须同样分派，否则固定单一响应体的
    // FakeUpstream 会让整段响应退化成 `whoami:null`（实测 `usage-endpoints #0` 逐字节比红）。
    let is_usage = label == "usage-endpoints";
    let is_usage_down = label == "usage-degraded";
    let up = FakeUpstream::start(status, ct, body).await;
    let usage = if is_usage { Some(FakeUsage::start().await) } else { None };
    let usage_down = if is_usage_down { Some(FakeUsageDown::start().await) } else { None };

    // 服务端默认 key 由基准给出：三个「无 key → 401」场景用的是空 key，其余是
    // `server-default-key`（采集器在段 A/B 记 `proxy_key`，见 `.work/p6/` 与采集器注释）。
    // 不写死在门禁里，否则基准与门禁对「该场景的 key」各持一套说法。
    let mut proxy = rs_proxy::Proxy::new(recorded_proxy_key(scenario));
    proxy.base_url = match (&usage, &usage_down) {
        (Some(u), _) => u.base.clone(),
        (_, Some(d)) => d.base.clone(),
        _ => up.base.clone(),
    };
    // 版本号固定，避免门禁依赖网络（黄金里的 `<CCVER>` 占位与它无关：头值被归一化）
    rs_proxy::version::set_version_for_test("9.9.9-gate");
    let dir = TestDir::new(&format!("golden-{index}"));
    // 统计文件同理，由基准决定：Go 只在「段 D」调 `SetStatsFile`，其余场景 `p.Stats.file == ""`
    // ⇒ `/v1/stats` 的 `statsFile` 是 `filepath.Base("") == "."`。harness 若无条件设文件，
    // 就会把 `route-boundaries #17` 比成 `"stats.json"`（假差异，曾实际误导过一次）。
    if recorded_stats_file(scenario) {
        let stats_file = dir.path().join("stats.json");
        proxy.set_stats_file(&stats_file.to_string_lossy());
    }
    let target = Target::start(proxy).await;
    // 段 E 的最后一步（`无 key → 401`）用的是 `NewProxy("")` 的**另一个实例**，
    // 起在另一个端口上（采集器 `p2` / `down2`）。这一步仍要真打一次真 HTTP。
    let target_no_key = if is_usage {
        let mut p2 = rs_proxy::Proxy::new("");
        p2.base_url = usage.as_ref().map(|u| u.base.clone()).unwrap_or_default();
        Some(Target::start(p2).await)
    } else {
        None
    };

    let mut out = Vec::new();
    for step in scenario["steps"].as_array().cloned().unwrap_or_default() {
        up.clear();
        // 步骤自带 method/path/req_header；请求体优先取**本步**记录（`req_body`），
        // 缺则回落场景级 `probe.body`（段 A/B 的 probe 与唯一那一步同体）。
        // 不能一律读 `scenario.probe.body`：段 D 的步骤没有 scenario 级 probe，
        // 那样会把 `/v1/calibration` 三种 form 与两种 chat 体全发成空体（曾实测 #1 得 400）。
        let probe = &scenario["probe"];
        let method = step["method"].as_str().unwrap_or("GET").to_string();
        if !method.is_empty() {
            let mut headers: HashMap<String, String> = HashMap::new();
            if let Some(h) = step["req_header"].as_object() {
                for (k, v) in h {
                    if !v.is_null() {
                        headers.insert(k.clone(), v.as_str().unwrap_or("").to_string());
                    }
                }
            }
            // 段 E 的 `无 key → 401` 步走的是 `NewProxy("")` 那个实例（采集器 `p2`）。
            // 判据取自基准本身：本步没带客户端 Authorization 才可能落到「无默认 key」分支。
            let has_auth = headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"));
            let base = match (&target_no_key, has_auth) {
                (Some(t2), false) => t2.base.as_str(),
                _ => target.base.as_str(),
            };
            let body = step
                .get("req_body")
                .and_then(|b| b.as_str())
                .or_else(|| probe["body"].as_str())
                .map(|b| expand_big(b).into_bytes());
            let ds = send_raw(
                base,
                &method,
                step["path"].as_str().unwrap_or("/"),
                &headers,
                body,
            )
            .await;
            out.push(ScenarioRunner {
                hits: up.hits(),
                downstream: ds,
            });
        } else {
            // 无 probe 的步骤（占位）
            out.push(ScenarioRunner {
                hits: up.hits(),
                downstream: Downstream {
                    status: 0,
                    headers: Vec::new(),
                    body: Vec::new(),
                },
            });
        }
    }
    let _ = label;
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenarios_match_go_golden() {
    let g = load_golden();
    let bodies = &g["bodies"];
    let labels = scenario_labels(&g);
    let mut step_checks = 0usize;
    let mut skipped = 0usize;

    for (idx, scenario) in g["scenarios"].as_array().unwrap().iter().enumerate() {
        let label = labels[idx].clone();
        let runs = run_scenario(scenario, idx).await;
        let steps = scenario["steps"].as_array().cloned().unwrap_or_default();
        for (si, (step, run)) in steps.iter().zip(runs.iter()).enumerate() {
            let desc = step["desc"].as_str().unwrap_or("");
            let tag = format!("[{label} #{si} {desc}]");

            // ① 下游状态码
            let want_status = step["status"].as_u64().unwrap_or(0) as u16;
            assert_eq!(
                run.downstream.status, want_status,
                "{tag} 下游状态码不一致（实得 {} want {}）",
                run.downstream.status, want_status
            );

            // ② 下游 Content-Type（黄金里记的是响应头全量，取 content-type 逐字比）
            if let Some(want_ct) = step["resp_header"]["Content-Type"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
            {
                let got_ct = run.downstream.header("content-type").unwrap_or("");
                assert_eq!(got_ct, want_ct, "{tag} Content-Type 不一致");
            }

            // ③ 下游体（归一化后逐字节）
            let want_body_raw = bodies[&label][step["body_ref"].as_str().unwrap_or("")]
                .as_str()
                .unwrap_or("")
                .to_string();
            let want_body = norm_expected_body(&want_body_raw);
            let got_body = norm_volatile(&String::from_utf8_lossy(&run.downstream.body));
            let got_body = if got_body.is_empty() && !want_body.is_empty() && want_body.trim() == "null" {
                got_body
            } else {
                got_body
            };
            assert_eq!(got_body, want_body, "{tag} 下游体不一致");
            if std::env::var("ZZDUMP").is_ok() {
                println!(
                    "ZZDUMP {tag} got_len={} got_tail={:?} want_len={} want_tail={:?}",
                    got_body.len(),
                    &got_body[got_body.len().saturating_sub(3)..],
                    want_body.len(),
                    &want_body[want_body.len().saturating_sub(3)..]
                );
            }

            // ④ 上游命中：方法 / 路径 / 体
            //
            // `upstream` 在基准里有**三种**形态，语义都是「本步期望的命中列表」：
            // * `null` —— 采集时那条 `st.Upstream` 是 nil 切片（段 D/G/H 的步骤
            //   `*hooks = nil` 后没有再命中，Go 的 nil 切片就不是空数组）；
            // * `[]` —— 非 nil 空切片（段 A/B 的显式赋值）；
            // * `[{...}]` —— 真命中。
            // 三种都按「期望次数 = 列表长度」比，**不能**把 `null` 当「本步不比」——
            // 早期那次 `stats-and-calibration #1 实得 1 want 0` 就是这么来的（基准里是 null）。
            let want_hits = step["upstream"].as_array().cloned().unwrap_or_default();
            // 基准必须自描述「本步期望的上游命中」：`upstream` 键缺失 = 采集时没量到
            // （基准残缺），显式计数而不是当 0 逃过去。
            if step.get("upstream").is_none() {
                skipped += 1;
            }
            assert_eq!(
                run.hits.len(),
                want_hits.len(),
                "{tag} 上游命中次数不一致（实得 {}，want {}）",
                run.hits.len(),
                want_hits.len()
            );
            for (hi, wh) in want_hits.iter().enumerate() {
                let gh = &run.hits[hi];
                assert_eq!(
                    gh.method,
                    wh["method"].as_str().unwrap_or("POST"),
                    "{tag} 上游第 {hi} 次方法"
                );
                assert_eq!(
                    gh.path,
                    wh["path"].as_str().unwrap_or(""),
                    "{tag} 上游第 {hi} 次路径"
                );
                let want_b = golden_upstream_body(wh["body"].as_str().unwrap_or(""));
                let got_b = norm_hop_body(expand_big(&String::from_utf8_lossy(&gh.body)).as_bytes());
                if want_b != got_b {
                    let (d, n) = first_diff(&want_b, &got_b);
                    panic!(
                        "{tag} 上游第 {hi} 次体不一致（首处差异 @{d}）：\n want: …{}…\n  got: …{}…",
                        &want_b[d.saturating_sub(n)..(d + n).min(want_b.len())],
                        &got_b[d.saturating_sub(n)..(d + n).min(got_b.len())]
                    );
                }
                // 合同头逐条比（只比黄金记录的那几个：accept/authorization/content-type/
                // x-command-code-version/x-cli-environment）
                if let Some(wh_hdr) = wh["header"].as_object() {
                    for (k, vs) in wh_hdr {
                        let want_v = vs.as_array().and_then(|a| a.first()).and_then(|v| v.as_str()).unwrap_or("");
                        let got_v = gh.header(k).unwrap_or("");
                        // 头值本身不带 `名字:` 前缀，拼成一行再走同一套归一化
                        let want_norm = norm_header_line(k, want_v);
                        let got_norm = norm_header_line(k, got_v);
                        assert_eq!(got_norm, want_norm, "{tag} 上游头 {k} 不一致");
                    }
                }
                step_checks += 1;
            }
        }
    }
    println!(
        "黄金对照汇总：{} 个场景 / {} 步出站体逐字节一致；基准未量到上游命中的步 {} 步（应为 0）",
        g["scenarios"].as_array().unwrap().len(),
        step_checks,
        skipped
    );
}

/// 黄金里已经归一化过一次，这里把展开后的形态再归一化一次（`[BIG:…]` 展开后
/// 体里出现的数字不会再被规则碰到，故只补 `Content-Length` 之外的必要处理）。
fn norm_expected_body(s: &str) -> String {
    let s = expand_big(s);
    norm_volatile(&s)
}

/// 把「头名 + 值」拼成一行做归一化（`norm_volatile` 的规则按 `名字: 值` 文本设计）。
fn norm_header_line(name: &str, value: &str) -> String {
    norm_volatile(&format!("{name}: {value}"))
}

fn first_diff(a: &str, b: &str) -> (usize, usize) {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut i = 0;
    while i < ab.len() && i < bb.len() && ab[i] == bb[i] {
        i += 1;
    }
    (i, 120)
}

/// 门禁非空转自检：把黄金改一个字节必须让门禁变红（由 `.work` 的突变脚本驱动），
/// 这里只断言「基准里的场景数与已知相符」，防止基准被意外截断。
#[test]
fn golden_shape_is_complete() {
    let g = load_golden();
    let labels = scenario_labels(&g);
    assert_eq!(labels.len(), 52, "场景数变了（基准被改？）labels={labels:?}");
    let bodies = g["bodies"].as_object().expect("bodies");
    assert_eq!(bodies.len(), 52, "bodies 的 scenario 数与场景数不符");
    assert_eq!(
        g["pure"]["catalog"].as_array().unwrap().len(),
        61,
        "目录条数变了"
    );
    assert_eq!(
        g["pure"]["alias_table"].as_object().unwrap().len(),
        92,
        "别名表条数变了"
    );
}
