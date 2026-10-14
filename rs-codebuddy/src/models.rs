//! models.rs —— `/v1/models` 与 `/model/info`，对应 Go `internal/codebuddy/server.go`
//! 的 `modelCandidates` / `modelMeta` / `Models` / `probeOne` / `handleModels` / `handleModelInfo`。
//!
//! 机制要点（Go 注释里的实测结论，逐条照抄行为）：
//! * 上游**没有**模型列表端点（`/models`、`/v1/models`、`/v2/models` 全 404）→ 候选池硬编码，
//!   每小时**在线探测过滤**一次（TTL 1h）：探测只能把池内不可用的筛掉，**发现不了上游新增**；
//! * 探测 = 并行最小流式请求（并发 10，单条 25s 超时），「读到数据」即认为后端接受该模型名；
//! * 过滤结果**按候选池顺序**输出（稳定），全失败则回退整池（列表不为空）；
//! * INTL `NoProbe`：直接返回候选池，零上游（付费模型探测会持续扣费）；
//! * `/model/info` 的 `modelMeta` **只填实测值**：未命中 `modelMeta` 的模型不硬造字段
//!   （`note` 缺省补空串，其余留给前端如实显示「未核实」）；INTL 不硬造元数据（空 meta）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use crate::gjson;
use crate::AppState;

/// 探测结果缓存 TTL（Go `Models` 里的 `time.Since(s.probeAt) < time.Hour`）。
pub const PROBE_TTL: Duration = Duration::from_secs(3600);
/// 单条探测请求的超时（Go `probeOne` 的 `context.WithTimeout(ctx, 25*time.Second)`）。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
/// 探测并发上限（Go 的 `sem := make(chan struct{}, 10)`）。
pub const PROBE_CONCURRENCY: usize = 10;

