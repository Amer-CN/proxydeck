//! water_handler.rs —— `/water-probe` 端点（`handlers_extra.go:387-507` 的对应物）。
//!
//! 已迁的部分（其余如实 501，不假装已通）：
//!
//! * `GET ?history=1` —— 检测历史（内存环形 20 条，旧→新）；
//! * `GET ?channels=1` —— 八渠道**静态清单**（`ok:true` + id/name/port/strong/need_key/
//!   models/note 的字段形状；`models` 不拉取，恒空数组——见下）；
//! * `POST action=quick`（含缺省 action）—— 金丝雀探针（漂移 + 答题）+ 第一层管道探针；
//! * 其余动作（`check` / `deep` / `baseline` / `setkey`）与 `GET` 的其它形态 —— 405 / 501，
//!   文案点名属哪一块（`handlers_extra.go:509-1056` 的报告组装、渠道密钥 DPAPI 落盘）。
//!
//! ## 与 Go 的登记差异
//!
//! 1. `channels` 的 `models` 字段**不拉取**（Go 的 `buildChannels` 要逐渠道打 `/v1/models`，
//!    团结渠道还要走账号池与外部 provider 合并）——本片恒 `[]`，并用 `note` 如实标注
//!    "未拉取（Rust 侧渠道模型清单未迁）"，`ok` 仍为 `true`（渠道本身可达性未探测）；
//! 2. `quick` 里 Go 用 `loadAccessToken()` 取本地登录态再 `ProbeAccount`（内部再换 key）；
//!    Rust 侧走 [`crate::client::Client::api_key_cached`]（同一把 cli_api_key 的官方换取
//!    路径，红线 5 不换源）——**换 key 次数少一次**，金丝雀三题的请求体与 Go 逐字节同款；
//! 3. `resetProbeCost()` 在 Rust 侧清零的是 [`crate::server::AppState::probe_cost`]
//!    （Go 是包级 atomic；语义相同，见 `probes.rs` 的 `ProbeCost` 说明）。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::accounts::format_go_seconds;
use crate::server::{gobj, method_not_allowed, write_json, AppState, GJ};
use crate::water::{WaterProbeResult, CANARY_QUESTIONS};

/// 检测历史环形上限 —— `handlers_extra.go:406` 的 `>= 20`。
pub const WATER_HISTORY_CAPACITY: usize = 20;

/// 一条注水检测历史 —— `waterHistoryEntry`（`handlers_extra.go:393-400`）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WaterHistoryEntry {
    pub at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel: String,
    pub model: String,
    pub account: String,
    pub verdict_light: String,
    pub verdict_score: f64,
}

/// 检测历史（内存环形 20 条，旧→新）—— `pushWaterHistory`/`waterHistory`
/// （`handlers_extra.go:403-420`）。
#[derive(Debug, Default)]
pub struct WaterHistory {
    entries: std::sync::Mutex<Vec<WaterHistoryEntry>>,
}

impl WaterHistory {
    pub fn new() -> Self {
        WaterHistory {
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 入列（超限丢最旧）。
    pub fn push(&self, e: WaterHistoryEntry) {
        let mut g = self.entries.lock().unwrap_or_else(|x| x.into_inner());
        if g.len() >= WATER_HISTORY_CAPACITY {
            g.remove(0);
        }
        g.push(e);
    }

    /// 副本（旧→新）。
    pub fn snapshot(&self) -> Vec<WaterHistoryEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|x| x.into_inner())
            .clone()
    }
}

/// `WaterHistoryEntry` → `writeJSON` 口径（Go 用 `map[string]any` 会按**字节序**排键，
/// 这里逐字段按同一顺序写；`channel` 是 `omitempty`）。
pub fn history_to_gj(e: &WaterHistoryEntry) -> GJ {
    let mut pairs: Vec<(&str, GJ)> = vec![("at", GJ::Str(e.at.clone()))];
    if !e.channel.is_empty() {
        pairs.push(("channel", GJ::Str(e.channel.clone())));
    }
    pairs.push(("model", GJ::Str(e.model.clone())));
    pairs.push(("account", GJ::Str(e.account.clone())));
    pairs.push(("verdict_light", GJ::Str(e.verdict_light.clone())));
    pairs.push(("verdict_score", GJ::Float(e.verdict_score)));
    gobj(pairs)
}

