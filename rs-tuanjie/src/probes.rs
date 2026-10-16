//! probes.rs —— 管道探针集 + 绝对基准库的**内存态**（`internal/tuanjie/probes.go` +
//! `baseline.go:26-233` 的加载/迁移/查询面）。
//!
//! ## 交付面
//!
//! * **探针集**：[`run_pipeline_probes`]（tokenizer ×4 双发一致性、错误文本 ×2、
//!   finish_reason ×2）、[`probe_call`]（单请求：429/5xx 退避重试 ≤3 次、
//!   状态码先于正文、错误原文脱敏、上游自报名回填）、
//!   **max_tokens 自适应名单**（思考型模型 `deepseek`/`glm-5`/`kimi`/`o1`/`o3`/`codely`
//!   给 96，其余 8）、限流风暴早停（429 累计 ≥6 即中止剩余请求）。
//! * **成本累加器**：[`ProbeCost`]（请求数 / prompt / completion / 429 计数，
//!   入口 `reset`、出口 `snapshot`——Go 是包级 atomic）。
//! * **基准库存内存态**：[`BaselineStore`]（`LoadBaselines` 的加载即迁移语义：
//!   版本门禁 <2 丢弃、旧键重键为 `模型|渠道`、仅官方渠道且过质量门槛保留、
//!   撞键取 `SampledAt` 最新；`GetAnchors`/`Get` 读内存、不重新读盘）。
//!
//! **`/water-probe` 端点不在本片**（属 P8c 的 `water.go` + `handlers_extra.go` 注水链路）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::channels::is_strong_channel;
use crate::dist::{compare_dist, dist_stats_of, DistSimilarity, DistStats};

/// 单请求超时 —— `probes.go:66`（慢渠道 workbuddy 实测单请求可达 90s+）。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// 429/5xx 退避重试间隔（≤3 次：2s/6s/18s）—— `probes.go:70`。
pub const PROBE_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(6),
    Duration::from_secs(18),
];

/// 限流风暴判定阈值（一次检测内累计 ≥6 次 429）—— `probes.go:57`。
pub const RATE_LIMIT_STORM_THRESHOLD: i64 = 6;

/// `"a"` 单字符基准的离群剔除阈值 —— `probes.go:164`。
pub const A_PROBE_OUTLIER_DEVIATION: i32 = 12;

/// 基准数据格式当前版本 —— `baseline.go:49`。
pub const BASELINE_SCHEMA_VERSION: i64 = 3;

/// 可迁移（可加载）的最低内容版本 —— `baseline.go:53`。
pub const BASELINE_MIGRATABLE_VERSION: i64 = 2;

/// 基准库文件 —— `baseline.go:70-72`。
pub const BASELINES_FILE: &str = "tuanjie-baselines.json";

/// 四段钉死文本（**不动！基准依赖逐字节稳定**）—— `tokenizerProbeTexts`（`probes.go:139-144`）。
pub const TOKENIZER_PROBE_TEXTS: [(&str, &str); 4] = [
    ("tokenizer_en", "The quick brown fox jumps over the lazy dog while forty-two philosophers quietly debated the nature of consciousness beneath the ancient oak trees yesterday evening."),
    ("tokenizer_cjk", "团结引擎在凌晨三点重启了三台推理主机。质量守恒定律指出孤立系统的总能量保持不变。长江流域的梅雨季节通常持续二十天左右。"),
    ("tokenizer_code", "def quicksort(arr):\n    if len(arr) <= 1:\n        return arr\n    pivot = arr[len(arr) // 2]\n    left = [x for x in arr if x < pivot]\n    return quicksort(left) + [pivot] + quicksort(right)"),
    ("tokenizer_mixed", "ᚠᚢᚦ ⠓⠑⠇⠇⠕ 🌟🌱🚀 مرحبا بالعالم שלום עולם emoji+runes+braille+arabic+hebrew 混合文本"),
];

// ---------------------------------------------------------------- 成本累加器

/// 探针成本累加器 —— `probeCostRequests` / `probeCostPromptTokens` /
/// `probeCostCompletionTok` / `probe429Count`（`probes.go:28-32`、`:52`）。
///
/// Go 是包级变量（并发检测会互相混计，注释里写明「当前检测均为单飞场景，可接受」）；
/// Rust 侧把它收进一个值对象，生产同样单飞、测试可各持一份（语义不变、测试可并行）。
#[derive(Debug, Default)]
pub struct ProbeCost {
    requests: AtomicI64,
    prompt_tokens: AtomicI64,
    completion_tokens: AtomicI64,
    count_429: AtomicI64,
}

impl ProbeCost {
    pub fn new() -> Self {
        Self::default()
    }

    /// 清零成本累加器与 429 限流计数 —— `resetProbeCost`（`probes.go:35-40`）。
    pub fn reset(&self) {
        self.requests.store(0, Ordering::SeqCst);
        self.prompt_tokens.store(0, Ordering::SeqCst);
        self.completion_tokens.store(0, Ordering::SeqCst);
        self.count_429.store(0, Ordering::SeqCst);
    }

    /// `(requests, prompt_tokens, completion_tokens)` —— `probeCostSnapshot`（`:43-45`）。
    pub fn snapshot(&self) -> (i64, i64, i64) {
        (
            self.requests.load(Ordering::SeqCst),
            self.prompt_tokens.load(Ordering::SeqCst),
            self.completion_tokens.load(Ordering::SeqCst),
        )
    }

    /// 429 计数（风暴判定用）。
    pub fn count_429(&self) -> i64 {
        self.count_429.load(Ordering::SeqCst)
    }

    /// `rateLimitStorm()`（`:60-62`）—— 阈值可注入（对应 Go 的 `rateLimitStormThreshold`）。
    pub fn rate_limit_storm(&self, threshold: i64) -> bool {
        self.count_429.load(Ordering::SeqCst) >= threshold
    }

    fn add_request(&self) {
        self.requests.fetch_add(1, Ordering::SeqCst);
    }

    fn add_tokens(&self, p: i64, c: i64) {
        self.prompt_tokens.fetch_add(p, Ordering::SeqCst);
        self.completion_tokens.fetch_add(c, Ordering::SeqCst);
    }

