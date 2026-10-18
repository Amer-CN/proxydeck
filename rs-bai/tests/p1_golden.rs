// p1_golden.rs —— P1 黄金对照门禁：`/model/matrix` 与 `/v1/stats` 必须与 Go 侧同形。
//
// 基准文件 `rs-bai/tests/golden/golden-p1-go.json`（**入库**；缺则回落 `.work/rust-rewrite/`）
// 由 Go 侧（`internal/bai` 包内一次性采集器：真 `Server.Start()` 起真 socket + 假上游 +
// 临时渠道配置）采集，逐条记：
//   * 下游请求（方法 + 路径 + 当时的上游形态）
//   * 逐字节响应（`body_raw`）、响应头（`resp_headers`）与上游命中数（`upstream`）
//   * 统计场景还记当时的服务端累计表（`seed_stats`）——Go 侧 `addStat` 是包内方法，
//     HTTP 路径进不来；`/v1/stats` 的对外合同是「给定累计表 → 给定响应」，
//     所以回放时用同一张表预置 `bai-stats.json` 再取响应，测的是同一件事。
//
// 文件缺失时测试**失败**而不是跳过：黄金门禁是这条路线的前提，静默跳过等于没有门禁。
//
// 两条采集约束决定了本文件的形状：
//   * `fetched_at` / `uptimeSec` 每次运行必然不同 → 比对时把这两个数字归一化成占位符，
//     其余字节一字不动；同时核对基准里记的可复现事实（零/非零口径）
//   * Go 侧 `/v1/chat/completions` 的**透传**不可注入传输层（会真打 api.b.ai），
//     故基准不含该路径；chat 改写由 P0 的 `tests/golden_adapt.rs` 守着
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::Value;

/// 基准里的假上游形态之一：`@@raw:<原文>` 表示「逐字节回这段」（对应采集器的同名模式）。
/// 用 `@@` 前缀是为了与 Go 采集器的 `raw:` 区分——两边写在各自的代码里，不必同名，
/// 但含义必须一致：把这串原文当 /v1/models 的响应体原样回去。
const RAW_PREFIX: &str = "@@raw:";

fn load_golden() -> Value {
    let p = rs_bai::golden_path("golden-p1-go.json");
    let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "读不到 P1 黄金基准 {}: {e}（入库副本在 rs-bai/tests/golden/，重新采集见 .work/）",
            p.display()
        )
    });
    serde_json::from_str(&raw).expect("golden JSON 解析失败")
}

// ---------- 假上游（回放基准里的清单 / 502 / 空数组 / data:null / 任意原文） ----------

#[derive(Clone)]
struct FakeUpstream {
    hits: Arc<AtomicUsize>,
    mode: Arc<std::sync::Mutex<String>>,
    list_raw: Vec<u8>,
}

impl FakeUpstream {
    fn set_mode(&self, mode: &str) {
        *self.mode.lock().unwrap() = mode.to_string();
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn fake_upstream(State(up): State<FakeUpstream>, headers: HeaderMap, _body: Bytes) -> Response {
    up.hits.fetch_add(1, Ordering::SeqCst);
    let mode = up.mode.lock().unwrap().clone();
    let json_ct = [(axum::http::header::CONTENT_TYPE, "application/json")];
    // 任意原文（采集器里 `@@raw:` 的同口径）：逐字节回这段，用于 data 带空白/HTML 敏感字符等场景
    if let Some(raw) = mode.strip_prefix(RAW_PREFIX) {
        return (StatusCode::OK, json_ct, raw.as_bytes().to_vec()).into_response();
    }
    match mode.as_str() {
        "bad" => {
            return (
                StatusCode::BAD_GATEWAY,
                json_ct,
                br#"{"error":{"message":"bad gateway"}}"#.to_vec(),
            )
                .into_response()
        }
        "empty" => {
            return (
                StatusCode::OK,
                json_ct,
                br#"{"object":"list","data":[]}"#.to_vec(),
            )
                .into_response()
        }
        "null" => {
            return (
                StatusCode::OK,
                json_ct,
                br#"{"object":"list","data":null}"#.to_vec(),
            )
                .into_response()
        }
        _ => {}
    }
    if headers.get("authorization").is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            json_ct,
            br#"{"error":{"message":"Invalid token"}}"#.to_vec(),
        )
            .into_response();
    }
    (StatusCode::OK, json_ct, up.list_raw.clone()).into_response()
}

