// tests/golden_vibex.rs —— P4 黄金门禁：纯函数 + HTTP/WS 场景逐条对 Go 基准。
//
// 基准来源：Go 侧临时采集器 `internal/vibex/zz_vibex_golden_collect_test.go`（已删除，留档
// `.work/rust-rewrite/collect-vibex_test.go`），产物 `golden-vibex-go.json`。
// 加载顺序：先 `rs-vibex/tests/golden/`，缺则回落 `.work/rust-rewrite/`。
mod common;

use std::path::PathBuf;

use common::*;
use serde_json::{json, Value};

const GOLDEN: &str = "golden-vibex-go.json";

/// 假凭据一律**取自黄金**：token 是拼接好的字符串，自己再造一份必然字节不同
/// （Go 的 map 排序 vs 手写字段序），比出来的差异全是脚手架造成的。
fn golden_tok(g: &Value, key: &str) -> String {
    g["meta"]["fake_tokens"][key]
        .as_str()
        .unwrap_or_else(|| panic!("黄金 meta.fake_tokens 缺 {key}"))
        .to_string()
}

/// 归一 WS 握手头行：名字大小写不敏感（HTTP 语义），值里的端口换成占位符。
fn norm_header_line(line: &str) -> String {
    match line.split_once(':') {
        Some((k, v)) => format!("{}: {}", k.trim().to_ascii_lowercase(), replace_ports(v.trim())),
        None => replace_ports(line),
    }
}
fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

/// 取黄金里的数组字段。
fn arr<'a>(g: &'a Value, key: &str) -> &'a Vec<Value> {
    g.get(key)
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("黄金缺数组段 {key}"))
}

// ------------------------------ 纯函数段 ------------------------------

