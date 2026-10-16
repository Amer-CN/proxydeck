//! P8b 第二片门禁（二）：quota 差分对照 —— 与**真 Go 采收**的 `golden-p8b-quota-go.json` 逐字段比对。
//!
//! 基准由 Go 侧一次性采集器现采（`internal/tuanjie/zz_p8b_quota_collect_test.go`，跑完即删、
//! 留档 `.work/p8b/`；采集口径见 `tests/golden/README.md`）。加载顺序：
//! 先入库副本 `tests/golden/`，缺则回落 `.work/p8b/`。
//!
//! **两段口径**：
//! 1. **纯函数/形态**（缓存键、TTL 常量、请求头、JSON 逐字节）——直接比黄金，离线可跑；
//! 2. **行为对照**（live → cache → force → 503 → 负缓存 → 回退旧值 → 坏 JSON）——
//!    Rust 侧起一个**假计费上游**（按 token 分派应答序列），逐用例断言 `source`/`err`/`body`
//!    与 Go 采收值一致，并断言「上游命中次数」逐例相同（缓存语义的核心证据）。
//!
//! 纪律：假上游只监听 127.0.0.1 的临时端口；**不碰 8788、不出网、不读真实凭据**。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use rs_tuanjie::quota::{
    quota_cache_key, Quota, QuotaCache, QuotaEndpoint, QUOTA_ERR_TTL, QUOTA_TTL,
};

// ---------------------------------------------------------------- 基准加载

fn golden_dir() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    if vendored.join("golden-p8b-quota-go.json").exists() {
        return vendored;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".work/p8b")
}

fn load_golden() -> Value {
    let p = golden_dir().join("golden-p8b-quota-go.json");
    let bytes = std::fs::read(&p).unwrap_or_else(|e| {
        panic!(
            "读不到 P8b quota 黄金基准 {}: {e}（再生成见 tests/golden/README.md）",
            p.display()
        )
    });
    eprintln!("P8b quota 黄金基准: {}", p.display());
    serde_json::from_slice(&bytes).expect("黄金基准是合法 JSON")
}

// ---------------------------------------------------------------- 假计费上游

#[derive(Default)]
struct FakeState {
    /// 合成 token（Authorization **头全值**，如 `"Bearer sk-..."`）→ 应答体序列。
    /// 按全值分键是故意的：差分门禁比的就是两侧送出的头全值逐字节一致；
    /// 若上游只认 token 段，两侧同错会一起绿——但出站头集合对照（P8a 门禁）已独立锁住
    /// 头全值，故此处无叠加风险。
    seq: BTreeMap<String, Vec<String>>,
    /// 同上 → 状态码序列
    status: BTreeMap<String, Vec<i32>>,
    /// 同上 → 已命中次数（用例用它做「上游是否被打」的断言）
    hits: BTreeMap<String, usize>,
    /// (路径, 方法, authorization, accept, user-agent) 形态记录
    seen: Vec<(String, String, String, String, String)>,
}

struct FakeUpstream {
    addr: String,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<Mutex<bool>>,
}

