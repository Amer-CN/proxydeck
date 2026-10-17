//! water.rs —— 注水检测（`internal/tuanjie/water.go` 的逐条对应物）。
//!
//! 三块内容：
//!
//! 1. **被动观测**（[`WaterCheck::record_passive`]）：转发时比对请求模型名与上游响应
//!    `model` 字段，归一化后不同即记一条（环形 50 条）；
//! 2. **别名解析观测**（[`WaterCheck::record_resolution`]）：归一化请求名 → 最近返回名，
//!    首次落基线、无变化不落盘、变化落盘并回旧值（官方改映射当天日志可见）；
//! 3. **金丝雀探针**（[`WaterCheck::probe_account_target`] / [`WaterCheck::probe_account`]）：
//!    三题固定题库 + tokenizer 指纹基线（漂移超 8% 判换模型）。未作答不算答错，
//!    独立记 `unanswered`。
//!
//! 落盘形态 `tuanjie-water.json`（`json.MarshalIndent`，0600）与字段名逐字对应 Go 的
//! `WaterCheck`（`baseline` / `passive` / `resolved`）——**缩进两格**、字段按 Go 的
//! 结构体声明序，故这里自己写出（不靠 serde 的默认序列化，以免键序/缩进口径漂移）。
//!
//! ## 差异（登记）
//!
//! 1. `waterFilePath()` 取 `exeDirForAccounts()` = `appdir.DataDir()`；本 crate 用
//!    [`crate::pacing::data_path`]（同一条判定链，P8b 已登记）；
//! 2. `containsCI` 的 Go 实现是**字节级** `normalizeModelName` 后的子串查找；
//!    Rust 侧同款（[`contains_ci`]），不做 Unicode 折叠——两者一致；
//! 3. `probe_account` 里换取 key 的网络路径依赖 [`crate::client`] 的
//!    `fetch_key_with_token` 等价物；本片只迁**探针编排与记账**，
//!    `fetchKeyWithToken` 的独立直连版本用 client 的同一实现（见函数内说明）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Local;
use serde::Deserialize;
use serde::Serialize;

use crate::accounts::format_go_seconds;
use crate::probes::{ProbeCost, ProbeTarget};

/// `time.Now().Format("2006-01-02 15:04:05")`（`water.go:109/178/247` 的 `At`）。
pub fn stamp_now() -> String {
    format_go_seconds(Local::now())
}

/// 一道金丝雀题的 messages（Go 用 `[]map[string]any{{"role":"user","content":…}}`）。
fn one_user_msg(text: &str) -> Vec<serde_json::Value> {
    vec![serde_json::json!({"role": "user", "content": text})]
}

/// 空正文重试的 extra —— `map[string]any{"max_tokens": 800}`（`water.go:196/265`）。
fn canary_retry_extra() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "max_tokens".to_string(),
        serde_json::Value::Number(serde_json::Number::from(CANARY_RETRY_MAX_TOKENS)),
    );
    m
}

/// 指纹漂移阈值（%）—— `water.go:28`。
pub const FINGERPRINT_DRIFT_PCT: f64 = 8.0;

/// 被动事件环形上限 —— `water.go:111` 的 `len(w.Passive) > 50`。
pub const PASSIVE_CAPACITY: usize = 50;

/// 空正文重试用的大预算 —— `water.go:196` 的 `map[string]any{"max_tokens": 800}`。
pub const CANARY_RETRY_MAX_TOKENS: i64 = 800;

/// 落盘文件 —— `water.go:73`。
pub const WATER_FILE: &str = "tuanjie-water.json";

