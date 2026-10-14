//! P5b 黄金门禁：`rs-codebuddy` 的**池 / 凭据 / 额度 / failover** 与真 Go 的对照。
//!
//! 口径（P5b 修正轮更正）：**响应体与落盘文件按「键序无关一致」比**——Go 侧
//! `poolState` / `failoverCfg` / `AccountView` / `Quota` 都是 `json.Marshal(结构体)`
//! （字段声明序），Rust 侧一律键序（`gjson::to_go` 等价 `map[string]any` 的语义）。
//! 两侧都按 Go 的 `json.Marshal` 规则重序列化后比「字段集合 + 取值 + 数组顺序」，
//! 这才是门禁的真实强度（登记在 README 差异 22）。非 JSON 的响应体（`text/plain`）
//! 与上游请求体**仍是逐字节**比。
//!
//! 与 P5a 的门禁（`golden_codebuddy.rs`）的分工：
//! * P5a 场景用 `StubCredential` 缝喂固定头（黄金手工记的），验的是「这组头被用在哪」；
//! * **P5b 场景用生产装配**（`AppState::production`：真凭据层 + 真账号池 + 真 failover 配置），
//!   验的是「凭据/池/failover 自己造出来的东西」。黄金来自真 Go 的 `NewServerForRegion`，
//!   **不是** P5a 那个零值 Server 的口径（生产缺省 `enabled:true` / `deepseek-v4-pro` / `max`）。
//!
//! 每个场景比：状态码 + 响应头（白名单）+ 响应体（含本地化）+ 上游看到的请求（体 + 头）
//! + `per_user_hits`（池轮换路径的可核对证据）+ 池状态文件与 failover 配置文件的落盘形态。

mod common;

use common::*;
use serde_json::Value;

use rs_codebuddy::gjson;
use rs_codebuddy::godecode;
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

/// 把一段 JSON 解析后按 **Go 的 `json.Marshal` 规则**重序列化（键排序、数字按 float64）。
///
/// 用途：Go 侧凡是序列化**结构体**的地方（`poolState` / `failoverCfg` / `AccountView` /
/// `Quota`）都是字段声明序，Rust 侧一律是键序（`gjson::to_go` 等价 `map[string]any`
/// 的语义）。两侧比的是「字段集合 + 取值 + 数组顺序」——**键序无关一致**，
/// 不是逐字节一致（登记在 README 差异 22）。非 JSON（`text/plain` 的 404/405）与上游
/// 请求体走逐字节比对。
fn canon_json(s: &str) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(v) => gjson::to_go(&v),
        Err(e) => panic!("待归一的内容不是合法 JSON: {e}\n{s}"),
    }
}

/// 响应体比对：两侧都能解析成 JSON 时按「键序无关」比，否则逐字节比。
fn assert_body_equal(name: &str, desc: &str, got: &str, want: &str) {
    let (Ok(gv), Ok(wv)) = (
        serde_json::from_str::<Value>(got),
        serde_json::from_str::<Value>(want),
    ) else {
        assert_eq!(got, want, "[{name} / {desc}] 响应体与 Go 不一致（非 JSON，逐字节比）");
        return;
    };
    assert_eq!(
        canon_json(got),
        canon_json(want),
        "[{name} / {desc}] 响应体（键序无关、数组顺序敏感）与 Go 不一致"
    );
    let _ = (gv, wv);
}

/// 上游请求体归一：把**墙钟时间**占位化。
///
/// 唯一一处：积分请求的 `PackageStartTimeRangeEnd` = `time.Now().Format("2006-01-02 15:04:05")`
/// （Go 与 Rust 同式），两侧运行时刻必然不同——**格式**仍被锁住（字段名与
/// `YYYY-MM-DD HH:MM:SS` 形状照比），只把具体时刻换成标记（README 差异 19）。
fn norm_upstream_body(s: &str) -> String {
    let key = "\"PackageStartTimeRangeEnd\":\"";
    match s.find(key) {
        Some(i) => {
            let start = i + key.len();
            let rest = &s[start..];
            match rest.find('"') {
                Some(end) => format!("{}{}__NOW__{}", &s[..start], "", &rest[end..]),
                None => s.to_string(),
            }
        }
        None => s.to_string(),
    }
}

/// 比较一条上游命中（方法 / 路径 / 体逐字节 / 头逐条）。
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
        "[{scenario} # {i}] 上游请求体与 Go 不一致"
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

