//! catalog.rs —— 上游模型目录（`/v3/config`），替代「硬编码候选池 + 探测」。
//!
//! 2026-11-21 实测（本机 CN 健康号，逐组合验证）：
//! ```text
//! GET /v3/config
//!   User-Agent: WorkBuddy/5.9.3 ............................ 200, 39 个模型（27 个可对话）
//!   User-Agent: WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0  200, 58 个模型（44 个可对话）
//!   User-Agent: codebuddy2openai/1.0 ...................... 400
//!   ＋再加 X-IDE-Type/Name/Version、X-Private-Data ........ 字节数完全不变
//! ```
//! 结论：**目录的丰富程度只由 UA 形态决定**，`X-IDE-*` 不参与；未批准的 UA 直接 400。
//! 所以这里用「桌面端三段 UA」读目录，与对话路径的 UA 分开管理——对话 UA 仍是单段，
//! 换它属于另一个决策，不要在这里顺手改。
//!
//! 这条端点推翻了两条旧结论（docs/model-intel.md 里已同步）：
//! 1. 「上游没有模型列表端点」——有，只是必须带对 UA 才给全量；
//! 2. 「上游新增模型必须手工入池」——目录本身就是权威清单，还能拿到倍率
//!    （`credits: "x0.08"`）、`maxInputTokens`/`maxOutputTokens`、`supportsImages`、
//!    `supportsReasoning` 与默认 `reasoning.effort`。
//!
//! 兜底纪律：读不到（网络/401/UA 被收紧/测试用假上游）一律回退旧的候选池路径，
//! 不因为一个可选增强把 `/v1/models` 变成空列表。`CODEBUDDY_CATALOG=0` 可整体关闭。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// 目录缓存 TTL。上游促销/新模型按天变，1 小时足够新，也不会把 `/v1/models` 变成
/// 每次轮询都打上游的放大器（GUI 是 3 秒一轮的）。
pub const CATALOG_TTL: Duration = Duration::from_secs(3600);

/// 读目录用的 UA（**每区各一份**，与对话 UA 分开）。
/// CN 的值是 2026-11-21 实测拿到 58 个模型的形态；INTL 尚未独立复核，读不到就回退。
const CATALOG_UA_CN: &str = "WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0";
const CATALOG_UA_INTL: &str = "WorkBuddy/5.9.3 workbuddy-ai/5.9.3 CLI/2.165.0";

/// 目录里的一个模型（只收代理真正用得到的字段）。
#[derive(Debug, Clone, Default)]
pub struct Model {
    pub id: String,
    pub name: String,
    /// 积分倍率，上游原样给（`x0.08` / `x0.00`）。缺省 = 上游没给，别自己编。
    pub credits: Option<String>,
    pub max_input: u64,
    pub max_output: u64,
    pub supports_images: bool,
    pub supports_tool: bool,
    pub supports_reasoning: bool,
    /// 上游给的**默认**思考档（`low`/`medium`/`high`…），不是完整矩阵。
    pub effort: Option<String>,
}

/// 一次成功的目录读取。
pub struct Catalog {
    pub at: Instant,
    pub models: Vec<Model>,
}

fn cache() -> &'static Mutex<HashMap<&'static str, Catalog>> {
    static C: OnceLock<Mutex<HashMap<&'static str, Catalog>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `CODEBUDDY_CATALOG=0` 关闭目录（关闭后行为与改造前完全一致）。
pub fn enabled() -> bool {
    match std::env::var("CODEBUDDY_CATALOG") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "off" || v == "no")
        }
        Err(_) => true,
    }
}

fn ua_for(region: crate::region::Region) -> &'static str {
    match region {
        crate::region::Region::Cn => CATALOG_UA_CN,
        crate::region::Region::Intl => CATALOG_UA_INTL,
    }
}