    fn inc_429(&self) {
        self.count_429.fetch_add(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- 探针目标

/// 一次探针要打到哪、带什么头 —— `probeTarget`（`probes.go:83-90`）。
#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub base_url: String,
    pub headers: BTreeMap<String, String>,
    pub channel: String,
    returned_model: std::sync::Arc<Mutex<String>>,
}

impl ProbeTarget {
    /// 团结渠道目标 —— `tuanjieTarget`（`probes.go:111-117`）。
    pub fn tuanjie(cli_key: &str, base_url: &str) -> Self {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {cli_key}"));
        ProbeTarget {
            base_url: base_url.to_string(),
            headers,
            channel: "tuanjie".to_string(),
            returned_model: std::sync::Arc::new(Mutex::new(String::new())),
        }
    }

    /// 记录上游自报名（空值不覆盖）—— `setReturnedModel`（`:93-100`）。
    pub fn set_returned_model(&self, m: &str) {
        if m.is_empty() {
            return;
        }
        if let Ok(mut g) = self.returned_model.lock() {
            *g = m.to_string();
        }
    }

    /// 读取上游自报名 —— `returnedModelOf`（`:103-107`）。
    pub fn returned_model(&self) -> String {
        self.returned_model
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// 本次请求的完整请求头 —— `requestHeaders`（`probes.go:122-136`）。
    ///
    /// 团结渠道追加 `x-litellm-session-id` 与签名头（签名密钥取 Authorization 里的
    /// cli_api_key，时间戳每请求现算）。
    pub fn request_headers(&self, path: &str, now_unix: i64) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        h.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        if let Ok(v) = reqwest::header::HeaderValue::from_str(crate::ua::process_cli_user_agent()) {
            h.insert(reqwest::header::USER_AGENT, v);
        }
        for (k, v) in &self.headers {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                h.insert(name, val);
            }
        }
        if self.channel == "tuanjie" {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&crate::session::new_litellm_session_id())
            {
                h.insert(
                    reqwest::header::HeaderName::from_static("x-litellm-session-id"),
                    v,
                );
            }
            let key = self
                .headers
                .get("Authorization")
                .map(|a| a.strip_prefix("Bearer ").unwrap_or(a).to_string())
                .unwrap_or_default();
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&crate::sign::sign_litellm(
                path, &key, now_unix,
            )) {
                // 注意：`HeaderName::from_static` 只接受**全小写**名字（大写会 panic）——
                // 这里用 `from_bytes`（内部小写化），与 `headers.rs` 的签名头同款。
                h.insert(
                    reqwest::header::HeaderName::from_bytes(crate::headers::HEADER_CODELY_SIGNATURE.as_bytes())
                        .expect("签名头名合法"),
                    v,
                );
            }
        }
        h
    }
}

// ---------------------------------------------------------------- 单请求

/// 单个探针的结构化结果 —— `probeResult`（`probes.go:150-156`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub status: String, // ok | unstable | error
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub http_status: u16,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

/// `probeCall` 的返回 —— 对应 Go 的七元组。
#[derive(Debug, Clone, Default)]
pub struct ProbeCall {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub status: u16,
    pub finish: String,
    pub content: String,
    pub err_text: String,
    pub err: Option<String>,
}

/// max_tokens 自适应名单 —— `probeCall` 开头的判据（`probes.go:185-191`）：
/// 思考型模型（会先出 reasoning_content）8 个预算全烧在推理段、content 恒空，
/// 给 96 保 content 出得来；`codely` 系上游即 GLM-5.3 系，**名单必须带上**。
pub fn probe_max_tokens(model: &str) -> i64 {
    let lm = model.to_lowercase();
    if lm.contains("deepseek")
        || lm.contains("glm-5")
        || lm.contains("kimi")
        || lm.contains("o1")
        || lm.contains("o3")
        || lm.contains("codely")
    {
        96
    } else {
        8
    }
}

/// 错误原文脱敏 —— `scrubErrorText`（`probes.go:297-305`）：抹 uuid / 13 位以上数字，
/// 截断到 300 **字节**。
pub fn scrub_error_text(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::new();
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // uuid：8-4-4-4-12 的 hex
        if let Some(len) = match_uuid(&bytes[i..]) {
            out.push_str("<uuid>");
            i += len;
            continue;
        }
        // 13 位以上连续数字
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start >= 13 {
                out.push_str("<num>");
            } else {
                out.push_str(&trimmed[start..i]);
            }
            continue;
        }
        // 按字符推进（多字节 UTF-8 不能被切开）
        let ch = trimmed[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    if out.len() > 300 {
        // Go 的 `s[:300]` 是**字节**截断（可能切开 UTF-8，如实复刻其字节行为）
        let mut b = out.into_bytes();
        b.truncate(300);
        out = String::from_utf8_lossy(&b).into_owned();
    }
    out
}

fn match_uuid(b: &[u8]) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut i = 0usize;
    for (gi, g) in GROUPS.iter().enumerate() {
        if gi > 0 {
            if i >= b.len() || b[i] != b'-' {
                return None;
            }
            i += 1;
        }
        for _ in 0..*g {
            if i >= b.len() || !b[i].is_ascii_hexdigit() {
                return None;
            }
            i += 1;
        }
    }
    Some(i)
}

