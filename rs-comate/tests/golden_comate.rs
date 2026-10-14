// tests/golden_comate.rs —— P2 黄金门禁：把 Rust 实现逐条打回 Go 采下来的基准上。
//
// 基准两份（都在 tests/golden/ 入库，见该目录 README.md）：
// * `golden-comate-go.json`        —— part 1：纯函数（flatten/chunk/complete/readFullAnswer）、
//   生产 `postUpstream` 的真实出站字节（out/wire）、真 HTTP 路径的 health/models/once、路由与边界
// * `golden-comate-stream-go.json` —— part 2：流式读循环的下游帧（由生成器从 server.go 机械抽取后采）
//
// 口径与采集器逐条对齐：上游是进程内假服务（对应采集器的 Transport 拦截器），
// 登录态写在临时目录，**绝不碰 8792 与真实 `~/.comate`**；易变字段（`created`/`id`/`date`）
// 按采集器同一套规则归一化后逐字节比。

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::*;
use rs_comate::translate::{chunk_json, chunk_reasoning_json, complete_json, flatten_messages, read_full_answer};
use rs_comate::AppState;
use serde_json::Value;

fn part1() -> Value {
    load_golden("golden-comate-go.json")
}

fn part2() -> Value {
    load_golden("golden-comate-stream-go.json")
}

/// 按 `hop` 记录发一跳（真 HTTP，端口随机）。GET/HEAD 不带体，POST 带记录里的体。
async fn send_hop(base: &str, hop: &Value) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let method_str = hop["method"].as_str().expect("hop.method");
    let path = hop["path"].as_str().expect("hop.path");
    let body = hop["body"].as_str().unwrap_or("").to_string();
    let method = reqwest::Method::from_bytes(method_str.as_bytes()).expect("method");
    let mut req = client().request(method.clone(), format!("{base}{path}"));
    if let Some(headers) = hop["headers"].as_object() {
        for (k, v) in headers {
            req = req.header(k, v.as_str().unwrap_or(""));
        }
    }
    if method != reqwest::Method::GET && method != reqwest::Method::HEAD {
        req = req.body(body);
    }
    let resp = req.send().await.expect("发下游请求");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await.unwrap_or_default().to_vec();
    (status, headers, bytes)
}

fn nonempty_lines(s: &str) -> usize {
    s.lines().filter(|l| !l.trim().is_empty()).count()
}

// ---------- part 1：纯函数 + 真实出站字节 ----------