/// 金丝雀题库（**固定不动！** prompt_tokens 指纹依赖题面逐字节稳定）——
/// `canaryQuestions`（`water.go:31-40`）。
pub const CANARY_QUESTIONS: [CanaryQuestion; 3] = [
    CanaryQuestion {
        id: "repeat",
        title: "指令复读",
        prompt: "请原样输出以下内容，不要添加任何解释或标点：QX7-8842 光子 torpedo 0.73 E=mc^2",
        expect: "QX7-8842",
    },
    CanaryQuestion {
        id: "math",
        title: "算术",
        prompt: "计算 17×23+5 的值。只输出最终数字，不要输出任何其他字符。",
        expect: "396",
    },
    CanaryQuestion {
        id: "knowledge",
        title: "常识",
        prompt: "中国的陆地国土面积大约是多少万平方公里？只输出数字，保留整数。",
        expect: "960",
    },
];

/// 一道金丝雀题 —— `canaryQuestions` 的元素。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanaryQuestion {
    pub id: &'static str,
    pub title: &'static str,
    pub prompt: &'static str,
    pub expect: &'static str,
}

/// 单账号单模型的探针结果 —— `WaterProbeResult`（`water.go:43-54`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WaterProbeResult {
    pub user_id: String,
    pub model: String,
    pub pass: bool,
    /// 本次指纹。
    pub prompt_tokens: i64,
    /// 历史基线（0 = 首次无基线）。
    pub base_tokens: i64,
    pub drift_pct: f64,
    /// 各题对错（仅实际作答的题）。
    pub answers: BTreeMap<String, bool>,
    /// 未作答各题及原因（网络/上游错误、空正文重试后仍空）——**不算答错**。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unanswered: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    pub at: String,
}

/// 被动观测记录 —— `passiveEvent`（`water.go:57-62`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassiveEvent {
    pub at: String,
    pub model: String,
    pub returned: String,
    pub user_id: String,
}

/// 落盘形态 —— `WaterCheck` 的三字段（`water.go:65-71`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WaterFile {
    #[serde(default)]
    baseline: BTreeMap<String, i64>,
    #[serde(default)]
    passive: Vec<PassiveEvent>,
    #[serde(default)]
    resolved: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct WaterInner {
    baseline: BTreeMap<String, i64>,
    passive: Vec<PassiveEvent>,
    resolved: BTreeMap<String, String>,
}

/// 注水检测器 —— `WaterCheck`（`water.go:65-71`）。
///
/// `path == None` = 不落盘（测试/不写真实数据目录；Go 侧 `path == ""` 时
/// `os.WriteFile("")` 必然失败、行为等价于不落盘）。
#[derive(Debug)]
pub struct WaterCheck {
    inner: Mutex<WaterInner>,
    path: Option<PathBuf>,
}

impl Default for WaterCheck {
    fn default() -> Self {
        Self::new()
    }
}

impl WaterCheck {
    /// `LoadWater`（`water.go:76-88`）：从磁盘恢复。
    pub fn new() -> Self {
        let path = Some(crate::pacing::data_path(WATER_FILE));
        Self::load(path)
    }

    /// 指定路径加载（`path=None` 不读不写）。
    pub fn load(path: Option<PathBuf>) -> Self {
        let w = WaterCheck {
            inner: Mutex::new(WaterInner::default()),
            path,
        };
        if let Some(p) = w.path.clone() {
            if let Ok(b) = std::fs::read(&p) {
                if let Ok(f) = serde_json::from_slice::<WaterFile>(&b) {
                    let mut g = w.lock();
                    g.baseline = f.baseline;
                    g.passive = f.passive;
                    g.resolved = f.resolved;
                }
            }
        }
        w
    }

    /// 不落盘的实例（单测/测试缝）。
    pub fn detached() -> Self {
        WaterCheck {
            inner: Mutex::new(WaterInner::default()),
            path: None,
        }
    }

