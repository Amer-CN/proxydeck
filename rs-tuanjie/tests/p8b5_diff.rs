//! P8b 第五片差分对照：与**真 Go 采收**的 `tests/golden/golden-p8b5-go.json` 逐条比对。
//!
//! 覆盖面（与 Go 采集器 `zz_p8b5_collect_test.go` 同一份场景定义，见金标的 `scenario`）：
//!
//! * **媒体改路由 / 远程图片预下载**：含图请求改写 model、识图模型与用户扩展清单放行、
//!   远程 URL 图片转 base64（下游响应 + 出站体逐字节）。
//!   **2026-10-18 更新**：改路由与生图/生视频/媒体配置已按用户裁决删除，对应 Go 金标
//!   场景**有意跳过**（见 `REMOVED_MEDIA_SCENARIOS`）；保留并继续覆盖三条：
//!   「多模态模型直收」一条行为不变；两条远程图片场景按 [`scenario_step`] 把**输入**
//!   模型改成 `codely-vl`（金标出站体本来就是这个模型），继续覆盖
//!   `fetch_remote_images` 的下载 → data URI 成功分支。
//! * **外部 provider 三条转发路径**（OpenAI 兼容 chat / `/responses` / `/v1/messages`）与
//!   **`/providers` 管理端点**：**本版有意删除**（2026-10-18 用户裁决，自定义服务商整体
//!   下线），对应 Go 金标场景全部跳过（见 `REMOVED_PROVIDER_SCENARIOS` 与 README 差异 #47）。
//! * ~~`/media-config` + `/vision-config`~~、~~`/v1/images/generations` + `/v1/videos` +
//!   `/v1/videos/{id}` + `/agnesapi`~~：**本版有意删除**（端点 404、改动见 README 差异表）。
//! * **探针**：`probeCall` 的 max_tokens 自适应名单（出站体逐字节）+ 整轮
//!   `RunPipelineProbes` 的结果列表。
//! * **基线内存态**：同一份 `tuanjie-baselines.json` fixture 下的加载/迁移/回写字节。
//!
//! ## 归一化（**只抹掉每请求随机的会话层与地址**，不抹形状）
//!
//! 1. `https?://127.0.0.1:<port>` → `<UP>`（两侧假上游端口不同）；
//! 2. `"id":"chatcmpl-<…>"` → `"id":"chatcmpl-<ID>"`（每请求随机 rid）；
//! 3. `"created":<数字>` → `"created":<TS>`（`time.Now().Unix()`）；
//! 4. **团结域出站体**里的会话层键（`metadata`/`litellm_session_id`/`prompt_cache_key`，
//!    每请求随机 uuid、`cwd` 还是各自进程的 exe 目录）先摘掉再**规范化比对**；
//!    这些键的逐字节合同已由 P8a / P8b 第三片的 `go-outbound-wire` 金标覆盖。
//!    （外部 provider 的 responses/anthropic 转换体已随自定义服务商删除。）
//!
//! ## 纪律
//!
//! 合成账号 + 假上游（回环临时端口）+ 临时数据目录；**不碰 8788**、不读真实凭据与真实
//! 数据目录。进程级环境变量（`USERPROFILE`/`PROXYDECK_DATA_DIR`）全程持一条锁串行。

#![allow(clippy::type_complexity)]

#[path = "p8b3_scenario/mod.rs"]
mod scenario;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use rs_tuanjie::server::{router, AppState};

// ---------------------------------------------------------------- 金标结构