/// 对单渠道+模型发一次裸请求（无 system prompt）—— `probeCall`（`probes.go:177-287`）。
///
/// 请求打到哪、带什么头全部由 target 决定。退避重试 ≤3 次（2s/6s/18s）；
/// **先判状态码再解析正文**（one-api 限流回 429 + Content-Length: 0，先 Decode
/// 会把 429 吞成裸 EOF）；HTTP 200 + 空正文同样如实带状态码、绝不回裸 EOF。
pub async fn probe_call(
    http: &reqwest::Client,
    target: &ProbeTarget,
    model: &str,
    msgs: &[serde_json::Value],
    extra: Option<&serde_json::Map<String, serde_json::Value>>,
    cost: &ProbeCost,
    backoff: &[Duration],
    now_unix: i64,
) -> ProbeCall {
    let mt = probe_max_tokens(model);
    let mut body = serde_json::Map::new();
    body.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    body.insert("stream".to_string(), serde_json::Value::Bool(false));
    body.insert("messages".to_string(), serde_json::Value::Array(msgs.to_vec()));
    body.insert(
        "max_tokens".to_string(),
        serde_json::Value::Number(serde_json::Number::from(mt)),
    );
    if let Some(extra) = extra {
        for (k, v) in extra {
            body.insert(k.clone(), v.clone());
        }
    }
    let bytes = crate::gojson::marshal_go_value(&serde_json::Value::Object(body));
    let url = format!("{}/v1/chat/completions", target.base_url);

    let mut attempt = 0usize;
    loop {
        cost.add_request();
        let headers = target.request_headers("/v1/chat/completions", now_unix);
        let resp = match http
            .post(&url)
            .headers(headers)
            .body(bytes.clone())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ProbeCall {
                    err: Some(e.to_string()),
                    ..Default::default()
                }
            }
        };
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            cost.inc_429();
        }
        if (status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error())
            && attempt < backoff.len()
        {
            // 排空并关闭本响应，睡完重试（请求体每轮重建）
            let _ = crate::chat::read_body_bounded(resp, 4096).await;
            tokio::time::sleep(backoff[attempt]).await;
            attempt += 1;
            continue;
        }
        let status_u16 = status.as_u16();
        if status != reqwest::StatusCode::OK {
            let raw = crate::chat::read_body_bounded(resp, 1 << 20).await;
            let text = String::from_utf8_lossy(&raw).to_string();
            let err_text = match serde_json::from_slice::<serde_json::Value>(&raw) {
                Ok(v) => match v.get("error") {
                    Some(e) if !e.is_null() => {
                        scrub_error_text(&crate::gojson::marshal_go_value(e).iter().map(|b| *b as char).collect::<String>())
                    }
                    _ => {
                        let t = text.trim();
                        if !t.is_empty() {
                            format!("HTTP {status_u16}（{}）", scrub_error_text(t))
                        } else {
                            format!("HTTP {status_u16}（空响应体）")
                        }
                    }
                },
                Err(_) => {
                    let t = text.trim();
                    if !t.is_empty() {
                        format!("HTTP {status_u16}（{}）", scrub_error_text(t))
                    } else {
                        format!("HTTP {status_u16}（空响应体）")
                    }
                }
            };
            return ProbeCall {
                status: status_u16,
                err_text,
                ..Default::default()
            };
        }
        let raw = crate::chat::read_body_bounded(resp, 1 << 20).await;
        let text = String::from_utf8_lossy(&raw).to_string();
        if text.trim().is_empty() {
            return ProbeCall {
                status: status_u16,
                err: Some(format!("HTTP {status_u16}（空响应体）")),
                ..Default::default()
            };
        }
        let rr = match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(v) => v,
            Err(e) => {
                // Go 这里返回 `json.Unmarshal` 的报错文案（`err != nil` ⇒ 探针 note 带原文）。
                // 措辞由各自的 JSON 库给（README 差异 12/32 同族），但**必须带出真实错误**，
                // 不能填一句固定文案——否则 200 半截正文的现场就丢了。
                return ProbeCall {
                    status: status_u16,
                    err: Some(e.to_string()),
                    ..Default::default()
                };
            }
        };
        let prompt = rr
            .get("usage")
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|p| p.as_i64())
            .unwrap_or(0);
        let completion = rr
            .get("usage")
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|p| p.as_i64())
            .unwrap_or(0);
        cost.add_tokens(prompt, completion);
        target.set_returned_model(rr.get("model").and_then(|m| m.as_str()).unwrap_or(""));
        let choice = rr.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first());
        let finish = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        let content = choice
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        return ProbeCall {
            prompt_tokens: prompt,
            completion_tokens: completion,
            status: status_u16,
            finish,
            content,
            err_text: String::new(),
            err: None,
        };
    }
}

/// `firstErrText`（`probes.go:482-489`）。
fn first_err_text(errs: &[&Option<String>]) -> String {
    for e in errs {
        if let Some(e) = e {
            return e.clone();
        }
    }
    String::new()
}

/// 第一层管道探针 —— `RunPipelineProbes`（`probes.go:322-472`）。
///
/// `threshold` = Go 的 `rateLimitStormThreshold`（生产 6，测试可调）。
pub async fn run_pipeline_probes(
    http: &reqwest::Client,
    target: &ProbeTarget,
    model: &str,
    cost: &ProbeCost,
    backoff: &[Duration],
    threshold: i64,
    now_unix: i64,
) -> Vec<ProbeResult> {
    let mut out: Vec<ProbeResult> = Vec::new();
    let msgs_one = |text: &str| {
        vec![serde_json::json!({"role": "user", "content": text})]
    };

    // "a" 单字符基准（所有 tokenizer 探针的归一化底座），采 3 次取中位数
    let mut a_samples: Vec<i32> = Vec::new();
    for _ in 0..3 {
        if cost.rate_limit_storm(threshold) {
            break;
        }
        let r = probe_call(
            http,
            target,
            model,
            &msgs_one("a"),
            None,
            cost,
            backoff,
            now_unix,
        )
        .await;
        if r.err.is_none() && r.prompt_tokens > 0 {
            a_samples.push(r.prompt_tokens as i32);
        }
    }
    let mut a_tok = 0i32;
    let mut a_unstable = false;
    if !a_samples.is_empty() {
        a_samples.sort_unstable();
        a_tok = a_samples[a_samples.len() / 2];
        if a_tok - a_samples[0] > A_PROBE_OUTLIER_DEVIATION {
            a_unstable = true;
        }
    }

    for (name, text) in TOKENIZER_PROBE_TEXTS {
        if cost.rate_limit_storm(threshold) {
            break;
        }
        let msgs = msgs_one(text);
        let r1 = probe_call(http, target, model, &msgs, None, cost, backoff, now_unix).await;
        let r2 = probe_call(http, target, model, &msgs, None, cost, backoff, now_unix).await;
        let tok1 = r1.prompt_tokens as i32;
        let tok2 = r2.prompt_tokens as i32;
        if r1.err.is_some() || r2.err.is_some() {
            out.push(ProbeResult {
                name: name.to_string(),
                status: "error".to_string(),
                note: first_err_text(&[&r1.err, &r2.err]),
                ..Default::default()
            });
        } else if tok1 <= 0 || tok2 <= 0 {
            // usage 缺失：有上游错误就保留（渠道被限的确凿信号）
            let (note, st, hst) = if !r1.err_text.is_empty() {
                (r1.err_text.clone(), "error", r1.status)
            } else if !r2.err_text.is_empty() {
                (r2.err_text.clone(), "error", r2.status)
            } else {
                ("usage 缺失（prompt_tokens=0）".to_string(), "unstable", 0)
            };
            out.push(ProbeResult {
                name: name.to_string(),
                status: st.to_string(),
                note,
                http_status: hst,
                ..Default::default()
            });
        } else if a_unstable {
            out.push(ProbeResult {
                name: name.to_string(),
                status: "unstable".to_string(),
                note: format!(
                    "a 基准采样离群（中位数 {} vs 最小 {}），无法归一化",
                    a_tok, a_samples[0]
                ),
                ..Default::default()
            });
        } else if a_tok <= 0 {
            out.push(ProbeResult {
                name: name.to_string(),
                status: "unstable".to_string(),
                note: "a 基准采样失败，无法归一化".to_string(),
                ..Default::default()
            });
        } else if tok1 != tok2 {
            out.push(ProbeResult {
                name: name.to_string(),
                value: format!("{}", tok1 - a_tok),
                status: "unstable".to_string(),
                note: format!("双发不一致 {tok1} vs {tok2}（多主机路由？）"),
                ..Default::default()
            });
        } else if tok1 - a_tok < 1 {
            out.push(ProbeResult {
                name: name.to_string(),
                status: "unstable".to_string(),
                note: format!(
                    "归一化值 {} 非正整数（a 基准 {} vs 文本 {}），无法归一化",
                    tok1 - a_tok,
                    a_tok,
                    tok1
                ),
                ..Default::default()
            });
        } else {
            out.push(ProbeResult {
                name: name.to_string(),
                value: format!("{}", tok1 - a_tok),
                status: "ok".to_string(),
                ..Default::default()
            });
        }
    }

    // 错误探针 1：temperature=2.0
    if cost.rate_limit_storm(threshold) {
        return out;
    }
    let mut extra = serde_json::Map::new();
    extra.insert("temperature".to_string(), serde_json::json!(2.0));
    let r = probe_call(
        http,
        target,
        model,
        &msgs_one("hello"),
        Some(&extra),
        cost,
        backoff,
        now_unix,
    )
    .await;
    match classify_error_probe(&r, "error_temp2") {
        Some(x) => out.push(x),
        None => {}
    }

    // 错误探针 2：max_tokens=10^9
    if cost.rate_limit_storm(threshold) {
        return out;
    }
    let mut extra2 = serde_json::Map::new();
    extra2.insert("max_tokens".to_string(), serde_json::json!(1000000000i64));
    let r = probe_call(
        http,
        target,
        model,
        &msgs_one("hello"),
        Some(&extra2),
        cost,
        backoff,
        now_unix,
    )
    .await;
    if let Some(x) = classify_error_probe(&r, "error_maxtok") {
        out.push(x);
    }

    // finish_reason 词汇 1：max_tokens=400 正常说完 → 期望 stop
    if cost.rate_limit_storm(threshold) {
        return out;
    }
    let mut extra3 = serde_json::Map::new();
    extra3.insert("max_tokens".to_string(), serde_json::json!(400));
    let r = probe_call(
        http,
        target,
        model,
        &msgs_one("回复一个字：好"),
        Some(&extra3),
        cost,
        backoff,
        now_unix,
    )
    .await;
    out.push(classify_finish_probe(&r, "finish_stop"));

    // finish_reason 词汇 2：max_tokens=6 强制截断 → 期望 length
    if cost.rate_limit_storm(threshold) {
        return out;
    }
    let mut extra4 = serde_json::Map::new();
    extra4.insert("max_tokens".to_string(), serde_json::json!(6));
    let r = probe_call(
        http,
        target,
        model,
        &msgs_one("写一篇五百字的散文，主题是秋天。"),
        Some(&extra4),
        cost,
        backoff,
        now_unix,
    )
    .await;
    out.push(classify_finish_probe(&r, "finish_length"));
    out
}