async fn spawn_upstream(list_raw: Vec<u8>) -> (String, FakeUpstream) {
    let up = FakeUpstream {
        hits: Arc::new(AtomicUsize::new(0)),
        mode: Arc::new(std::sync::Mutex::new("ok".to_string())),
        list_raw,
    };
    let app = Router::new()
        .fallback(any(fake_upstream))
        .with_state(up.clone());
    let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ln.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ln, app).await;
    });
    (format!("http://{addr}"), up)
}

// ---------- 被测服务 ----------

struct Srv {
    base: String,
    case_dir: std::path::PathBuf,
    stats_path: std::path::PathBuf,
    upstream: FakeUpstream,
}

/// 按基准里的一条场景起服务：写渠道配置、预置 stats、切上游模式。
/// 全部路径都是临时目录（每个场景一个），绝不碰 `%APPDATA%` 下的真实文件。
async fn start_case_with_seed(scenario: &Value, upstream_list: Vec<u8>, seed_stats: &Value) -> Srv {
    let id = scenario["id"].as_str().unwrap();
    let tmp = std::env::temp_dir().join(format!("rs-bai-p1-{}-{}", id, std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let channels = tmp.join("tuanjie-water-channels.json");
    match scenario["channels_config"].as_str() {
        Some(c) if !c.is_empty() => std::fs::write(&channels, c.as_bytes()).unwrap(),
        _ => {
            let _ = std::fs::remove_file(&channels);
        }
    }

    let stats_path = tmp.join("bai-stats.json");
    // 统计场景：基准里逐步记了当时的累计表，这里按同一张表预置（走 `Stats::load` 的读回路径，
    // 与 Go 的 `loadStats` 对位）。基准没有 seed 的场景（矩阵场景）保持「文件不存在」初始态。
    if !seed_stats.is_null() {
        std::fs::write(&stats_path, serde_json::to_vec(seed_stats).unwrap()).unwrap();
    } else {
        let _ = std::fs::remove_file(&stats_path);
    }

    let (up_base, upstream) = spawn_upstream(upstream_list).await;
    upstream.set_mode(scenario["upstream_mode"].as_str().unwrap_or("ok"));

    // base 用不上的地址即可（本场景所有出站都是假上游）；数据目录路径显式指定
    let client = rs_bai::UpstreamClient::with_proxy("http://127.0.0.1:1", None).unwrap();
    let state = Arc::new(
        rs_bai::AppState::with_paths(client, channels, stats_path.clone())
            .with_models_url(format!("{up_base}/v1/models")),
    );
    // 与生产**同一份** 405 补体中间件（`run()` 里挂的就是它；`router()` 本身不挂）。
    // 第 5 轮 R5：此前这里是自造副本，比生产多设了 `nosniff`，注释却写成「照抄同一实现」。
    let app = rs_bai::router(state).layer(axum::middleware::from_fn(rs_bai::method_not_allowed_text));
    let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = ln.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ln, app).await;
    });
    Srv {
        base: format!("http://{addr}"),
        case_dir: tmp,
        stats_path,
        upstream,
    }
}

// ---------- 比对工具 ----------

async fn raw_response(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    path: &str,
) -> (u16, Vec<u8>, String, HeaderMap) {
    let url = format!("{base}{path}");
    let req = match method {
        "POST" => client.post(&url),
        _ => client.get(&url),
    };
    let resp = req
        .send()
        .await
        .unwrap_or_else(|e| panic!("{method} {path} 请求失败: {e}"));
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let headers = resp.headers().clone();
    let body = resp.bytes().await.unwrap().to_vec();
    (status, body, ct, headers)
}