    /// 覆盖落盘路径（测试缝）。
    pub fn with_path(mut self, path: Option<PathBuf>) -> Self {
        self.path = path;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WaterInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `save`（`water.go:90-96`）：`MarshalIndent` + 0600，失败静默（Go 同样忽略错误）。
    fn save_locked(&self, g: &WaterInner) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, water_file_bytes(g));
    }

    /// 被动观测 —— `RecordPassive`（`water.go:99-115`）。
    pub fn record_passive(&self, requested: &str, returned: &str, user_id: &str) {
        if requested.is_empty() || returned.is_empty() {
            return;
        }
        if normalize_model_name(requested) == normalize_model_name(returned) {
            return;
        }
        let mut g = self.lock();
        g.passive.push(PassiveEvent {
            at: stamp_now(),
            model: requested.to_string(),
            returned: returned.to_string(),
            user_id: user_id.to_string(),
        });
        if g.passive.len() > PASSIVE_CAPACITY {
            let drop = g.passive.len() - PASSIVE_CAPACITY;
            g.passive.drain(..drop);
        }
        self.save_locked(&g);
    }

    /// 别名解析观测 —— `RecordResolution`（`water.go:121-141`）。
    ///
    /// 返回 `(旧值, 是否变化)`：直连真实模型（归一化相等）返回 `("", false)`；
    /// 首次落基线（旧值为空、changed=true）；与上次相同不再落盘。
    pub fn record_resolution(&self, requested: &str, returned: &str) -> (String, bool) {
        if requested.is_empty() || returned.is_empty() {
            return (String::new(), false);
        }
        let key = normalize_model_name(requested);
        let val = normalize_model_name(returned);
        if key == val {
            return (String::new(), false);
        }
        let mut g = self.lock();
        let from = g.resolved.get(&key).cloned().unwrap_or_default();
        let changed = from != val;
        if changed {
            g.resolved.insert(key, val);
            self.save_locked(&g);
        }
        (from, changed)
    }

    /// 被动观测记录 —— `PassiveEvents`（`water.go:161-167`）。
    pub fn passive_events(&self) -> Vec<PassiveEvent> {
        self.lock().passive.clone()
    }

    /// 别名解析表快照（门禁用）。
    pub fn resolved_map(&self) -> BTreeMap<String, String> {
        self.lock().resolved.clone()
    }

    /// 指纹基线表快照（门禁用）。
    pub fn baseline_map(&self) -> BTreeMap<String, i64> {
        self.lock().baseline.clone()
    }

    /// 记录指纹基线并回 `(base, drift_pct)` —— `ProbeAccount` 的基线段
    /// （`water.go:277-292`）。
    fn note_fingerprint(&self, user_id: &str, model: &str, prompt_tokens: i64) -> (i64, f64) {
        let mut g = self.lock();
        let key = format!("{}|{}", user_id, model);
        let base = *g.baseline.get(&key).unwrap_or(&0);
        let (base_tok, drift_pct) = if base > 0 {
            let mut drift = (prompt_tokens - base) as f64 / base as f64 * 100.0;
            if drift < 0.0 {
                drift = -drift;
            }
            (base, ((drift * 10.0) as i64) as f64 / 10.0)
        } else {
            g.baseline.insert(key, prompt_tokens);
            (prompt_tokens, 0.0)
        };
        self.save_locked(&g);
        (base_tok, drift_pct)
    }

    /// 渠道无关版金丝雀探针 —— `ProbeAccountTarget`（`water.go:174-231`）。
    pub async fn probe_account_target(
        &self,
        cost: &ProbeCost,
        client: &reqwest::Client,
        target: &ProbeTarget,
        user_id: &str,
        model: &str,
    ) -> WaterProbeResult {
        let mut result = WaterProbeResult {
            user_id: user_id.to_string(),
            model: model.to_string(),
            at: stamp_now(),
            ..Default::default()
        };
        for (i, q) in CANARY_QUESTIONS.iter().enumerate() {
            let msgs = one_user_msg(q.prompt);
            let call = crate::probes::probe_call(
                client,
                target,
                model,
                &msgs,
                None,
                cost,
                &crate::probes::PROBE_RETRY_BACKOFF,
                crate::client::now_unix(),
            )
            .await;
            if let Some(err) = &call.err {
                result
                    .unanswered
                    .insert(q.id.to_string(), format!("未作答（上游请求失败：{err}）"));
                continue;
            }
            if call.status != 200 {
                let te = if call.err_text.is_empty() {
                    format!("HTTP {}", call.status)
                } else {
                    call.err_text.clone()
                };
                result
                    .unanswered
                    .insert(q.id.to_string(), format!("未作答（上游未返回正文：{te}）"));
                continue;
            }
            if i == 0 {
                result.prompt_tokens = call.prompt_tokens; // 指纹题：只记 prompt_tokens
            }
            let (mut _finish, mut answer) = (call.finish.clone(), call.content.clone());
            if _finish == "length" && answer.is_empty() {
                // 思考型预算耗尽正文未出：加大预算重试一次
                let retry = crate::probes::probe_call(
                    client,
                    target,
                    model,
                    &msgs,
                    Some(&canary_retry_extra()),
                    cost,
                    &crate::probes::PROBE_RETRY_BACKOFF,
                    crate::client::now_unix(),
                )
                .await;
                let ok = retry.err.is_none()
                    && retry.status == 200
                    && !(retry.finish == "length" && retry.content.is_empty());
                if ok {
                    _finish = retry.finish;
                    answer = retry.content;
                } else {
                    let te2 = if retry.err_text.is_empty() {
                        "重试未返回正文".to_string()
                    } else {
                        retry.err_text
                    };
                    result
                        .unanswered
                        .insert(q.id.to_string(), format!("未作答（上游未返回正文：{te2}）"));
                    continue;
                }
            }
            result
                .answers
                .insert(q.id.to_string(), contains_ci(&answer, q.expect));
        }
        // 判定：作答题全对且无未作答才算过；detail 按实情拼
        result.pass = !result.answers.is_empty()
            && result.unanswered.is_empty()
            && result.answers.values().all(|ok| *ok);
        if !result.pass {
            let mut parts: Vec<String> = Vec::new();
            if result.answers.values().any(|ok| !*ok) {
                parts.push("金丝雀答题有错误".to_string());
            }
            if !result.unanswered.is_empty() {
                parts.push(format!(
                    "{} 题未作答（上游未返回正文）",
                    result.unanswered.len()
                ));
            }
            if parts.is_empty() {
                parts.push("金丝雀答题有错误".to_string());
            }
            result.detail = parts.join("；");
        }
        result
    }

    /// 单账号金丝雀探针（直连换取该账号 cli_api_key）—— `ProbeAccount`
    /// （`water.go:237-324`）。
    ///
    /// 指纹题只关心 `prompt_tokens`（正文空不影响指纹）；其余题做答案比对，
    /// 空正文加大预算重试一次仍空 → 未作答。最后做指纹基线比对与判定。
    pub async fn probe_account(
        &self,
        cost: &ProbeCost,
        client: &reqwest::Client,
        base_url: &str,
        cli_key: &str,
        user_id: &str,
        model: &str,
    ) -> Result<WaterProbeResult, String> {
        let target = ProbeTarget::tuanjie(cli_key, base_url);
        self
            .probe_account_with_target(cost, client, &target, user_id, model)
            .await
    }

    /// [`WaterCheck::probe_account`] 的内核（target 由调用方建好——
    /// 门禁用假上游 target，生产由 `tuanjieTarget` 现建）。
    ///
    /// 错误语义与 Go `water.go:237-324` 逐条一致：指纹题出错 → 整个探针返回错误
    /// （不得把 baseline 落成 0，否则首轮污染基线、后续全 PASS）；
    /// 其余题出错 → 记 `unanswered`「未作答（上游请求失败：…）」，**不是**答错。
    pub async fn probe_account_with_target(
        &self,
        cost: &ProbeCost,
        client: &reqwest::Client,
        target: &ProbeTarget,
        user_id: &str,
        model: &str,
    ) -> Result<WaterProbeResult, String> {
        let mut result = WaterProbeResult {
            user_id: user_id.to_string(),
            model: model.to_string(),
            at: stamp_now(),
            ..Default::default()
        };
        // 指纹题（repeat）：只关心 prompt_tokens（正文空不影响指纹）。
        // 出错 → 整个探针失败（Go `water.go:250-253` 硬返回 error；吞掉会把
        // baseline 落成 0，首轮污染、后续全 PASS——P8c 审查高危项）。
        // 注意：Go 的 probeOnce 对传输错**和**非 200 都返回 err（非 200 是
        // `"上游 %d"`）；Rust 的 `probe_call` 只在传输错设 `err`，非 200 只填
        // `status`——故这里两个都要看（`status != 200` 即失败，含传输错的 status=0）。
        let fp = crate::probes::probe_call(
            client,
            target,
            model,
            &one_user_msg(CANARY_QUESTIONS[0].prompt),
            None,
            cost,
            &crate::probes::PROBE_RETRY_BACKOFF,
            crate::client::now_unix(),
        )
        .await;
        if let Some(e) = probe_err_text(&fp) {
            return Err(e);
        }
        result.prompt_tokens = fp.prompt_tokens;

        for q in CANARY_QUESTIONS.iter().skip(1) {
            let msgs = one_user_msg(q.prompt);
            let r = crate::probes::probe_call(
                client,
                target,
                model,
                &msgs,
                None,
                cost,
                &crate::probes::PROBE_RETRY_BACKOFF,
                crate::client::now_unix(),
            )
            .await;
            // 出错 → 未作答（Go `water.go:257-260`），**不是**答错。
            if let Some(e) = probe_err_text(&r) {
                result.unanswered.insert(
                    q.id.to_string(),
                    format!("未作答（上游请求失败：{e}）"),
                );
                continue;
            }
            let mut answer = r.content.clone();
            if r.finish == "length" && answer.is_empty() {
                let r2 = crate::probes::probe_call(
                    client,
                    target,
                    model,
                    &msgs,
                    Some(&canary_retry_extra()),
                    cost,
                    &crate::probes::PROBE_RETRY_BACKOFF,
                    crate::client::now_unix(),
                )
                .await;
                if probe_err_text(&r2).is_none() && !(r2.finish == "length" && r2.content.is_empty()) {
                    answer = r2.content;
                } else {
                    result
                        .unanswered
                        .insert(q.id.to_string(), "未作答（上游未返回正文）".to_string());
                    continue;
                }
            }
            result
                .answers
                .insert(q.id.to_string(), contains_ci(&answer, q.expect));
        }

        // 指纹基线比对
        let (base_tok, drift_pct) =
            self.note_fingerprint(user_id, model, result.prompt_tokens);
        result.base_tokens = base_tok;
        result.drift_pct = drift_pct;

        // 判定：指纹漂移超阈值 或 任一作答题答错 → 注水嫌疑；未作答不算答错，
        // 但有未作答时本次金丝雀不算干净通过（测不了≠通过）
        let fingerprint_ok = result.base_tokens == 0 || result.drift_pct <= FINGERPRINT_DRIFT_PCT;
        let answers_ok = result.answers.values().all(|ok| *ok);
        result.pass = fingerprint_ok
            && answers_ok
            && result.unanswered.is_empty()
            && !result.answers.is_empty();
        if !result.pass {
            let mut reason = String::new();
            if !fingerprint_ok {
                reason += &format!(
                    "指纹漂移 {:.1}%（基线 {} → {}）",
                    result.drift_pct, result.base_tokens, result.prompt_tokens
                );
            }
            if !answers_ok {
                if !reason.is_empty() {
                    reason += "；";
                }
                reason += "金丝雀答题有错误";
            }
            if !result.unanswered.is_empty() {
                if !reason.is_empty() {
                    reason += "；";
                }
                reason += &format!("{} 题未作答（上游未返回正文）", result.unanswered.len());
            }
            result.detail = reason;
        }
        Ok(result)
    }
}