/// 错误探针分类 —— `probes.go:404-413` / `:423-432`：
/// 只有参数校验类（400/422）的报错原文才是指纹，其余非 200 是渠道状态信号。
fn classify_error_probe(r: &ProbeCall, name: &str) -> Option<ProbeResult> {
    if let Some(e) = &r.err {
        return Some(ProbeResult {
            name: name.to_string(),
            status: "error".to_string(),
            note: e.clone(),
            ..Default::default()
        });
    }
    match r.status {
        200 => Some(ProbeResult {
            name: name.to_string(),
            status: "unstable".to_string(),
            note: "未返回错误文本".to_string(),
            ..Default::default()
        }),
        400 | 422 => Some(ProbeResult {
            name: name.to_string(),
            value: r.err_text.clone(),
            status: "ok".to_string(),
            http_status: r.status,
            ..Default::default()
        }),
        st => Some(ProbeResult {
            name: name.to_string(),
            status: "error".to_string(),
            note: r.err_text.clone(),
            http_status: st,
            ..Default::default()
        }),
    }
}

/// finish_reason 探针分类 —— `probes.go:442-451` / `:461-470`。
fn classify_finish_probe(r: &ProbeCall, name: &str) -> ProbeResult {
    if let Some(e) = &r.err {
        return ProbeResult {
            name: name.to_string(),
            status: "error".to_string(),
            note: e.clone(),
            ..Default::default()
        };
    }
    if r.status != 200 {
        return ProbeResult {
            name: name.to_string(),
            status: "error".to_string(),
            note: r.err_text.clone(),
            http_status: r.status,
            ..Default::default()
        };
    }
    if r.finish.is_empty() {
        return ProbeResult {
            name: name.to_string(),
            status: "unstable".to_string(),
            note: "finish_reason 缺失".to_string(),
            ..Default::default()
        };
    }
    ProbeResult {
        name: name.to_string(),
        value: r.finish.clone(),
        status: "ok".to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- 基准库（内存态）

/// 一次分布采样的结果 —— `distResult`（`dist.go:36-44`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DistResult {
    pub counts: Vec<i64>,
    pub valid: i64,
    pub invalid: i64,
    pub unanswered: i64,
    pub stats: DistStats,
    pub insufficient: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub rate_limited: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl DistResult {
    /// `compare_dist` 的入口（与 Go 的 `CompareDist` 同源实现）。
    pub fn compare(&self, other: &DistResult) -> DistSimilarity {
        compare_dist(
            &self.counts,
            &other.counts,
            self.valid,
            other.valid,
        )
    }
}

/// 单模型的官方锚 —— `Baseline`（`baseline.go:27-38`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub returned_model: String,
    #[serde(default)]
    pub official: bool,
    #[serde(default)]
    pub schema_version: i64,
    #[serde(default)]
    pub probes: Vec<ProbeResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dist: Option<DistResult>,
    #[serde(default)]
    pub sampled_at: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub sample_count: i64,
}

/// 基准库文件形态（`json.MarshalIndent(BaselineStore)` → 只有 `baselines` 一个键）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BaselineFile {
    #[serde(default, deserialize_with = "de_null_tolerant_baselines")]
    baselines: BTreeMap<String, Baseline>,
}

/// `null` 条目容忍：Go 用 `map[string]*Baseline`，`null` 值解成 nil 指针，
/// 迁移时丢弃该条、其余照载（`baseline.go:137`）；serde 直接解
/// `BTreeMap<String, Baseline>` 遇到 `null` 则整份报错。
/// 这里先按 `Option` 解再丢弃 `None`（P8b 第五片审查 #4；
/// Go 对类型错条目同样整份失败，故只容忍 `null`，不逐条吞错——与 Go 同边界）。
fn de_null_tolerant_baselines<'de, D>(
    d: D,
) -> Result<BTreeMap<String, Baseline>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let m = BTreeMap::<String, Option<Baseline>>::deserialize(d)?;
    Ok(m.into_iter()
        .filter_map(|(k, v)| v.map(|b| (k, b)))
        .collect())
}