/// `WaterProbeResult` → `writeJSON` 口径（Go 的 `json.Marshal` 走**结构体声明序**：
/// `user_id/model/pass/prompt_tokens/base_tokens/drift_pct/answers?/unanswered?/detail?/at`）。
pub fn probe_result_to_gj(r: &WaterProbeResult) -> GJ {
    let answers = GJ::Obj(
        r.answers
            .iter()
            .map(|(k, v)| (k.clone(), GJ::Bool(*v)))
            .collect(),
    );
    let mut pairs: Vec<(&str, GJ)> = vec![
        ("user_id", GJ::Str(r.user_id.clone())),
        ("model", GJ::Str(r.model.clone())),
        ("pass", GJ::Bool(r.pass)),
        ("prompt_tokens", GJ::Int(r.prompt_tokens)),
        ("base_tokens", GJ::Int(r.base_tokens)),
        ("drift_pct", GJ::Float(r.drift_pct)),
    ];
    // Go：`answers` 无 omitempty（空 map 也写 `{}`）；`unanswered`/`detail` 是 omitempty
    pairs.push(("answers", answers));
    if !r.unanswered.is_empty() {
        pairs.push((
            "unanswered",
            GJ::Obj(
                r.unanswered
                    .iter()
                    .map(|(k, v)| (k.clone(), GJ::Str(v.clone())))
                    .collect(),
            ),
        ));
    }
    if !r.detail.is_empty() {
        pairs.push(("detail", GJ::Str(r.detail.clone())));
    }
    pairs.push(("at", GJ::Str(r.at.clone())));
    crate::server::gobj_ordered(pairs)
}

/// `probeResult` 列表 → JSON（Go 走结构体声明序 + `omitempty`）。
pub fn probe_list_to_gj(list: &[crate::probes::ProbeResult]) -> GJ {
    GJ::Arr(
        list.iter()
            .map(|p| {
                let mut pairs: Vec<(&str, GJ)> = vec![
                    ("name", GJ::Str(p.name.clone())),
                    ("value", GJ::Str(p.value.clone())),
                    ("status", GJ::Str(p.status.clone())),
                ];
                if !p.note.is_empty() {
                    pairs.push(("note", GJ::Str(p.note.clone())));
                }
                if p.http_status != 0 {
                    pairs.push(("http_status", GJ::Int(p.http_status as i64)));
                }
                crate::server::gobj_ordered(pairs)
            })
            .collect(),
    )
}

/// `GET|POST /water-probe`（`handleWaterProbe`，`handlers_extra.go:432-507`）。
pub async fn handle_water_probe(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let uri: Uri = req.uri().clone();
    let method = req.method().clone();
    let query = uri.query().unwrap_or("").to_string();
    if method == Method::GET {
        if query_param(&query, "history").as_deref() == Some("1") {
            let list: Vec<GJ> = st.water_history.snapshot().iter().map(history_to_gj).collect();
            return write_json(&gobj(vec![
                ("ok", GJ::Bool(true)),
                ("history", GJ::Arr(list)),
            ]));
        }
        if query_param(&query, "channels").as_deref() == Some("1") {
            return channels_static(&st);
        }
        return method_not_allowed(None);
    }
    if method != Method::POST {
        return method_not_allowed(None);
    }

    // 检测动作入口清零成本与 429 限流计数（窗口 = 一次检测）
    st.probe_cost.reset();

    let body = axum::body::to_bytes(req.into_body(), crate::server::MAX_BODY_BYTES)
        .await
        .unwrap_or_default();
    let Some(reqj) = decode_water_body(&body) else {
        return write_json(&gobj(vec![
            ("ok", GJ::Bool(false)),
            ("msg", GJ::Str("请求体解析失败".to_string())),
        ]));
    };
    let action = reqj
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let user_id = reqj
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut model = reqj
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let channel = reqj
        .get("channel")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut samples = reqj.get("samples").and_then(|v| v.as_i64()).unwrap_or(0);
    if model.is_empty() {
        model = "GLM-5.3".to_string(); // 默认探测最贵的
    }
    if samples <= 0 {
        samples = 60;
    }

    match action.as_str() {
        "check" => write_not_implemented(
            "action=check（一键全流程 + 报告组装）属后续小轮：报告项/限流早停报告/建基准标注在 handlers_extra.go:509-1056，本片只迁 quick",
        ),
        "deep" | "baseline" => write_not_implemented(
            "action=deep/baseline（分布采样 + 与基准库比对 + 落盘建锚）属后续小轮（probes.go 的 collectDistSamples 与 baseline 采集编排）",
        ),
        "setkey" => write_not_implemented(
            "action=setkey（渠道密钥 DPAPI 落盘）未迁：SaveChannelKey/LoadChannelKeys 依赖 channels.go 的 DPAPI 面",
        ),
        // quick（含缺省 action，旧调用兼容）
        _ => water_quick(&st, &user_id, &model, &channel, samples).await,
    }
}

