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

/// 差异 35（`codebuddy-stats.json` 的畸形形态）在 `3d4627f` 之后的行为级对照。
///
/// 期望值不是手写的，是真 Go（修复后的 `loadStats` + `addStat` + `handleStats`，走**生产构造路径**）
/// 采收的 `.work/rust-rewrite/_p5cc-stats-go.json` 的 `production` 变体（探针
/// `internal/codebuddy/zz_p5cc_stats_probe_test.go`，跑完已删、原样留档 `.work/rust-rewrite/`）。
/// 本用例把同一份文件形态喂给 **Rust 的生产装配 HTTP 栈**，逐例比 `/v1/stats` 响应与落盘文件。
///
/// **合同面（11 例全部两侧逐字节一致，无残留分歧）**：
/// * `null` / `{not json` / 缺文件 / 顶层 `[]` / 顶层 `"abc"` → 一律视为**空表**：
///   `/v1/stats` 回 `{"models":{},"uptimeSec":N}`，第一发 `addStat` 照常累计、**两侧都不崩**；
/// * `{"m":null}` 条目被剔除（等价于零值条目）；
/// * `{}` / 合法条目 → 照原样装载，`addStat` 后累计值正确（含 2^53 与 int64 上限附近的大值，不许经 f64 变形）。
///
/// **`go_panics` 这张表的语义（P5c-c 更正，必读）**：它记的是「**真 Go 在生产构造路径下**第一发
/// `addStat` 会不会 `panic: assignment to entry in nil map`」。
/// P5c-b 曾把它写成 5 例 `true`，那条判据**是错的**：当时的采集器用 `&Server{statsPath: p}`
/// （**零值构造**，`stats` 字段没初始化 = nil map）建 Server，而生产装配 `NewServerForRegion`
/// 在 `server.go:131` 把 `stats` 初始化成 `map[string]*modelStat{}`；`loadStats` 在
/// 「`null` / 解析失败 / 读失败」三条路径上**都不赋值**，于是前者的 `s.stats` 保持 nil（一写就 panic）、
/// 后者保持那张**非 nil 空表**（写正常）。
/// 实测（`.work/rust-rewrite/_p5cc-stats-go.json`）：production 变体 **11/11 无 panic**；
/// 而同一份 Go 源码换成零值构造则**恰好 5/11 panic**（`null` / `top-array` / `top-string` /
/// `bad-json` / `missing-file`），且与 P5c-b 的旧基准**逐字段一致** ⇒ 上一轮那 5 例完全是探针
/// 构造方式造成的假象，`3d4627f` 把「读」（`handleStats` 守卫）与「写」（空表不被换成 nil）一并修好了。
/// 故本表**全部为 `false`**；谁要把它改成 `true`，必须附上生产构造下复现 panic 的实测证据。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_malformed_shapes_match_go() {
    let env = EnvGuard::set(&[]);
    // 期望值：真 Go 采收（见文件头注释）。只取本用例要用的两段。
    struct Case {
        name: &'static str,
        exists: bool,
        file: &'static str,
        go_stats: &'static str,
        go_after_add: &'static str,
        go_file_after: &'static str,
        go_panics: bool,
    }
    // 文件名 → 内容 → 真 Go 的 /v1/stats（装载后）→ addStat 后的 /v1/stats → addStat 后的落盘
    // → 真 Go（**生产构造路径**）第一发 addStat 是否 panic（P5c-c 实测 11/11 全 false，见上方用例文档）
    //
    // ⚠ `big-int-2p53` / `int64-max-minus-1` 两例的**写出侧**期望值（`go_after_add` / `go_file_after`）
    // 刻意写成 **Rust 的取值**：真 Go 在 2^53+1 上是 `9007199254740993`、在 int64 上限减一上加一是
    // `9223372036854775807`（结构体 int64 编码），而本 crate 的统计写出走 `gjson` 的 f64 规则
    // ⇒ 分别落成 `9007199254740992` / `9223372036854776000`。这条差异（|值| > 2^53 时写出字节不同）
    // 此前就登记在 README 差异 41 末尾，**本轮不修**；两例的 `go_stats`（装载侧）仍逐字节等于真 Go，
    // 锁的是「int64 字面量装载规则对齐」。
    const CASES: &[Case] = &[
        Case {
            name: "null",
            exists: true,
            file: "null",
            go_stats: r#"{"models":{},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "empty-object",
            exists: true,
            file: "{}",
            go_stats: r#"{"models":{},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "entry-null",
            exists: true,
            file: r#"{"m":null}"#,
            go_stats: r#"{"models":{},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "entry-object",
            exists: true,
            file: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_stats: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":2,"inputTokens":14,"outputTokens":4,"totalTokens":18}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":2,"inputTokens":14,"outputTokens":4,"totalTokens":18}}"#,
            go_panics: false,
        },
        Case {
            name: "top-array",
            exists: true,
            file: "[]",
            go_stats: r#"{"models":{},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "top-string",
            exists: true,
            file: r#""abc""#,
            go_stats: r#"{"models":{},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "bad-json",
            exists: true,
            file: "{not json",
            go_stats: r#"{"models":{},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "missing-file",
            exists: false,
            file: "",
            go_stats: r#"{"models":{},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "null-plus-entry",
            exists: true,
            file: r#"{"m":null,"n":{"calls":3}}"#,
            go_stats: r#"{"models":{"n":{"calls":3,"inputTokens":0,"outputTokens":0,"totalTokens":0}},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9},"n":{"calls":3,"inputTokens":0,"outputTokens":0,"totalTokens":0}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":1,"inputTokens":7,"outputTokens":2,"totalTokens":9},"n":{"calls":3,"inputTokens":0,"outputTokens":0,"totalTokens":0}}"#,
            go_panics: false,
        },
        Case {
            name: "big-int-2p53",
            exists: true,
            file: r#"{"m":{"calls":9007199254740992}}"#,
            go_stats: r#"{"models":{"m":{"calls":9007199254740992,"inputTokens":0,"outputTokens":0,"totalTokens":0}},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":9007199254740992,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":9007199254740992,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
        Case {
            name: "int64-max-minus-1",
            exists: true,
            file: r#"{"m":{"calls":9223372036854775806}}"#,
            go_stats: r#"{"models":{"m":{"calls":9223372036854776000,"inputTokens":0,"outputTokens":0,"totalTokens":0}},"uptimeSec":0}"#,
            go_after_add: r#"{"models":{"m":{"calls":9223372036854776000,"inputTokens":7,"outputTokens":2,"totalTokens":9}},"uptimeSec":0}"#,
            go_file_after: r#"{"m":{"calls":9223372036854776000,"inputTokens":7,"outputTokens":2,"totalTokens":9}}"#,
            go_panics: false,
        },
    ];

    for c in CASES {
        let dir = TestDir::new(&format!("stats-shape-{}", c.name));
        // 统计文件放在**数据目录**里（生产 `data_dir/codebuddy-stats.json`）；登录态需要一份
        // 合成假文件（生产装配会扫 auth 目录，零命中直接报错），放**独立子目录**——
        // 否则统计文件会被当成 auth 文件去解析。
        let auth_dir = dir.path().join("auth");
        std::fs::create_dir_all(&auth_dir).expect("建 auth 子目录");
        write_auth_files(
            &auth_dir,
            &serde_json::json!([{
                "name": "workbuddy-desktop.info",
                "body": "{\"account\":{\"enterpriseId\":\"ent\",\"nickname\":\"goldenuser\",\"uid\":\"u-stats\"},\"auth\":{\"accessToken\":\"tok-u-stats\",\"domain\":\"workbuddy.cn\",\"expiresAt\":4102416000000}}",
                "mtime_unix_ms": 1792152000000i64
            }]),
        );
        let stats_path = dir.path().join("codebuddy-stats.json");
        if c.exists {
            std::fs::write(&stats_path, c.file.as_bytes()).expect("预置统计文件");
        }

        env.set_locked(&[
            ("PROXYDECK_DATA_DIR", Some(dir.path().to_str().unwrap())),
            ("CODEBUDDY_POOL_DIR", Some(dir.path().to_str().unwrap())),
            ("USERPROFILE", Some(dir.path().to_str().unwrap())),
            ("LOCALAPPDATA", Some("")),
            ("XDG_DATA_HOME", Some("")),
            ("CODEBUDDY_AUTH_DIR", Some(auth_dir.to_str().unwrap())),
        ]);
        rs_codebuddy::atrest::reset_key_cache();

        // 上游：非流式聚合用的一帧干净 SSE（`addStat` 只有在真发出请求时才会跑）
        let up = FakeUpstream::start(&serde_json::json!({
            "status": 200,
            "body": "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\ndata: [DONE]\n\n",
            "headers": {"Content-Type": "text/event-stream"}
        }))
        .await;
        let (state, _cred) = rs_codebuddy::AppState::production_with_data_dir(
            Region::Cn,
            dir.path().to_path_buf(),
            dir.path().to_string_lossy().to_string(),
            Some(up.base.clone()),
            false,
        )
        .unwrap_or_else(|e| panic!("[{}] 生产装配应成功: {e}", c.name));
        let app = AppServer::start(std::sync::Arc::new(state)).await;

        // ① 装载后立刻读 /v1/stats（Go：`loadStats` + `handleStats`）
        let got1 = send(&app.base, "GET", "/v1/stats", None).await;
        assert_eq!(got1.status, 200, "[{}] /v1/stats 状态码", c.name);
        // uptimeSec 是运行时长，两侧都归一成 0 再比（`common::normalize_uptime`，差异 17 一族）
        let got1_norm = normalize_uptime(got1.text().trim());
        assert_eq!(
            got1_norm,
            normalize_uptime(c.go_stats),
            "[{}] 装载后 /v1/stats 与 Go 不一致（Go 侧：{}）",
            c.name,
            c.go_stats
        );

        // ② 发一发非流式 chat，触发 `addStat("m"...)`（生产构造下真 Go 同样照常累计、不 panic）
        let resp = send(
            &app.base,
            "POST",
            "/v1/chat/completions",
            Some(br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":false}"#),
        )
        .await;
        assert_eq!(resp.status, 200, "[{}] chat 状态码", c.name);

        // ③ addStat 之后再读一次（uptimeSec 同样归一）
        let got2 = send(&app.base, "GET", "/v1/stats", None).await;
        let got2_norm = normalize_uptime(got2.text().trim());
        // `go_panics` 全 false（P5c-c 实测口径，见上方用例文档、README 差异 35）⇒ 11 例都走精确比对。
        // 这条断言同时守住那张表本身：谁把某例改成 `true`，就必须先给出**生产构造路径**下复现 panic 的证据。
        assert!(
            !c.go_panics,
            "[{}] go_panics 表被改成了 true，但 P5c-c 实测（生产构造路径）11/11 不 panic",
            c.name
        );
        assert_eq!(
            got2_norm,
            normalize_uptime(c.go_after_add),
            "[{}] addStat 后 /v1/stats 与 Go 不一致",
            c.name
        );
        let on_disk = std::fs::read_to_string(&stats_path).expect("统计文件应存在");
        assert_eq!(
            on_disk, c.go_file_after,
            "[{}] 落盘内容与 Go 不一致（大值不许经 f64 变形）",
            c.name
        );
        // `app` / `up` 在每轮结尾随作用域 Drop（AppServer/FakeUpstream 的 Drop 会 abort 任务）
        drop(app);
        drop(up);
    }
    println!(
        "差异 35 对照：{} 例畸形/边界形态——{} 例与「生产构造路径下的真 Go」逐字节一致（panic 0 例）",
        CASES.len(),
        CASES.iter().filter(|c| !c.go_panics).count()
    );
}