/// 基准库（键 = `规范化模型名|来源渠道id`）—— `BaselineStore`（`baseline.go:57-61`）。
pub struct BaselineStore {
    baselines: Mutex<BTreeMap<String, Baseline>>,
    path: Option<PathBuf>,
}

/// 规范化模型名 —— `normalizeModelName`（`dist.go` 尾部）：**小写 + 去路由前缀**
/// （取最后一个 `/` 之后）。
pub fn normalize_model_name(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.rsplit_once('/') {
        Some((_, tail)) => tail.to_string(),
        None => lower,
    }
}

/// 多锚存储键 —— `baselineKey`（`baseline.go:66-68`）。
pub fn baseline_key(model: &str, channel: &str) -> String {
    format!("{}|{}", normalize_model_name(model), channel)
}

/// 基准质量门槛 —— `baselineQualityOK`（`baseline.go:80-98`）：
/// 探针 ok ≥6、分布有效样本 ≥40、全部 ok 的 tokenizer 探针值为正整数。
pub fn baseline_quality_ok(bl: &Baseline) -> bool {
    let mut ok = 0;
    for p in &bl.probes {
        if p.status != "ok" {
            continue;
        }
        ok += 1;
        if p.name.starts_with("tokenizer_") {
            match p.value.parse::<i64>() {
                Ok(n) if n >= 1 => {}
                _ => return false,
            }
        }
    }
    ok >= 6 && bl.dist.as_ref().map(|d| d.valid >= 40).unwrap_or(false)
}

impl BaselineStore {
    /// 空库（不读盘）—— 测试用。
    pub fn empty() -> Self {
        BaselineStore {
            baselines: Mutex::new(BTreeMap::new()),
            path: None,
        }
    }

    /// `LoadBaselines`（`baseline.go:111-123`）：从磁盘恢复（缺失 = 空库），
    /// 就地迁移旧键/旧版本，迁移有结构变化时回写。
    pub fn load(path: Option<PathBuf>) -> Self {
        let bs = BaselineStore {
            baselines: Mutex::new(BTreeMap::new()),
            path,
        };
        if let Some(p) = bs.path.clone() {
            if let Ok(raw) = std::fs::read(&p) {
                if let Ok(file) = serde_json::from_slice::<BaselineFile>(&raw) {
                    *bs.baselines.lock().unwrap() = file.baselines;
                    if bs.migrate_legacy_keys() {
                        bs.save();
                    }
                }
            }
        }
        bs
    }

    /// 生产构造：数据目录的 `tuanjie-baselines.json`。
    pub fn new() -> Self {
        Self::load(Some(crate::pacing::data_path(BASELINES_FILE)))
    }

    /// 当前内存态的全部键（有序）。
    pub fn keys(&self) -> Vec<String> {
        self.baselines.lock().unwrap().keys().cloned().collect()
    }

    /// 直接读一条（测试/门禁用）。
    pub fn raw_get(&self, key: &str) -> Option<Baseline> {
        self.baselines.lock().unwrap().get(key).cloned()
    }

    /// 内存态快照（键 → 条目），门禁用（与 Go 的 `map[string]Baseline` 对照）。
    pub fn raw_dump(&self) -> serde_json::Map<String, serde_json::Value> {
        let g = self.baselines.lock().unwrap();
        let mut out = serde_json::Map::new();
        for (k, v) in g.iter() {
            out.insert(
                k.clone(),
                serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
            );
        }
        out
    }

    /// `migrateLegacyKeys`（`baseline.go:127-199`）：返回是否有结构变化需落盘。
    ///
    /// 逐条照抄 Go：按 key 排序后处理（map 遍历无序，排序保证多次加载结果确定）；
    /// 版本 <2 丢弃；v3 新键按最后一个 `|` 分割、旧键按 v1「渠道|模型」/v2 裸模型名；
    /// 数据体 `Model` 字段优先；仅官方渠道且过质量门槛保留；同键取 `SampledAt` 最新。
    pub fn migrate_legacy_keys(&self) -> bool {
        let mut guard = self.baselines.lock().unwrap();
        let mut keys: Vec<String> = guard.keys().cloned().collect();
        keys.sort();
        let mut out: BTreeMap<String, Baseline> = BTreeMap::new();
        let mut changed = false;
        for k in keys {
            let v = guard.get(&k).cloned();
            let Some(mut v) = v else {
                changed = true;
                continue;
            };
            if v.schema_version < BASELINE_MIGRATABLE_VERSION {
                changed = true;
                continue;
            }
            let (mut model, mut channel);
            if v.schema_version >= BASELINE_SCHEMA_VERSION {
                model = k.clone();
                channel = v.channel.clone();
                if let Some(i) = k.rfind('|') {
                    model = k[..i].to_string();
                    channel = k[i + 1..].to_string();
                }
            } else {
                channel = "tuanjie".to_string();
                model = k.clone();
                if let Some(i) = k.find('|') {
                    channel = k[..i].to_string();
                    model = k[i + 1..].to_string();
                } else if !v.channel.is_empty() {
                    channel = v.channel.clone();
                }
            }
            if !v.model.is_empty() {
                model = v.model.clone();
            }
            if !is_strong_channel(&channel) || !baseline_quality_ok(&v) {
                changed = true;
                continue;
            }
            if v.schema_version < BASELINE_SCHEMA_VERSION {
                v.schema_version = BASELINE_SCHEMA_VERSION;
                changed = true;
            }
            let nk = baseline_key(&model, &channel);
            if k != nk {
                changed = true;
            }
            if !v.official {
                v.official = true;
                changed = true;
            }
            if v.channel.is_empty() {
                v.channel = channel.clone();
                changed = true;
            }
            match out.get(&nk) {
                Some(cur) if cur.sampled_at >= v.sampled_at => {}
                _ => {
                    out.insert(nk, v);
                }
            }
        }
        *guard = out;
        changed
    }