/// 归一化易变字段（`fetched_at` / `uptimeSec`）：把数字位换成占位符，其余一字不动。
/// 不是重排 JSON、也不是只比字段集合——归一化之后仍是逐字节比较。
fn normalize(body: &[u8], key: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(body).into_owned();
    let needle = format!("\"{key}\":");
    let Some(i) = text.find(&needle) else {
        return body.to_vec();
    };
    let start = i + needle.len();
    let end = text[start..]
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .map(|o| start + o)
        .unwrap_or(text.len());
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str("<N>");
    out.push_str(&text[end..]);
    out.into_bytes()
}

fn fmt_body(b: &[u8]) -> String {
    // 尾部差异（例如 Go `json.Encoder` 多出来的换行）最容易在截断里被吃掉，
    // 所以这里显式把末尾 8 个字节转义出来。
    let tail = &b[b.len().saturating_sub(8)..];
    let tail_escaped: String = tail
        .iter()
        .map(|c| match c {
            b'\n' => "\\n".to_string(),
            b'\r' => "\\r".to_string(),
            b'\t' => "\\t".to_string(),
            c if c.is_ascii_graphic() || *c == b' ' => (*c as char).to_string(),
            c => format!("\\x{c:02x}"),
        })
        .collect();
    let s = String::from_utf8_lossy(b);
    let head = if s.len() > 240 {
        format!("{}…", &s[..240])
    } else {
        s.into_owned()
    };
    format!("{head}\n[末尾 8 字节] {tail_escaped}（共 {} 字节）", b.len())
}

fn golden_raw(step: &Value) -> Vec<u8> {
    step["body_raw"].as_str().unwrap_or("").as_bytes().to_vec()
}

/// 响应体逐字节比对（归一化易变字段后必须完全一致）+ 核对基准记的可复现事实。
fn compare_body(label: &str, step: &Value, got: &[u8], normalize_key: Option<&str>) {
    let want = golden_raw(step);
    if want.is_empty() {
        return;
    }
    let (want_n, got_n) = match normalize_key {
        Some(k) => (normalize(&want, k), normalize(got, k)),
        None => (want.clone(), got.to_vec()),
    };
    assert_eq!(
        String::from_utf8_lossy(&got_n),
        String::from_utf8_lossy(&want_n),
        "{label} 响应体与 Go 不一致\n--- Go  ---\n{}\n--- Rust ---\n{}",
        fmt_body(&want),
        fmt_body(got)
    );
    if let Some(extra) = step["extra"].as_object() {
        // 只对矩阵响应核对 `fetched_at`（统计响应的 extra 里不该有这一项；
        // 若基准错误地记了它，这里必须报出来而不是静默跳过）
        if let Some(expect_zero) = extra.get("fetched_at_is_zero").and_then(|v| v.as_bool()) {
            let v: Value = serde_json::from_slice(got).expect("响应应为 JSON");
            let Some(n) = v.get("fetched_at").and_then(|f| f.as_i64()) else {
                panic!("{label} 基准记了 fetched_at 事实，但本响应里没有 fetched_at 字段");
            };
            assert_eq!(
                n == 0,
                expect_zero,
                "{label} fetched_at 的零/非零口径与 Go 不一致（fetched_at={n}）"
            );
        }
    }
}

/// 响应头形状核对：基准逐步记了 Go 侧响应头，这里逐项对齐。
/// 只比**语义相关**的头——`date` 与 `content-length` 每次运行都可能不同，且 Go 与
/// axum 的表达方式不同（chunked / 长度），比它们只会得到噪声。
fn compare_headers(label: &str, step: &Value, headers: &HeaderMap) {
    let Some(want) = step["resp_headers"].as_object() else {
        return;
    };
    for key in ["content-type", "x-content-type-options"] {
        let Some(w) = want.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        let got = headers
            .get(key)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if key == "content-type" {
            assert_eq!(got, w, "{label} content-type 与 Go 不一致");
        } else {
            assert_eq!(got, w, "{label} {key} 与 Go 不一致");
        }
    }
}