impl FakeUpstream {
    fn start(state: Arc<Mutex<FakeState>>) -> FakeUpstream {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
        let addr = listener.local_addr().unwrap().to_string();
        listener
            .set_nonblocking(true)
            .expect("nonblocking 便于停机");
        let stop = Arc::new(Mutex::new(false));
        let stop2 = stop.clone();
        let state_for_thread = state.clone();
        std::thread::spawn(move || {
            let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
            loop {
                if *stop2.lock().unwrap() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let st = state_for_thread.clone();
                        // 每连接一个线程：reqwest 会复用连接，逐连接循环读请求
                        workers.push(std::thread::spawn(move || serve_conn(stream, st)));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        FakeUpstream { addr, state, stop }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn hits(&self, token: &str) -> usize {
        *self.state.lock().unwrap().hits.get(token).unwrap_or(&0)
    }

    fn seen(&self) -> Vec<(String, String, String, String, String)> {
        self.state.lock().unwrap().seen.clone()
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
    }
}

fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<FakeState>>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or(());
    let mut buf = [0u8; 4096];
    loop {
        // 读一个请求（只要头；计费接口是 GET，无体）
        let mut head = Vec::new();
        loop {
            let n = match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => n,
                Err(_) => return,
            };
            head.extend_from_slice(&buf[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head).to_string();
        let mut lines = text.split("\r\n");
        let first = lines.next().unwrap_or_default();
        let mut parts = first.split(' ');
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut auth = String::new();
        let mut accept = String::new();
        let mut ua = String::new();
        for line in lines {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            match k.as_str() {
                "authorization" => auth = v,
                "accept" => accept = v,
                "user-agent" => ua = v,
                _ => {}
            }
        }
        let (status, body) = {
            let mut st = state.lock().unwrap();
            st.seen.push((path.clone(), method, auth.clone(), accept, ua));
            let n = st.hits.entry(auth.clone()).or_insert(0);
            let idx = *n;
            *n += 1;
            let body = match st.seq.get(&auth) {
                Some(seq) if !seq.is_empty() => {
                    seq[idx.min(seq.len() - 1)].clone()
                }
                _ => String::new(),
            };
            let status = match st.status.get(&auth) {
                Some(list) if !list.is_empty() => list[idx.min(list.len() - 1)],
                _ => 200,
            };
            (status, body)
        };
        let resp = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            body.len(),
            body
        );
        eprintln!(
            "[假上游] auth={} path={path} status={status} body_len={}",
            &auth[..auth.len().min(34)],
            body.len()
        );
        if stream.write_all(resp.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

// ---------------------------------------------------------------- 工具

/// 剥掉黄金里的 `Bearer ` 前缀（Rust 侧 `fetch_quota_with_token` 自己拼前缀）。
fn strip_bearer(v: &str) -> &str {
    v.strip_prefix("Bearer ").unwrap_or(v)
}

/// **官网原始报文**（`/api/user/billing/usage/summary` 的响应形态）——与 Go 采集器
/// `zz_p8b_quota_collect_test.go` 里的 `zzQuotaOKBody` 逐字同源（含缩进）。
/// points 类是字符串、token 类是数字，`by_source` 刻意给两条同名 label 供合并验证。
fn raw_summary_body() -> String {
    [
        r#"{"billing":{"effective_available_points":1234.5},"#,
        r#""daily_allowance":{"quota_points":"10000.000000000","used_points":"10.500000000","remaining_points":"9989.500000000","period_end_at":"2099-01-01T08:00:00Z"},"#,
        r#""gift_credits":{"remaining_points":"500.250000000"},"#,
        r#""coding_plan":{"effective_remaining_points":"9989.500000000","next_boundary_at":"2099-01-02T00:00:00+08:00","#,
        r#""windows":[{"window_type":"usage_5h","quota_points":"800","used_points":"1","remaining_points":"799","exhausted":false,"period":{"end_at":"2099-01-01T13:00:00+08:00"}},"#,
        r#"{"window_type":"subscription_month","quota_points":"16000","used_points":"6000","remaining_points":"10000","exhausted":false,"period":{"end_at":"2099-02-01T00:00:00+08:00"}}]},"#,
        r#""period":{"start_date":"2099-01-01","end_date":"2099-01-31"},"#,
        r#""totals":{"recorded_points":42.5,"settlement_count":7,"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"#,
        r#""by_source":[{"label":"codely-core","recorded_points":10,"settlement_count":2,"total_tokens":50},"#,
        r#"{"label":"codely-core","recorded_points":5,"settlement_count":1,"total_tokens":30},"#,
        r#"{"label":"kimi-k3","recorded_points":27.5,"settlement_count":4,"total_tokens":40}]}}"#,
    ]
    .concat()
}

fn case<'a>(g: &'a Value, name: &str) -> &'a Value {
    g["cases"]
        .as_array()
        .expect("cases 是数组")
        .iter()
        .find(|c| c["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("黄金里没有用例 {name}"))
}

fn case_body(g: &Value, name: &str) -> String {
    case(g, name)["body"].as_str().unwrap().to_string()
}

/// 黄金 `upstream` 段里出现过的 token（Authorization 全值）。
fn upstream_token(g: &Value, name: &str) -> String {
    case(g, name)["upstream"][0]["auth"]
        .as_str()
        .unwrap_or_else(|| panic!("{name} 没有 upstream 记录"))
        .to_string()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
}

async fn fetch(
    cache: &QuotaCache,
    ep: &QuotaEndpoint,
    cl: &reqwest::Client,
    token: &str,
    force: bool,
) -> Quota {
    rs_tuanjie::quota::fetch_quota_cached(cache, cl, ep, token, force).await
}

/// 稳态断言：`source` / `err` / JSON 逐字节，加「**本步**的上游命中次数」。
///
/// **err 的比对口径（已知差异，不静默）**：错误文案里含各自的 JSON 库措辞——
/// Go 是 `encoding/json`（`invalid character 'o' in literal null (expecting 'u')`），
/// Rust 是 `serde_json`（`expected ident at line 1 column 2`）。两侧的**前缀**
/// （`官网积分接口响应解析失败: `）逐字一致，措辞必然不同 ⇒
/// 比法是「前缀逐字 + 两侧都非空 + **把 err 值替换成 `<ERR>` 后整体逐字节比**」，
/// 这样除措辞以外的每一个字段仍然逐字节对照。差异在 `README.md` 差异清单登记
/// （与 P5c「差异 16 标准库 JSON 措辞」同族）。
fn assert_matches(golden_case: &Value, got: &Quota, hits: usize) {
    let name = golden_case["name"].as_str().unwrap();
    assert_eq!(
        got.source,
        golden_case["source"].as_str().unwrap(),
        "[{name}] source 与 Go 不一致"
    );
    let want_err = golden_case["err"].as_str().unwrap_or("");
    let prefix = |s: &str| s.split(": ").next().unwrap_or(s).to_string();
    assert_eq!(
        prefix(&got.err),
        prefix(want_err),
        "[{name}] err 前缀与 Go 不一致（全文：got={:?} want={:?}）",
        got.err,
        want_err
    );
    if !want_err.is_empty() {
        assert!(!got.err.is_empty(), "[{name}] Go 有错而 Rust 没报错");
    }
    assert_eq!(
        blank_err(&got.to_go_json(), &got.err),
        blank_err(golden_case["body"].as_str().unwrap(), want_err),
        "[{name}] 响应体与 Go 不逐字节一致（err 值已归一成 <ERR>）"
    );
    if let Some(up) = golden_case["upstream"].as_array() {
        assert_eq!(
            hits,
            up.len(),
            "[{name}] 本步上游命中次数与 Go 不一致（缓存语义的核心证据）"
        );
    }
}

/// 把 JSON 里的 `"err":"…"` 值换成占位符（值本身的措辞两侧必然不同，见上）。
fn blank_err(body: &str, err: &str) -> String {
    if err.is_empty() {
        return body.to_string();
    }
    // JSON 里的形态是 `"err":"<转义后的 err>"`
    let mut quoted = String::from("\"err\":\"");
    for ch in err.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    body.replace(&quoted, "\"err\":\"<ERR>\"")
}

// ---------------------------------------------------------------- 1. 纯函数 / 形态

#[test]
fn cache_key_ttl_and_request_shape_match_go() {
    let g = load_golden();
    let meta = &g["meta"];

    // 缓存键：sha256(token)[:8] 的十六进制，逐值比对
    for (name, want) in g["cache_keys"].as_object().unwrap() {
        let token = match name.as_str() {
            "live" => meta["tokens"]["live"].as_str().unwrap(),
            "viewA" => meta["tokens"]["viewA"].as_str().unwrap(),
            "empty" => "",
            other => panic!("未知的 cache_keys 条目 {other}"),
        };
        assert_eq!(
            &quota_cache_key(token),
            want.as_str().unwrap(),
            "[{name}] 缓存键与 Go 不一致"
        );
    }

    // TTL 常量
    assert_eq!(meta["quota_ttl"].as_str().unwrap(), "5m0s");
    assert_eq!(meta["quota_err_ttl"].as_str().unwrap(), "1m0s");
    assert_eq!(QUOTA_TTL, Duration::from_secs(300));
    assert_eq!(QUOTA_ERR_TTL, Duration::from_secs(60));

    // 请求形态与端点：路径 / Accept / UA / Host
    let shape = case(&g, "request-shape");
    let up = &shape["upstream"][0];
    assert_eq!(up["method"].as_str().unwrap(), "GET");
    assert_eq!(
        up["path"].as_str().unwrap(),
        "/api/user/billing/usage/summary"
    );
    assert_eq!(up["accept"].as_str().unwrap(), "application/json", "Accept 头");
    assert_eq!(
        meta["user_agent"].as_str().unwrap(),
        up["ua"].as_str().unwrap(),
        "UA 取自黄金（本机 npm 版本）"
    );
    assert_eq!(
        up["host"].as_str().unwrap(),
        "codely.tuanjie.cn",
        "Host 头（Go 侧）"
    );
}

#[test]
fn quota_json_shape_matches_go_byte_for_byte() {
    let g = load_golden();
    // 用真 Go 采的 body 反解成 Rust 结构，再用 Rust 写出——字节必须一致（字段序 + 浮点形态）
    for name in [
        "live",
        "http-503",
        "bad-json",
        "view-not-in-pool",
        "stale-fallback-fail",
    ] {
        let body = case_body(&g, name);
        let q = Quota::from_go_json(&body).unwrap_or_else(|| panic!("{name} 的体解不开"));
        assert_eq!(q.to_go_json(), body, "[{name}] Quota 的 JSON 形态与 Go 不一致");
    }
}

// ---------------------------------------------------------------- 2. 行为对照（假上游）

#[tokio::test]
async fn quota_fetch_sequence_matches_go() {
    let g = load_golden();

    // 假上游的应答序列：与 Go 采集器逐条同源。
    // ⚠ 上游应给**官网原始报文**（snake_case 的那种），黄金里的 `body` 是 **Go 提取后的
    // Quota JSON**——两者不能混（第一版就栽在这儿：拿黄金 body 喂上游，全部字段落成零）。
    // 原始报文在 `raw_summary_body()` 里，与采集器里的常量逐字同源。
    let ok_body = raw_summary_body();
    let mut st = FakeState::default();
    let live_key = g["cache_keys"]["live"].as_str().unwrap().to_string();
    let key_of = |name: &str| {
        let auth = upstream_token(&g, name);
        let tok = strip_bearer(&auth);
        let computed = quota_cache_key(tok);
        if name == "live" {
            assert_eq!(computed, live_key, "[live] 黄金记的 token 与 Rust 复算的键对不上");
        }
        assert_eq!(
            auth,
            format!("Bearer {tok}"),
            "[{name}] 黄金里的 Authorization 必须是 `Bearer ` + token"
        );
        (auth, computed)
    };
    let (k_live, _) = key_of("live");
    let (k_503, c_503) = key_of("http-503");
    let (k_stale, c_stale) = key_of("stale-fallback-live");
    let (k_bad, c_bad) = key_of("bad-json");
    let (k_shape, c_shape) = key_of("request-shape");
    assert_eq!(
        [&c_503, &c_stale, &c_bad, &c_shape]
            .iter()
            .filter(|k| **k == &live_key)
            .count(),
        0,
        "每个用例必须是独立 token（否则会撞上 5 分钟缓存，测的就不是同一件事了）"
    );
    st.seq.insert(k_live.clone(), vec![ok_body.clone()]);
    st.seq
        .insert(k_503.clone(), vec![r#"{"msg":"boom"}"#.to_string()]);
    st.status.insert(k_503.clone(), vec![503]);
    st.seq
        .insert(k_stale.clone(), vec![ok_body.clone(), "{}".to_string()]);
    st.status.insert(k_stale.clone(), vec![200, 500]);
    st.seq
        .insert(k_bad.clone(), vec!["not json at all".to_string()]);
    st.seq.insert(k_shape.clone(), vec![ok_body.clone()]);
    // 送进 fetch 的是**裸 token**（etch_quota_with_token 自己拼 Bearer  前缀）
    let t_live = strip_bearer(&k_live).to_string();
    let t_503 = strip_bearer(&k_503).to_string();
    let t_stale = strip_bearer(&k_stale).to_string();
    let t_bad = strip_bearer(&k_bad).to_string();
    let t_shape = strip_bearer(&k_shape).to_string();
    let state = Arc::new(Mutex::new(st));
    let up = FakeUpstream::start(state);
    let ep = QuotaEndpoint {
        url: up.url("/api/user/billing/usage/summary"),
        ..Default::default()
    };
    let cl = client();
    let cache = QuotaCache::new();

    // ---------------------------------------------------------------- 逐用例对照
    // 每一例都比「**本步**的上游命中次数」（= 黄金 `upstream` 段的长度），
    // 与 Go 采集器的 before/after 取法一致——比累计值会越比越错。
    macro_rules! case_step {
        ($name:literal, $token:expr, $key:expr, $force:expr) => {{
            let before = up.hits(&$key);
            let q = fetch(&cache, &ep, &cl, &$token, $force).await;
            let delta = up.hits(&$key) - before;
            assert_matches(case(&g, $name), &q, delta);
        }};
    }
    case_step!("live", t_live, k_live, false);
    case_step!("cache", t_live, k_live, false);
    case_step!("force", t_live, k_live, true);
    case_step!("http-503", t_503, k_503, false);
    case_step!("err-cache", t_503, k_503, false);
    case_step!("stale-fallback-live", t_stale, k_stale, false);
    case_step!("stale-fallback-fail", t_stale, k_stale, true);
    case_step!("bad-json", t_bad, k_bad, false);
    case_step!("request-shape", t_shape, k_shape, false);

    // 上游看到的请求形态逐条比：路径 / 方法 / Authorization / Accept / UA
    let want = case(&g, "request-shape")["upstream"][0].clone();
    let seen = up.seen();
    let last = seen.last().expect("至少有一条记录");
    assert_eq!(last.0, want["path"].as_str().unwrap(), "路径");
    assert_eq!(last.1, want["method"].as_str().unwrap(), "方法");
    assert_eq!(last.2, want["auth"].as_str().unwrap(), "Authorization");
    assert_eq!(last.3, want["accept"].as_str().unwrap(), "Accept");
    assert_eq!(last.4, want["ua"].as_str().unwrap(), "User-Agent");

    // 视图层的两条固定文案（handleQuota 的分支）
    assert_eq!(
        rs_tuanjie::quota::QuotaView::not_in_pool().to_go_json(),
        case_body(&g, "view-not-in-pool"),
        "「账号不在池中」文案"
    );
    let tok_err = upstream_token(&g, "view-uid-a-live"); // 仅用于证明该基准来自真实采集
    assert!(tok_err.starts_with("Bearer "));
    assert_eq!(
        rs_tuanjie::quota::QuotaView::token_error("读取失败：disk on fire").to_go_json(),
        r#"{"total":0,"daily":{"quota":0,"used":0,"remaining":0},"gift":{"remaining":0},"month":{"points":0,"totalTokens":0,"promptTokens":0,"completionTokens":0,"requests":0},"plan":{"effective_remaining":0},"source":"none","err":"读取 token 失败: 读取失败：disk on fire"}"#,
        "取 token 失败文案（Go: \"读取 token 失败: \" + err）"
    );
}