    /// `save`（`baseline.go:201-207`）：`MarshalIndent` + 0600。
    pub fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let raw = self.to_bytes();
        let _ = std::fs::write(path, raw);
    }

    /// `GetAnchors`（`baseline.go:212-224`）：键前缀 = `规范化模型名|`，
    /// 按 `SampledAt` 新→旧排序。
    pub fn get_anchors(&self, model: &str) -> Vec<Baseline> {
        let prefix = format!("{}|", normalize_model_name(model));
        let g = self.baselines.lock().unwrap();
        let mut out: Vec<Baseline> = g
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v.clone())
            .collect();
        out.sort_by(|a, b| b.sampled_at.cmp(&a.sampled_at));
        out
    }

    /// `Get`（`baseline.go:227-233`）：最新的一条锚。
    pub fn get(&self, model: &str) -> Option<Baseline> {
        self.get_anchors(model).into_iter().next()
    }

    /// 门禁用：从 JSON 字节加载（等价于 `LoadBaselines` 读文件那一步 + 迁移）。
    pub fn load_from_bytes(&self, raw: &[u8]) -> bool {
        if let Ok(file) = serde_json::from_slice::<BaselineFile>(raw) {
            *self.baselines.lock().unwrap() = file.baselines;
            return self.migrate_legacy_keys();
        }
        false
    }

    /// 落盘字节（门禁用：不写盘、只取 `MarshalIndent` 的结果）。
    ///
    /// **不是**「先 `to_value` 再写」——那样会丢结构体声明序（`serde_json::Value` 的对象是
    /// `BTreeMap`，键会被排成字节序，而 Go 的 `json.Marshal(struct)` 是**声明序**）。
    /// 这里按 `Baseline`/`distResult`/`probeResult`/`distStats` 的 Go 字段声明序逐字段写出，
    /// 缩进两格、无末尾换行、数字走 Go 的 `strconv` 口径。
    pub fn to_bytes(&self) -> Vec<u8> {
        let g = self.baselines.lock().unwrap();
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"{\n  \"baselines\": {");
        if g.is_empty() {
            out.extend_from_slice(b"}");
        } else {
            for (i, (k, v)) in g.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(b"\n    ");
                crate::gojson::write_go_string(&mut out, k);
                out.extend_from_slice(b": ");
                // 顶层条目的 `{` 落在 4 空格处（`"baselines": {` 之下）⇒ 缩进单位=2
                write_baseline(&mut out, v, 2);
            }
            out.extend_from_slice(b"\n  }");
        }
        out.extend_from_slice(b"\n}");
        out
    }
}

/// 分布统计的便捷重算（纯函数面，供门禁构造 fixture）。
pub fn dist_stats(counts: &[i64]) -> DistStats {
    dist_stats_of(counts)
}

// ---------------------------------------------------------------- MarshalIndent 写出

fn pad(out: &mut Vec<u8>, n: usize) {
    for _ in 0..n {
        out.extend_from_slice(b"  ");
    }
}

fn write_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(v.to_string().as_bytes());
}

fn write_bool(out: &mut Vec<u8>, v: bool) {
    out.extend_from_slice(if v { b"true" } else { b"false" });
}

/// `json.MarshalIndent(Baseline, "", "  ")` 的等价物（字段按**声明序**、`omitempty` 逐条照抄）。
fn write_baseline(out: &mut Vec<u8>, b: &Baseline, indent: usize) {
    out.extend_from_slice(b"{\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"model\": ");
    crate::gojson::write_go_string(out, &b.model);
    out.extend_from_slice(b",\n");
    if !b.channel.is_empty() {
        pad(out, indent + 1);
        out.extend_from_slice(b"\"channel\": ");
        crate::gojson::write_go_string(out, &b.channel);
        out.extend_from_slice(b",\n");
    }
    if !b.returned_model.is_empty() {
        pad(out, indent + 1);
        out.extend_from_slice(b"\"returned_model\": ");
        crate::gojson::write_go_string(out, &b.returned_model);
        out.extend_from_slice(b",\n");
    }
    pad(out, indent + 1);
    out.extend_from_slice(b"\"official\": ");
    write_bool(out, b.official);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"schema_version\": ");
    write_i64(out, b.schema_version);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"probes\": ");
    if b.probes.is_empty() {
        // Go 的 nil 切片是 `null`；非 nil 空切片是 `[]`。Rust 侧统一写 `[]`
        // （已登记差异：空探针列表的形态）。
        out.extend_from_slice(b"[]");
    } else {
        out.extend_from_slice(b"[\n");
        for (i, p) in b.probes.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b",\n");
            }
            pad(out, indent + 2);
            write_probe_result(out, p, indent + 2);
        }
        out.push(b'\n');
        pad(out, indent + 1);
        out.extend_from_slice(b"]");
    }
    out.extend_from_slice(b",\n");
    if let Some(d) = &b.dist {
        pad(out, indent + 1);
        out.extend_from_slice(b"\"dist\": ");
        write_dist_result(out, d, indent + 1);
        out.extend_from_slice(b",\n");
    }
    pad(out, indent + 1);
    out.extend_from_slice(b"\"sampled_at\": ");
    crate::gojson::write_go_string(out, &b.sampled_at);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"account\": ");
    crate::gojson::write_go_string(out, &b.account);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"sample_count\": ");
    write_i64(out, b.sample_count);
    out.push(b'\n');
    pad(out, indent);
    out.push(b'}');
}

fn write_probe_result(out: &mut Vec<u8>, p: &ProbeResult, indent: usize) {
    out.extend_from_slice(b"{\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"name\": ");
    crate::gojson::write_go_string(out, &p.name);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"value\": ");
    crate::gojson::write_go_string(out, &p.value);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"status\": ");
    crate::gojson::write_go_string(out, &p.status);
    if !p.note.is_empty() {
        out.extend_from_slice(b",\n");
        pad(out, indent + 1);
        out.extend_from_slice(b"\"note\": ");
        crate::gojson::write_go_string(out, &p.note);
    }
    if p.http_status != 0 {
        out.extend_from_slice(b",\n");
        pad(out, indent + 1);
        out.extend_from_slice(b"\"http_status\": ");
        write_i64(out, p.http_status as i64);
    }
    out.push(b'\n');
    pad(out, indent);
    out.push(b'}');
}