/// P5b 黄金门禁主测。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p5b_scenarios_match_go_golden() {
    // 门禁启动时取一次进程环境锁（整场门禁串行），之后每个场景只换目录、不重复取锁。
    let env = EnvGuard::set(&[]);
    let g = load_golden();
    let all = field(&g, "scenarios").as_array().expect("scenarios 段");
    let scenarios: Vec<&Value> = all
        .iter()
        .filter(|s| field(s, "scope").as_str() == Some("P5b"))
        .collect();
    assert!(
        scenarios.len() >= 10,
        "P5b 场景数不该缩水（实得 {}）",
        scenarios.len()
    );

    let mut done = 0usize;
    let mut steps_done = 0usize;
    let mut skipped_downstream = 0usize;
    let mut content_length_skipped = 0usize;

    for sc in &scenarios {
        let name = field(sc, "name").as_str().unwrap_or("?").to_string();

        // 装配失败场景（`credentials-missing`）：不起服务，只比装配错误文案。
        let is_build_failure = field(sc, "steps")
            .as_array()
            .map(|a| a.iter().any(|s| field(s, "method").as_str() == Some("N/A")))
            .unwrap_or(false);
        if is_build_failure {
            let auth = TestDir::new(&format!("p5b-fail-{name}"));
            let data = TestDir::new(&format!("p5b-faildata-{name}"));
            let home = TestDir::new(&format!("p5b-failhome-{name}"));
            write_auth_files(auth.path(), field(sc, "auth_files"));
            let auth_s = auth.path().to_string_lossy().to_string();
            let data_s = data.path().to_string_lossy().to_string();
            let home_s = home.path().to_string_lossy().to_string();
            env.set_locked(&[
                ("CODEBUDDY_AUTH_DIR", Some(auth_s.as_str())),
                ("PROXYDECK_DATA_DIR", Some(data_s.as_str())),
                ("CODEBUDDY_POOL_DIR", Some(data_s.as_str())),
                ("WORKBUDDY_AT_REST_SECRET", Some("c2VjcmV0LWdvbGRlbi1zZWNyZXQ=")),
                ("USERPROFILE", Some(home_s.as_str())),
                ("LOCALAPPDATA", Some("")),
                ("XDG_DATA_HOME", Some("")),
            ]);
            let err = rs_codebuddy::AppState::production_with_data_dir(
                region_of(sc),
                data.path().to_path_buf(),
                data_s.clone(),
                None,
            )
            .err()
            .unwrap_or_default();
            let err = err
                .replace(&auth_s, "__AUTHDIR__")
                .replace(&data_s, "__DATA__");
            let want = field(&sc["steps"][0], "resp_body").as_str().unwrap_or("");
            assert_eq!(err, want, "[{name}] 装配错误文案与 Go 不一致");
            env.point_to_empty(std::path::Path::new(
                &std::env::temp_dir().join("rs-codebuddy-empty-probe"),
            ));
            steps_done += 1;
            done += 1;
            continue;
        }

        let rp = start_real_replay(&name, region_of(sc), sc, None, &env).await;
        for step in field(sc, "steps").as_array().expect("steps 段") {
            let desc = field(step, "desc").as_str().unwrap_or("");
            let method = field(step, "method").as_str().unwrap();
            let path = field(step, "path").as_str().unwrap();
            let body = field(step, "req_body").as_str().unwrap_or("");
            let resp = send(
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
                resp.status,
                field(step, "resp_status").as_u64().unwrap() as u16,
                "[{name} / {desc}] 状态码与 Go 不一致（实得 body: {}）",
                resp.text()
            );
            // 下游体是否可比：**逐级回落**——步骤自己的 `body_compare` 优先，
            // 没有才看场景级（`StepOutboundOnly` 语义：采集侧把「下游聚合属 P5c」的
            // 200 步标成 `outbound-only`，其余步骤照比）。
            let outbound_only = match field(step, "body_compare").as_str() {
                Some(v) => v == "outbound-only",
                None => field(sc, "body_compare").as_str() == Some("outbound-only"),
            };
            if outbound_only {
                skipped_downstream += 1;
                println!("SKIP(downstream-p5c): [{name}] {desc}（只比出站请求与状态码）");
            } else {
                let want_norm = rp.normalize_json_lib_error(&rp.normalize_body(
                    field(step, "resp_body").as_str().unwrap_or(""),
                ));
                let got_norm = rp.normalize_json_lib_error(&rp.normalize_body(&resp.text()));
                assert_body_equal(&name, desc, &got_norm, &want_norm);
                if let Some(want) = field(step, "resp_headers").as_object() {
                    for (k, v) in want {
                        if k == "content-length" {
                            // 派生头部：黄金里的数字含绝对路径长度（采集机目录），与本机不一致；
                            // 体本身已逐字节比过，这里跳过并登记（同 golden_codebuddy.rs 的口径）。
                            content_length_skipped += 1;
                            continue;
                        }
                        let wv = v.as_str().unwrap_or("");
                        match resp.header(k) {
                            Some(g) => assert_eq!(g, wv, "[{name} / {desc}] 响应头 {k} 与 Go 不一致"),
                            None => panic!("[{name} / {desc}] 响应头 {k} 缺失（Go 侧为 {wv}）"),
                        }
                    }
                }
            }
            steps_done += 1;
        }

        // 上游看到的请求
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
            compare_hop(&name, i, w, &got[i]);
        }

        // per_user_hits（池轮换路径的可核对证据）
        if let Some(want) = field(sc, "per_user_hits").as_object() {
            let mut got_map: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
            for h in &got {
                let uid = h.header("x-user-id").unwrap_or("").to_string();
                *got_map.entry(uid).or_insert(0) += 1;
            }
            let want_map: std::collections::BTreeMap<String, i64> = want
                .iter()
                .map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0)))
                .collect();
            assert_eq!(got_map, want_map, "[{name}] 每号命中数与 Go 不一致");
        }

        // 池状态文件（落盘形态）
        if field(sc, "pool_state_exists").as_bool().unwrap_or(false) {
            let suffix = if region_of(sc) == Region::Intl { "-intl" } else { "" };
            let p = rp.data.path().join(format!("codebuddy-pool{suffix}.json"));
            let got = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("[{name}] 池状态文件应存在 {}: {e}", p.display()));
            // 键序归一：Go 侧 `json.Marshal(poolState{...})` 是**结构体**、按字段声明序输出；
            // Rust 侧走 `gjson::to_go`（等价 `map[string]any`）、按**键排序**输出。
            // 两者是同一份内容的不同键序（Go 自己的 map 字段也按键排序），故先解析成
            // `Value` 再按 Go 的规则重序列化——比的是**字段集合与取值**，不是结构体声明序。
            assert_eq!(
                canon_json(&got),
                canon_json(field(sc, "pool_state").as_str().unwrap_or("")),
                "[{name}] 池状态落盘内容与 Go 不一致"
            );
        } else {
            let suffix = if region_of(sc) == Region::Intl { "-intl" } else { "" };
            let p = rp.data.path().join(format!("codebuddy-pool{suffix}.json"));
            assert!(
                !p.exists(),
                "[{name}] Go 侧没落盘池状态，Rust 侧却落了 {}",
                p.display()
            );
        }

        // failover 配置文件（落盘形态）
        if field(sc, "failover_file_exists").as_bool().unwrap_or(false) {
            let suffix = if region_of(sc) == Region::Intl { "-intl" } else { "" };
            let p = rp.data.path().join(format!("codebuddy-failover{suffix}.json"));
            let got = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("[{name}] failover 文件应存在 {}: {e}", p.display()));
            // 键序同样归一：Go 是结构体（字段声明序），Rust 是键序（`gjson::to_go`）。
            assert_eq!(
                canon_json(&got),
                canon_json(field(sc, "failover_file").as_str().unwrap_or("")),
                "[{name}] failover 落盘内容与 Go 不一致"
            );
        }

        // 认证文件只读：回放结束后 auth 目录的字节必须与黄金夹具一致（真凭据层不得改写它）
        if let Some(arr) = field(sc, "auth_files").as_array() {
            for f in arr {
                let fname = field(f, "name").as_str().unwrap_or("");
                let p = rp.auth.path().join(fname);
                let got = std::fs::read_to_string(&p).unwrap_or_default();
                assert_eq!(
                    got,
                    field(f, "body").as_str().unwrap_or(""),
                    "[{name}] auth 文件 {fname} 被改写了（凭据层必须只读）"
                );
            }
        }
        rp.finish();
        done += 1;
    }

    println!(
        "P5b 黄金对比：{done} 个场景 / {steps_done} 步「键序无关一致」（JSON 响应体与落盘文件按字段集合+取值比，非 JSON 与上游请求体逐字节）；显式跳过下游聚合 {skipped_downstream} 步（P5c）+ {content_length_skipped} 处 content-length（派生头部，体已比过）"
    );
}

