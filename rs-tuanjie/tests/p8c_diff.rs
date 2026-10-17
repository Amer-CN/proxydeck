//! P8c 门禁：与**真 Go 采收**的 `tests/golden/golden-p8c-go.json` 逐条比对。
//!
//! 覆盖面（与 Go 采集器 `zz_p8c_collect_test.go` 同一份场景定义）：
//!
//! * `baseline.go:289-627` 的比对面：`probeValueMatch` 真值表、`tokenizerBatchShift`、
//!   `CompareToBaseline` 10 场景、`CompareAgainstAnchors`、锚间一致性提示；
//! * `seedcheck.go` 的纯函数：`evalSeedScan` 全表、`labelDesktopSources`、
//!   `installHint`、`localCliSources` 路径形态、`combineSeedAlerts`；
//! * `seedcheck.go` 的 tgz 解包（**合成** tgz，走 `httptest` 服务下载）：
//!   找到文件 / 缺文件 / 无特征 / 路径穿越 / 坏 gzip 五种形态；
//! * `water.go`：`normalizeModelName`、`containsCI`、`RecordResolution` 记账、
//!   `PassiveEvents` 环形上限、落盘字节；
//! * `appdir` 的 `Migrate`（真建临时目录）与历史状态文件清单。
//!
//! **纪律**：合成账号/合成特征串/临时目录；不碰 8788、不读真实凭据与真实数据目录、
//! 不出网（tgz 走本进程 httptest 服务 —— Go 侧那次采集同理）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use rs_tuanjie::baseline::{
    anchor_conflict_note, anchor_match_phrase, compare_against_anchors, compare_to_baseline,
    gateway_wording_probe, is_numeric_str, probe_value_match, tokenizer_batch_shift, ProbeCompare,
};
use rs_tuanjie::probes::{Baseline, ProbeResult};
use rs_tuanjie::seedcheck::{
    combine_seed_alerts, eval_seed_scan, install_hint, label_desktop_sources, DesktopCand,
};
use rs_tuanjie::water::{contains_ci, normalize_model_name, WaterCheck, PASSIVE_CAPACITY};

/// 合成特征串（**不是**任何真实种子值；与 Go 采集器同一对常量）。
const SYNTH_SEED: &str = "SYNTHETICSEED0001";
const SYNTH_HEADER: &str = "X-Codely-Signature";

fn golden_path() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("golden-p8c-go.json");
    if vendored.exists() {
        return vendored;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".work")
        .join("p8c")
        .join("golden-p8c-go.json")
}

fn golden() -> serde_json::Value {
    let p = golden_path();
    let raw = std::fs::read(&p).unwrap_or_else(|e| panic!("读不到金标 {}: {e}", p.display()));
    serde_json::from_slice(&raw).expect("金标 JSON 解析失败")
}

fn probe(name: &str, value: &str, status: &str) -> ProbeResult {
    ProbeResult {
        name: name.into(),
        value: value.into(),
        status: status.into(),
        ..Default::default()
    }
}

fn baseline(channel: &str, probes: Vec<ProbeResult>) -> Baseline {
    Baseline {
        channel: channel.into(),
        probes,
        ..Default::default()
    }
}

fn cmp_of(c: &ProbeCompare) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("name".into(), c.name.clone().into());
    m.insert("current".into(), c.current.clone().into());
    m.insert("baseline".into(), c.baseline.clone().into());
    m.insert("match".into(), c.r#match.into());
    m.insert("comparable".into(), c.comparable.into());
    m.insert("status".into(), c.status.clone().into());
    if !c.note.is_empty() {
        m.insert("note".into(), c.note.clone().into());
    }
    serde_json::Value::Object(m)
}

fn cmps_of(list: &[ProbeCompare]) -> serde_json::Value {
    serde_json::Value::Array(list.iter().map(cmp_of).collect())
}

/// `tokenizer_batch_shift` 的 6 组场景（与 Go 采集器逐条对应）。
fn tokenizer_batch_shift_cases(
    cases: &[Vec<ProbeCompare>],
) -> Vec<(bool, i64, usize)> {
    cases
        .iter()
        .map(|c| {
            let (shift, med) = tokenizer_batch_shift(c);
            (shift, med, c.len())
        })
        .collect()
}