fn write_dist_result(out: &mut Vec<u8>, d: &DistResult, indent: usize) {
    out.extend_from_slice(b"{\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"counts\": ");
    if d.counts.is_empty() {
        out.extend_from_slice(b"[]");
    } else {
        out.extend_from_slice(b"[\n");
        for (i, c) in d.counts.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b",\n");
            }
            pad(out, indent + 2);
            write_i64(out, *c);
        }
        out.push(b'\n');
        pad(out, indent + 1);
        out.extend_from_slice(b"]");
    }
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"valid\": ");
    write_i64(out, d.valid);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"invalid\": ");
    write_i64(out, d.invalid);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"unanswered\": ");
    write_i64(out, d.unanswered);
    out.extend_from_slice(b",\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"stats\": {\n");
    pad(out, indent + 2);
    out.extend_from_slice(b"\"mean\": ");
    out.extend_from_slice(crate::gojson::format_go_float(d.stats.mean).as_bytes());
    out.extend_from_slice(b",\n");
    pad(out, indent + 2);
    out.extend_from_slice(b"\"median\": ");
    out.extend_from_slice(crate::gojson::format_go_float(d.stats.median).as_bytes());
    out.extend_from_slice(b",\n");
    pad(out, indent + 2);
    out.extend_from_slice(b"\"std_dev\": ");
    out.extend_from_slice(crate::gojson::format_go_float(d.stats.std_dev).as_bytes());
    out.extend_from_slice(b",\n");
    pad(out, indent + 2);
    out.extend_from_slice(b"\"mode\": ");
    write_i64(out, d.stats.mode);
    out.extend_from_slice(b",\n");
    pad(out, indent + 2);
    out.extend_from_slice(b"\"mode_count\": ");
    write_i64(out, d.stats.mode_count);
    out.push(b'\n');
    pad(out, indent + 1);
    out.extend_from_slice(b"},\n");
    pad(out, indent + 1);
    out.extend_from_slice(b"\"insufficient\": ");
    write_bool(out, d.insufficient);
    if d.rate_limited {
        out.extend_from_slice(b",\n");
        pad(out, indent + 1);
        out.extend_from_slice(b"\"rate_limited\": true");
    }
    out.push(b'\n');
    pad(out, indent);
    out.push(b'}');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_max_tokens_list_matches_go() {
        // 思考型（会先出 reasoning_content）给 96，其余 8 —— 名单含 codely 系
        for m in [
            "deepseek-v4-flash",
            "GLM-5.3",
            "glm-5.3-flash",
            "KIMI-K3",
            "o1-mini",
            "o3",
            "codely-core",
            "CODELY-BASIC",
        ] {
            assert_eq!(probe_max_tokens(m), 96, "m={m}");
        }
        for m in ["gpt-4o", "claude-3", "muse-spark-1.3-contributor-free", ""] {
            assert_eq!(probe_max_tokens(m), 8, "m={m}");
        }
    }

    #[test]
    fn scrub_error_text_masks_volatile_bits() {
        assert_eq!(
            scrub_error_text("id 123e4567-e89b-12d3-a456-426614174000 done"),
            "id <uuid> done"
        );
        // 13 位以上数字（时间戳类）→ <num>；12 位保留
        assert_eq!(scrub_error_text("ts 1792189602061"), "ts <num>");
        assert_eq!(scrub_error_text("n 123456789012"), "n 123456789012");
        assert_eq!(scrub_error_text("  pad  "), "pad");
        // 截断到 300 字节（Go 是字节切片）
        let long = "a".repeat(400);
        assert_eq!(scrub_error_text(&long).len(), 300);
    }

    #[test]
    fn baseline_keys_and_quality_match_go() {
        assert_eq!(normalize_model_name("Codely-Core"), "codely-core");
        assert_eq!(normalize_model_name("alias/team/GLM-5.3"), "glm-5.3");
        assert_eq!(baseline_key("Codely-Core", "tuanjie"), "codely-core|tuanjie");
        // 质量门槛：探针 ok ≥6、分布 valid ≥40、ok 的 tokenizer 值为正整数
        let good = Baseline {
            probes: (0..6)
                .map(|i| ProbeResult {
                    name: format!("tokenizer_{i}"),
                    value: "3".to_string(),
                    status: "ok".to_string(),
                    ..Default::default()
                })
                .collect(),
            dist: Some(DistResult {
                valid: 40,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(baseline_quality_ok(&good));
        let mut bad = good.clone();
        bad.probes[0].value = "0".to_string();
        assert!(!baseline_quality_ok(&bad));
        let mut bad2 = good.clone();
        bad2.dist.as_mut().unwrap().valid = 39;
        assert!(!baseline_quality_ok(&bad2));
        let mut bad3 = good.clone();
        bad3.probes.truncate(5);
        assert!(!baseline_quality_ok(&bad3));
        assert!(!baseline_quality_ok(&Baseline::default()));
    }

    fn fixture() -> Vec<u8> {
        // 与 Go 采集器的 fixture 同一形态：v1 旧键 / v2 裸键 / 版本 1 / 非官方渠道 / 劣质
        serde_json::json!({
            "baselines": {
                "tuanjie|codely-core": good("codely-core", "tuanjie", 2, "2026-01-01T00:00:00Z"),
                "codely-basic": good("", "", 2, "2026-02-01T00:00:00Z"),
                "old-model": good("old-model", "tuanjie", 1, "2026-01-01T00:00:00Z"),
                "bai|codely-core": good("codely-core", "bai", 2, "2026-03-01T00:00:00Z"),
                "weak-model": serde_json::json!({
                    "model": "weak-model", "channel": "tuanjie", "official": true,
                    "schema_version": 2,
                    "probes": [{"name": "finish_stop", "value": "stop", "status": "ok"}],
                    "sampled_at": "2026-01-01T00:00:00Z"
                }),
                "codely-core|tuanjie": good("codely-core", "tuanjie", 3, "2026-04-01T00:00:00Z"),
                "codely-core|comate": good("codely-core", "comate", 3, "2026-05-01T00:00:00Z")
            }
        })
        .to_string()
        .into_bytes()
    }

    fn good(model: &str, channel: &str, version: i64, sampled_at: &str) -> serde_json::Value {
        serde_json::json!({
            "model": model, "channel": channel, "official": true, "schema_version": version,
            "probes": [
                {"name": "tokenizer_en", "value": "12", "status": "ok"},
                {"name": "tokenizer_cjk", "value": "30", "status": "ok"},
                {"name": "tokenizer_code", "value": "44", "status": "ok"},
                {"name": "tokenizer_mixed", "value": "20", "status": "ok"},
                {"name": "error_temp2", "value": "bad temp", "status": "ok"},
                {"name": "error_maxtok", "value": "too large", "status": "ok"},
                {"name": "finish_stop", "value": "stop", "status": "ok"},
                {"name": "finish_length", "value": "length", "status": "ok"}
            ],
            "dist": {"counts": [1, 2, 3], "valid": 710, "invalid": 0, "unanswered": 0,
                "stats": {"mean": 177.0, "median": 178.0, "std_dev": 102.5, "mode": 0, "mode_count": 5},
                "insufficient": false},
            "sampled_at": sampled_at, "account": "uid-x", "sample_count": 710
        })
    }

    #[test]
    fn load_baselines_migrates_like_go() {
        let store = BaselineStore::empty();
        store.load_from_bytes(&fixture());
        let mut keys = store.keys();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "codely-basic|tuanjie".to_string(),
                "codely-core|comate".to_string(),
                "codely-core|tuanjie".to_string()
            ]
        );
        // v2 裸键重键为「模型|渠道」，渠道缺省按团结
        let migrated = store.raw_get("codely-basic|tuanjie").expect("重键后存在");
        assert_eq!(migrated.schema_version, BASELINE_SCHEMA_VERSION);
        assert!(migrated.official);
        assert_eq!(migrated.channel, "tuanjie");
        // v3 与迁移出的同键撞键：取 SampledAt 最新（2026-04 > 2026-01）
        let core = store.raw_get("codely-core|tuanjie").unwrap();
        assert_eq!(core.sampled_at, "2026-04-01T00:00:00Z");
        // 多锚：同一模型的另一渠道条目独立存活，GetAnchors 按 SampledAt 新→旧
        let anchors = store.get_anchors("Codely-Core");
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].sampled_at, "2026-05-01T00:00:00Z");
        assert_eq!(store.get("codely-core").unwrap().channel, "comate");
        assert!(store.get("nope").is_none());
    }

    #[test]
    fn to_bytes_writes_go_marshal_indent() {
        let store = BaselineStore::empty();
        store.load_from_bytes(&fixture());
        let text = String::from_utf8(store.to_bytes()).unwrap();
        // 文件级合同：外层 {"baselines": {...}}、键按字节序、结构体按声明序、无末尾换行
        assert!(text.starts_with("{\n  \"baselines\": {\n    \"codely-basic|tuanjie\": {\n      \"model\": \"\","), "{text}");
        assert!(text.ends_with("}\n}"), "{text}");
        assert!(text.contains("\n      \"official\": true,\n      \"schema_version\": 3,\n"), "{text}");
        assert!(text.contains("\n        \"stats\": {\n          \"mean\": 177,\n          \"median\": 178,\n          \"std_dev\": 102.5,\n          \"mode\": 0,\n          \"mode_count\": 5\n        },\n"), "{text}");
        // `omitempty`：空 channel/note/http_status/rate_limited 不写出
        assert!(!text.contains("rate_limited"), "{text}");
        assert!(!text.contains("http_status"), "{text}");
        // 幂等：同一内存态两次写出逐字节相同
        assert_eq!(store.to_bytes(), store.to_bytes());
    }

    /// P8b 第五片审查 #4：`null` 条目与缺字段条目不得整份拒载。
    /// Go 用 `map[string]*Baseline`：`null` 值解成 nil 指针，迁移时丢弃该条、其余照载
    /// （`baseline.go:137`）；缺字段取零值；类型错才整份失败。
    #[test]
    fn baseline_file_tolerates_null_and_missing_fields_like_go() {
        // 反序列化层：null 丢弃、缺字段零值、类型错整份拒绝。
        let f: BaselineFile = serde_json::from_slice(
            br#"{"baselines":{"k-null":null,"k-bare":{"model":"m"}}}"#,
        )
        .expect("null/缺字段不得整份拒载");
        assert!(!f.baselines.contains_key("k-null"), "null 条目丢弃（Go 同）");
        assert_eq!(f.baselines["k-bare"].model, "m");
        assert!(serde_json::from_slice::<BaselineFile>(
            br#"{"baselines":{"k-bad":{"model":123}}}"#
        )
        .is_err(), "类型错整份拒绝（与 Go 同边界）");
        // 加载层：同文件里的 null 不得牵连好条目（旧实现整份拒载，好条目也丢）。
        let store = BaselineStore::empty();
        store.load_from_bytes(
            br#"{"baselines":{
                "k-null":null,
                "codely-core|tuanjie":{
                    "model":"codely-core","channel":"tuanjie","official":true,"schema_version":3,
                    "probes":[
                        {"name":"tokenizer_en","value":"12","status":"ok"},
                        {"name":"tokenizer_cjk","value":"30","status":"ok"},
                        {"name":"tokenizer_code","value":"44","status":"ok"},
                        {"name":"tokenizer_mixed","value":"20","status":"ok"},
                        {"name":"error_temp2","value":"bad temp","status":"ok"},
                        {"name":"error_maxtok","value":"too large","status":"ok"}],
                    "dist":{"counts":[1],"valid":100,"invalid":0,"unanswered":0,
                        "stats":{"mean":1.0,"median":1.0,"std_dev":0.0,"mode":0,"mode_count":1},
                        "insufficient":false},
                    "sampled_at":"2026-06-01T00:00:00Z"}}}"#,
        );
        assert!(
            store.raw_get("codely-core|tuanjie").is_some(),
            "好条目不受同文件 null 牵连"
        );
    }

    #[test]
    fn probe_cost_reset_and_snapshot() {
        let cost = ProbeCost::new();
        cost.add_request();
        cost.add_tokens(3, 4);
        cost.inc_429();
        assert_eq!(cost.snapshot(), (1, 3, 4));
        assert_eq!(cost.count_429(), 1);
        assert!(!cost.rate_limit_storm(6));
        cost.reset();
        assert_eq!(cost.snapshot(), (0, 0, 0));
        assert_eq!(cost.count_429(), 0);
        for _ in 0..6 {
            cost.inc_429();
        }
        assert!(cost.rate_limit_storm(6));
        assert!(!cost.rate_limit_storm(7));
    }

    #[test]
    fn probe_target_headers_include_signature_only_for_tuanjie() {
        let t = ProbeTarget::tuanjie("sk-x", "https://h");
        let h = t.request_headers("/v1/chat/completions", 1700000000);
        assert!(h.contains_key("x-litellm-session-id"));
        assert!(h.contains_key("X-Codely-Signature"));
        assert_eq!(h.get("user-agent").unwrap(), crate::ua::process_cli_user_agent());
        // 非团结渠道不带签名/会话头
        let other = ProbeTarget {
            base_url: "https://h".to_string(),
            headers: BTreeMap::new(),
            channel: "bai".to_string(),
            returned_model: std::sync::Arc::new(Mutex::new(String::new())),
        };
        let h = other.request_headers("/v1/chat/completions", 1700000000);
        assert!(!h.contains_key("X-Codely-Signature"));
        assert!(!h.contains_key("x-litellm-session-id"));
        // 自报名：空值不覆盖
        other.set_returned_model("glm-5.3");
        other.set_returned_model("");
        assert_eq!(other.returned_model(), "glm-5.3");
    }
}