/// 探针一次调用是否失败（Go `probeOnce` 的 err 语义）：传输错（`err: Some`）或
/// 非 200（Go 报 `"上游 %d"`）。返回 `Some(文案)` 即失败。
fn probe_err_text(r: &crate::probes::ProbeCall) -> Option<String> {
    if let Some(e) = r.err.clone() {
        return Some(e);
    }
    if r.status != 200 {
        return Some(format!("上游 {}", r.status));
    }
    None
}

/// 归一化模型名：小写 + 去路由前缀（取最后一个 `/` 之后）—— `normalizeModelName`
/// （`water.go:144-158`）。与 [`crate::probes::normalize_model_name`] 同源，
/// 这里保留一份独立实现供注水面单测逐值对照（两处必须同值，门禁锁住）。
pub fn normalize_model_name(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.rsplit_once('/') {
        Some((_, tail)) => tail.to_string(),
        None => lower,
    }
}

/// 大小写不敏感子串查找 —— `containsCI`（`water.go:424-436`）：**先把两侧都过一遍
/// `normalizeModelName`**（小写 + 去前缀），再做字节子串查找。空串任一侧 → false。
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    if haystack.is_empty() || needle.is_empty() {
        return false;
    }
    let h = normalize_model_name(haystack);
    let n = normalize_model_name(needle);
    h.contains(&n)
}