/// Go `modelMeta`（**只填实测值**；查无实据的上下文/最大输出一律缺省不填）。
/// 每项是 `(键, 值)`；写出时按键排序（Go 的 map marshal）。
pub fn meta_entries(model: &str) -> Vec<(&'static str, Value)> {
    let reasoning_all = "off/low/medium/high/max";
    let mut out: Vec<(&'static str, Value)> = Vec::new();
    let mut push = |k: &'static str, v: Value| out.push((k, v));
    match model {
        "auto" => {
            push("reasoning", gjson::s("low/medium/high/max"));
            push("note", gjson::s("后端自动路由到合适模型"));
        }
        "hy4-preview" | "hy3" | "hy3-preview" | "hy3-preview-agent" | "glm-5.3-flash"
        | "glm-5.1" | "minimax-m3" | "minimax-m3-pay" | "kimi-k3" | "kimi-k2.7" | "kimi-k2.6"
        | "kimi-k2.5" => {
            push("reasoning", gjson::s(reasoning_all));
        }
        "glm-5.3" => {
            push("reasoning", gjson::s(reasoning_all));
            push("note", gjson::s("新模型"));
        }
        "glm-5.2" => {
            push("maxInput", Value::Number(1048576.into()));
            push("reasoning", gjson::s(reasoning_all));
            push("note", gjson::s("上下文实测 1M"));
        }
        "glm-5v-turbo" => {
            push("reasoning", gjson::s(reasoning_all));
            push("note", gjson::s("视觉模型"));
        }
        "deepseek-v4.1-flash" => {
            push("reasoning", gjson::s("low/medium/high/max"));
            push("note", gjson::s("新模型 · 官方 0.03x 独家优惠"));
        }
        "deepseek-v4-pro" => {
            push("maxInput", Value::Number(1048576.into()));
            push("reasoning", gjson::s("low/medium/high/max"));
            push("note", gjson::s("上下文实测 1M"));
        }
        "deepseek-v4-flash" => {
            push("maxInput", Value::Number(1048576.into()));
            push("reasoning", gjson::s("low/medium/high/max"));
            push(
                "note",
                gjson::s("思考深：思考消耗输出预算，max_tokens 建议 ≥8000"),
            );
        }
        "deepseek-v3.2" => {
            push("reasoning", gjson::s("low/medium/high/max"));
        }
        _ => {}
    }
    out
}

/// Go `reasoningDefaultFor`：缺省 `reasoning_effort` 注入判定（纯逻辑，便于单测）。
/// 返回空串表示不注入：模型无元数据 / reasoning 矩阵缺失，或配置档位不在该模型矩阵。
pub fn reasoning_default_for(model: &str, cfg: &str) -> String {
    if cfg.is_empty() {
        return String::new();
    }
    match reasoning_matrix(model) {
        Some(matrix) if matrix.split('/').any(|tier| tier == cfg) => cfg.to_string(),
        _ => String::new(),
    }
}

/// 该模型的 `reasoning` 矩阵（`modelMeta` 里的那一项）；无元数据给 `None`。
/// `/model/info` 与缺省档注入（`/v1/chat/completions` 的 CN 路径）共用同一份表。
pub fn reasoning_matrix(model: &str) -> Option<String> {
    for (k, v) in meta_entries(model) {
        if k == "reasoning" {
            return v.as_str().map(|s| s.to_string());
        }
    }
    None
}

/// Go `Models`：候选池 + 每小时在线探测过滤（TTL 1h）。
pub async fn models(state: &Arc<AppState>) -> Vec<String> {
    let cfg = state.region.cfg();
    let candidates: Vec<String> = cfg.candidates.iter().map(|s| s.to_string()).collect();
    if cfg.no_probe {
        return candidates;
    }
    {
        let cache = state.probe_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.at.map(|t| t.elapsed() < PROBE_TTL).unwrap_or(false) && !cache.list.is_empty() {
            return cache.list.clone();
        }
    }
    if let Ok(hdr) = state.cred.headers() {
        let ok_set = probe_all(state, &hdr, &candidates).await;
        if !ok_set.is_empty() {
            // 按候选池顺序输出（稳定）
            let list: Vec<String> = candidates
                .iter()
                .filter(|m| ok_set.contains(*m))
                .cloned()
                .collect();
            let mut cache = state.probe_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.list = list.clone();
            cache.at = Some(Instant::now());
            println!(
                "[codebuddy] 模型探测完成 region={}: {}/{} 可用",
                state.region.as_str(),
                list.len(),
                candidates.len()
            );
            return list;
        }
    }
    // 全部失败（网络异常）→ 回退候选池，保证列表不空
    let mut cache = state.probe_cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.list = candidates.clone();
    cache.at = Some(Instant::now());
    candidates
}

async fn probe_all(state: &Arc<AppState>, hdr: &reqwest::header::HeaderMap, candidates: &[String]) -> Vec<String> {
    use futures_util::stream::{self, StreamExt};
    let results: Vec<(String, bool)> = stream::iter(candidates.iter().cloned())
        .map(|m| {
            let st = state.clone();
            let hdr = hdr.clone();
            async move {
                let ok = probe_one(&st, &hdr, &m).await;
                (m, ok)
            }
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;
    results
        .into_iter()
        .filter(|(_, ok)| *ok)
        .map(|(m, _)| m)
        .collect()
}

/// Go `probeOne`：发最小流式请求探测模型是否可用（读到数据即认为后端接受该模型名）。
pub async fn probe_one(state: &Arc<AppState>, hdr: &reqwest::header::HeaderMap, model: &str) -> bool {
    let cfg = state.region.cfg();
    let mut msgs: Vec<Value> = Vec::new();
    if cfg.force_system {
        msgs.push(gjson::obj(vec![
            ("role", gjson::s("system")),
            ("content", gjson::s(crate::region::INTL_DEFAULT_SYSTEM)),
        ]));
    }
    msgs.push(gjson::obj(vec![
        ("role", gjson::s("user")),
        ("content", gjson::s("hi")),
    ]));
    let body = gjson::obj(vec![
        ("model", gjson::s(model)),
        ("stream", Value::Bool(true)),
        ("messages", Value::Array(msgs)),
    ]);
    let url = format!("{}/v2/chat/completions", state.base_url);
    let req = state
        .http
        .post(&url)
        .headers(hdr.clone())
        .body(gjson::to_go_bytes(&body));
    let resp = match tokio::time::timeout(PROBE_TIMEOUT, req.send()).await {
        Ok(Ok(r)) => r,
        _ => return false,
    };
    if resp.status().as_u16() != 200 {
        return false;
    }
    let mut resp = resp;
    match tokio::time::timeout(PROBE_TIMEOUT, resp.chunk()).await {
        // Go：`if _, err := resp.Body.Read(buf); err != nil && err.Error() == "EOF" { return false }`
        // ——只有「读到 EOF（一个字节都没有）」才算不可用；非 EOF 错误按可用处理（逐字照抄）。
        Ok(Ok(Some(b))) => !b.is_empty(),
        Ok(Ok(None)) => false,
        Ok(Err(_)) => true,
        Err(_) => true,
    }
}

/// Go `handleModels`（不看方法）。
pub async fn handle_models(State(st): State<Arc<AppState>>) -> Response {
    let list = models(&st).await;
    let data: Vec<Value> = list
        .iter()
        .map(|m| {
            gjson::obj(vec![
                ("id", gjson::s(m)),
                ("object", gjson::s("model")),
                ("created", Value::Number(1700000000i64.into())),
                ("owned_by", gjson::s("codebuddy")),
            ])
        })
        .collect();
    json_response(gjson::obj(vec![
        ("object", gjson::s("list")),
        ("data", Value::Array(data)),
    ]))
}

/// Go `handleModelInfo`（不看方法）。
pub async fn handle_model_info(State(st): State<Arc<AppState>>) -> Response {
    let list = models(&st).await;
    let mut out: Vec<Value> = Vec::with_capacity(list.len());
    for m in list {
        // INTL 模型矩阵未经实测，不硬造元数据：CN 的 modelMeta 不出国际版。
        let entries = if st.region == crate::region::Region::Cn {
            meta_entries(&m)
        } else {
            Vec::new()
        };
        let mut entry: Map<String, Value> = Map::new();
        entry.insert("name".to_string(), gjson::s(&m));
        for (k, v) in entries {
            entry.insert(k.to_string(), v);
        }
        if !entry.contains_key("note") {
            entry.insert("note".to_string(), gjson::s(""));
        }
        out.push(Value::Object(entry));
    }
    json_response(Value::Array(out))
}

/// `writeJSON`：`Content-Type: application/json` + `json.Encoder` 的末尾换行。
pub fn json_response(v: Value) -> Response {
    let mut body = gjson::to_go(&v);
    body.push('\n');
    (
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}