#[derive(serde::Deserialize, Debug, Clone)]
struct ReplyIn {
    code: u16,
    /// Go 的 `map[string]string` 为 nil 时 marshal 成 `null` —— 用 `Option` 接住。
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    body: String,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct StepIn {
    name: String,
    #[serde(default)]
    method: String,
    path: String,
    #[serde(default)]
    body: String,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct ScenarioIn {
    name: String,
    /// Go 的 nil 切片 marshal 成 `null`。
    #[serde(default)]
    accounts: Option<Vec<String>>,
    #[serde(default)]
    script: Option<BTreeMap<String, Vec<ReplyIn>>>,
    #[serde(default)]
    steps: Option<Vec<StepIn>>,
}

impl ScenarioIn {
    fn accounts(&self) -> &[String] {
        self.accounts.as_deref().unwrap_or(&[])
    }
    fn steps(&self) -> &[StepIn] {
        self.steps.as_deref().unwrap_or(&[])
    }
    fn script(&self) -> BTreeMap<String, Vec<ReplyIn>> {
        self.script.clone().unwrap_or_default()
    }
}

#[derive(serde::Deserialize, Debug, Clone, PartialEq)]
struct SeenIn {
    method: String,
    path: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    accept: String,
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    opencode_session: bool,
    #[serde(default)]
    ua: String,
    #[serde(default)]
    body: String,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct CaseRec {
    name: String,
    status: u16,
    content_type: String,
    body: String,
    #[serde(default)]
    upstream: Vec<SeenIn>,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct ProbeRec {
    model: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    err_text: String,
    #[serde(default)]
    prompt: i64,
    #[serde(default)]
    finish: String,
    #[serde(default)]
    err: String,
    #[serde(default)]
    has_sign: bool,
    #[serde(default)]
    has_session: bool,
    #[serde(default)]
    ua: String,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct BaselineDump {
    keys: Vec<String>,
    dump: serde_json::Value,
    file: String,
    fixture: String,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct Golden {
    scenario: Vec<ScenarioIn>,
    cases: Vec<CaseRec>,
    probes: Vec<ProbeRec>,
    pipeline: Vec<serde_json::Value>,
    baseline: BaselineDump,
}

/// 金标路径：**先 `rs-tuanjie/tests/golden/`，缺则回落 `.work/p8b5/`**（与其余片同惯例）。
fn golden_path() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("golden-p8b5-go.json");
    if vendored.exists() {
        return vendored;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".work")
        .join("p8b5")
        .join("golden-p8b5-go.json")
}

fn golden() -> Golden {
    let p = golden_path();
    let raw = std::fs::read(&p)
        .unwrap_or_else(|e| panic!("读金标 {} 失败：{e}（先跑 Go 采集器）", p.display()));
    serde_json::from_slice(&raw).expect("金标是合法 JSON")
}

// ---------------------------------------------------------------- 归一化

/// `https?://127.0.0.1:<port>` 与 `localhost:<port>` → `<UP>`。
fn mask_up(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let rest = &s[i..];
        let host_len = if rest.starts_with("http://127.0.0.1:")
            || rest.starts_with("http://localhost:")
        {
            Some("http://".len())
        } else if rest.starts_with("https://127.0.0.1:") || rest.starts_with("https://localhost:") {
            Some("https://".len())
        } else {
            None
        };
        if let Some(host_start) = host_len {
            // 吃掉 host（数字与点）与 `:port`
            let bytes = s.as_bytes();
            let mut j = i + host_start;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b':' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
            }
            out.push_str("<UP>");
            i = j;
            continue;
        }
        let ch = s[i..].chars().next().expect("非空");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `"id":"chatcmpl-<…>"` → `"id":"chatcmpl-<ID>"`。
fn mask_chunk_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find("\"id\":\"chatcmpl-") {
        let start = idx + "\"id\":\"chatcmpl-".len();
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail.find('"').unwrap_or(tail.len());
        out.push_str("<ID>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `"created":<数字>` → `"created":<TS>`。
fn mask_created(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find("\"created\":") {
        let start = idx + "\"created\":".len();
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        if end == 0 {
            out.push_str(&tail[..1]);
            rest = &tail[1..];
            continue;
        }
        out.push_str("<TS>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// 生成的 tool_call id（`call_` + 12 位小写十六进制）归一化。
///
/// `next_rid` 是**进程级全局计数器**：删掉媒体场景后，同一条 `ext-chat-sanitize`
/// 用例拿到的序号必然变化（本次实测 Go=…0b / Rust=…06）。序号不是行为合同——
/// 12 位零填充小写十六进制的形态由 `inflight::rid_from_n` 的单测锁死——故这里归一化。
fn mask_rid(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        if s[i..].starts_with("call_") {
            let rest = &s[i + 5..];
            let hex: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || ('a'..='f').contains(c))
                .collect();
            if hex.len() == 12 {
                out.push_str("call_<RID>");
                i += 5 + 12;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 下游响应体的归一化（见文件头「归一化」1~3）。
fn norm_downstream(s: &str) -> String {
    mask_rid(&mask_created(&mask_chunk_id(&mask_up(s))))
}

/// 团结域出站体的会话层键（每请求随机）。
const VOLATILE_KEYS: [&str; 3] = ["metadata", "litellm_session_id", "prompt_cache_key"];

/// 出站体归一化：含会话层键时摘掉再规范化比对，否则**逐字节**比对。
fn norm_upstream_body(s: &str) -> (String, bool) {
    let masked = mask_rid(&mask_up(s));
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&masked) else {
        return (masked, false);
    };
    let Some(obj) = v.as_object() else {
        return (masked, false);
    };
    if !VOLATILE_KEYS.iter().any(|k| obj.contains_key(*k)) {
        return (masked, false);
    }
    let mut out = obj.clone();
    for k in VOLATILE_KEYS {
        out.remove(k);
    }
    (
        serde_json::to_string(&serde_json::Value::Object(out)).unwrap_or(masked),
        true,
    )
}

/// 数字统一成 Go 的浮点文本形态（`177` ↔ `177.0` 视为同一个值）。
///
/// 为什么需要：Go 的 `json.Marshal(float64(177))` 写 `177`，`serde_json` 写 `177.0`——
/// 值相等、文本不同。**文件字节比对**仍走各自的原生写出器（`to_bytes()` 用 Go 口径），
/// 这里只用于「内存态条目」的值级比对。
fn norm_numbers(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Number(n) => serde_json::Value::String(rs_tuanjie::gojson::format_go_float(
            n.as_f64().unwrap_or(0.0),
        )),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(norm_numbers).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), norm_numbers(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn norm_seen(list: &[SeenIn]) -> (Vec<SeenIn>, usize) {
    let mut canonical = 0usize;
    let out = list
        .iter()
        .map(|s| {
            let (body, canon) = norm_upstream_body(&s.body);
            if canon {
                canonical += 1;
            }
            SeenIn {
                body,
                path: mask_up(&s.path),
                auth: s.auth.clone(),
                ..s.clone()
            }
        })
        .collect();
    (out, canonical)
}

// ---------------------------------------------------------------- 运行时

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn run_locked<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 current-thread 运行时");
    let out = rt.block_on(f());
    drop(guard);
    out
}

/// 本文件共用的假上游（`client::set_test_bases` 是进程级全局）。
fn shared_upstream() -> &'static scenario::FakeUpstream {
    static UP: OnceLock<scenario::FakeUpstream> = OnceLock::new();
    UP.get_or_init(|| scenario::FakeUpstream::start(vec![]))
}

/// 假上游的 `/img/fetch.png` 字节 —— **与 Go 采集器同一串**（含 0x89 等非 UTF-8 字节，
/// 两侧都应吐出同一个 data URI）。
const FAKE_IMAGE: [u8; 12] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];

struct Outcome {
    status: u16,
    content_type: String,
    body: String,
    upstream: Vec<SeenIn>,
}

#[derive(Default)]
struct DiffStats {
    steps: usize,
    upstream_byte_exact: usize,
    upstream_canonical: usize,
    failures: Vec<String>,
}

impl DiffStats {
    fn fail(&mut self, msg: String) {
        self.failures.push(msg);
    }
}

// ---------------------------------------------------------------- 主用例

#[test]
fn p8b5_diff_against_go_golden() {
    let g = golden();
    let failures = run_locked(|| async { run_all(g).await });
    assert!(
        failures.is_empty(),
        "P8b 第五片差分对照失败 {} 条：\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// 2026-10-18 用户裁决删除的 Go 金标场景：生图/生视频、三路媒体配置、识图改路由。
/// 这些端点在 Rust 侧已不注册、`codely-core` + 图片已改为 400 ——
/// **有意差异，非漂移**（README 差异表 #46）。
///
/// 注意：`media-fetch-remote-image` / `-sniff` **不在此列**——它们覆盖的是**保留**的
/// `fetch_remote_images` 下载成功分支；做法见 [`scenario_step`]（输入模型改成
/// `codely-vl`，与金标出站体本来就一致的 `codely-vl` 对齐）。
const REMOVED_MEDIA_SCENARIOS: [&str; 7] = [
    "media-reroute-image-to-vision",
    "media-no-reroute-extra-capable",
    "media-reroute-configured-vision",
    "media-config-get-post",
    "images-generations",
    "images-generations-rewrite-and-errors",
    "videos-create-and-query",
];

/// 2026-10-18 用户裁决删除的 Go 金标场景：自定义服务商（三协议转发 + `/providers` 管理）。
///
/// 逐条核对过 golden，这些场景**只**覆盖被删功能：
/// * `ext-chat-*` / `ext-chat-unormalized-protocol`：出站 OpenAI 兼容 `/chat/completions`
///   （鉴权 Bearer 静态 key、`tool_call_id` 修复、4xx 透传 / 5xx→空 200）；
/// * `responses-*`：`/responses` 出站转换与翻回（含流式与异常形态）；
/// * `anthropic-*`：`/v1/messages` 出站转换与翻回（含流式与 5xx）；
/// * `providers-*`：`/providers` 管理端点（GET / 六个 action / 错误形态 / 405）。
///
/// 其中没有一条同时覆盖保留功能（远程图片下载、chat 主路径、探针、基线），故整条跳过；
/// README 差异 #47 登记为**有意差异，非漂移**。
const REMOVED_PROVIDER_SCENARIOS: [&str; 16] = [
    "ext-chat-200",
    "ext-chat-stream",
    "ext-chat-400-passthrough",
    "ext-chat-500-empty-200",
    "ext-chat-sanitize-toolids",
    "ext-chat-unormalized-protocol",
    "responses-nonstream",
    "responses-stream",
    "responses-unexpected-shape",
    "responses-400-passthrough",
    "anthropic-nonstream",
    "anthropic-stream",
    "anthropic-500-empty-200",
    "providers-get-cold",
    "providers-post-actions",
    "providers-models-error-shapes",
];

/// 金标场景输入的就地修正（只改**输入**，不改期望）。
///
/// 两条远程图片场景的 Go 金标里，出站体与响应本来就是 `"model":"codely-vl"`
/// （旧行为把 `codely-core` 改路由成 `codely-vl`）。Rust 的改路由已删，若继续用
/// `codely-core` 会命中新的 400 分支；把输入模型换成 `codely-vl` 后，Go 侧
/// （vision-capable，不改写）与 Rust 侧行为一致，而 `fetch_remote_images` 的
/// 下载 → data URI 成功分支仍被完整覆盖。
fn scenario_step(name: &str, step: &StepIn) -> StepIn {
    let mut out = step.clone();
    if matches!(
        name,
        "media-fetch-remote-image" | "media-fetch-remote-image-sniff"
    ) {
        out.body = out
            .body
            .replace("\"model\":\"codely-core\"", "\"model\":\"codely-vl\"");
    }
    out
}

async fn run_all(g: Golden) -> Vec<String> {
    let up = shared_upstream();
    let mut stats = DiffStats::default();
    let upstream_base = up.base();

    let by_name: BTreeMap<String, CaseRec> = g
        .cases
        .iter()
        .map(|c| (c.name.clone(), c.clone()))
        .collect();

    // ---------------- HTTP 场景
    for sc in &g.scenario {
        // 已删除功能对应的 Go 金标：跳过（有意差异，见常量注释与 README #46 / #47）。
        if REMOVED_MEDIA_SCENARIOS.contains(&sc.name.as_str())
            || REMOVED_PROVIDER_SCENARIOS.contains(&sc.name.as_str())
        {
            continue;
        }
        // 一个场景 = 一个隔离环境 + 一个 AppState + 一个真 HTTP 服务；
        // 场景内所有步骤**共享状态**（媒体场景的跨步骤语义靠这一点才可比）。
        let sess = ScenarioRun::start(up, sc).await;
        for step in sc.steps() {
            let step = scenario_step(&sc.name, step);
            let got = sess.step(up, &step, &upstream_base).await;
            let name = format!("{}/{}", sc.name, step.name);
            let Some(want) = by_name.get(&name) else {
                stats.fail(format!("[{name}] 金标里没有这条用例"));
                continue;
            };
            stats.steps += 1;
            if got.status != want.status {
                stats.fail(format!(
                    "[{name}] status 分歧：Go={} Rust={}",
                    want.status, got.status
                ));
            }
            if got.content_type != want.content_type {
                stats.fail(format!(
                    "[{name}] content-type 分歧：Go={:?} Rust={:?}",
                    want.content_type, got.content_type
                ));
            }
            let gb = norm_downstream(&want.body);
            let rb = norm_downstream(&got.body);
            if gb != rb {
                stats.fail(format!(
                    "[{name}] 响应体分歧\n  Go  ={gb}\n  Rust={rb}"
                ));
            }
            // 上游观测：路径/方法/认证/头 + 出站体
            let (want_up, want_canon) = norm_seen(&want.upstream);
            let (got_up, got_canon) = norm_seen(&got.upstream);
            stats.upstream_canonical += want_canon.max(got_canon);
            stats.upstream_byte_exact += want_up.len().saturating_sub(want_canon.max(got_canon));
            if want_up.len() != got_up.len() {
                stats.fail(format!(
                    "[{name}] 上游命中数分歧：Go={} Rust={}（Go={:?} Rust={:?}）",
                    want_up.len(),
                    got_up.len(),
                    want_up.iter().map(|s| s.path.clone()).collect::<Vec<_>>(),
                    got_up.iter().map(|s| s.path.clone()).collect::<Vec<_>>()
                ));
            }
            for (i, (w, r)) in want_up.iter().zip(got_up.iter()).enumerate() {
                if w.method != r.method || w.path != r.path {
                    stats.fail(format!(
                        "[{name}] 上游 #{} 请求行分歧：Go={} {} Rust={} {}",
                        i, w.method, w.path, r.method, r.path
                    ));
                }
                // 平台级头差异（已在 README 登记）：reqwest 对未显式设 Accept 的请求
                // 自动补 `*/*`，Go 的 `net/http` 不补。空 ↔ `*/*` 视为同一条。
                let accept_same = w.accept == r.accept || (w.accept.is_empty() && r.accept == "*/*");
                if w.auth != r.auth || w.api_key != r.api_key || !accept_same {
                    stats.fail(format!(
                        "[{name}] 上游 #{} 头分歧：Go(auth={:?} key={:?} accept={:?}) Rust(auth={:?} key={:?} accept={:?})",
                        i, w.auth, w.api_key, w.accept, r.auth, r.api_key, r.accept
                    ));
                }
                if w.content_type != r.content_type {
                    stats.fail(format!(
                        "[{name}] 上游 #{} content-type 分歧：Go={:?} Rust={:?}",
                        i, w.content_type, r.content_type
                    ));
                }
                if w.opencode_session != r.opencode_session {
                    stats.fail(format!(
                        "[{name}] 上游 #{} x-opencode-session 存在性分歧：Go={} Rust={}",
                        i, w.opencode_session, r.opencode_session
                    ));
                }
                // 出站体：无会话层键 ⇒ 逐字节；有 ⇒ 规范化（`norm_seen` 已处理，这里只比对）
                if w.body != r.body {
                    stats.fail(format!(
                        "[{name}] 上游 #{} 出站体分歧\n  Go  ={}\n  Rust={}",
                        i, w.body, r.body
                    ));
                }
            }
        }
    }

    // ---------------- 探针：max_tokens 自适应名单（出站体逐字节）
    for want in &g.probes {
        let got = run_probe_call(up, &want.model).await;
        if got.0 != want.body {
            stats.fail(format!(
                "[probe/{}] 出站体分歧\n  Go  ={}\n  Rust={}",
                want.model, want.body, got.0
            ));
        }
        if got.1.prompt_tokens != want.prompt || got.1.finish != want.finish {
            stats.fail(format!(
                "[probe/{}] 回读分歧：Go(prompt={} finish={:?}) Rust(prompt={} finish={:?})",
                want.model, want.prompt, want.finish, got.1.prompt_tokens, got.1.finish
            ));
        }
        if got.2 != want.has_sign || got.3 != want.has_session {
            stats.fail(format!(
                "[probe/{}] 签名/会话头存在性分歧：Go({}, {}) Rust({}, {})",
                want.model, want.has_sign, want.has_session, got.2, got.3
            ));
        }
        if got.4 != want.ua {
            stats.fail(format!(
                "[probe/{}] UA 分歧：Go={:?} Rust={:?}",
                want.model, want.ua, got.4
            ));
        }
        if got.1.err_text != want.err_text || got.1.err.clone().unwrap_or_default() != want.err {
            stats.fail(format!(
                "[probe/{}] 错误面分歧：Go(err_text={:?} err={:?}) Rust(err_text={:?} err={:?})",
                want.model,
                want.err_text,
                want.err,
                got.1.err_text,
                got.1.err.clone().unwrap_or_default()
            ));
        }
    }

    // ---------------- 探针：整轮 RunPipelineProbes
    let pipeline = run_pipeline(up).await;
    let want_pipeline: Vec<serde_json::Value> = g.pipeline.clone();
    let got_pipeline: Vec<serde_json::Value> = pipeline
        .iter()
        .map(|p| serde_json::to_value(p).expect("探针结果是 JSON"))
        .collect();
    if want_pipeline.len() != got_pipeline.len() {
        stats.fail(format!(
            "[pipeline] 探针条数分歧：Go={} Rust={}",
            want_pipeline.len(),
            got_pipeline.len()
        ));
    }
    for (w, r) in want_pipeline.iter().zip(got_pipeline.iter()) {
        if w != r {
            stats.fail(format!("[pipeline] 差异\n  Go  ={w}\n  Rust={r}"));
        }
    }

    // ---------------- 基线内存态
    {
        let store = rs_tuanjie::probes::BaselineStore::empty();
        store.load_from_bytes(g.baseline.fixture.as_bytes());
        let mut keys = store.keys();
        keys.sort();
        if keys != g.baseline.keys {
            stats.fail(format!(
                "[baseline] 键分歧：Go={:?} Rust={:?}",
                g.baseline.keys, keys
            ));
        }
        let mem = serde_json::json!({
            "baselines": norm_numbers(&serde_json::Value::Object(store.raw_dump())),
        });
        let want_mem = serde_json::json!({
            "baselines": norm_numbers(&g.baseline.dump),
        });
        if mem != want_mem {
            stats.fail(format!(
                "[baseline] 内存态分歧\n  Go  ={want_mem}\n  Rust={mem}"
            ));
        }
        let rust_file = String::from_utf8_lossy(&store.to_bytes()).into_owned();
        if rust_file != g.baseline.file {
            stats.fail(format!(
                "[baseline] 回写文件分歧\n  Go  ={}\n  Rust={}",
                g.baseline.file, rust_file
            ));
        }
    }

    println!(
        "P8b 第五片差分对照：{} 步 HTTP 用例（出站体逐字节 {} / 规范化 {}）+ {} 条探针体 + \
         1 轮探针 + 1 组基线；失败 {} 条",
        stats.steps,
        stats.upstream_byte_exact,
        stats.upstream_canonical,
        g.probes.len(),
        stats.failures.len()
    );
    stats.failures
}

// ---------------------------------------------------------------- 单场景执行

/// 一个场景的运行态：隔离环境 + AppState + 真 HTTP 服务（Drop 时收摊）。
struct ScenarioRun {
    root: PathBuf,
    addr: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
    client: reqwest::Client,
}

impl ScenarioRun {
    async fn start(up: &scenario::FakeUpstream, sc: &ScenarioIn) -> ScenarioRun {
    // 每个场景一个全新的隔离环境（临时 HOME + 临时数据目录）——与 Go 采集器的
    // 「每场景清空数据目录 + 复位媒体全局」等价。
    let root = std::env::temp_dir().join(format!(
        "rs-tuanjie-p8b5-{}-{}-{:?}",
        sc.name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建临时根目录");
    let sub = sc
        .accounts()
        .first()
        .cloned()
        .unwrap_or_else(|| "uid-p8b5-local".to_string());
    let env = scenario::Env::create_with_salt(&root, &sub, "p8b5");
    env.apply();
    let data = root.join("data");

    if !sc.accounts().is_empty() {
        let mut entries: Vec<String> = Vec::new();
        for (i, id) in sc.accounts().iter().enumerate() {
            let token = scenario::synthetic_jwt_for(id, "p8b5");
            let (source, at) = if i == 0 { ("local", "") } else { ("pool", token.as_str()) };
            entries.push(format!(
                "{{\"user_id\":\"{id}\",\"access_token\":\"{at}\",\"enabled\":true,\
                  \"has_glm53\":true,\"source\":\"{source}\"}}"
            ));
            up.set_token_key(&token, &format!("sk-key-{id}"));
        }
        std::fs::write(
            data.join("tuanjie-accounts.json"),
            format!("{{\"accounts\":[{}]}}", entries.join(",")),
        )
        .expect("写合成账号池");
    }

    up.reset_scripts();
    up.set_image_body(FAKE_IMAGE.to_vec());
    for (key, replies) in &sc.script() {
        let mapped: Vec<scenario::Reply> = replies
            .iter()
            .map(|r| scenario::Reply {
                code: r.code,
                headers: r
                    .headers
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                body: r.body.as_bytes().to_vec(),
            })
            .collect();
        if key.starts_with("sk-key-") {
            up.set_unity_script(key, mapped);
        } else {
            up.set_path_script(key, mapped);
        }
    }

    rs_tuanjie::client::set_test_bases(up.base(), up.base());
    let st = Arc::new(AppState::new().expect("构造 AppState"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑临时端口");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router(st)).await;
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("建 HTTP 客户端");
    ScenarioRun {
        root,
        addr,
        server,
        client,
    }
    }

    /// 跑一条步骤（响应 + 本步的上游观测）。
    async fn step(
        &self,
        up: &scenario::FakeUpstream,
        step: &StepIn,
        upstream_base: &str,
    ) -> Outcome {
        let before = up.seen().len();
        let method = if step.method.is_empty() {
            "POST".to_string()
        } else {
            step.method.clone()
        };
        let mut rb = self.client.request(
            method.parse().expect("方法名合法"),
            format!("http://{}{}", self.addr, step.path),
        );
        if !step.body.is_empty() {
            rb = rb
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                // `{UP}` = 假上游地址（场景定义里保留占位符，两侧各自替换成自己的端口）
                .body(step.body.replace("{UP}", upstream_base));
        }
        let resp = rb.send().await.expect("打本地服务");
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = String::from_utf8_lossy(&resp.bytes().await.expect("读响应体")).into_owned();

        let seen = up.seen();
        let upstream: Vec<SeenIn> = seen[before..]
            .iter()
            .map(|s| SeenIn {
                method: s.method.clone(),
                path: s.path.clone(),
                auth: s.authorization.clone(),
                api_key: s.api_key.clone(),
                accept: s.accept.clone(),
                content_type: s.content_type.clone(),
                opencode_session: s.opencode_session,
                ua: s.user_agent.clone(),
                body: s.body.clone(),
            })
            .collect();
        Outcome {
            status,
            content_type,
            body,
            upstream,
        }
    }
}

impl Drop for ScenarioRun {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 单次 `probeCall` 的观测：(出站体, 回读, 有签名头, 有会话头, UA)。
async fn run_probe_call(
    up: &scenario::FakeUpstream,
    model: &str,
) -> (String, rs_tuanjie::probes::ProbeCall, bool, bool, String) {
    up.reset_scripts();
    up.set_path_script(
        "/v1/chat/completions",
        vec![scenario::Reply::json(
            200,
            &format!(
                "{{\"model\":{model:?},\"choices\":[{{\"finish_reason\":\"stop\",\"message\":{{\"content\":\"ok\"}}}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":2}}}}"
            ),
        )],
    );
    let client = reqwest::Client::builder()
        .timeout(rs_tuanjie::probes::PROBE_TIMEOUT)
        .no_proxy()
        .build()
        .expect("建探针客户端");
    let target = rs_tuanjie::probes::ProbeTarget::tuanjie("sk-probe", &up.base());
    let cost = rs_tuanjie::probes::ProbeCost::new();
    let before = up.seen().len();
    let backoff = vec![std::time::Duration::from_millis(1); 3];
    let call = rs_tuanjie::probes::probe_call(
        &client,
        &target,
        model,
        &[serde_json::json!({"role": "user", "content": "a"})],
        None,
        &cost,
        &backoff,
        rs_tuanjie::client::now_unix(),
    )
    .await;
    let seen = up.seen();
    let body = seen[before..]
        .first()
        .map(|s| s.body.clone())
        .unwrap_or_default();
    let headers = target.request_headers("/v1/chat/completions", rs_tuanjie::client::now_unix());
    (
        body,
        call,
        headers.contains_key("X-Codely-Signature"),
        headers.contains_key("x-litellm-session-id"),
        seen[before..]
            .first()
            .map(|s| s.user_agent.clone())
            .unwrap_or_default(),
    )
}

async fn run_pipeline(up: &scenario::FakeUpstream) -> Vec<rs_tuanjie::probes::ProbeResult> {
    up.reset_scripts();
    up.set_pipeline_mode(true);
    let client = reqwest::Client::builder()
        .timeout(rs_tuanjie::probes::PROBE_TIMEOUT)
        .no_proxy()
        .build()
        .expect("建探针客户端");
    let target = rs_tuanjie::probes::ProbeTarget::tuanjie("sk-probe", &up.base());
    let cost = rs_tuanjie::probes::ProbeCost::new();
    let backoff = vec![std::time::Duration::from_millis(1); 3];
    let out = rs_tuanjie::probes::run_pipeline_probes(
        &client,
        &target,
        "codely-core",
        &cost,
        &backoff,
        rs_tuanjie::probes::RATE_LIMIT_STORM_THRESHOLD,
        rs_tuanjie::client::now_unix(),
    )
    .await;
    up.set_pipeline_mode(false);
    out
}