/// `modelMeta` 全表逐键门禁（P5a 审查低危项：黄金 `meta.model_meta` 现成，此前没锁）。
#[test]
fn model_meta_full_table_matches_go() {
    let g = load_golden();
    let want_json = field(field(&g, "meta"), "model_meta")
        .as_str()
        .expect("meta.model_meta 是 Go 采收的 JSON 字符串");
    let want: Value = serde_json::from_str(want_json).expect("meta.model_meta 是合法 JSON");
    let want_obj = want.as_object().expect("modelMeta 是 map");

    // 用 crate 的同一份表重建：`meta_entries` 是唯一来源（`/model/info` 也走它）。
    let models = field(field(&g, "meta"), "candidates")
        .as_array()
        .expect("meta.candidates");
    let mut checked = 0usize;
    for m in models {
        let name = m.as_str().unwrap();
        let mut got: serde_json::Map<String, Value> = serde_json::Map::new();
        for (k, v) in rs_codebuddy::models::meta_entries(name) {
            got.insert(k.to_string(), v);
        }
        match want_obj.get(name) {
            Some(w) => {
                assert_eq!(
                    Value::Object(got),
                    *w,
                    "[{name}] modelMeta 条目与 Go 不一致（逐键比）"
                );
            }
            None => {
                assert!(
                    got.is_empty(),
                    "[{name}] Go 侧没有 modelMeta 条目，Rust 侧却造了 {got:?}"
                );
            }
        }
        checked += 1;
    }
    // 反向覆盖面：Go 表里有、候选池里没有的键（deepseek-v3.2 等）也要一致
    for (k, w) in want_obj {
        let mut got: serde_json::Map<String, Value> = serde_json::Map::new();
        for (kk, v) in rs_codebuddy::models::meta_entries(k) {
            got.insert(kk.to_string(), v);
        }
        assert_eq!(Value::Object(got), *w, "[{k}] modelMeta 条目与 Go 不一致（表外键）");
        checked += 1;
    }
    println!("modelMeta 全表门禁：{checked} 个键逐条与 Go 采收值一致");
}