#[test]
fn pure_functions_match_go_golden() {
    let g = load_golden(GOLDEN);
    let p = &g["pure"];
    let mut checked = 0usize;

    // C1 缺省值
    {
        use rs_vibex::config::Config;
        let mut c = Config::defaults(PathBuf::new());
        c.normalize();
        let got = json!({
            "base_url": c.base_url,
            "app_name": c.app_name,
            "app_type": c.app_type,
            "turn_timeout": c.turn_timeout_sec,
            "ready_timeout": c.ready_timeout_sec,
            "queue_limit": c.queue_limit,
            "heartbeat_s": c.heartbeat_sec,
            "connect_mode": c.connect_mode,
        });
        assert_eq!(got, p["config_defaults"], "[C1] 缺省值");
        checked += 1;
    }

    // C2/C3 normalize
    for (i, case) in arr(p, "config_normalize").iter().enumerate() {
        use rs_vibex::config::{apply_json, Config};
        let mut c = Config::defaults(PathBuf::new());
        apply_json(&mut c, &case["in"]);
        c.normalize();
        assert_eq!(c.base_url, case["base_url"], "[C2] normalize #{i} base_url");
        let text = c.to_go_indent2();
        let mut got: Value = serde_json::from_str(&text).expect("配置 JSON 应可解析");
        got.as_object_mut().unwrap().remove("tokens");
        let mut want = case["out"].clone();
        want.as_object_mut().unwrap().remove("tokens");
        assert_eq!(got, want, "[C3] normalize #{i} 全过程");
        checked += 1;
    }

    // C10/C11 JWT
    for (i, case) in arr(p, "jwt").iter().enumerate() {
        let tok = case["token"].as_str().unwrap_or("");
        let info = rs_vibex::jwt::decode_jwt(tok);
        assert_eq!(info.is_none(), case["nil"].as_bool().unwrap(), "[C10] jwt #{i} nil");
        if let Some(info) = info {
            assert_eq!(info.sub, case["sub"].as_str().unwrap_or(""), "[C10] jwt #{i} sub");
            assert_eq!(
                info.username,
                case["username"].as_str().unwrap_or(""),
                "[C10] jwt #{i} username"
            );
            assert_eq!(
                info.has_exp(),
                case["has_exp"].as_bool().unwrap_or(false),
                "[C10] jwt #{i} has_exp"
            );
            if case.get("exp_unix").is_some() {
                assert_eq!(info.exp, case["exp_unix"].as_i64(), "[C11] jwt #{i} exp");
            }
        }
        checked += 1;
    }

    // C17 daysLeft
    for (i, case) in arr(p, "days_left").iter().enumerate() {
        let now: i64 = 1_800_000_000_000;
        let exp = now + case["delta_ns"].as_i64().unwrap() / 1_000_000;
        assert_eq!(
            rs_vibex::pool::days_left(exp, now) as i64,
            case["days"].as_i64().unwrap(),
            "[C17] days_left #{i}"
        );
        checked += 1;
    }

    // C19/C20/C21 str / truthy
    for (i, case) in arr(p, "str_conversions").iter().enumerate() {
        let v = parse(case["in"].as_str().unwrap());
        assert_eq!(
            rs_vibex::util::str_v(&v),
            case["str"].as_str().unwrap(),
            "[C19] str #{i}"
        );
        assert_eq!(
            rs_vibex::util::truthy(&v),
            case["truthy"].as_bool().unwrap(),
            "[C21] truthy #{i}"
        );
        checked += 1;
    }

    // C22 appsFrom
    for case in arr(p, "apps_from") {
        let v = parse(case["in"].as_str().unwrap());
        let got = rs_vibex::util::apps_from(&v);
        match got {
            None => assert_eq!(case["out"].as_str().unwrap(), "null", "[C22] appsFrom null"),
            Some(items) => {
                let j = rs_vibex::gjson::J::Arr(
                    items.iter().map(rs_vibex::gjson::J::from_value).collect(),
                )
                .to_go();
                assert_eq!(j, case["out"].as_str().unwrap(), "[C22] appsFrom 非空");
            }
        }
        checked += 1;
    }

    // C23 unwrapObj
    for case in arr(p, "unwrap_obj") {
        let v = parse(case["in"].as_str().unwrap());
        let got = rs_vibex::gjson::J::from_value(&rs_vibex::util::unwrap_obj(&v)).to_go();
        assert_eq!(got, case["out"].as_str().unwrap(), "[C23] unwrapObj");
        checked += 1;
    }

    // C24 liveStatus
    for case in arr(p, "live_status") {
        let v = parse(case["in"].as_str().unwrap());
        let got = rs_vibex::util::live_status(v.as_object());
        assert_eq!(got, case["out"].as_str().unwrap(), "[C24] liveStatus {}", case["in"]);
        checked += 1;
    }

    // C25 portsOK
    for case in arr(p, "ports_ok") {
        let v = parse(case["in"].as_str().unwrap());
        let got = v
            .as_object()
            .map(rs_vibex::util::ports_ok)
            .unwrap_or(false);
        assert_eq!(got, case["out"].as_bool().unwrap(), "[C25] portsOK {}", case["in"]);
        checked += 1;
    }

    // C26 initRootMeta
    for case in arr(p, "init_root_meta") {
        let v = parse(case["in"].as_str().unwrap());
        let got = rs_vibex::gjson::J::from_value(&rs_vibex::util::init_root_meta(
            v.as_object().unwrap(),
        ))
        .to_go();
        assert_eq!(got, case["out"].as_str().unwrap(), "[C26] initRootMeta");
        checked += 1;
    }

    // C27 shapeOf
    for case in arr(p, "shape_of") {
        let v = parse(case["in"].as_str().unwrap());
        assert_eq!(
            rs_vibex::util::shape_of(&v),
            case["out"].as_str().unwrap(),
            "[C27] shapeOf {}",
            case["in"]
        );
        checked += 1;
    }

    // C28 extractText
    for case in arr(p, "extract_text") {
        let v = parse(case["in"].as_str().unwrap());
        assert_eq!(
            rs_vibex::prompt::extract_text(&v),
            case["out"].as_str().unwrap(),
            "[C28] extractText {}",
            case["in"]
        );
        checked += 1;
    }

    // C29~C31 buildPrompt
    for case in arr(p, "build_prompt") {
        let msgs = case["msgs"].as_array().cloned().unwrap_or_default();
        let instruction = case["instruction"].as_str().unwrap_or("");
        assert_eq!(
            rs_vibex::prompt::build_prompt(&msgs, instruction),
            case["out"].as_str().unwrap(),
            "[C29] buildPrompt {msg}",
            msg = case["msgs"]
        );
        checked += 1;
    }

    // C32 claudeEvents
    for case in arr(p, "claude_events") {
        let v = parse(case["in"].as_str().unwrap());
        let off = rs_vibex::prompt::events_to_go(
            rs_vibex::prompt::claude_events(&v, false).as_deref(),
        );
        let on = rs_vibex::prompt::events_to_go(
            rs_vibex::prompt::claude_events(&v, true).as_deref(),
        );
        assert_eq!(off, case["out_off"].as_str().unwrap(), "[C32] claudeEvents(expose=false)");
        assert_eq!(on, case["out_on"].as_str().unwrap(), "[C32] claudeEvents(expose=true)");
        checked += 1;
    }

    // C33 oaiUsage
    for case in arr(p, "oai_usage") {
        let v = parse(case["in"].as_str().unwrap());
        let u = rs_vibex::prompt::oai_usage(v.as_object());
        assert_eq!(
            rs_vibex::prompt::usage_to_go(u),
            case["out"].as_str().unwrap(),
            "[C33] oaiUsage {}",
            case["in"]
        );
        checked += 1;
    }

    // C34 foldSync（黄金里的 `in` 是**字符串**形态的 JSON 数组，见采集器）
    for case in arr(p, "fold_sync") {
        let parsed = parse(case["in"].as_str().unwrap());
        let events: Vec<rs_vibex::prompt::OutEvent> = parsed
            .as_array()
            .unwrap()
            .iter()
            .map(|e| rs_vibex::prompt::OutEvent {
                kind: e["Kind"].as_str().unwrap_or("").to_string(),
                text: e["Text"].as_str().unwrap_or("").to_string(),
                usage: e.get("Usage").cloned().filter(|v| !v.is_null()),
                flags: e.get("Flags").cloned().filter(|v| !v.is_null()),
            })
            .collect();
        let (content, usage, flags, last_err) = rs_vibex::prompt::fold_sync(&events);
        assert_eq!(content, case["content"].as_str().unwrap(), "[C34] foldSync content");
        assert_eq!(last_err, case["last_err"].as_str().unwrap(), "[C34] foldSync lastErr");
        let got_flags = flags
            .as_ref()
            .map(|v| rs_vibex::gjson::J::from_value(v).to_go())
            .unwrap_or_else(|| "null".into());
        let got_usage = usage
            .as_ref()
            .map(|v| rs_vibex::gjson::J::from_value(v).to_go())
            .unwrap_or_else(|| "null".into());
        assert_eq!(got_flags, case["flags"].as_str().unwrap(), "[C34] foldSync flags");
        assert_eq!(got_usage, case["usage"].as_str().unwrap(), "[C34] foldSync usage");
        checked += 1;
    }

    // C13 addrOf
    for case in arr(p, "addr_of") {
        assert_eq!(
            rs_vibex::pool::addr_of(case["in"].as_str().unwrap()),
            case["out"].as_str().unwrap(),
            "[C13] addrOf {:?}",
            case["in"]
        );
        checked += 1;
    }

    // C40 unauthErr
    for case in arr(p, "unauth_err") {
        let status = case["status"].as_u64().unwrap() as u16;
        let raw = case["raw"].as_str().unwrap().as_bytes();
        let got = rs_vibex::client::unauth_err(status, raw);
        assert_eq!(
            got.is_none(),
            case["nil"].as_bool().unwrap(),
            "[C40] unauthErr nil status={status} raw={:?}",
            case["raw"]
        );
        if let Some(e) = got {
            assert_eq!(e.message, case["message"].as_str().unwrap(), "[C40] unauthErr message");
            assert_eq!(
                e.status as u64,
                case["status_out"].as_u64().unwrap(),
                "[C40] unauthErr status"
            );
            assert_eq!(e.code, case["code"].as_str().unwrap(), "[C40] unauthErr code");
        }
        checked += 1;
    }

    // C45 wsURL
    for case in arr(p, "ws_url") {
        let base = case["base"].as_str().unwrap();
        let app = case["app"].as_str().unwrap();
        let got = rs_vibex::client::ws_url(base, app);
        assert_eq!(
            got.is_err(),
            case["err"].as_bool().unwrap(),
            "[C45] wsURL err base={base}"
        );
        if let Ok(u) = got {
            assert_eq!(u, case["url"].as_str().unwrap(), "[C45] wsURL base={base}");
        }
        checked += 1;
    }

    // 浮点写出（Go json.Marshal 的 float64 规则）
    let mut tie_break = 0usize;
    for case in arr(&g, "floats") {
        let f = case["in"].as_f64().unwrap();
        let got = format!("{{\"v\":{}}}", rs_vibex::gjson::J::Float(f).to_go());
        let want = case["out"].as_str().unwrap();
        if got != want {
            // 已登记的实现差异（README 差异 4）：**极大浮点**的最短十进制表示，
            // Go 的 Ryu 与 Rust 的 Display 在最后一位取整方向上会不同（本样本即如此：
            // Go 出 `…68e20`、Rust 出 `…67e20`，回读成**相邻**的两个 f64）。
            // 判据不是「看着差不多」：必须**相邻（≤1 ULP）**，差得更远即判红——
            // 否则就变成拿「都很大」当借口放行真实错误。
            let back_a: f64 = got
                .trim_start_matches("{\"v\":")
                .trim_end_matches('}')
                .parse()
                .unwrap_or(f64::NAN);
            let back_b: f64 = want
                .trim_start_matches("{\"v\":")
                .trim_end_matches('}')
                .parse()
                .unwrap_or(f64::NAN);
            let ulp = (back_a.to_bits() as i64 - back_b.to_bits() as i64).abs();
            assert!(
                ulp <= 1,
                "[floats] {f}：两串回读相差 {ulp} ULP（{got} vs {want}），超出已登记的取整方向差异"
            );
            assert!(f.abs() >= 1e17, "[floats] {f}：小量级也出现表示差异，属未登记的新差异");
            tie_break += 1;
        }
        checked += 1;
    }

    println!(
        "纯函数黄金对比：{checked} 项与 Go 一致；其中浮点最短表示的取整方向差异 {tie_break} 项\
         （回读相差 ≤1 ULP 且量级达标，已登记，见 README 差异 4）"
    );
}