/// `action=quick`（`handlers_extra.go:489-506`）。
async fn water_quick(
    st: &Arc<AppState>,
    user_id: &str,
    model: &str,
    channel: &str,
    _samples: i64,
) -> Response {
    let key = match st.client.api_key_cached().await {
        Ok(k) => k,
        Err(e) => {
            return write_json(&gobj(vec![
                ("ok", GJ::Bool(false)),
                ("msg", GJ::Str(crate::server::go_err_text(&e))),
            ]))
        }
    };
    let account = if user_id.is_empty() { "local" } else { user_id };
    let target = crate::probes::ProbeTarget::tuanjie(&key, st.client.litellm_base());
    // 指纹题出错 → 整个探针失败（Go 硬返回 error；quick 无 Go 对应物，回可读错误，
    // 不记历史——传输失败不是注水信号）。
    let res = match st
        .water
        .probe_account_with_target(&st.probe_cost, st.client.http(), &target, account, model)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return write_json(&gobj(vec![
                ("ok", GJ::Bool(false)),
                ("msg", GJ::Str(format!("注水探针失败：{e}"))),
            ]))
        }
    };
    let probes = crate::probes::run_pipeline_probes(
        st.client.http(),
        &target,
        model,
        &st.probe_cost,
        &crate::probes::PROBE_RETRY_BACKOFF,
        crate::probes::RATE_LIMIT_STORM_THRESHOLD,
        crate::client::now_unix(),
    )
    .await;
    // 检测历史入列（Go 在 check 路径入列；quick 也记一条便于 GUI 展示）
    st.water_history.push(WaterHistoryEntry {
        at: format_go_seconds(chrono::Local::now()),
        channel: channel.to_string(),
        model: model.to_string(),
        account: account.to_string(),
        verdict_light: if res.pass { "green" } else { "red" }.to_string(),
        verdict_score: 0.0,
    });
    write_json(&gobj(vec![
        ("ok", GJ::Bool(true)),
        ("action", GJ::Str("quick".to_string())),
        (
            "results",
            GJ::Arr(vec![probe_result_to_gj(&res)]),
        ),
        ("probes", probe_list_to_gj(&probes)),
    ]))
}

/// `GET ?channels=1` 的静态清单（见模块头登记差异 1：`models` 不拉取）。
fn channels_static(st: &Arc<AppState>) -> Response {
    // `need_key`：`handlers_extra` 用的是 `c.BearerFrom == "config" && keys[c.ID] == ""`
    // （`channels.go:175`）。`LoadChannelKeys` 的**完整**面（DPAPI + `tuanjie-water-channels.json`
    // 的全部字段）未迁，但「该渠道有没有配 key」这条判据只用文件里的 `id`/`key`，
    // 故按同一份文件读一次（读不到 = 没有 key）——不是猜的。
    let keys = read_channel_keys();
    let list: Vec<GJ> = crate::channels::WATER_CHANNELS
        .iter()
        .map(|c| {
            let need_key = c.bearer_from == "config"
                && keys.get(c.id).map(|k| k.is_empty()).unwrap_or(true);
            crate::server::gobj_ordered(vec![
                ("id", GJ::Str(c.id.to_string())),
                ("name", GJ::Str(c.name.to_string())),
                ("port", GJ::Int(c.port as i64)),
                ("ok", GJ::Bool(true)),
                ("strong", GJ::Bool(crate::channels::is_strong_channel(c.id))),
                ("need_key", GJ::Bool(need_key)),
                ("models", GJ::Arr(Vec::new())),
                (
                    "note",
                    GJ::Str("models 未拉取（Rust 侧渠道模型清单未迁）".to_string()),
                ),
            ])
        })
        .collect();
    let _ = st;
    write_json(&gobj(vec![
        ("ok", GJ::Bool(true)),
        ("channels", GJ::Arr(list)),
    ]))
}