/// `tuanjie-water.json` 的落盘字节（`json.MarshalIndent(w, "", "  ")`，**无末尾换行**）。
///
/// 逐字段按 Go `WaterCheck` 的声明序写：`baseline` → `passive` → `resolved`；
/// `passive` 非空时每个元素占多行（`MarshalIndent` 对 slice 的缩进）。
pub fn water_file_bytes(g: &WaterInner) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"{\n  \"baseline\": {");
    if g.baseline.is_empty() {
        out.extend_from_slice(b"}");
    } else {
        for (i, (k, v)) in g.baseline.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"\n    ");
            crate::gojson::write_go_string(&mut out, k);
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.to_string().as_bytes());
        }
        out.extend_from_slice(b"\n  }");
    }
    out.extend_from_slice(b",\n  \"passive\": ");
    if g.passive.is_empty() {
        out.extend_from_slice(b"[]");
    } else {
        out.extend_from_slice(b"[");
        for (i, e) in g.passive.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"\n    {\n      \"at\": ");
            crate::gojson::write_go_string(&mut out, &e.at);
            out.extend_from_slice(b",\n      \"model\": ");
            crate::gojson::write_go_string(&mut out, &e.model);
            out.extend_from_slice(b",\n      \"returned\": ");
            crate::gojson::write_go_string(&mut out, &e.returned);
            out.extend_from_slice(b",\n      \"user_id\": ");
            crate::gojson::write_go_string(&mut out, &e.user_id);
            out.extend_from_slice(b"\n    }");
        }
        out.extend_from_slice(b"\n  ]");
    }
    out.extend_from_slice(b",\n  \"resolved\": {");
    if g.resolved.is_empty() {
        out.extend_from_slice(b"}");
    } else {
        for (i, (k, v)) in g.resolved.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"\n    ");
            crate::gojson::write_go_string(&mut out, k);
            out.extend_from_slice(b": ");
            crate::gojson::write_go_string(&mut out, v);
        }
        out.extend_from_slice(b"\n  }");
    }
    out.extend_from_slice(b"\n}");
    out
}

