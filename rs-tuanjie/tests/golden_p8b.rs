//! P8b 切片黄金门禁 —— 与**真 Go 采收**的 `golden-p8b-go.json` 逐字段对照。
//!
//! 基准由 Go 侧一次性采集器 `internal/tuanjie/zz_p8b_collect_test.go` 现采
//! （跑完即删，留档 `.work/p8b/zz_p8b_collect_test.go`；再生成口径见 `tests/golden/README.md`）。
//! 加载顺序：先入库副本 `tests/golden/`，缺则回落 `.work/p8b/`。
//!
//! 覆盖本切片：`pacing.go` / `inflight.go` / `opencode_session.go` /
//! `dist.go`（纯函数）/ `channels.go`（静态表）。

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

// ---------------------------------------------------------------- 基准加载

fn golden_dir() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    if vendored.join("golden-p8b-go.json").exists() {
        return vendored;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".work/p8b")
}

fn load_golden() -> Value {
    let p = golden_dir().join("golden-p8b-go.json");
    let bytes = std::fs::read(&p).unwrap_or_else(|e| {
        panic!(
            "读不到 P8b 黄金基准 {}: {e}（再生成见 tests/golden/README.md）",
            p.display()
        )
    });
    eprintln!("P8b 黄金基准: {}", p.display());
    serde_json::from_slice(&bytes).expect("黄金基准是合法 JSON")
}

/// 改进程级环境变量的测试必须串行（本 crate 沿用 rs-codebuddy 的 `env_lock` 惯例）。
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- 1. inflight

