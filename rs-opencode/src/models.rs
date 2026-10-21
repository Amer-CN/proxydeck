//! models.rs —— `/api/model` 的解析、白/黑名单过滤、OpenAI `/v1/models` 输出，
//! 以及 429 冷却（撞限流的模型只对它自己生效，绝不静默换模型）。
//!
//! 内部选模/冷却用的键统一是 `<providerID>/<modelID>`；对外 `/v1/models` 只给裸模型名，入站再按配置的 provider 顺序还原（见 `resolve_ref`）。
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// `/api/model` 里一条模型记录（只取路由需要的字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub enabled: bool,
    pub released: i64,
    pub name: String,
}

impl ModelEntry {
    /// 对外/冷却表使用的键：`<providerID>/<modelID>`。
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// 解析 `/api/model` 的 `{"data":[...]}`。坏条目跳过，不 panic。
pub fn parse_models(value: &Value) -> Vec<ModelEntry> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let arr = match value.get("data").and_then(Value::as_array) {
        Some(v) => v,
        None => return out,
    };
    for item in arr {
        if !item.is_object() {
            continue;
        }
        let provider = item
            .get("providerID")
            .and_then(Value::as_str)
            .or_else(|| item.get("provider").and_then(Value::as_str))
            .unwrap_or("")
            .trim();
        let id = item
            .get("modelID")
            .and_then(Value::as_str)
            .or_else(|| item.get("id").and_then(Value::as_str))
            .unwrap_or("")
            .trim();
        if provider.is_empty() || id.is_empty() {
            continue;
        }
        let key = format!("{provider}/{id}");
        if !seen.insert(key) {
            continue;
        }
        let enabled = item.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let released = item
            .get("time")
            .and_then(|t| t.get("released"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        out.push(ModelEntry {
            provider: provider.to_string(),
            id: id.to_string(),
            enabled,
            released,
            name,
        });
    }
    out
}

/// 只保留「enabled 且 provider 在白名单且未撞黑名单」的条目。
///
/// 黑名单做大小写不敏感子串匹配（`beam-free` 命中 `beam-free`；写 `beam` 也能全挡）。
pub fn filter_models(
    entries: &[ModelEntry],
    providers: &[String],
    denylist: &[String],
) -> Vec<ModelEntry> {
    let providers_lower: Vec<String> = providers.iter().map(|p| p.to_lowercase()).collect();
    entries
        .iter()
        .filter(|m| m.enabled)
        .filter(|m| {
            providers_lower
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&m.provider))
        })
        .filter(|m| {
            let key = m.key().to_lowercase();
            !denylist.iter().any(|d| !d.is_empty() && key.contains(d))
        })
        .cloned()
        .collect()
}

/// OpenAI `/v1/models` 响应（id = 裸模型名，不带 provider 前缀；
/// `owned_by` 保留 provider 供区分；created 固定 0）。
pub fn openai_models(entries: &[ModelEntry]) -> Value {
    let data: Vec<Value> = entries
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": 0,
                "owned_by": m.provider,
            })
        })
        .collect();
    json!({"object": "list", "data": data})
}

/// 把入站模型名还原成完整的 `<providerID>/<modelID>` 引用：
/// * 含 `/` 的显式引用（如 `opencode/xxx`）→ 原样采用，即使不在模型列表里
///   也照样尝试（让上游如实报错，而不是本地猜一个）。
/// * 裸名（如 `deepseek-v4.1-flash-free`，正是 `/v1/models` 给出的形态）→ 按配置的
///   provider 顺序，取第一个实际提供它的 provider；都没提供就缺省 `opencode/<id>`
///   （上游如实报错）。
/// * `#variant` 后缀照旧去掉。
pub fn resolve_ref(raw: &str, entries: &[ModelEntry], providers: &[String]) -> String {
    let raw = raw.trim();
    let raw = raw.split('#').next().unwrap_or(raw).trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.contains('/') {
        return raw.to_string();
    }
    for p in providers {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(e) = entries
            .iter()
            .find(|e| e.provider.eq_ignore_ascii_case(p) && e.id.eq_ignore_ascii_case(raw))
        {
            return e.key();
        }
    }
    format!("opencode/{raw}")
}

/// 把入站模型名规范成 `<providerID>/<modelID>`（保留作显式引用的快速归一化；
/// 裸名的路由请走 `resolve_ref`，它会按配置的 provider 顺序找）：
/// * `deepseek-v4.1-flash-free` → `opencode/deepseek-v4.1-flash-free`（缺省 provider 为 opencode）
/// * `opencode/xxx` → 原样
/// * `opencode/xxx#high` → `opencode/xxx`（变体暂不转发）
pub fn normalize_key(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.split('#').next().unwrap_or(raw).trim();
    if raw.contains('/') {
        raw.to_string()
    } else if raw.is_empty() {
        String::new()
    } else {
        format!("opencode/{raw}")
    }
}