// ---------- 主用例 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_p1_endpoints_match_go_golden() {
    let golden = load_golden();
    let cases = golden["cases"].as_array().expect("golden 缺 cases 数组");
    let upstream_list = golden["upstream_list_raw"]
        .as_str()
        .expect("golden 缺 upstream_list_raw")
        .as_bytes()
        .to_vec();
    assert!(!cases.is_empty(), "golden cases 为空");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();

    let mut scenarios = 0usize;
    let mut steps_checked = 0usize;

    for scenario in cases {
        let id = scenario["id"].as_str().unwrap();
        let steps = scenario["steps"].as_array().expect("场景缺 steps");

        // 统计场景：起服务时按「第一步的累计表」预置（基准逐步记了同一张表的形态）
        let seed = steps
            .first()
            .map(|s| s["seed_stats"].clone())
            .unwrap_or(Value::Null);
        let srv = start_case_with_seed(scenario, upstream_list.clone(), &seed).await;
        if let Some(warm) = scenario["warmup_upstream_mode"].as_str() {
            // 有些场景的「预热」不计入场景 steps（例如「已有清单后上游挂」：预热本身不是被测步骤）。
            // 基准用这一字段说明预热该用什么上游形态；不预热就会拿「从未加载」的形态去比。
            srv.upstream.set_mode(warm);
            let (st, _b, _, _) = raw_response(&client, &srv.base, "GET", "/model/matrix").await;
            assert_eq!(st, 200, "[{id}] 预热请求应 200");
        }

        // 上游命中数按「基准记的这一步打了几次」**逐步精确**核对：
        // 基准为 0 的步骤，Rust 侧累计值不许涨；非 0 的必须**正好涨那么多**
        // （多一次 = 重试口径与 Go 不同，少一次 = 漏打）。
        // 注意：预热打的上游次数也要计入基准（所以 last_hits 在预热之后才清零）。
        let mut last_hits = srv.upstream.hits();
        // 并发批量场景的批量命中数已单独核对，别让它落到下一步的增量里
        let mut batch_hits_checked: Option<usize> = None;

        if id == "matrix-single-flight" {
            // 并发场景：8 个并发请求打**已有缓存**的路径，上游只许命中 `batch_upstream` 次（=0）
            // `batch_upstream` 用 `omitempty` 落盘，值为 0 时字段会缺省 —— 缺省即 0。
            let batch = scenario["batch_upstream"].as_i64().unwrap_or(0);
            let before = srv.upstream.hits();
            let mut handles = Vec::new();
            for _ in 0..8 {
                let base = srv.base.clone();
                let c = client.clone();
                handles.push(tokio::spawn(async move {
                    let (s, b, _, _) = raw_response(&c, &base, "GET", "/model/matrix").await;
                    (s, b)
                }));
            }
            let mut non_empty = 0usize;
            for h in handles {
                let (status, body) = h.await.unwrap();
                assert_eq!(status, 200, "[{id}] 并发请求应 200");
                if !body.is_empty() {
                    non_empty += 1;
                }
            }
            let batch_hits = srv.upstream.hits() - before;
            assert_eq!(
                batch_hits, batch as usize,
                "[{id}] 8 并发应只打上游 {batch} 次（缓存态不发散），实际 {batch_hits}"
            );
            assert_eq!(non_empty, 8, "[{id}] 8 个响应都应非空");
            batch_hits_checked = Some(srv.upstream.hits());
        }

        for (idx, step) in steps.iter().enumerate() {
            let label = format!("[{id} #{idx}]");
            let method = step["method"].as_str().unwrap();
            let path = step["path"].as_str().unwrap();
            let want_up = step["upstream"].as_i64().unwrap();

            // 上游形态按基准**逐步**重放：场景内可能切过 bad/empty/null/raw（Go 侧同一步骤打的
            // 就是那个形态）。若只按场景初始形态设置，比出来的差异全是脚手架自己造成的。
            srv.upstream.set_mode(step["upstream_mode"].as_str().unwrap_or("ok"));

            if method == "FILE" {
                // 统计文件形态：Go 侧这一步读的是 `saveStats` 落的那份（= `json.Marshal(累计表)`，
                // 与同一步的 `seed_stats` 同源）。这里把基准记的那张表按 Rust 的落盘口径写出来，
                // 与 Go 的落盘字节逐字节比；再读回来确认闭环。累加语义本身由
                // `src/stats.rs` 的单测（对着 Go 的实测数字：calls=2 / 110 / 55 / 165）把关。
                let seed = &step["seed_stats"];
                assert!(
                    !seed.is_null(),
                    "{label} 落盘步骤缺 seed_stats（基准应当逐步记录累计表）"
                );
                let stats_file = srv.stats_path.clone();
                let snapshot: std::collections::BTreeMap<String, rs_bai::stats::ModelStat> =
                    serde_json::from_value(seed.clone()).expect("seed_stats 应能解成累计表");
                let s = rs_bai::stats::Stats::from_snapshot(stats_file.clone(), snapshot);
                s.save();
                let got = std::fs::read(&stats_file).unwrap_or_else(|e| {
                    panic!("{label} 读不到落盘统计 {}: {e}", stats_file.display())
                });
                assert_eq!(
                    String::from_utf8_lossy(&got),
                    serde_json::to_string(seed).unwrap(),
                    "{label} 落盘序列化口径与基准不一致"
                );
                compare_body(&label, step, &got, None);
                steps_checked += 1;
                continue;
            }

            // 统计请求：基准逐步记了当时的累计表，用同一张表预置一个**新的**实例
            // （走 `Stats::load` 的读回路径，与 Go `loadStats` 对位），再取响应比。
            let (srv_step, got) = if path.starts_with("/v1/stats") && !step["seed_stats"].is_null() {
                let s = start_case_with_seed(scenario, upstream_list.clone(), &step["seed_stats"]).await;
                let got = raw_response(&client, &s.base, method, path).await;
                (Some(s), got)
            } else {
                (None, raw_response(&client, &srv.base, method, path).await)
            };
            let srv_ref = srv_step.as_ref().unwrap_or(&srv);
            let (status, body, ct, headers) = got;

            assert_eq!(
                status,
                step["status"].as_u64().unwrap() as u16,
                "{label} 状态码不一致（{method} {path}）"
            );

            let hits_now = batch_hits_checked.unwrap_or_else(|| srv_ref.upstream.hits());
            let delta = hits_now.saturating_sub(last_hits);
            assert_eq!(
                delta as i64, want_up,
                "{label} 上游命中数与 Go 不一致：Go 记了 {want_up} 次，Rust 打了 {delta} 次"
            );
            last_hits = hits_now;

            compare_headers(&label, step, &headers);

            if let Some(want_text) = step["body_text"].as_str() {
                assert_eq!(
                    String::from_utf8_lossy(&body),
                    want_text,
                    "{label} 纯文本响应体不一致（content-type={ct}）"
                );
                steps_checked += 1;
                if let Some(s) = &srv_step {
                    let _ = std::fs::remove_dir_all(&s.case_dir);
                }
                continue;
            }

            compare_body(&label, step, &body, Some("fetched_at"));
            if path.starts_with("/v1/stats") {
                // 统计响应体的 uptimeSec 也每次不同：单独归一化后再比一次
                compare_body(&label, step, &body, Some("uptimeSec"));
            }
            if path.starts_with("/health") {
                compare_body(&label, step, &body, Some("uptimeSec"));
            }
            steps_checked += 1;
            if let Some(s) = srv_step {
                let _ = std::fs::remove_dir_all(&s.case_dir);
            }
        }

        scenarios += 1;
        let _ = std::fs::remove_dir_all(&srv.case_dir);
    }

    println!(
        "P1 黄金对照通过：{scenarios} 个场景 / {steps_checked} 步与 Go 同形（响应体归一化易变字段后逐字节一致）"
    );
}