#[test]
fn inflight_registry_and_activity_match_go() {
    use rs_tuanjie::inflight::*;

    let g = load_golden();
    let want = &g["inflight"];

    // rid 形态：12 位零填充小写十六进制，逐值锁死（纯函数可比）
    let first: Vec<String> = want["rid_first_5"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for (i, w) in first.iter().enumerate() {
        assert_eq!(&rid_from_n(i as u64 + 1), w, "rid_from_n({})", i + 1);
    }
    assert_eq!(want["rid_len"].as_u64().unwrap() as usize, 12);
    assert_eq!(rid_from_n(1).len(), 12);
    assert!(rid_from_n(0xabcdef).chars().all(|c| c.is_ascii_hexdigit()));

    // 容量与淘汰
    assert_eq!(
        want["capacity"].as_u64().unwrap() as usize,
        REGISTRY_CAPACITY
    );
    let reg = RequestRegistry::new();
    let mut rids = Vec::new();
    for _ in 0..(REGISTRY_CAPACITY + 1) {
        rids.push(reg.register("m", "u1", true));
    }
    let snap = reg.inflight();
    assert_eq!(snap.len() as u64, want["after_501"].as_u64().unwrap());
    let oldest_evicted = !snap.iter().any(|r| r.rid == rids[0]);
    let newest_present = snap.iter().any(|r| r.rid == rids[REGISTRY_CAPACITY]);
    assert_eq!(oldest_evicted, want["oldest_evicted"].as_bool().unwrap());
    assert_eq!(newest_present, want["newest_present"].as_bool().unwrap());
    reg.finish(&rids[REGISTRY_CAPACITY]);
    assert_eq!(
        reg.inflight().len() as u64,
        want["after_finish"].as_u64().unwrap()
    );

    // LoadOf
    let reg2 = RequestRegistry::new();
    for _ in 0..3 {
        reg2.register("m", "uA", true);
    }
    reg2.register("m", "uB", false);
    assert_eq!(
        reg2.load_of("uA") as u64,
        want["load_of"]["uA"].as_u64().unwrap()
    );
    assert_eq!(
        reg2.load_of("uB") as u64,
        want["load_of"]["uB"].as_u64().unwrap()
    );
    assert_eq!(
        reg2.load_of("nobody") as u64,
        want["load_of"]["none"].as_u64().unwrap()
    );

    // 耗时降序
    let reg3 = RequestRegistry::new();
    reg3.register("m", "u", true);
    std::thread::sleep(std::time::Duration::from_millis(15));
    reg3.register("m", "u", true);
    let s3 = reg3.inflight();
    let desc = s3.windows(2).all(|w| w[1].elapsed <= w[0].elapsed);
    assert_eq!(desc, want["elapsed_desc"].as_bool().unwrap());

    // 快照字段集合
    let mut got_fields: Vec<String> = serde_json::to_value(&s3[0])
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    got_fields.sort();
    let want_fields: Vec<String> = want["snapshot_fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(got_fields, want_fields, "InflightReq 字段集合");

    // 动态日志
    let al = ActivityLog::new();
    let n = ACTIVITY_CAPACITY as i64;
    for i in 0..(n + 10) {
        al.add("ok", &format!("msg-{i}"), "m", "u", i, i * 2, 200);
    }
    let all = al.list(0);
    let act = &want["activity"];
    assert_eq!(
        (all.len() == 200),
        act["cap_200"].as_bool().unwrap(),
        "环形容量"
    );
    assert_eq!(
        all[0].message == format!("msg-{}", n + 9),
        act["first_is_newest"].as_bool().unwrap()
    );
    assert_eq!(
        all[199].message == "msg-10",
        act["last_is_oldest"].as_bool().unwrap()
    );
    assert_eq!(all[0].seq, act["seq_of_first"].as_i64().unwrap());
    assert_eq!(al.list(3).len() as u64, act["limit3"].as_u64().unwrap());

    // 事件字段集合（model/user_id/latency_ms/tokens/status 为空时必须省略）
    let probe = ActivityEvent {
        seq: 1,
        time: "00:00:00".into(),
        kind: "ok".into(),
        message: "x".into(),
        model: String::new(),
        user_id: String::new(),
        latency_ms: 0,
        tokens: 0,
        status: 0,
    };
    let mut got_ev: Vec<String> = serde_json::to_value(&probe)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    got_ev.sort();
    let want_ev: Vec<String> = act["event_fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(got_ev, want_ev, "ActivityEvent 字段集合（omitempty 口径）");

    // 本机时区 HH:MM:SS 形态
    let hms = local_hms();
    let parts: Vec<&str> = hms.split(':').collect();
    assert_eq!(parts.len(), 3, "{hms}");
    assert!(parts.iter().all(|p| p.len() == 2), "{hms}");
    eprintln!("inflight 对照：rid 形态 / 容量 500 / LoadOf / 耗时降序 / 动态环形 200 全一致");
}

// ---------------------------------------------------------------- 2. pacing

#[test]
fn pacing_matches_go() {
    use rs_tuanjie::pacing::*;

    let _guard = env_lock();
    let g = load_golden();
    let want = &g["pacing"];

    assert_eq!(
        want["retry_default_s"].as_u64().unwrap(),
        PACING_RETRY_DEFAULT_SECS
    );
    assert_eq!(want["max_wait_s"].as_u64().unwrap(), PACING_MAX_WAIT_SECS);

    for row in want["retry_wait"].as_array().unwrap() {
        let input = row["in"].as_str().unwrap();
        let want_s = row["out_s"].as_u64().unwrap();
        assert_eq!(retry_wait_secs(input), want_s, "RetryWait({input:?})");
    }
    for row in want["is_pacing_model"].as_array().unwrap() {
        let m = row["model"].as_str().unwrap();
        assert_eq!(
            is_pacing_model(m),
            row["want"].as_bool().unwrap(),
            "IsPacingModel({m:?})"
        );
    }

    // 落盘形态 + 读回（临时数据目录）
    let dir = std::env::temp_dir().join(format!("rs-tuanjie-p8b-pacing-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("tuanjie-pacing.json");
    let _ = std::fs::remove_file(&path);
    std::env::set_var(DATA_DIR_ENV, &dir);

    let p = Pacer::new();
    assert_eq!(p.enabled(), want["default_enabled"].as_bool().unwrap());
    p.set_enabled(true);
    let raw = std::fs::read(&path).expect("pacing 文件应已落盘");
    assert_eq!(
        String::from_utf8_lossy(&raw),
        want["file_on"].as_str().unwrap(),
        "落盘形态"
    );
    let p2 = Pacer::new();
    assert_eq!(p2.enabled(), want["reload_on"].as_bool().unwrap());
    p2.set_enabled(false);
    let raw2 = std::fs::read(&path).unwrap();
    assert_eq!(String::from_utf8_lossy(&raw2), want["file_off"].as_str().unwrap());
    let p3 = Pacer::new();
    assert_eq!(p3.enabled(), want["reload_off"].as_bool().unwrap());
    let tmp_left = path.with_extension("json.tmp").exists()
        || std::path::PathBuf::from(format!("{}.tmp", path.display())).exists();
    assert_eq!(tmp_left, want["tmp_left_behind"].as_bool().unwrap());

    std::env::remove_var(DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("pacing 对照：RetryWait 12 例 / IsPacingModel 10 例 / 落盘与读回全一致");
}

// ---------------------------------------------------------------- 3. opencode

#[test]
fn opencode_session_matches_go() {
    use rs_tuanjie::opencode_session::*;

    let g = load_golden();
    let want = &g["opencode"];
    assert_eq!(want["header_name"].as_str().unwrap(), OPENCODE_SESSION_HEADER);

    let a = opencode_session();
    let b = opencode_session();
    assert_eq!(a == b, want["stable"].as_bool().unwrap());
    let u = uuid::Uuid::parse_str(a).expect("UUID");
    assert_eq!(
        (u.get_version_num() == 4),
        want["uuid_v4_ok"].as_bool().unwrap()
    );

    for row in want["apply_table"].as_array().unwrap() {
        let base = row["base"].as_str().unwrap();
        let mut h = reqwest::header::HeaderMap::new();
        apply_opencode_session(base, &mut h);
        let set = h.contains_key(OPENCODE_SESSION_HEADER);
        assert_eq!(set, row["set"].as_bool().unwrap(), "base={base:?}");
        let same = h
            .get(OPENCODE_SESSION_HEADER)
            .map(|v| v.to_str().unwrap() == a)
            .unwrap_or(false);
        assert_eq!(same, row["same_as_id"].as_bool().unwrap(), "base={base:?} 取值");
    }
    eprintln!("opencode 对照：UUID v4 / 进程级稳定 / 7 条 base 判定全一致");
}

// ---------------------------------------------------------------- 4. dist

#[test]
fn dist_pure_functions_match_go() {
    use rs_tuanjie::dist::*;

    let g = load_golden();
    let want = &g["dist"];

    assert_eq!(want["buckets"].as_u64().unwrap() as usize, DIST_BUCKETS);
    assert_eq!(want["max_samples"].as_u64().unwrap() as usize, DIST_MAX_SAMPLES);
    assert_eq!(want["prompt"].as_str().unwrap(), DIST_PROMPT);

    for row in want["parse"].as_array().unwrap() {
        let input = row["in"].as_str().unwrap();
        assert_eq!(
            parse_dist_answer(input),
            row["out"].as_i64().unwrap(),
            "parseDistAnswer({input:?})"
        );
    }

    for row in want["stats"].as_array().unwrap() {
        let mut counts = vec![0i64; DIST_BUCKETS + 1];
        for (k, v) in row["counts"].as_object().unwrap() {
            let idx: usize = k.parse().unwrap();
            counts[idx] = v.as_i64().unwrap();
        }
        let s = dist_stats_of(&counts);
        let label = row["counts"].to_string();
        assert_eq!(s.mean, row["mean"].as_f64().unwrap(), "mean {label}");
        assert_eq!(s.median, row["median"].as_f64().unwrap(), "median {label}");
        assert_eq!(s.std_dev, row["std_dev"].as_f64().unwrap(), "std_dev {label}");
        assert_eq!(s.mode, row["mode"].as_i64().unwrap(), "mode {label}");
        assert_eq!(
            s.mode_count,
            row["mode_count"].as_i64().unwrap(),
            "mode_count {label}"
        );
    }

    for row in want["compare"].as_array().unwrap() {
        let a = sparse(&row["a"]);
        let b = sparse(&row["b"]);
        let s = compare_dist(
            &a,
            &b,
            row["valid_a"].as_i64().unwrap(),
            row["valid_b"].as_i64().unwrap(),
        );
        assert_eq!(s.cosine, row["cosine"].as_f64().unwrap(), "cosine");
        assert_eq!(s.js_divergence, row["js_divergence"].as_f64().unwrap(), "js");
        assert_eq!(s.distrib_score, row["distrib_score"].as_f64().unwrap(), "distrib");
        assert_eq!(s.mode_a, row["mode_a"].as_i64().unwrap(), "mode_a");
        assert_eq!(s.mode_b, row["mode_b"].as_i64().unwrap(), "mode_b");
        assert_eq!(s.mode_score, row["mode_score"].as_f64().unwrap(), "mode_score");
        assert_eq!(s.overall, row["overall"].as_f64().unwrap(), "overall");
    }

    for row in want["verdict"].as_array().unwrap() {
        let sim = DistSimilarity {
            overall: row["overall"].as_f64().unwrap(),
            ..Default::default()
        };
        let (pct, v) = dist_verdict(&sim);
        assert_eq!(pct, row["pct"].as_f64().unwrap(), "pct");
        assert_eq!(v, row["verdict"].as_str().unwrap(), "verdict");
        assert_eq!(
            dist_verdict_label(v),
            row["label"].as_str().unwrap(),
            "label"
        );
    }
    assert_eq!(
        dist_verdict_label("不是档位"),
        want["unknown_label"].as_str().unwrap()
    );
    assert_eq!(
        fmt_dist_sample_progress(3, 60),
        want["progress"].as_str().unwrap()
    );

    // 取整/绝对值口径（Go math.Round 是 half-away-from-zero）
    let ties: Vec<f64> = want["math_round_ties"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    assert_eq!(round2(0.125), ties[0]);
    assert_eq!(round2(1.005), ties[1]);
    assert_eq!(round4(0.00005), ties[2]);
    assert_eq!(round2(1.005), want["round2_sample"].as_f64().unwrap());
    assert_eq!(round4(0.12345), want["round4_sample"].as_f64().unwrap());
    assert_eq!(abs_i64(-7), want["abs_neg"].as_i64().unwrap());
    assert_eq!(2.5f64.round(), want["math_round_check"].as_f64().unwrap());
    eprintln!("dist 对照：parse 25 例 / stats 6 例 / compare 4 例 / verdict 7 例全一致");
}

/// 稀疏 `{数字: 次数}` → Go `CompareDist` 要的**下标 0..354** 数组。
fn sparse(v: &Value) -> Vec<i64> {
    use rs_tuanjie::dist::DIST_BUCKETS;
    let mut out = vec![0i64; DIST_BUCKETS];
    for (k, n) in v.as_object().unwrap() {
        let idx: usize = k.parse().unwrap();
        if (1..=DIST_BUCKETS).contains(&idx) {
            out[idx - 1] = n.as_i64().unwrap();
        }
    }
    out
}

// ---------------------------------------------------------------- 5. channels

#[test]
fn channels_static_tables_match_go() {
    use rs_tuanjie::channels::*;

    let g = load_golden();
    let want = &g["channels"];
    assert_eq!(want["config_fn"].as_str().unwrap(), CHANNELS_CONFIG_FILE);

    let defs = want["defs"].as_array().unwrap();
    assert_eq!(defs.len(), WATER_CHANNELS.len(), "渠道条数");
    for (i, d) in defs.iter().enumerate() {
        let c = &WATER_CHANNELS[i];
        assert_eq!(d["id"].as_str().unwrap(), c.id, "id[{i}]");
        assert_eq!(d["name"].as_str().unwrap(), c.name, "name[{i}]");
        assert_eq!(d["port"].as_u64().unwrap() as u16, c.port, "port[{i}]");
        assert_eq!(d["base_url"].as_str().unwrap(), c.base_url, "base_url[{i}]");
        assert_eq!(
            d["bearer_from"].as_str().unwrap(),
            c.bearer_from,
            "bearer_from[{i}]"
        );
    }
    for row in want["strong"].as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        assert_eq!(
            is_strong_channel(id),
            row["want"].as_bool().unwrap(),
            "isStrongChannel({id:?})"
        );
    }
    for row in want["name_of"].as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        assert_eq!(
            channel_name_of(id),
            row["want"].as_str().unwrap(),
            "channelNameOf({id:?})"
        );
    }
    eprintln!("channels 对照：8 条渠道定义 / strong 9 例 / nameOf 11 例全一致");
}