/// `/v1/models` 的候选池与区域配置也要与 Go 一致（门禁的一部分）。
#[test]
fn region_config_and_candidates_match_go_meta() {
    let g = load_golden();
    let meta = field(&g, "meta");
    let want_cn: Vec<String> = field(meta, "candidates")
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let got_cn: Vec<String> = Region::Cn
        .cfg()
        .candidates
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(got_cn, want_cn, "CN 候选池与 Go 不一致");

    // region_cn / region_intl 是 Go 的 regionCfg JSON（字段名与值都要对得上）
    let cn: Value = serde_json::from_str(field(meta, "region_cn").as_str().unwrap()).unwrap();
    assert_eq!(field(&cn, "BaseURL").as_str().unwrap(), Region::Cn.cfg().base_url);
    assert_eq!(
        field(&cn, "UserAgent").as_str().unwrap(),
        Region::Cn.cfg().user_agent
    );
    assert_eq!(field(&cn, "ForceSystem").as_bool().unwrap(), Region::Cn.cfg().force_system);
    assert_eq!(field(&cn, "NoRefresh").as_bool().unwrap(), Region::Cn.cfg().no_refresh);
    assert_eq!(field(&cn, "NoProbe").as_bool().unwrap(), Region::Cn.cfg().no_probe);
    let intl: Value = serde_json::from_str(field(meta, "region_intl").as_str().unwrap()).unwrap();
    assert_eq!(
        field(&intl, "BaseURL").as_str().unwrap(),
        Region::Intl.cfg().base_url
    );
    assert_eq!(
        field(&intl, "UserAgent").as_str().unwrap(),
        Region::Intl.cfg().user_agent
    );
    assert!(Region::Intl.cfg().force_system && Region::Intl.cfg().no_refresh && Region::Intl.cfg().no_probe);
    println!("区域配置与候选池门禁：CN/INTL 逐字段与 Go 一致");
}

/// `godecode` / `gjson` 的基本自洽（P5b 没改它们，但门禁要用，保一条存在性断言）。
#[test]
fn golden_helpers_still_work() {
    let m = godecode::unmarshal_map(br#"{"a":1}"#).unwrap();
    assert_eq!(gjson::to_go(&Value::Object(m)), r#"{"a":1}"#);
}