/// 拆出 `(providerID, modelID)`；provider/id 任一为空返回 None。
pub fn parse_ref(raw: &str) -> Option<(String, String)> {
    let key = normalize_key(raw);
    let (provider, id) = key.split_once('/')?;
    if provider.trim().is_empty() || id.trim().is_empty() {
        return None;
    }
    Some((provider.to_string(), id.to_string()))
}

/// 选模型的结果。
///
/// 铁律：**显式指定的模型绝不静默换成别的**。撞限流的模型只能如实返回 429，
/// 让调用方自己决定重试还是换模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    Selected(String),
    /// 明确要求的模型正在冷却：必须回 429，不许用其它模型代替。
    RequestedCooling { key: String, retry_after: u64 },
    /// 未指定模型且没有可用的默认模型。
    Empty,
}

/// 429 冷却表：进程内内存态，不落盘（重启清零）。
pub struct ModelPool {
    cooldown: Duration,
    cooling: Mutex<HashMap<String, Instant>>,
}

impl ModelPool {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            cooling: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        // 锁中毒也不 panic：拿回内部状态继续用。
        self.cooling.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 标记某个模型冷却（键为 `<provider>/<model>`）。
    pub fn mark_cooling(&self, key: &str) {
        if key.trim().is_empty() {
            return;
        }
        self.lock()
            .insert(key.to_string(), Instant::now() + self.cooldown);
    }

    pub fn is_cooling(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut guard = self.lock();
        match guard.get(key) {
            Some(until) if *until > now => true,
            Some(_) => {
                guard.remove(key);
                false
            }
            None => false,
        }
    }

    /// 某个模型冷却的剩余秒数（向上取整，至少 1；没在冷却返回 0）。
    pub fn retry_after_for(&self, key: &str) -> u64 {
        let now = Instant::now();
        let guard = self.lock();
        match guard.get(key) {
            Some(until) if *until > now => {
                (((*until - now).as_millis() + 999) / 1000).max(1) as u64
            }
            _ => 0,
        }
    }