// ------------------------------ HTTP/WS 场景 ------------------------------

/// 场景 → (token 列表, 上游预置)。与采集器 `collectHTTP/collectWS` 一一对应。
async fn setup_scenario(srv: &rs_vibex::Server, up: &FakeUpstream, name: &str, dir: &TestDir) {
    let _ = srv;
    let _ = dir;
    match name {
        "routes-no-token" => {}
        "accounts-add" => {}
        "models-shapes" | "models-cache" => {
            // 每个步骤用同一个 token（采集器每条场景一个 fresh env）
        }
        "token-rotation-rest" => {
            up.reject_token("FAKE-dead", 401).await;
        }
        "ws-handshake-401" => {
            up.ws_reject_token("FAKE-wsdead", 401).await;
        }
        "ws-handshake-503" => {
            up.ws_reject_token("FAKE-tok-1", 503).await;
        }
        "ws-ready 阶段 error" => {
            up.set_ready_error("连接失败原因").await;
        }
        "ws-ready 阶段 error 只有 code" => {
            up.set_ready_error("ONLY_CODE").await;
        }
        "ws-无文本无错误（空轮）" => {
            up.set_replay(vec![]).await;
        }
        "ws-balance_insufficient" => {
            up.set_replay(vec![]).await;
            up.set_done_flags(json!({"balance_insufficient": true})).await;
        }
        "ws-ready-后 error 继续泵" => {
            up.set_after_ready(vec![json!({"type": "error", "code": "X1", "message": "轮内错误"})])
                .await;
        }
        "ws-done 带 flags" => {
            up.set_done_flags(json!({"context_overflow": true, "primary_model": "pro-1"}))
                .await;
        }
        _ => {}
    }
    // 各场景的上游档位在每步按 `upstream_mode` 设置；这里只需覆盖起始档
    let mode = match name {
        "app-create" => Some("apps-none"),
        "app-create-limit" => Some("apps-create-free-limit"),
        "app-status-cached" => Some("app-status-cached-only"),
        _ => None,
    };
    if let Some(m) = mode {
        up.set_mode(m).await;
    }
}