/// `localCliSources` 的三组入参（与 Go 采集器逐条对应；**不读**真实环境/注册表）。
fn local_sources_case(i: usize) -> Vec<rs_tuanjie::seedcheck::SeedSource> {
    let (appdata, home, install, local) = match i {
        0 => (
            r"C:\Users\u\AppData\Roaming",
            r"C:\Users\u",
            r"D:\Program Files (x86)",
            r"C:\Users\u\AppData\Local",
        ),
        1 => (
            r"X:\appdata",
            r"C:\Users\u",
            r"D:\Program Files (x86)",
            r"C:\Users\u\AppData\Local",
        ),
        _ => ("", "", "", ""),
    };
    rs_tuanjie::seedcheck::local_cli_sources(
        if appdata.is_empty() { None } else { Some(appdata) },
        if home.is_empty() { None } else { Some(home) },
        install,
        if local.is_empty() { None } else { Some(local) },
    )
}

/// `compare_to_baseline` 的场景名 → 入参（与 Go 采集器逐条对应）。
fn scenario(name: &str) -> (Option<Baseline>, Vec<ProbeResult>, Option<rs_tuanjie::probes::DistResult>, &'static str) {
    match name {
        "nil-baseline" => (None, vec![], None, "tuanjie"),
        "all-match" => (
            Some(baseline("tuanjie", vec![probe("tokenizer_en", "26", "ok")])),
            vec![probe("tokenizer_en", "26", "ok")],
            None,
            "tuanjie",
        ),
        "one-mismatch" => (
            Some(baseline(
                "tuanjie",
                vec![probe("tokenizer_en", "26", "ok"), probe("tokenizer_cjk", "35", "ok")],
            )),
            vec![probe("tokenizer_en", "40", "ok"), probe("tokenizer_cjk", "35", "ok")],
            None,
            "tuanjie",
        ),
        "two-mismatch" => (
            Some(baseline(
                "tuanjie",
                vec![probe("tokenizer_en", "26", "ok"), probe("tokenizer_cjk", "35", "ok")],
            )),
            vec![probe("tokenizer_en", "40", "ok"), probe("tokenizer_cjk", "50", "ok")],
            None,
            "tuanjie",
        ),
        "partial-unstable" => (
            Some(baseline("tuanjie", vec![probe("tokenizer_en", "26", "ok")])),
            vec![
                probe("tokenizer_en", "26", "ok"),
                probe("finish_stop", "x", "error"),
            ],
            None,
            "tuanjie",
        ),
        "no-comparable" => (
            Some(baseline("tuanjie", vec![probe("tokenizer_en", "26", "ok")])),
            vec![probe("tokenizer_en", "26", "error")],
            None,
            "tuanjie",
        ),
        "cross-channel-skip" => (
            Some(baseline(
                "tuanjie",
                vec![probe("error_temp2", "invalid temperature", "ok")],
            )),
            vec![probe("error_temp2", "温度非法（temperature must be <=2）", "ok")],
            None,
            "bai",
        ),
        "same-channel-wording" => (
            Some(baseline(
                "tuanjie",
                vec![probe("error_temp2", "invalid temperature", "ok")],
            )),
            vec![probe("error_temp2", "temperature must be <= 2", "ok")],
            None,
            "tuanjie",
        ),
        "missing-probe-name" => (
            Some(baseline("tuanjie", vec![probe("tokenizer_en", "26", "ok")])),
            vec![probe("tokenizer_mixed", "12", "ok")],
            None,
            "tuanjie",
        ),
        "dist-ready-green" => (
            Some(baseline("tuanjie", vec![probe("tokenizer_en", "26", "ok")])),
            vec![probe("tokenizer_en", "26", "ok")],
            Some(rs_tuanjie::probes::DistResult {
                counts: vec![0; rs_tuanjie::dist::DIST_BUCKETS],
                valid: 40,
                ..Default::default()
            }),
            "tuanjie",
        ),
        other => panic!("金标里出现了未定义的场景 {other}"),
    }
}