/// 探针超时（Go `probeOnce` 里 `&http.Client{Timeout: 120s}`，`water.go:360`）。
pub const PROBE_ONCE_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_matches_go() {
        assert_eq!(normalize_model_name("Codely-Core"), "codely-core");
        assert_eq!(normalize_model_name("GLM-5.3"), "glm-5.3");
        assert_eq!(normalize_model_name("alias/team/GLM-5.3"), "glm-5.3");
        assert_eq!(normalize_model_name("z-ai/x"), "x");
        assert_eq!(normalize_model_name(""), "");
    }

    #[test]
    fn contains_ci_matches_go() {
        assert!(contains_ci("396", "396"));
        assert!(contains_ci("答案是 396。", "396"));
        assert!(contains_ci("QX7-8842", "qx7-8842"));
        assert!(!contains_ci("", "396"));
        assert!(!contains_ci("396", ""));
        assert!(!contains_ci("397", "396"));
    }

    #[test]
    fn passive_records_only_mismatch_and_caps() {
        let w = WaterCheck::detached();
        w.record_passive("codely-core", "codely-core", "u1");
        w.record_passive("Codely-Core", "alias/team/codely-core", "u1"); // 归一化相等 → 不记
        assert!(w.passive_events().is_empty());
        w.record_passive("codely-core", "glm-5.3", "u1");
        assert_eq!(w.passive_events().len(), 1);
        for i in 0..(PASSIVE_CAPACITY + 10) {
            w.record_passive("m", &format!("r{i}"), "u1");
        }
        let ev = w.passive_events();
        assert_eq!(ev.len(), PASSIVE_CAPACITY);
        assert_eq!(ev[ev.len() - 1].returned, format!("r{}", PASSIVE_CAPACITY + 9));
    }

    #[test]
    fn resolution_first_then_change_then_same() {
        let w = WaterCheck::detached();
        // 归一化相等（只差大小写）：直连真实模型，**不记**（返回空旧值 + 未变化）
        assert_eq!(w.record_resolution("glm-5.3", "GLM-5.3"), (String::new(), false));
        assert!(w.resolved_map().is_empty(), "归一化相等不落表");
        // 首次落基线（旧值为空、changed=true）
        assert_eq!(
            w.record_resolution("codely-core", "glm-5.3"),
            (String::new(), true)
        );
        // 与上次相同（归一化后同为 glm-5.3）：不落盘、changed=false，且**回上一次的值**——
        // Go 的 `from = w.Resolved[key]` 在归一化相等之前就取过了，故这里回 "glm-5.3"
        assert_eq!(
            w.record_resolution("Codely-Core", "alias/glm-5.3"),
            ("glm-5.3".to_string(), false)
        );
        // 变化：回旧值
        assert_eq!(
            w.record_resolution("codely-core", "kimi-k3"),
            ("glm-5.3".to_string(), true)
        );
        assert_eq!(
            w.resolved_map().get("codely-core").map(|s| s.as_str()),
            Some("kimi-k3")
        );
    }

    #[test]
    fn water_file_bytes_is_indent_two() {
        let v = WaterInner {
            baseline: [("uid|m".to_string(), 13i64)].into_iter().collect(),
            passive: vec![PassiveEvent {
                at: "2026-10-17 01:02:03".into(),
                model: "codely-core".into(),
                returned: "glm-5.3".into(),
                user_id: "uid".into(),
            }],
            resolved: [("codely-core".to_string(), "glm-5.3".to_string())]
                .into_iter()
                .collect(),
        };
        let s = String::from_utf8(water_file_bytes(&v)).unwrap();
        assert!(s.starts_with("{\n  \"baseline\": {"), "{s}");
        assert!(s.contains("\"uid|m\": 13"), "{s}");
        assert!(!s.ends_with('\n'), "MarshalIndent 无末尾换行");
        // 空表形态
        let empty = String::from_utf8(water_file_bytes(&WaterInner::default())).unwrap();
        assert_eq!(
            empty,
            "{\n  \"baseline\": {},\n  \"passive\": [],\n  \"resolved\": {}\n}"
        );
    }

    #[test]
    fn fingerprint_baseline_first_run_and_drift() {
        let w = WaterCheck::detached();
        assert_eq!(w.note_fingerprint("u1", "m", 100), (100, 0.0));
        assert_eq!(w.note_fingerprint("u1", "m", 100), (100, 0.0));
        // 108 → 8.0%（不超阈值）
        assert_eq!(w.note_fingerprint("u1", "m", 108), (100, 8.0));
        // 90 → 10.0%
        assert_eq!(w.note_fingerprint("u1", "m", 90), (100, 10.0));
        // 另一账号独立基线
        assert_eq!(w.note_fingerprint("u2", "m", 50), (50, 0.0));
    }

    /// 按脚本回确定性应答的裸 TCP 假上游（`Vec<(status, body)>` 按连接顺序消费，
    /// 耗尽后重复最后一条）。只读请求头+体（不校验内容）。
    fn scripted_fake(script: Vec<(u16, &'static str)>) -> String {
        let ln = std::net::TcpListener::bind("127.0.0.1:0").expect("绑假上游端口");
        let base = format!("http://{}", ln.local_addr().expect("读端口"));
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut iter = script.into_iter();
            let mut last = (200u16, "");
            for conn in ln.incoming().flatten() {
                let (status, body) = iter.next().unwrap_or(last);
                last = (status, body);
                let mut r = std::io::BufReader::new(conn.try_clone().expect("克隆句柄"));
                let mut line = String::new();
                let mut len = 0usize;
                loop {
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if line.trim().is_empty() {
                                break;
                            }
                            if line.to_lowercase().starts_with("content-length:") {
                                len = line.split(':').nth(1).unwrap_or("0").trim().parse().unwrap_or(0);
                            }
                        }
                    }
                }
                let mut body_buf = vec![0u8; len];
                let _ = std::io::Read::read_exact(&mut r, &mut body_buf);
                let mut sock = r.into_inner();
                let resp = format!(
                    "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        base
    }

    const FP_OK: &str = r#"{"choices":[{"message":{"content":"x"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":0,"total_tokens":100}}"#;

    /// P8c 审查 #2：指纹题传输失败 → 整个探针 Err（不得落 baseline 0 污染基线）。
    #[tokio::test]
    async fn probe_account_fingerprint_failure_fails_whole_probe() {
        let w = WaterCheck::detached();
        let cost = crate::probes::ProbeCost::new();
        let client = reqwest::Client::new();
        // 127.0.0.1:9（discard 端口，无监听）→ 连接拒绝，立即失败不重试。
        let target = crate::probes::ProbeTarget::tuanjie("sk-x", "http://127.0.0.1:9");
        let r = w
            .probe_account_with_target(&cost, &client, &target, "u", "m")
            .await;
        assert!(r.is_err(), "指纹题失败必须整个探针失败，实得 {r:?}");
    }

    /// P8c 审查 #2：非指纹题失败（这里用 400，快失败不重试）→ 未作答，不是答错。
    #[tokio::test]
    async fn probe_account_nonfingerprint_failure_is_unanswered_not_wrong() {
        let base = scripted_fake(vec![
            (200, FP_OK),
            (400, r#"{"error":{"message":"bad"}}"#),
        ]);
        let w = WaterCheck::detached();
        let cost = crate::probes::ProbeCost::new();
        let client = reqwest::Client::new();
        let target = crate::probes::ProbeTarget::tuanjie("sk-x", &base);
        let r = w
            .probe_account_with_target(&cost, &client, &target, "u", "m")
            .await
            .expect("非指纹失败不得整个失败");
        assert!(
            r.answers.values().all(|&v| v),
            "失败的题不得记成答错：{:?}",
            r.answers
        );
        assert!(
            !r.unanswered.is_empty(),
            "失败的题必须进未作答"
        );
        assert!(
            r.unanswered.values().all(|s| s.contains("上游请求失败")),
            "未作答须注明失败原因：{:?}",
            r.unanswered
        );
    }
}