fn tokens_for(g: &Value, name: &str) -> Vec<String> {
    match name {
        "routes-no-token" | "accounts-add" => vec![],
        "models-shapes" | "models-cache" | "ws-handshake-503" => vec!["FAKE-tok-1".into()],
        "token-rotation-rest" => vec!["FAKE-dead".into(), "FAKE-good".into()],
        "ws-handshake-401" => vec!["FAKE-wsdead".into(), "FAKE-wsgood".into()],
        "pool-converge" => vec![golden_tok(g, "A")],
        _ => vec![golden_tok(g, "A")],
    }
}

async fn replay_scenario(g: &Value, name: &str) {
    let scen = g["http"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("黄金里没有场景 {name}"))
        .clone();
    let dir = TestDir::new("golden");
    let up = FakeUpstream::start().await;
    let mut cfg = test_cfg(&up.base, dir.join("vibex-config.json"));
    cfg.tokens = tokens_for(g, name);
    if name == "queue-full" {
        cfg.queue_limit = 1;
    }
    let (srv, app) = start_app(cfg.clone(), rs_vibex::Seams::default(), &dir).await;
    setup_scenario(&srv, &up, name, &dir).await;
    if name == "queue-full" {
        // 占满信号量（采集器同样是先占满再发请求）
        let _hold = srv.queue_try_acquire().expect("应能占到唯一的槽");
        std::mem::forget(_hold);
    }
    // `models-shapes` 的每一步在采集器里都是**全新实例**（否则 5 分钟缓存会让后面几步
    // 一个上游请求都不打），这里必须同样每步重起，否则比出来的是脚手架差异。
    let mut current = app;
    let mut _keep_srv = srv.clone();

    let steps = scen["steps"].as_array().unwrap().clone();
    for (i, step) in steps.iter().enumerate() {
        if name == "models-shapes" && i > 0 {
            let (s2, a2) = start_app(cfg.clone(), rs_vibex::Seams::default(), &dir).await;
            _keep_srv = s2;
            current = a2;
        }
        let mode = step["upstream_mode"].as_str().unwrap_or("ok");
        up.set_mode(mode).await;
        let desc = step["desc"].as_str().unwrap_or("");
        // 采集器在场景中途改过上游形态的地方，这里按同一处、同一时机复刻
        // （不改上游 = 两侧打的上游不同，比出来的差异全是脚手架造成的）。
        if name == "accounts-add" && desc.contains("探活 401") {
            let c = g["meta"]["fake_tokens"]["C"].as_str().unwrap().to_string();
            up.reject_token(&c, 401).await;
        }
        let _ = up.take().await;

        let req = &step["req"];
        let method = req["method"].as_str().unwrap();
        let path = req["path"].as_str().unwrap();
        let mut extra: Vec<(String, String)> = Vec::new();
        if let Some(h) = req["headers"].as_object() {
            for (k, v) in h {
                extra.push((k.clone(), v.as_str().unwrap_or("").to_string()));
            }
        }
        let body = req["body"].as_str().unwrap_or("");
        let extra_ref: Vec<(&str, &str)> =
            extra.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (status, hdrs, text) = send(&current.base, method, path, &extra_ref, body).await;

        let case = format!("{name} #{i} {}", step["desc"].as_str().unwrap_or(""));
        let want = &step["resp"];
        assert_eq!(
            status as u64,
            want["status"].as_u64().unwrap(),
            "[{case}] 状态码（实得体：{}）",
            norm(&text)
        );
        assert_eq!(
            norm(&text),
            norm(want["body"].as_str().unwrap_or("")),
            "[{case}] 响应体"
        );
        // 响应头：黄金记了 6 个契约头（缺省 = ""）
        for (k, v) in want["headers"].as_object().unwrap() {
            // 黄金里的响应头名是 Go 的规范写法（`Access-Control-Allow-Headers`），
            // 实得侧是 reqwest 的小写；统一按小写比对。
            let got = hdrs.get(&k.to_lowercase()).cloned().unwrap_or_default();
            let want_v = v.as_str().unwrap_or("");
            assert_eq!(
                replace_ports(&got),
                replace_ports(want_v),
                "[{case}] 响应头 {k}"
            );
        }

        // 上游观测
        let rec = up.take().await;
        let want_up = step["upstream_seen"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            rec.reqs.len(),
            want_up.len(),
            "[{case}] 上游请求条数（实得 {:?}）",
            rec.reqs.iter().map(|r| format!("{} {}", r.method, r.path)).collect::<Vec<_>>()
        );
        for (r, w) in rec.reqs.iter().zip(want_up.iter()) {
            assert_eq!(r.method, w["method"].as_str().unwrap(), "[{case}] 上游 method");
            assert_eq!(r.path, w["path"].as_str().unwrap(), "[{case}] 上游 path");
            assert_eq!(
                norm(&r.body),
                norm(w["body"].as_str().unwrap_or("")),
                "[{case}] 上游请求体 {} {}",
                r.method,
                r.path
            );
            for (k, v) in w["headers"].as_object().unwrap() {
                let got = r.headers.get(k).cloned().unwrap_or_default();
                let want_v = v.as_str().unwrap_or("");
                // 非 ASCII（obs-text）头值同样**逐字节比对**，没有放行分支。
                // 早先这里有一段「实得为空」的 allowance：根因**不是**假上游丢 obs-text
                // （axum/hyper 服务端保留原始字节），而是脚手架 `pick_headers` 用
                // `HeaderValue::to_str()` 做有损读取——它只接受可见 ASCII，`username: 小明`
                // 必然 `Err` → 被读成空串，于是 87 处假 allowance 全部"通过"。
                // 已改为 `as_bytes()` + `from_utf8_lossy` 的无损读取，见 tests/common/mod.rs，
                // 根因与更正见 rs-vibex/README.md 差异 6。
                assert_eq!(
                    replace_ports(&got),
                    replace_ports(want_v),
                    "[{case}] 上游请求头 {k}（{} {}）",
                    r.method,
                    r.path
                );
            }
        }
        // WS 帧序与握手头
        let want_conns = step["ws_frames"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            rec.ws_conns.len(),
            want_conns.len(),
            "[{case}] WS 连接条数"
        );
        for (got_conn, want_conn) in rec.ws_conns.iter().zip(want_conns.iter()) {
            let want_frames: Vec<String> = want_conn
                .as_array()
                .unwrap()
                .iter()
                .map(|v| norm(v.as_str().unwrap_or("")))
                .collect();
            let got_frames: Vec<String> = got_conn.iter().map(|f| norm(f)).collect();
            assert_eq!(got_frames, want_frames, "[{case}] WS 帧序");
        }
        let want_heads = step["ws_handshake"].as_array().cloned().unwrap_or_default();
        for (got_head, want_head) in rec.ws_heads.iter().zip(want_heads.iter()) {
            let want_lines: Vec<String> = want_head
                .as_array()
                .unwrap()
                .iter()
                .map(|v| norm_header_line(v.as_str().unwrap_or("")))
                .collect();
            let got_lines: Vec<String> = got_head.iter().map(|v| norm_header_line(v)).collect();
            assert_eq!(got_lines, want_lines, "[{case}] WS 握手头");
        }
        // 上游命中计数（零轮询纪律：精确等值）
        let want_hits = step["upstream_hits"].as_object().cloned().unwrap_or_default();
        let got_hits = hits_json(&rec);
        for (k, v) in &want_hits {
            assert_eq!(
                got_hits.get(k).and_then(|x| x.as_i64()).unwrap_or(0),
                v.as_i64().unwrap_or(0),
                "[{case}] 上游命中数 {k}"
            );
        }
        for (k, v) in got_hits.as_object().unwrap() {
            assert_eq!(
                want_hits.get(k).and_then(|x| x.as_i64()).unwrap_or(0),
                v.as_i64().unwrap_or(0),
                "[{case}] 上游多打的请求 {k}"
            );
        }
    }
    // pool-converge 的落盘形态
    if let Some(extra) = scen.get("extra").and_then(|e| e.as_object()) {
        let raw = rs_vibex::crypto::read_file(&srv.cfg_snapshot.path);
        match raw {
            Err(e) => assert!(extra["read_err"].as_bool().unwrap_or(false), "读配置失败: {e}"),
            Ok(bytes) => {
                let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                assert_eq!(
                    v.get("tokens").cloned().unwrap_or(Value::Null),
                    extra["tokens"].clone(),
                    "[pool-converge] 落盘 tokens 收敛"
                );
                assert_eq!(
                    v.get("tenant_id").and_then(|t| t.as_str()).map(|s| s.len()),
                    Some(32),
                    "[pool-converge] tenant_id 长度"
                );
                let disk = std::fs::read(&srv.cfg_snapshot.path).unwrap_or_default();
                assert!(
                    disk.starts_with(b"PDAPI1\n"),
                    "[pool-converge] 磁盘上应是 DPAPI 密文"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_scenarios_match_go_golden() {
    let g = load_golden(GOLDEN);
    let names: Vec<String> = g["http"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    let mut done = 0usize;
    for name in &names {
        let _ = tokens_for(&g, name);
        replay_scenario(&g, name).await;
        done += 1;
    }
    println!(
        "HTTP/WS 场景黄金对比：{done} 个场景逐字节一致；显式跳过 0 个；\
         无 allowance 放行（非 ASCII 上游头值也逐字节比对）"
    );
}

// ------------------------------ CDP 场景 ------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cdp_scenarios_match_go_golden() {
    let g = load_golden(GOLDEN);
    let cases = g["cdp"].as_array().unwrap().clone();
    let mut done = 0usize;
    for scen in &cases {
        let name = scen["name"].as_str().unwrap();
        let (cookie, local) = match name {
            "cdp-cookie 命中" => (golden_tok(&g, "A"), String::new()),
            "cdp-localStorage 兜底" => (String::new(), golden_tok(&g, "A")),
            "cdp-都没有" => (String::new(), String::new()),
            "cdp-probe-401" => (golden_tok(&g, "C"), String::new()),
            other => panic!("未知 CDP 场景 {other}"),
        };
        let dir = TestDir::new("cdp");
        let up = FakeUpstream::start().await;
        if name == "cdp-probe-401" {
            up.reject_token(&golden_tok(&g, "C"), 401).await;
        }
        let cdp = FakeCdp::start(&cookie, &local).await;
        let mut seams = rs_vibex::Seams::default();
        seams.cdp_ports = vec![cdp.port.clone()];
        seams.auto_wait_ms = 1200;
        let launched: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let l2 = launched.clone();
        seams.launch_browser = std::sync::Arc::new(move |_port, dir| {
            l2.lock().unwrap().push(dir.to_string());
            Ok(r"C:\fake\chrome.exe".to_string())
        });
        let cfg = test_cfg(&up.base, dir.join("vibex-config.json"));
        let (_srv, app) = start_app(cfg, seams, &dir).await;

        let steps = scen["steps"].as_array().unwrap().clone();
        for (i, step) in steps.iter().enumerate() {
            up.set_mode(step["upstream_mode"].as_str().unwrap_or("ok")).await;
            let _ = up.take().await;
            let req = &step["req"];
            let (status, _h, text) = send(
                &app.base,
                req["method"].as_str().unwrap(),
                req["path"].as_str().unwrap(),
                &[],
                req["body"].as_str().unwrap_or(""),
            )
            .await;
            let case = format!("{name} #{i}");
            assert_eq!(
                status as u64,
                step["resp"]["status"].as_u64().unwrap(),
                "[{case}] 状态码"
            );
            assert_eq!(
                norm(&text),
                norm(step["resp"]["body"].as_str().unwrap_or("")),
                "[{case}] 响应体"
            );
        }
        let calls = cdp.calls().await;
        let extra = &scen["extra"];
        assert_eq!(
            json!(calls),
            extra["cdp_calls"],
            "[{name}] CDP 调用序"
        );
        assert_eq!(
            launched.lock().unwrap().len() as u64,
            extra["launched"].as_u64().unwrap(),
            "[{name}] 拉起次数"
        );
        if extra["profile_prefix_ok"].as_bool() == Some(true) {
            assert!(
                launched
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|d| d.contains("proxydeck-vibex-probe-")),
                "[{name}] profile 目录名不合约定"
            );
        }
        if let Some(n) = extra["pool_size"].as_u64() {
            assert_eq!(_srv.pool.len() as u64, n, "[{name}] 池大小");
        }
        // 探测成功的场景：token 应已落盘（DPAPI 密文）
        if cookie_or_local(&cookie, &local) && name != "cdp-probe-401" {
            let raw = std::fs::read(&_srv.cfg_snapshot.path).unwrap_or_default();
            assert!(raw.starts_with(b"PDAPI1\n"), "[{name}] 应落盘密文配置");
        }
        done += 1;
    }
    println!("CDP 场景黄金对比：{done} 个场景全部一致");
}

fn cookie_or_local(cookie: &str, local: &str) -> bool {
    !cookie.is_empty() || !local.is_empty()
}