#[test]
fn p8c_matches_go_golden() {
    let g = golden();
    let mut checked = 0usize;

    // ------------------------------------------------ 1. probeValueMatch
    for case in g["probe_value_match"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let cur = case["cur"].as_str().unwrap();
        let base = case["base"].as_str().unwrap();
        assert_eq!(
            probe_value_match(name, cur, base),
            case["want"].as_bool().unwrap(),
            "probeValueMatch({name}, {cur:?}, {base:?})"
        );
        checked += 1;
    }

    // ------------------------------------------------ 2. tokenizerBatchShift
    let mk = |cur: &str, base: &str| ProbeCompare {
        name: "tokenizer_en".into(),
        current: cur.into(),
        baseline: base.into(),
        r#match: false,
        comparable: true,
        status: "ok".into(),
        note: String::new(),
    };
    let bs_cases: Vec<Vec<ProbeCompare>> = vec![
        vec![mk("20", "26"), mk("27", "35")],
        vec![mk("20", "26"), mk("29", "35"), mk("51", "57")],
        vec![mk("20", "26"), mk("28", "35")],
        vec![],
        vec![mk("abc", "26")],
        vec![mk("26", "26")],
    ];
    for (got, want) in tokenizer_batch_shift_cases(&bs_cases)
        .iter()
        .zip(g["tokenizer_batch_shift"].as_array().unwrap())
    {
        assert_eq!(got.0, want["shift"].as_bool().unwrap(), "batch shift");
        assert_eq!(got.1, want["median"].as_i64().unwrap(), "batch median");
        assert_eq!(want["n"].as_i64().unwrap() as usize, got.2, "场景条数");
        checked += 1;
    }

    // ------------------------------------------------ 3. CompareToBaseline
    for case in g["compare_to_baseline"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let (b, probes, dist, channel) = scenario(name);
        let (cmps, _sim, v) = compare_to_baseline(b.as_ref(), &probes, dist.as_ref(), channel);
        assert_eq!(
            cmps_of(&cmps),
            case["cmps"],
            "[{name}] cmps 与 Go 不一致"
        );
        assert_eq!(v.light, case["verdict"]["light"].as_str().unwrap(), "[{name}] light");
        assert_eq!(v.label, case["verdict"]["label"].as_str().unwrap(), "[{name}] label");
        assert_eq!(
            v.score,
            case["verdict"]["score"].as_f64().unwrap(),
            "[{name}] score"
        );
        assert_eq!(
            v.reason,
            case["verdict"]["reason"].as_str().unwrap(),
            "[{name}] reason"
        );
        checked += 1;
    }

    // ------------------------------------------------ 4. CompareAgainstAnchors
    {
        let a = baseline("tuanjie", vec![probe("tokenizer_en", "26", "ok")]);
        let b = baseline("bai", vec![probe("tokenizer_en", "99", "ok")]);
        let probes = vec![probe("tokenizer_en", "26", "ok")];
        let (cmps, _, v, matched) =
            compare_against_anchors(&[a.clone(), b.clone()], &probes, None, "comate");
        let want = &g["compare_against_anchors_best"];
        assert_eq!(cmps_of(&cmps), want["cmps"], "多锚 cmps");
        assert_eq!(v.light, want["light"].as_str().unwrap(), "多锚 light");
        assert_eq!(v.score, want["score"].as_f64().unwrap(), "多锚 score");
        assert_eq!(v.reason, want["reason"].as_str().unwrap(), "多锚 reason");
        assert_eq!(
            matched.as_ref().map(|m| m.channel.clone()).unwrap_or_default(),
            want["matched"].as_str().unwrap(),
            "匹配锚来源"
        );
        let (cmps2, _, v2, m2) = compare_against_anchors(&[], &[], None, "tuanjie");
        let want2 = &g["compare_against_anchors_empty"];
        assert_eq!(cmps_of(&cmps2), want2["cmps"], "空锚 cmps");
        assert_eq!(v2.light, want2["light"].as_str().unwrap(), "空锚 light");
        assert_eq!(v2.reason, want2["reason"].as_str().unwrap(), "空锚 reason");
        assert!(m2.is_none(), "空锚不该有 matched");
        checked += 2;
    }

    // ------------------------------------------------ 5. anchorConflictNote / anchorMatchPhrase
    {
        let a2 = baseline(
            "tuanjie",
            vec![probe("tokenizer_en", "26", "ok"), probe("tokenizer_cjk", "35", "ok")],
        );
        let b2 = baseline(
            "bai",
            vec![probe("tokenizer_en", "40", "ok"), probe("tokenizer_cjk", "50", "ok")],
        );
        let b3 = baseline(
            "bai",
            vec![probe("tokenizer_en", "40", "ok"), probe("tokenizer_cjk", "35", "ok")],
        );
        let want = &g["anchor_conflict_note"];
        assert_eq!(
            anchor_conflict_note(&[a2.clone(), b2]),
            want["two_over"].as_str().unwrap(),
            "two_over"
        );
        assert_eq!(
            anchor_conflict_note(&[a2.clone(), b3]),
            want["one_over"].as_str().unwrap(),
            "one_over"
        );
        assert_eq!(
            anchor_conflict_note(&[]),
            want["empty"].as_str().unwrap(),
            "empty"
        );
        assert_eq!(
            anchor_match_phrase("green", Some(&a2)),
            want["phrase_green"].as_str().unwrap()
        );
        assert_eq!(
            anchor_match_phrase("yellow", Some(&a2)),
            want["phrase_yellow"].as_str().unwrap()
        );
        assert_eq!(anchor_match_phrase("grey", Some(&a2)), want["phrase_grey"].as_str().unwrap());
        assert_eq!(anchor_match_phrase("green", None), want["phrase_nil"].as_str().unwrap());
        checked += 7;
    }

    // ------------------------------------------------ 6. gatewayWordingProbe / isNumericStr
    for case in g["gateway_wording"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        assert_eq!(
            gateway_wording_probe(name),
            case["gateway"].as_bool().unwrap(),
            "gatewayWordingProbe({name})"
        );
        checked += 1;
    }
    for case in g["is_numeric_str"].as_array().unwrap() {
        let s = case["s"].as_str().unwrap();
        assert_eq!(
            is_numeric_str(s),
            case["numeric"].as_bool().unwrap(),
            "isNumericStr({s:?})"
        );
        checked += 1;
    }

    // ------------------------------------------------ 7. 种子卫兵纯函数
    for case in g["eval_seed_scan"].as_array().unwrap() {
        let (active, signal) = eval_seed_scan(
            case["scanned"].as_bool().unwrap(),
            case["seed"].as_bool().unwrap(),
            case["header"].as_bool().unwrap(),
        );
        assert_eq!(active, case["active"].as_bool().unwrap(), "evalSeedScan active");
        assert_eq!(signal, case["signal"].as_str().unwrap(), "evalSeedScan signal");
        checked += 1;
    }
    for (i, case) in g["label_desktop_sources"].as_array().unwrap().iter().enumerate() {
        let cands: Vec<DesktopCand> = match i {
            0 => vec![DesktopCand {
                dir: r"D:\a".into(),
                path: r"D:\a\codely.exe".into(),
            }],
            1 => vec![
                DesktopCand {
                    dir: r"D:\a".into(),
                    path: r"D:\a\codely.exe".into(),
                },
                DesktopCand {
                    dir: r"C:\b".into(),
                    path: r"C:\b\codely.exe".into(),
                },
            ],
            2 => vec![
                DesktopCand {
                    dir: r"D:\a".into(),
                    path: r"D:\a\codely.exe".into(),
                },
                DesktopCand {
                    dir: r"X:\z".into(),
                    path: r"D:\a\codely.exe".into(),
                },
            ],
            _ => vec![DesktopCand {
                dir: r"D:\a".into(),
                path: String::new(),
            }],
        };
        let got: Vec<serde_json::Value> = label_desktop_sources(&cands)
            .iter()
            .map(|s| {
                serde_json::json!({"label": s.label, "path": s.path})
            })
            .collect();
        assert_eq!(
            serde_json::Value::Array(got),
            case["labels"],
            "labelDesktopSources 第 {i} 组"
        );
        checked += 1;
    }
    for case in g["install_hint"].as_array().unwrap() {
        let dir = case["dir"].as_str().unwrap();
        assert_eq!(
            install_hint(dir),
            case["hint"].as_str().unwrap(),
            "installHint({dir:?})"
        );
        checked += 1;
    }
    // localCliSources：**不读真实环境变量/注册表**——按金标的三组入参逐组构造。
    for (i, case) in g["local_cli_sources"].as_array().unwrap().iter().enumerate() {
        let srcs = local_sources_case(i);
        let got: Vec<serde_json::Value> = srcs
            .iter()
            .map(|s| serde_json::json!({"label": s.label, "path": s.path}))
            .collect();
        assert_eq!(
            serde_json::Value::Array(got),
            case["srcs"],
            "localCliSources 第 {i} 组"
        );
        checked += 1;
    }
    for (i, case) in g["combine_seed_alerts"].as_array().unwrap().iter().enumerate() {
        let args: [(bool, &str, bool, &str); 4] = [
            (true, "本地", true, "在线"),
            (true, "本地", false, ""),
            (false, "ok", false, ""),
            (false, "cli_not_found", true, "在线2"),
        ];
        let (a, b2, c, d) = args[i];
        let (active, signal) = combine_seed_alerts(a, b2, c, d);
        assert_eq!(active, case["active"].as_bool().unwrap(), "combine active");
        assert_eq!(signal, case["signal"].as_str().unwrap(), "combine signal");
        checked += 1;
    }

    // ------------------------------------------------ 8. water 纯函数
    for case in g["normalize_model_name"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        assert_eq!(
            normalize_model_name(input),
            case["out"].as_str().unwrap(),
            "normalizeModelName({input:?})"
        );
        checked += 1;
    }
    for case in g["contains_ci"].as_array().unwrap() {
        let hay = case["hay"].as_str().unwrap();
        let needle = case["needle"].as_str().unwrap();
        assert_eq!(
            contains_ci(hay, needle),
            case["want"].as_bool().unwrap(),
            "containsCI({hay:?}, {needle:?})"
        );
        checked += 1;
    }
    {
        // RecordResolution 记账（不落盘）
        let w = WaterCheck::detached();
        let inputs: [(&str, &str); 6] = [
            ("glm-5.3", "GLM-5.3"),
            ("codely-core", "glm-5.3"),
            ("Codely-Core", "alias/glm-5.3"),
            ("codely-core", "kimi-k3"),
            ("", "x"),
            ("x", ""),
        ];
        for ((req, ret), case) in inputs
            .iter()
            .zip(g["record_resolution"].as_array().unwrap())
        {
            let (from, changed) = w.record_resolution(req, ret);
            assert_eq!(from, case["from"].as_str().unwrap(), "RecordResolution from");
            assert_eq!(
                changed,
                case["changed"].as_bool().unwrap(),
                "RecordResolution changed ({req}→{ret})"
            );
            checked += 1;
        }
        // 落盘表快照与 Go 的 MarshalIndent 逐键一致（不落盘、不读真实目录）
        let want: serde_json::Value =
            serde_json::from_str(g["water_file_bytes"].as_str().unwrap()).unwrap();
        let got: BTreeMap<String, String> = w.resolved_map();
        let want_resolved: BTreeMap<String, String> = want["resolved"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        assert_eq!(got, want_resolved, "resolved 表与 Go 落盘一致");
        checked += 1;
    }
    {
        // PassiveEvents：归一化相等不记 + 环形 50
        let w = WaterCheck::detached();
        w.record_passive("codely-core", "codely-core", "u1");
        w.record_passive("Codely-Core", "alias/team/codely-core", "u1");
        w.record_passive("codely-core", "glm-5.3", "u1");
        assert_eq!(
            w.passive_events().len() as i64,
            g["passive_after_3"].as_i64().unwrap(),
            "三条输入只有一条该记"
        );
        for i in 0..60 {
            w.record_passive("m", &format!("r{i}"), "u1");
        }
        let evs = w.passive_events();
        let want = &g["passive_cap"];
        assert_eq!(evs.len() as i64, want["len"].as_i64().unwrap(), "环形上限");
        assert_eq!(evs.len(), PASSIVE_CAPACITY, "与 PASSIVE_CAPACITY 同值");
        assert_eq!(
            evs[evs.len() - 1].returned,
            want["last"].as_str().unwrap(),
            "最后一条"
        );
        checked += 2;
    }

    // ------------------------------------------------ 9. 历史状态文件清单
    {
        let want: Vec<String> = g["legacy_state_files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let got: Vec<String> = rs_tuanjie::appdir::LEGACY_STATE_FILES
            .iter()
            .map(|f| f.name.to_string())
            .collect();
        assert_eq!(got, want, "legacyStateFiles 清单逐条对齐");
        checked += 1;
    }

    // ------------------------------------------------ 9b. 代理自探测（对同一台机器的真值）
    //
    // 金标里是 **Go 侧读到的东西**（`regReadProxy()` 的原样返回 + 注册表两个原始值）；
    // Rust 侧对同一台机器做同样的两读，并检查 `proxy_url_from_env` 与 Go 的
    // `regReadProxy()` **逐字一致**（这是 P8c「代理自探测 Windows 分支」的对照面）。
    {
        let want = &g["proxy_probe"];
        let rust_enable = rs_tuanjie::winpaths::read_proxy_enable();
        let rust_server = rs_tuanjie::winpaths::read_proxy_server();
        assert_eq!(
            rust_enable.as_deref().unwrap_or(""),
            want["proxy_enable"].as_str().unwrap(),
            "注册表 ProxyEnable 与 Go 读到的一致"
        );
        assert_eq!(
            rust_server.as_deref().unwrap_or(""),
            want["proxy_server"].as_str().unwrap(),
            "注册表 ProxyServer 与 Go 读到的一致"
        );
        let env = rs_tuanjie::winpaths::ProxyEnv {
            http_proxy: String::new(),
            https_proxy: String::new(),
            proxy_enable: rust_enable.clone(),
            proxy_server: rust_server.clone(),
        };
        assert_eq!(
            rs_tuanjie::winpaths::proxy_url_from_env(&env),
            want["reg_read_proxy"].as_str().unwrap(),
            "注册表判定 → 代理 URL 与 Go 的 regReadProxy() 逐字一致"
        );
        // 环境变量只比对**存在性**（值可能带凭据，两侧都不记原文）
        let seen = want["env_seen"].as_object().unwrap();
        for n in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
            let rust_has = std::env::var(n).map(|v| !v.is_empty()).unwrap_or(false);
            assert_eq!(rust_has, seen.contains_key(n), "环境变量 {n} 的存在性与 Go 一致");
        }
        checked += 4;
    }

    // ------------------------------------------------ 10. Migrate 行为
    {
        let base = std::env::temp_dir().join(format!("rs-p8c-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("exe");
        let dst = base.join("data");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("tuanjie-water.json"), b"{\"a\":1}").unwrap();
        std::fs::write(src.join("tuanjie-pacing.json"), b"{\"enabled\":true}").unwrap();
        std::fs::write(dst.join("tuanjie-pacing.json"), b"{\"enabled\":false}").unwrap();
        let notes = rs_tuanjie::appdir::migrate(&dst, &src);
        let empty = rs_tuanjie::appdir::migrate(&src, &src);
        let want = &g["migrate"];
        assert_eq!(
            notes,
            want["notes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            "Migrate 短句逐条一致"
        );
        assert_eq!(dst.join("tuanjie-water.json").exists(), want["dst_has_water"].as_bool().unwrap());
        assert_eq!(src.join("tuanjie-water.json").exists(), want["src_has_water"].as_bool().unwrap());
        assert_eq!(
            src.join("tuanjie-pacing.json.migrated").exists(),
            want["src_has_migrated"].as_bool().unwrap()
        );
        assert!(empty.is_empty(), "同一目录迁移应为空");
        assert_eq!(
            std::fs::read_to_string(dst.join("tuanjie-pacing.json")).unwrap(),
            want["dst_pacing_content"].as_str().unwrap(),
            "目标才是权威"
        );
        let _ = std::fs::remove_dir_all(&base);
        checked += 5;
    }

    // ------------------------------------------------ 11. tgz 解包（合成夹具）
    //
    // 夹具 `tests/golden/p8c-synthetic-seed.tgz` 由 `.work/p8c/mktgz.py` 生成（确定性），
    // **Go 侧也解同一份字节**（结果记在金标 `fixture_tgz` 里）。这里逐项对照 +
    // 用同一套构造覆盖「缺文件 / 无特征 / 路径穿越 / 坏 gzip」四种形态。
    {
        let want = &g["fixture_tgz"];
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("golden")
            .join("p8c-synthetic-seed.tgz");
        let raw = std::fs::read(&fixture).unwrap_or_else(|e| {
            panic!("读不到入库夹具 {}: {e}", fixture.display())
        });
        assert_eq!(
            raw.len() as i64,
            want["bytes"].as_i64().unwrap(),
            "夹具字节数与 Go 侧一致"
        );
        let out = rs_tuanjie::gziptar::extract_from_gzip(
            &raw,
            rs_tuanjie::seedcheck::SEED_BUNDLE_REL_PATH,
            rs_tuanjie::seedcheck::SEED_TARBALL_MAX_BYTES,
        )
        .expect("合成夹具应能解开");
        assert_eq!(out.found, want["found"].as_bool().unwrap(), "fixture.found");
        let mut body = std::io::Cursor::new(out.content.clone());
        let (sf, hf) =
            rs_tuanjie::seedcheck::contains_seed_features(&mut body, SYNTH_SEED, SYNTH_HEADER)
                .expect("特征扫描");
        assert_eq!(sf, want["seed"].as_bool().unwrap(), "fixture.seed");
        assert_eq!(hf, want["header"].as_bool().unwrap(), "fixture.header");
        // 目标文件确实只在 `package/bundle/gemini.js` 里（其余条目被跳过）
        assert!(
            String::from_utf8_lossy(&out.content).contains(SYNTH_SEED),
            "取到的应是 bundle/gemini.js 的内容"
        );
        checked += 1;

        // 四种构造形态（Go 侧同样跑过，见金标 tgz 段）
        let tgz = &g["tgz"];
        // (a) 包里没有目标文件
        let only_pkg = synth_tgz(&[("package/package.json", "{}")]);
        let o = rs_tuanjie::gziptar::extract_from_gzip(
            &only_pkg,
            rs_tuanjie::seedcheck::SEED_BUNDLE_REL_PATH,
            rs_tuanjie::seedcheck::SEED_TARBALL_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(
            o.found,
            tgz["missing-file"]["found"].as_bool().unwrap(),
            "缺文件 → found=false"
        );
        // (b) 文件在但特征缺失
        let nofeat = synth_tgz(&[("package/bundle/gemini.js", "nothing here")]);
        let o = rs_tuanjie::gziptar::extract_from_gzip(
            &nofeat,
            rs_tuanjie::seedcheck::SEED_BUNDLE_REL_PATH,
            rs_tuanjie::seedcheck::SEED_TARBALL_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(o.found, true);
        let mut c = std::io::Cursor::new(o.content);
        let (sf, hf) =
            rs_tuanjie::seedcheck::contains_seed_features(&mut c, SYNTH_SEED, SYNTH_HEADER).unwrap();
        assert_eq!(sf, tgz["no-features"]["seed"].as_bool().unwrap());
        assert_eq!(hf, tgz["no-features"]["header"].as_bool().unwrap());
        // (c) 路径穿越条目（`../bundle/gemini.js` 带特征、`package/bundle/gemini.js` 是干净内容）：
        //     Go 侧跳过穿越条目、取到干净内容 ⇒ 两特征都 false
        let evil = synth_tgz_raw(&[
            ("../bundle/gemini.js", "SYNTHETICSEED0001 X-Codely-Signature"),
            ("package/bundle/gemini.js", "clean"),
        ]);
        let o = rs_tuanjie::gziptar::extract_from_gzip(
            &evil,
            rs_tuanjie::seedcheck::SEED_BUNDLE_REL_PATH,
            rs_tuanjie::seedcheck::SEED_TARBALL_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(
            o.found,
            tgz["traversal"]["found"].as_bool().unwrap(),
            "穿越条目被跳过后仍找到干净的目标文件"
        );
        assert_eq!(
            String::from_utf8_lossy(&o.content),
            "clean",
            "取到的必须是**干净**的那一条（穿越条目不得被采信）"
        );
        let mut c = std::io::Cursor::new(o.content);
        let (sf, hf) =
            rs_tuanjie::seedcheck::contains_seed_features(&mut c, SYNTH_SEED, SYNTH_HEADER).unwrap();
        assert_eq!(sf, tgz["traversal"]["seed"].as_bool().unwrap());
        assert_eq!(hf, tgz["traversal"]["header"].as_bool().unwrap());
        // (d) 坏 gzip：如实报错（不静默当「无特征」）
        let err = rs_tuanjie::gziptar::extract_from_gzip(
            b"not a gzip at all",
            rs_tuanjie::seedcheck::SEED_BUNDLE_REL_PATH,
            rs_tuanjie::seedcheck::SEED_TARBALL_MAX_BYTES,
        )
        .unwrap_err();
        assert!(!err.is_empty(), "坏 gzip 要给出错误文本（Go: {:?}）", tgz["bad-gzip"]["err_text"]);
        checked += 4;
    }

    println!("P8c 门禁：{checked} 项与真 Go 采收逐条一致");
}

// ---------------------------------------------------------------- 合成 tar.gz 构造（门禁自用）
//
// 只造**未压缩**的 512 字节对齐 tar（gzip 层用固定头手工拼）——目的只是覆盖
// 「条目名形态 / 穿越防护 / 缺文件」这几条判定，不重复 Go 侧 httptest 的下载链路。

/// 手工拼一个最小 uSTAR（文件名 ≤100 字节）+ gzip 容器。
fn synth_tgz(files: &[(&str, &str)]) -> Vec<u8> {
    synth_tgz_raw(files)
}

fn synth_tgz_raw(files: &[(&str, &str)]) -> Vec<u8> {
    let mut tar: Vec<u8> = Vec::new();
    for (name, body) in files {
        let mut hdr = [0u8; 512];
        let nb = name.as_bytes();
        hdr[..nb.len()].copy_from_slice(nb);
        hdr[100..108].copy_from_slice(b"0000644\0");
        hdr[108..116].copy_from_slice(b"0000000\0");
        hdr[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", body.len());
        hdr[124..136].copy_from_slice(size.as_bytes());
        hdr[136..148].copy_from_slice(b"00000000000\0");
        hdr[148..156].copy_from_slice(b"        ");
        hdr[156] = b'0';
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263..265].copy_from_slice(b"00");
        // 校验和：先填空格算完再写回
        let sum: u32 = hdr.iter().map(|b| *b as u32).sum();
        let chk = format!("{:06o}\0 ", sum);
        hdr[148..156].copy_from_slice(chk.as_bytes());
        tar.extend_from_slice(&hdr);
        tar.extend_from_slice(body.as_bytes());
        let pad = (512 - (body.len() % 512)) % 512;
        tar.extend(std::iter::repeat(0u8).take(pad));
    }
    tar.extend(std::iter::repeat(0u8).take(1024)); // 归档结束
    gzip_wrap(&tar)
}

/// 手工 gzip 容器：`stored`（不压缩）DEFLATE 块 —— 解码器必须能吃下它。
fn gzip_wrap(data: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    let mut left = data;
    while !left.is_empty() {
        let take = std::cmp::min(left.len(), 65535);
        let last = take == left.len();
        out.push(if last { 0x01 } else { 0x00 });
        out.push((take & 0xff) as u8);
        out.push((take >> 8) as u8);
        out.push((!take & 0xff) as u8);
        out.push(((!take) >> 8) as u8);
        out.extend_from_slice(&left[..take]);
        left = &left[take..];
    }
    // CRC32 与 ISIZE
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
        }
        *entry = c;
    }
    let mut crc = 0xffff_ffffu32;
    for b in data {
        crc = table[((crc ^ *b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}