// ---------- 跨语言 DPAPI 往返 ----------

/// 磁盘上的渠道配置是 **DPAPI 密文**（`PDAPI1\n` + base64），只有解开才拿得到 B.AI 的
/// Bearer。这条用例验的是**生产路径**：`channel_key` 读真密文 → 解 DPAPI → 取 key。
///
/// ⚠ 2026-10-18 变更：原先靠 Go 写的 `dpapiwrite.exe` 产出夹具（证明「Go 加密、Rust 解密」）。
/// Go 实现已整体移除，改为**本用例自己用同一个 Windows API 造密文**——DPAPI 是系统 API，
/// 跨语言兼容由操作系统保证，那一半不需要我们自己验；这里守住的是**落盘形态与解密路径**
/// （前缀、标准 base64、CryptUnprotectData 三个环节），这才是会随我们的代码改坏的部分。
/// 明文兼容、密文损坏回空串两条对照仍在下文。
#[cfg(windows)]
#[test]
fn reads_dpapi_ciphertext() {
    const SENTINEL: &str = "sk-sentinel-dpapi-roundtrip";

    let dir = std::env::temp_dir().join(format!("rs-bai-dpapi-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("tuanjie-water-channels.json");

    // 自己造密文：与生产落盘同形（`PDAPI1\n` + 标准 base64(CryptProtectData(json))）
    let json = format!(r#"[{{"id":"bai","key":"{SENTINEL}"}}]"#);
    let mut on_disk = b"PDAPI1\n".to_vec();
    on_disk.extend_from_slice(dpapi_protect_b64(json.as_bytes()).as_bytes());
    std::fs::write(&cfg_path, &on_disk).unwrap();

    // 夹具本身必须是密文（否则这条用例测的还是明文路径，没有意义）
    let raw = std::fs::read(&cfg_path).unwrap();
    assert!(
        raw.starts_with(b"PDAPI1\n"),
        "夹具不是 DPAPI 密文形态（前 16 字节: {:?}）",
        &raw[..raw.len().min(16)]
    );

    // Rust 侧走与生产同一条路径：读渠道配置 → 取 id=="bai" 的 key
    let key = rs_bai::matrix::channel_key(&cfg_path);
    assert_eq!(
        key, SENTINEL,
        "Rust 未能从 Go 的 DPAPI 密文里解出 bai 的 key（拿到的长度 {}）",
        key.len()
    );

    // 反向对照：同样内容的**明文**文件也读得出（读兼容优先，与 Go appdir.ReadFile 同口径）
    let plain_path = dir.join("plain.json");
    std::fs::write(&plain_path, format!(r#"[{{"id":"bai","key":"{SENTINEL}"}}]"#)).unwrap();
    assert_eq!(rs_bai::matrix::channel_key(&plain_path), SENTINEL);

    // 密文损坏时必须回空串（→ 端点走 need_key），而不是报错或返回半截 key
    let broken = dir.join("broken.json");
    std::fs::write(&broken, format!("PDAPI1\n{}", "AAAA")).unwrap();
    assert_eq!(
        rs_bai::matrix::channel_key(&broken),
        "",
        "损坏密文必须回空串（等同无密钥），不得返回半截内容"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/* ───────── 测试用 DPAPI 造密文（与 rs-bai/src/crypto.rs 同一套系统 API） ───────── */

#[cfg(windows)]
mod dpapi_test_ffi {
    use std::ffi::c_void;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut c_void,
    }

    /// 失败即报错，绝不弹系统对话框（与生产同值）。
    const UI_FORBIDDEN: u32 = 0x1;

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            p_data_in: *const DataBlob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *mut c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    pub fn protect(plain: &[u8]) -> Vec<u8> {
        let mut input = DataBlob {
            cb_data: plain.len() as u32,
            pb_data: plain.as_ptr() as *mut c_void,
        };
        let mut out = DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &mut input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                UI_FORBIDDEN,
                &mut out,
            )
        };
        assert_eq!(ok, 1, "CryptProtectData 失败（GetLastError={}）", unsafe {
            GetLastError()
        });
        let bytes = unsafe { std::slice::from_raw_parts(out.pb_data as *const u8, out.cb_data as usize) }.to_vec();
        unsafe { LocalFree(out.pb_data) };
        bytes
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLastError() -> u32;
    }
}

/// 标准 base64（`+/` 表、要填充），与生产解码器的严格度对齐。
#[cfg(windows)]
fn base64_std(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for c in input.chunks(3) {
        let (b0, b1, b2) = (c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0));
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if c.len() > 1 {
            T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(windows)]
fn dpapi_protect_b64(plain: &[u8]) -> String {
    base64_std(&dpapi_test_ffi::protect(plain))
}