    /// 选模型：**绝不静默换模型**。
    ///
    /// * 入站指定了 `model` → 还原成完整引用后用它；若它正在冷却，返回
    ///   `RequestedCooling`（即使模型列表里没有该条目也照样尝试，让上游如实回错误）。
    /// * 入站未指定 → 用配置的默认模型；默认模型为空或正在冷却同样如实报错，
    ///   不会顺手换成别的模型。
    /// * 裸名按 `providers` 的顺序找第一个实际提供的 provider（见 `resolve_ref`）。
    pub fn pick(
        &self,
        requested: Option<&str>,
        entries: &[ModelEntry],
        providers: &[String],
        default_model: &str,
    ) -> Pick {
        let key = match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(req) => resolve_ref(req, entries, providers),
            None => resolve_ref(default_model, entries, providers),
        };
        if key.is_empty() {
            return Pick::Empty;
        }
        if self.is_cooling(&key) {
            return Pick::RequestedCooling {
                retry_after: self.retry_after_for(&key),
                key,
            };
        }
        Pick::Selected(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<ModelEntry> {
        parse_models(&json!({
            "data": [
                {"providerID": "opencode", "modelID": "deepseek-v4.1-flash-free", "enabled": true, "name": "DS"},
                {"providerID": "opencode", "modelID": "beam-free", "enabled": true},
                {"providerID": "opencode-go", "modelID": "go-1", "enabled": true},
                {"providerID": "other", "modelID": "x", "enabled": true},
                {"providerID": "opencode", "modelID": "disabled", "enabled": false},
                {"providerID": "opencode", "modelID": "deepseek-v4.1-flash-free", "enabled": true}
            ]
        }))
    }

    #[test]
    fn parse_skips_bad_and_dedups() {
        let e = entries();
        assert_eq!(e.len(), 5, "重复键要去掉：{e:?}");
        assert_eq!(e[0].key(), "opencode/deepseek-v4.1-flash-free");
        assert_eq!(e[0].name, "DS");
    }

    #[test]
    fn filter_applies_provider_whitelist_and_denylist() {
        let f = filter_models(
            &entries(),
            &["opencode".to_string(), "opencode-go".to_string()],
            &["beam-free".to_string()],
        );
        let keys: Vec<String> = f.iter().map(ModelEntry::key).collect();
        assert_eq!(
            keys,
            vec!["opencode/deepseek-v4.1-flash-free", "opencode-go/go-1"]
        );
    }

    #[test]
    fn openai_list_shape() {
        let f = filter_models(&entries(), &["opencode".to_string()], &[]);
        let v = openai_models(&f);
        assert_eq!(v["object"], "list");
        // 对外只给裸模型名，不带 provider 前缀（DSH 这类客户端直接展示 id）
        assert_eq!(v["data"][0]["id"], "deepseek-v4.1-flash-free");
        assert_eq!(v["data"][0]["object"], "model");
        assert_eq!(v["data"][0]["created"], 0);
        assert_eq!(v["data"][0]["owned_by"], "opencode");
    }

    #[test]
    fn normalize_and_parse_ref() {
        assert_eq!(
            normalize_key("deepseek-v4.1-flash-free"),
            "opencode/deepseek-v4.1-flash-free"
        );
        assert_eq!(normalize_key("opencode-go/kimi#high"), "opencode-go/kimi");
        assert_eq!(
            parse_ref("opencode/deepseek").unwrap(),
            ("opencode".to_string(), "deepseek".to_string())
        );
        assert_eq!(
            parse_ref("deepseek").unwrap(),
            ("opencode".to_string(), "deepseek".to_string())
        );
        assert!(parse_ref("/x").is_none());
        assert!(parse_ref("opencode/").is_none());
    }

    fn provs() -> Vec<String> {
        vec!["opencode".to_string(), "opencode-go".to_string()]
    }

    fn tie_entries() -> Vec<ModelEntry> {
        vec![
            ModelEntry {
                provider: "opencode".to_string(),
                id: "dup".to_string(),
                enabled: true,
                released: 0,
                name: "dup".to_string(),
            },
            ModelEntry {
                provider: "opencode-go".to_string(),
                id: "dup".to_string(),
                enabled: true,
                released: 0,
                name: "dup".to_string(),
            },
        ]
    }

    #[test]
    fn resolve_bare_name_by_provider_order() {
        let e = tie_entries();
        // 裸名按配置顺序找第一个实际提供的 provider
        assert_eq!(resolve_ref("dup", &e, &provs()), "opencode/dup");
        assert_eq!(
            resolve_ref("dup", &e, &[String::from("opencode-go")]),
            "opencode-go/dup"
        );
        // 大小写不敏感，但回填用目录里的原样
        assert_eq!(resolve_ref("DUP", &e, &provs()), "opencode/dup");
        // 显式引用原样采用（即使不在列表里）
        assert_eq!(
            resolve_ref("other/unknown", &e, &provs()),
            "other/unknown"
        );
        // 列表里没有的裸名：缺省 opencode，让上游如实报错
        assert_eq!(resolve_ref("nope", &e, &provs()), "opencode/nope");
        // 变体后缀照旧去掉；空串给空串
        assert_eq!(resolve_ref("dup#high", &e, &provs()), "opencode/dup");
        assert_eq!(resolve_ref("  ", &e, &provs()), "");
    }

    #[test]
    fn pool_never_switches_model_silently() {
        let pool = ModelPool::new(Duration::from_secs(60));
        let e = tie_entries();

        // 显式指定（完整引用）且未冷却 → 用它
        assert_eq!(
            pool.pick(Some("opencode/a"), &e, &provs(), "opencode/b"),
            Pick::Selected("opencode/a".to_string())
        );
        // 显式指定裸名：列表里没有 → 缺省 opencode/ 前缀（上游如实报错）
        assert_eq!(
            pool.pick(Some("a"), &tie_entries(), &provs(), "opencode/b"),
            Pick::Selected("opencode/a".to_string())
        );
        // 显式指定裸名且命中 → 按 provider 顺序还原成完整引用
        assert_eq!(
            pool.pick(Some("dup"), &tie_entries(), &provs(), "opencode/b"),
            Pick::Selected("opencode/dup".to_string())
        );

        pool.mark_cooling("opencode/a");
        assert!(pool.is_cooling("opencode/a"));
        // 冷却后：显式指定的仍按原名回 429，不许换成默认模型
        match pool.pick(Some("opencode/a"), &e, &provs(), "opencode/b") {
            Pick::RequestedCooling { key, retry_after } => {
                assert_eq!(key, "opencode/a");
                assert!(retry_after >= 1 && retry_after <= 60);
            }
            other => panic!("期望 RequestedCooling，实际 {other:?}"),
        }

        // 未指定：用默认模型；默认模型也在冷却 → 同样如实 429
        pool.mark_cooling("opencode/b");
        match pool.pick(None, &e, &provs(), "opencode/b") {
            Pick::RequestedCooling { key, .. } => assert_eq!(key, "opencode/b"),
            other => panic!("期望 RequestedCooling，实际 {other:?}"),
        }
    }

    #[test]
    fn pool_empty_when_no_candidates() {
        let pool = ModelPool::new(Duration::from_secs(60));
        assert_eq!(pool.pick(None, &[], &[], ""), Pick::Empty);
    }
}