#[tokio::test]
async fn golden_pure_functions_and_wire_match_go() {
    let g = part1();
    let meta = &g["meta"];
    let cwd_sentinel = meta["cwd_sentinel"].as_str().expect("cwd_sentinel");
    let list_json = meta["list_model_json"].as_str().expect("list_model_json");
    assert_eq!(
        meta["upstream_base"].as_str().unwrap(),
        rs_comate::DEFAULT_UPSTREAM,
        "黄金记录的上游 base 与本 crate 的常量不一致"
    );

    // ---- flattenMessages ----
    let mut flatten_n = 0;
    for c in g["flatten"].as_array().unwrap() {
        let msgs: Vec<Value> = match &c["in"] {
            Value::Array(a) => a.clone(),
            Value::Null => Vec::new(),
            other => panic!("[{}] 意外的 in 形态: {other}", c["id"]),
        };
        assert_eq!(
            flatten_messages(&msgs),
            c["out"].as_str().unwrap(),
            "[{}] flatten 与 Go 不一致",
            c["id"]
        );
        flatten_n += 1;
    }

    // ---- chunk / reasoning chunk / complete ----
    let mut chunk_n = 0;
    for c in g["chunks"].as_array().unwrap() {
        let id = c["id_arg"].as_str().unwrap();
        let model = c["model_arg"].as_str().unwrap();
        let created = c["created"].as_i64().unwrap();
        let got = match c["kind"].as_str().unwrap() {
            "text" => chunk_json(
                id,
                model,
                c["delta"].as_str().unwrap_or(""),
                c["finish"].as_str().unwrap_or(""),
                created,
            ),
            "reasoning" => chunk_reasoning_json(
                id,
                model,
                c["delta"].as_str().unwrap_or(""),
                c["finish"].as_str().unwrap_or(""),
                created,
            ),
            "complete" => complete_json(id, model, c["content"].as_str().unwrap_or(""), created),
            other => panic!("[{}] 未知 kind {other}", c["id"]),
        };
        assert_eq!(
            got,
            c["out"].as_str().unwrap(),
            "[{}] chunk 字节与 Go 不一致",
            c["id"]
        );
        chunk_n += 1;
    }

    // ---- readFullAnswer（非流式解） ----
    let mut read_n = 0;
    for c in g["readfull"].as_array().unwrap() {
        let sse = sse_from_ctx(&c["ctx"]).unwrap_or_else(|| panic!("[{}] ctx 里没有 SSE", c["id"]));
        let (content, err) = read_full_answer(sse.as_bytes());
        assert_eq!(
            content,
            c["resp_headers"]["content"].as_str().unwrap(),
            "[{}] readFullAnswer 的 content 与 Go 不一致",
            c["id"]
        );
        assert_eq!(
            err,
            c["resp_headers"]["err"].as_str().unwrap(),
            "[{}] readFullAnswer 的错误文案与 Go 不一致",
            c["id"]
        );
        read_n += 1;
    }

    // ---- 出站会话体（生产构造路径：flatten + cwd + mode + resolveModel） ----
    let zulu = FakeZulu::new("golden-wire");
    zulu.set_list(list_json);
    let wire = g["wire"].as_array().unwrap();
    let out = g["out"].as_array().unwrap();
    assert_eq!(wire.len(), out.len(), "黄金 out/wire 两段条目数不一致");
    let mut wire_n = 0;
    for (i, o) in out.iter().enumerate() {
        let case = o["id"].as_str().unwrap();
        let ctx = &o["ctx"];
        let msgs: Vec<Value> = ctx["messages"].as_array().cloned().unwrap_or_default();
        let mode = ctx["mode"].as_str().unwrap_or("");
        let lic = ctx["license"].as_str().unwrap_or("");
        let model_in = ctx["model_in"].as_str().unwrap_or("");
        let dir = TestDir::new("golden-wire-lic");
        let lic_path = write_license(&dir, "cli-login.json", lic);
        let st = AppState::new(
            lic_path,
            Some(zulu.path()),
            Some(cwd_sentinel.to_string()),
            Some(mode.to_string()),
        );
        let got = rs_comate::build_session_body(&st, &zulu.path(), lic, &msgs, model_in).await;
        let want = wire[i]["hop"]["body"].as_str().unwrap();
        assert_eq!(got, want, "[{case}] 出站会话体与 Go 不一致");
        assert_eq!(
            o["body_raw"].as_str().unwrap(),
            want,
            "[{case}] 黄金 out 段与 wire 段自相矛盾"
        );
        assert_eq!(wire[i]["hop"]["method"].as_str().unwrap(), "POST");
        assert_eq!(
            wire[i]["hop"]["path"].as_str().unwrap(),
            format!("{}/api/v1/conversations/init", rs_comate::DEFAULT_UPSTREAM)
        );
        wire_n += 1;
    }

    println!(
        "黄金 part1（纯函数/出站）对照汇总：flatten {flatten_n} 条、chunk {chunk_n} 条、\
         readFullAnswer {read_n} 条、出站会话体 {wire_n} 条 —— 与 Go 逐字节一致"
    );
}

// ---------- part 1：真 HTTP 路径（health / models / once / routes） ----------