/// 读渠道密钥表（`LoadChannelKeys`，`channels.go`）：数据目录的
/// `tuanjie-water-channels.json`，DPAPI 解密后逐条取 `id`/`key`；
/// 缺文件/坏 JSON 一律当**空表**（与 Go 的「读不到 = 没有 key」同款）。
///
/// 逐条容忍缺字段（`id`/`key` 缺失即跳过该条），与 `matrix.rs` 的同名口径一致。
pub fn read_channel_keys() -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let path = crate::pacing::data_path(crate::channels::CHANNELS_CONFIG_FILE);
    let Ok(raw) = crate::crypto::read_file(&path) else {
        return out;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return out;
    };
    let Some(arr) = v.as_array() else {
        return out;
    };
    for item in arr {
        let Some(o) = item.as_object() else { continue };
        let id = o.get("id").and_then(|x| x.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let key = o.get("key").and_then(|x| x.as_str()).unwrap_or("");
        out.insert(id.to_string(), key.to_string());
    }
    out
}

fn write_not_implemented(msg: &str) -> Response {
    crate::server::write_err(StatusCode::NOT_IMPLEMENTED, msg).into_response()
}

/// `json.NewDecoder(r.Body).Decode(&req)` 的字段类型判据 —— 与 `/accounts` 同款
/// （Go 对类型不匹配整份失败；未知字段忽略）。
fn decode_water_body(body: &[u8]) -> Option<serde_json::Value> {
    if body.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let o = v.as_object()?;
    for k in ["action", "user_id", "model", "channel", "channel_key_id", "channel_key"] {
        if let Some(f) = o.get(k) {
            if !f.is_string() && !f.is_null() {
                return None;
            }
        }
    }
    if let Some(f) = o.get("samples") {
        if !f.is_number() && !f.is_null() {
            return None;
        }
    }
    Some(v)
}

fn query_param(q: &str, name: &str) -> Option<String> {
    crate::server::query_param(q, name)
}

/// 金丝雀题库的只读视图（GUI 展示用；生产不带 —— 只为门禁能引用题面）。
pub fn canary_ids() -> Vec<&'static str> {
    CANARY_QUESTIONS.iter().map(|q| q.id).collect()
}

/// 429 限流风暴是否已达阈值（Go `rateLimitStorm()` 的等价读出口）。
pub fn rate_limit_storm(st: &AppState, threshold: i64) -> bool {
    st.probe_cost.count_429() >= threshold
}

/// 探针成本快照（Go `probeCostSnapshot()`）。
pub fn probe_cost_snapshot(st: &AppState) -> (i64, i64, i64) {
    st.probe_cost.snapshot()
}

/// 429 计数（`probe429Count.Load()`）。
pub fn probe_429_count(st: &AppState) -> i64 {
    st.probe_cost.count_429()
}

/// 便于门禁的 `Load`（避免测试直接持原子）。
pub fn atomic_load(v: &std::sync::atomic::AtomicI64) -> i64 {
    v.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_ring_of_20_oldest_dropped() {
        let h = WaterHistory::new();
        for i in 0..25 {
            h.push(WaterHistoryEntry {
                at: format!("t{i}"),
                model: "m".into(),
                account: "a".into(),
                verdict_light: "green".into(),
                ..Default::default()
            });
        }
        let snap = h.snapshot();
        assert_eq!(snap.len(), WATER_HISTORY_CAPACITY);
        assert_eq!(snap[0].at, "t5", "最旧的被丢");
        assert_eq!(snap[19].at, "t24");
    }

    #[test]
    fn canary_ids_match_go_table() {
        assert_eq!(canary_ids(), vec!["repeat", "math", "knowledge"]);
        assert!(!CANARY_QUESTIONS[0].prompt.is_empty());
    }

    /// `need_key` 的判据（`channels.go:175`）：只在**没配 key** 时为真。
    /// 这条测试只看判据本身（不碰真实数据目录——`read_channel_keys` 的读入由
    /// `p8c_diff` 的 registry/文件面覆盖）。
    #[test]
    fn need_key_rule_matches_go() {
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("bai".to_string(), "sk-x".to_string());
        for c in crate::channels::WATER_CHANNELS.iter() {
            let need = c.bearer_from == "config"
                && keys.get(c.id).map(|k| k.is_empty()).unwrap_or(true);
            match c.id {
                // 只有 bai 配了 key ⇒ 它的 need_key 为 false；tokenrouter 没配 ⇒ true
                "bai" => assert!(!need, "bai 配了 key 就不该说需要"),
                "tokenrouter" => assert!(need, "tokenrouter 没配 key 就该说需要"),
                // 非 config 渠道恒 false
                _ => assert!(!need, "{} 不该需要 key", c.id),
            }
        }
    }
}