/// 解析 `{"code":0,"data":{"models":[…]}}`。
///
/// 宽容但**不猜**：字段缺失就留缺省（`credits` 缺省而不是编一个 `x1.00`）；
/// `code != 0` 或没有 `data.models` 数组一律当失败（回退候选池）。
pub fn parse(raw: &[u8]) -> Option<Vec<Model>> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    if v.get("code").and_then(Value::as_i64).unwrap_or(0) != 0 {
        return None;
    }
    let arr = v.get("data")?.get("models")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for m in arr {
        let Some(id) = m.get("id").and_then(Value::as_str) else {
            continue; // 没有 id 的条目无法调用，跳过
        };
        let name = m
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        out.push(Model {
            id: id.to_string(),
            name,
            credits: m.get("credits").and_then(Value::as_str).map(str::to_string),
            max_input: m.get("maxInputTokens").and_then(Value::as_u64).unwrap_or(0),
            max_output: m.get("maxOutputTokens").and_then(Value::as_u64).unwrap_or(0),
            supports_images: m.get("supportsImages").and_then(Value::as_bool).unwrap_or(false),
            supports_tool: m.get("supportsToolCall").and_then(Value::as_bool).unwrap_or(false),
            supports_reasoning: m
                .get("supportsReasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            effort: m
                .get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 给 `/v1/models` 用的 id 列表：只要**可对话**的（`supportsToolCall`）。
///
/// 上游目录里还混着补全、生图、生视频（`codewise-*`、`hunyuan-image-*`、`kling-video-*`），
/// 它们接到 `/v1/chat/completions` 上只会报错，所以按能力位过滤，而不是看名字猜。
pub fn chat_ids(c: &Catalog) -> Vec<String> {
    c.models
        .iter()
        .filter(|m| m.supports_tool)
        .map(|m| m.id.clone())
        .collect()
}

/// 按 id 查目录条目（`/model/info` 的元数据来源）。
pub fn find<'a>(c: &'a Catalog, id: &str) -> Option<&'a Model> {
    c.models.iter().find(|m| m.id == id)
}

/// 取目录（带 1h 缓存）。
///
/// 返回 `None` 的情形都会让调用方回退候选池：UA 被拒 / 网络失败 / 测试用假上游
/// （`--upstream-base` 覆盖时**不读目录**，否则黄金回放会多出一跳未记录的请求）。
pub async fn get(st: &std::sync::Arc<crate::AppState>) -> Option<Catalog> {
    if !enabled() {
        return None;
    }
    // 假上游（黄金回放/合同测试）不读目录：那一跳不在黄金里，会打乱「上游命中数」断言。
    if st.base_url != st.region.cfg().base_url {
        return None;
    }
    let key = st.region.as_str();
    {
        let c = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = c.get(key) {
            if hit.at.elapsed() < CATALOG_TTL && !hit.models.is_empty() {
                return Some(Catalog {
                    at: hit.at,
                    models: hit.models.clone(),
                });
            }
        }
    }
    let Ok(mut hdr) = st.cred.headers() else {
        return None;
    };
    hdr.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static(ua_for(st.region)),
    );
    let url = format!("{}/v3/config", st.base_url);
    let resp = match st.http.get(&url).headers(hdr).send().await {
        Ok(r) => r,
        Err(e) => {
            println!("[codebuddy] 目录读取失败（{}），回退候选池", e);
            return None;
        }
    };
    let status = resp.status().as_u16();
    if status != 200 {
        println!(
            "[codebuddy] 目录读取被拒 status={}（UA 形态可能收紧），回退候选池",
            status
        );
        return None;
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return None,
    };
    let Some(models) = parse(&bytes) else {
        println!("[codebuddy] 目录解析失败（{} 字节），回退候选池", bytes.len());
        return None;
    };
    let chat = models.iter().filter(|m| m.supports_tool).count();
    println!(
        "[codebuddy] 目录读取成功 region={}：{} 个模型（{} 个可对话）",
        key,
        models.len(),
        chat
    );
    let cat = Catalog {
        at: Instant::now(),
        models,
    };
    let snapshot = Catalog {
        at: cat.at,
        models: cat.models.clone(),
    };
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(static_key(st.region), snapshot);
    Some(cat)
}

/// 缓存键必须是 `&'static str`（`Region::as_str` 正是），这里显式转换一次让类型闭环。
fn static_key(r: crate::region::Region) -> &'static str {
    match r {
        crate::region::Region::Cn => "CN",
        crate::region::Region::Intl => "INTL",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = br#"{"code":0,"msg":"OK","data":{"models":[
      {"id":"glm-5.3-flash","name":"GLM-5.3-Flash","credits":"x0.08","maxInputTokens":1000000,
       "maxOutputTokens":131072,"supportsImages":true,"supportsToolCall":true,
       "supportsReasoning":true,"reasoning":{"effort":"medium","summary":"auto"}},
      {"id":"hunyuan-image-alpha","name":"hunyuan-image","maxInputTokens":4096,
       "maxOutputTokens":1024,"supportsImages":true,"supportsToolCall":false},
      {"id":"hy4flash-free","name":"free","credits":"x0.00","maxInputTokens":960000,
       "maxOutputTokens":64000,"supportsImages":true,"supportsToolCall":true}
    ]}}"#;

    #[test]
    fn parses_models_with_credits_and_capabilities() {
        let m = parse(FIXTURE).expect("应能解析");
        assert_eq!(m.len(), 3);
        let g = &m[0];
        assert_eq!(g.id, "glm-5.3-flash");
        assert_eq!(g.credits.as_deref(), Some("x0.08"));
        assert_eq!(g.max_input, 1_000_000);
        assert_eq!(g.max_output, 131_072);
        assert!(g.supports_images && g.supports_tool && g.supports_reasoning);
        assert_eq!(g.effort.as_deref(), Some("medium"));
        // 缺省不编：没有 credits 的条目就是 None
        assert_eq!(m[1].credits, None);
    }

    #[test]
    fn chat_ids_keep_only_tool_capable() {
        let c = Catalog {
            at: Instant::now(),
            models: parse(FIXTURE).unwrap(),
        };
        let ids = chat_ids(&c);
        assert_eq!(ids, vec!["glm-5.3-flash", "hy4flash-free"], "生图模型不该出现在对话列表");
    }

    #[test]
    fn non_zero_code_or_missing_models_is_none() {
        assert!(parse(br#"{"code":401,"msg":"no"}"#).is_none());
        assert!(parse(br#"{"code":0,"data":{}}"#).is_none());
        assert!(parse(br#"[1,2,3]"#).is_none());
        assert!(parse(br#"{"code":0,"data":{"models":[]}}"#).is_none(), "空数组等于没读到");
    }

    #[test]
    fn find_by_id() {
        let c = Catalog {
            at: Instant::now(),
            models: parse(FIXTURE).unwrap(),
        };
        assert_eq!(find(&c, "hy4flash-free").unwrap().credits.as_deref(), Some("x0.00"));
        assert!(find(&c, "nope").is_none());
    }

    #[test]
    fn enabled_parses_off_values() {
        let saved = std::env::var("CODEBUDDY_CATALOG").ok();
        std::env::remove_var("CODEBUDDY_CATALOG");
        assert!(enabled(), "缺省必须开");
        for off in ["0", "false", "off", "NO"] {
            std::env::set_var("CODEBUDDY_CATALOG", off);
            assert!(!enabled(), "{off:?} 应视为关闭");
        }
        match saved {
            Some(v) => std::env::set_var("CODEBUDDY_CATALOG", v),
            None => std::env::remove_var("CODEBUDDY_CATALOG"),
        }
    }
}