/// health 用例的登录态文件内容（采集器里每条的写盘内容；None = 文件不存在）。
fn health_license_content(id: &str, sentinel: &str) -> Option<String> {
    match id {
        "health-no-license" => Some(r#"{"license":"   "}"#.to_string()),
        "health-no-license-null" => Some(r#"{"license":null}"#.to_string()),
        "health-no-license-missing-file" => None,
        "health-bad-json" => Some("{not json".to_string()),
        "health-with-license" => Some(format!(r#"{{"license":"{sentinel}"}}"#)),
        other => panic!("未知 health 用例 {other}"),
    }
}

#[tokio::test]
async fn golden_http_endpoints_match_go() {
    let g = part1();
    let meta = &g["meta"];
    let sentinel = meta["license_sentinel"].as_str().unwrap();
    let cwd_sentinel = meta["cwd_sentinel"].as_str().unwrap();
    let list_json = meta["list_model_json"].as_str().unwrap();

    // ---- /health ----
    let mut health_n = 0;
    for c in g["health"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let base_id = id.trim_end_matches("-post");
        let dir = TestDir::new("golden-health");
        let lic_path = dir.join("cli-login.json");
        if let Some(content) = health_license_content(base_id, sentinel) {
            std::fs::write(&lic_path, content).unwrap();
        }
        let st = Arc::new(AppState::new(lic_path, None, None, None));
        let app = AppServer::start(st).await;
        let (status, headers, body) = send_hop(&app.base, &c["hop"]).await;
        assert_eq!(status as i64, c["status"].as_i64().unwrap(), "[{id}] 状态码");
        assert_headers_like_go(id, &c["resp_headers"], &headers, body.len(), false);
        assert_eq!(
            String::from_utf8_lossy(&body),
            c["body_raw"].as_str().unwrap(),
            "[{id}] /health 响应体与 Go 不一致"
        );
        health_n += 1;
    }

    // ---- /v1/models（含 10 分钟缓存的调用计数） ----
    // 同一个「用例」的多跳必须落在**同一个 AppState** 上（缓存是 per-Server 的）：
    // 采集器里每变一次 list/许可就新建 Server，同一用例内的多跳则复用 —— 这里按 id 前缀分组。
    let zulu = FakeZulu::new("golden-models");
    let mut models_n = 0;
    let models = g["models"].as_array().unwrap();
    let mut idx = 0usize;
    while idx < models.len() {
        let first = &models[idx];
        let first_id = first["id"].as_str().unwrap();
        let base = first_id.split('#').next().unwrap().to_string();
        let prefix = format!("{base}#");
        let mut group: Vec<&Value> = vec![first];
        let mut j = idx + 1;
        while j < models.len() && models[j]["id"].as_str().unwrap().starts_with(&prefix) {
            group.push(&models[j]);
            j += 1;
        }
        let ctx = &first["ctx"];
        zulu.set_list(ctx["list"].as_str().unwrap_or(""));
        zulu.reset_calls();
        let dir = TestDir::new("golden-models");
        let lic_path = write_license(&dir, "cli-login.json", ctx["license"].as_str().unwrap_or(""));
        let zulu_path = if ctx["zulu"].as_bool().unwrap_or(false) {
            Some(zulu.path())
        } else {
            None
        };
        let st = Arc::new(AppState::new(lic_path, zulu_path, None, None));
        let app = AppServer::start(st).await;
        for c in &group {
            let id = c["id"].as_str().unwrap();
            let (status, headers, body) = send_hop(&app.base, &c["hop"]).await;
            assert_eq!(status as i64, c["status"].as_i64().unwrap(), "[{id}] 状态码");
            assert_headers_like_go(id, &c["resp_headers"], &headers, body.len(), false);
            assert_eq!(
                String::from_utf8_lossy(&body),
                c["body_raw"].as_str().unwrap(),
                "[{id}] /v1/models 响应体与 Go 不一致"
            );
            assert_eq!(
                zulu.calls(),
                nonempty_lines(c["ctx"]["zulu_calls"].as_str().unwrap_or("")),
                "[{id}] 假 zulu 的 list-model 调用次数与 Go 不一致（缓存口径）"
            );
            models_n += 1;
        }
        idx = j;
    }

    // ---- 非流式 HTTP（真 Server 路径 + 假上游） ----
    let up = FakeUpstream::start().await;
    let health = FakeHealth::start(0).await;
    let once_zulu = FakeZulu::new("golden-once");
    once_zulu.set_list(list_json);
    let mut once_n = 0;
    for c in g["once"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let ctx = &c["ctx"];
        let dir = TestDir::new("golden-once");
        let lic_path = if id == "once-no-license" {
            std::fs::write(dir.join("cli-login.json"), r#"{"license":""}"#).unwrap();
            dir.join("cli-login.json")
        } else {
            write_license(&dir, "cli-login.json", sentinel)
        };
        // `once-unset-zulu-env` 在采集时清掉了 COMATE_ZULU、于是落到了 D: 的真实 Comate CLI；
        // 这里按「zulu 路径存在但 list-model 跑不出结果」注入（可观察行为一致：回退硬编码目录）。
        let zulu_path: Option<PathBuf> = if id == "once-unset-zulu-env" {
            Some(dir.join("nonexistent-zulu.cmd"))
        } else {
            Some(once_zulu.path())
        };
        let mode = ctx["mode"].as_str().unwrap_or("");
        let st = Arc::new(
            AppState::new(
                lic_path,
                zulu_path,
                Some(cwd_sentinel.to_string()),
                Some(mode.to_string()),
            )
            .with_upstream(up.base.clone())
            .with_zulu_health(health.url.clone(), "0"),
        );
        let app = AppServer::start(st).await;

        let expected_hits = c["upstream"].as_array().cloned().unwrap_or_default();
        // 采集器的命中字段是 url（`zuluHit{Method,URL,Body}`），不是 path
        let expected_init: Vec<Value> = expected_hits
            .iter()
            .filter(|h| {
                h["url"]
                    .as_str()
                    .unwrap_or("")
                    .contains("/api/v1/conversations/init")
            })
            .cloned()
            .collect();
        let expected_health = expected_hits
            .iter()
            .filter(|h| {
                h["url"]
                    .as_str()
                    .unwrap_or("")
                    .ends_with("/health")
            })
            .count();

        up.take_hits().await;
        let probes_before = health.probes();
        if !expected_init.is_empty() {
            // ctx 里有 SSE 就用它；没有 SSE 但确实打了上游的只有 `once-http-500` 那一条
            match sse_from_ctx(ctx) {
                Some(s) if !s.is_empty() => up.set_sse(&s).await,
                _ => up.set_http_500().await,
            }
        }
        let (status, headers, body) = send_hop(&app.base, &c["hop"]).await;
        let got_body = String::from_utf8_lossy(&body).to_string();

        assert_eq!(status as i64, c["status"].as_i64().unwrap(), "[{id}] 状态码");
        assert_headers_like_go(id, &c["resp_headers"], &headers, body.len(), false);
        assert_eq!(
            normalize_volatile(&got_body),
            normalize_volatile(c["body_raw"].as_str().unwrap()),
            "[{id}] 响应体与 Go 不一致"
        );

        let hits = up.take_hits().await;
        let got_init: Vec<&Hit> = hits
            .iter()
            .filter(|h| h.path == "/api/v1/conversations/init")
            .collect();
        assert_eq!(got_init.len(), expected_init.len(), "[{id}] 上游会话请求次数");
        for (want, got) in expected_init.iter().zip(got_init.iter()) {
            assert_eq!(got.method, want["method"].as_str().unwrap(), "[{id}] 上游方法");
            assert_eq!(
                got.body,
                want["body"].as_str().unwrap(),
                "[{id}] 上游会话体与 Go 不一致"
            );
        }
        assert_eq!(
            health.probes() - probes_before,
            expected_health,
            "[{id}] zulu 探活次数与 Go 不一致"
        );
        once_n += 1;
    }

    // ---- 路由与边界（ServeMux 语义） ----
    let routes_zulu = FakeZulu::new("golden-routes");
    routes_zulu.set_list(list_json);
    let dir = TestDir::new("golden-routes");
    let lic_path = write_license(&dir, "cli-login.json", sentinel);
    let st = Arc::new(AppState::new(
        lic_path,
        Some(routes_zulu.path()),
        Some(cwd_sentinel.to_string()),
        None,
    ));
    let app = AppServer::start(st).await;
    let mut routes_n = 0;
    for c in g["routes"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        let hop = &c["hop"];
        let mut extra: Vec<(&str, &str)> = Vec::new();
        if let Some(hs) = hop["headers"].as_object() {
            for (k, v) in hs {
                extra.push((k.as_str(), v.as_str().unwrap_or("")));
            }
        }
        // 原始 HTTP/1.1：reqwest 会把 `.`/`..`/重复斜杠归一化，测不到路径清洗
        let (status, raw_headers, body) = raw_request(
            &app.addr,
            hop["method"].as_str().unwrap(),
            hop["path"].as_str().unwrap(),
            &extra,
            hop["body"].as_str().unwrap_or(""),
        )
        .await;
        let headers = headers_from_raw(&raw_headers);
        let is_head = hop["method"].as_str().unwrap() == "HEAD";
        assert_eq!(status as i64, c["status"].as_i64().unwrap(), "[{id}] 状态码");
        assert_headers_like_go(id, &c["resp_headers"], &headers, body.len(), is_head);
        // HEAD 的 body_raw 在黄金里是空串（Go 不写体），只比状态与头
        if !is_head {
            assert_eq!(
                String::from_utf8_lossy(&body),
                c["body_raw"].as_str().unwrap(),
                "[{id}] 响应体与 Go 不一致"
            );
        } else {
            assert!(body.is_empty(), "[{id}] HEAD 不该有响应体");
        }
        routes_n += 1;
    }

    println!(
        "黄金 part1（HTTP）对照汇总：/health {health_n} 条、/v1/models {models_n} 条、\
         非流式 {once_n} 条、路由边界 {routes_n} 条 —— 与 Go 逐字节一致"
    );
}

// ---------- part 2：流式读循环的下游帧 ----------

#[tokio::test]
async fn golden_stream_matches_go() {
    let g = part2();
    let p1 = part1();
    let sentinel = p1["meta"]["license_sentinel"].as_str().unwrap();
    let cwd_sentinel = p1["meta"]["cwd_sentinel"].as_str().unwrap();
    let list_json = p1["meta"]["list_model_json"].as_str().unwrap();

    let zulu = FakeZulu::new("golden-stream");
    zulu.set_list(list_json);
    let dir = TestDir::new("golden-stream");
    let lic_path = write_license(&dir, "cli-login.json", sentinel);
    let up = FakeUpstream::start().await;
    let health = FakeHealth::start(0).await;

    let cases = g["cases"].as_array().expect("part2 cases");
    let mut n = 0;
    for c in cases {
        let id = c["id"].as_str().unwrap();
        // part 2 没有 500 用例：ctx 里一定有 SSE（`stream-sse-cut-before-output` 是空串，
        // 那是「上游 200 但立刻结束」，**不是** HTTP 500）
        let sse = sse_from_ctx(&c["ctx"]).unwrap_or_default();
        up.set_sse(&sse).await;
        up.take_hits().await;

        let st = Arc::new(
            AppState::new(
                lic_path.clone(),
                Some(zulu.path()),
                Some(cwd_sentinel.to_string()),
                None,
            )
            .with_upstream(up.base.clone())
            .with_zulu_health(health.url.clone(), "0"),
        );
        let app = AppServer::start(st).await;
        let (status, headers, body) = send_hop(&app.base, &c["hop"]).await;
        let got = String::from_utf8_lossy(&body).to_string();

        assert_eq!(status as i64, c["status"].as_i64().unwrap(), "[{id}] 状态码");
        assert_headers_like_go(id, &c["resp_headers"], &headers, body.len(), false);
        assert_eq!(
            normalize_volatile(&got),
            c["normalized"].as_str().unwrap(),
            "[{id}] 下游帧与 Go 不一致"
        );

        let hits = up.take_hits().await;
        let init: Vec<&Hit> = hits
            .iter()
            .filter(|h| h.path == "/api/v1/conversations/init")
            .collect();
        assert_eq!(init.len(), 1, "[{id}] 上游会话请求次数");
        let want = &c["upstream"][0];
        assert_eq!(init[0].method, want["method"].as_str().unwrap(), "[{id}] 上游方法");
        assert_eq!(
            init[0].body,
            want["body"].as_str().unwrap(),
            "[{id}] 上游会话体与 Go 不一致"
        );

        let committed = c["loop_committed"].as_bool().unwrap();
        assert_eq!(
            committed,
            status == 200,
            "[{id}] committed 与状态不一致（已提交必然 200，未提交才可能 502）"
        );
        n += 1;
    }

    println!(
        "黄金 part2（流式）对照汇总：{n} 条用例的下游帧（含 finish_reason=stop 与 [DONE]）\
         与 Go 逐字节一致；无跳过（两份基准均已入库，见 tests/golden/README.md）"
    );
}
