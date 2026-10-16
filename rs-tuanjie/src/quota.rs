//! quota.rs —— 团结积分（官网 dashboard 口径）查询，逐条对应 `internal/tuanjie/quota.go`（301 行）。
//!
//! 数据源：`GET https://codely.tuanjie.cn/api/user/billing/usage/summary`（Bearer access_token
//! 认证，直连官网不走 LiteLLM Forward）。总积分 = 今日配额剩余 + 赠送剩余 + 有效赠送余额。
//!
//! 本模块照搬的三件事（`/quota?uid=` 的三条对外合同）：
//!
//! 1. **缓存键**：`sha256(access_token)[:8]` 的十六进制（`quotaCacheKey`，`quota.go:113-116`）
//!    —— 按账号 token 分键，不同区/不同号天然各占一格，明文 token 不进内存键。
//! 2. **缓存口径**：成功 5 分钟（`quotaTTL`）、失败负缓存 60 秒（`quotaErrTTL`）；
//!    缓存命中且原值 `source == "live"` 时改写成 `"cache"` 返回；失败但有旧 live 值 →
//!    回退旧值（`source = "cache"` + `err`）；`force`（`?refresh=1`）跳过缓存直拉。
//! 3. **字段与取值**：`Quota` 各字段的**逐字节形态**（字段声明序 + `omitempty` 口径 +
//!    字符串型 points 的 `ParseFloat` 失败取 0 + `bySource` 同名合并再按积分降序）。
//!
//! `/quota` 端点本身的装配（`uid`/`refresh`、账号不在池中、取 token 失败的文案）见
//! [`QuotaView`] 与 `server` 面（P8b 后续小轮接上手 HTTP 路由）。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::gojson::{format_go_float, go_string};

/// 官网计费摘要端点 —— `quota.go:26`。
pub const BILLING_SUMMARY_URL: &str =
    "https://codely.tuanjie.cn/api/user/billing/usage/summary";

/// 积分接口超时 —— `quota.go:27` 的 `billingTimeout = 15s`。
pub const BILLING_TIMEOUT: Duration = Duration::from_secs(15);

/// 成功缓存 TTL —— `quota.go:108`。
pub const QUOTA_TTL: Duration = Duration::from_secs(5 * 60);

/// 失败负缓存 TTL —— `quota.go:109`。
pub const QUOTA_ERR_TTL: Duration = Duration::from_secs(60);

/// 读入上限 —— `quota.go:203` 的 `io.LimitReader(resp.Body, 2<<20)`。
pub const QUOTA_MAX_BODY: usize = 2 << 20;

// ---------------------------------------------------------------- 数据形态

/// 积分快照 —— `quota.go:31-40` 的 `Quota`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quota {
    pub total: f64,
    pub daily: DailyQuota,
    pub gift: GiftQuota,
    pub month: MonthQuota,
    /// 消耗 TOP（按积分降序）；空 = 整体省略（`omitempty`）。
    pub by_source: Vec<SourceSpend>,
    pub plan: PlanQuota,
    /// `live` / `cache` / `none`。
    pub source: String,
    pub err: String,
}

/// 今日配额 —— `quota.go:43-48`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DailyQuota {
    pub quota: f64,
    pub used: f64,
    pub remaining: f64,
    pub period_end: String,
}

/// 赠送积分 —— `quota.go:51-53`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GiftQuota {
    pub remaining: f64,
}

/// 套餐窗口 —— `quota.go:57-61` 的 `PlanQuota`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanQuota {
    pub effective_remaining: f64,
    pub next_boundary_at: String,
    pub windows: Vec<PlanWindow>,
}

/// 单窗口（`type=usage_5h|subscription_week|subscription_month`）—— `quota.go:64-71`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanWindow {
    pub r#type: String,
    pub quota: f64,
    pub used: f64,
    pub remaining: f64,
    pub exhausted: bool,
    pub period_end: String,
}

/// 月度统计 —— `quota.go:74-82`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonthQuota {
    pub points: f64,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub requests: i64,
    pub start_date: String,
    pub end_date: String,
}

/// 单来源消耗 —— `quota.go:85-90`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceSpend {
    pub label: String,
    pub points: f64,
    pub tokens: i64,
    pub requests: i64,
}

impl Quota {
    /// `&Quota{Source: "none", Err: …}`（`quota.go:152,164,183` 等处的失败返回）。
    pub fn none(err: impl Into<String>) -> Self {
        Quota {
            source: "none".to_string(),
            err: err.into(),
            ..Default::default()
        }
    }

    /// 与 Go `json.Marshal(Quota)` 的逐字节等价物：字段**声明序**、`omitempty` 口径
    /// （`by_source`/`plan.windows`/各 `period*` 字符串为空即省），浮点走 `format_go_float`。
    pub fn to_go_json(&self) -> String {
        let mut out = String::new();
        out.push('{');
        out.push_str("\"total\":");
        out.push_str(&format_go_float(self.total));
        out.push_str(",\"daily\":{");
        out.push_str("\"quota\":");
        out.push_str(&format_go_float(self.daily.quota));
        out.push_str(",\"used\":");
        out.push_str(&format_go_float(self.daily.used));
        out.push_str(",\"remaining\":");
        out.push_str(&format_go_float(self.daily.remaining));
        if !self.daily.period_end.is_empty() {
            out.push_str(",\"periodEnd\":");
            out.push_str(&go_string(&self.daily.period_end));
        }
        out.push_str("},\"gift\":{\"remaining\":");
        out.push_str(&format_go_float(self.gift.remaining));
        out.push_str("},\"month\":{");
        out.push_str("\"points\":");
        out.push_str(&format_go_float(self.month.points));
        out.push_str(",\"totalTokens\":");
        out.push_str(&self.month.total_tokens.to_string());
        out.push_str(",\"promptTokens\":");
        out.push_str(&self.month.prompt_tokens.to_string());
        out.push_str(",\"completionTokens\":");
        out.push_str(&self.month.completion_tokens.to_string());
        out.push_str(",\"requests\":");
        out.push_str(&self.month.requests.to_string());
        if !self.month.start_date.is_empty() {
            out.push_str(",\"startDate\":");
            out.push_str(&go_string(&self.month.start_date));
        }
        if !self.month.end_date.is_empty() {
            out.push_str(",\"endDate\":");
            out.push_str(&go_string(&self.month.end_date));
        }
        out.push('}');
        if !self.by_source.is_empty() {
            out.push_str(",\"bySource\":[");
            for (i, s) in self.by_source.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str("{\"label\":");
                out.push_str(&go_string(&s.label));
                out.push_str(",\"points\":");
                out.push_str(&format_go_float(s.points));
                out.push_str(",\"tokens\":");
                out.push_str(&s.tokens.to_string());
                out.push_str(",\"requests\":");
                out.push_str(&s.requests.to_string());
                out.push('}');
            }
            out.push(']');
        }
        // Plan 是结构体：`omitempty` 对它无效，**永远写出**
        out.push_str(",\"plan\":{");
        out.push_str("\"effective_remaining\":");
        out.push_str(&format_go_float(self.plan.effective_remaining));
        if !self.plan.next_boundary_at.is_empty() {
            out.push_str(",\"next_boundary_at\":");
            out.push_str(&go_string(&self.plan.next_boundary_at));
        }
        if !self.plan.windows.is_empty() {
            out.push_str(",\"windows\":[");
            for (i, w) in self.plan.windows.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str("{\"type\":");
                out.push_str(&go_string(&w.r#type));
                out.push_str(",\"quota\":");
                out.push_str(&format_go_float(w.quota));
                out.push_str(",\"used\":");
                out.push_str(&format_go_float(w.used));
                out.push_str(",\"remaining\":");
                out.push_str(&format_go_float(w.remaining));
                out.push_str(",\"exhausted\":");
                out.push_str(if w.exhausted { "true" } else { "false" });
                if !w.period_end.is_empty() {
                    out.push_str(",\"period_end\":");
                    out.push_str(&go_string(&w.period_end));
                }
                out.push('}');
            }
            out.push(']');
        }
        out.push('}');
        out.push_str(",\"source\":");
        out.push_str(&go_string(&self.source));
        if !self.err.is_empty() {
            out.push_str(",\"err\":");
            out.push_str(&go_string(&self.err));
        }
        out.push('}');
        out
    }

    /// 读回自己写出的形态（缓存回退路径要「逐字段复制」，这里是给测试与回放用的解码）。
    pub fn from_go_json(s: &str) -> Option<Quota> {
        let v: Value = serde_json::from_str(s).ok()?;
        Some(quota_from_value(&v))
    }
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn quota_from_value(v: &Value) -> Quota {
    let daily = &v["daily"];
    let mut out = Quota {
        total: num(&v["total"]),
        daily: DailyQuota {
            quota: num(&daily["quota"]),
            used: num(&daily["used"]),
            remaining: num(&daily["remaining"]),
            period_end: str_of(&daily["periodEnd"]),
        },
        gift: GiftQuota {
            remaining: num(&v["gift"]["remaining"]),
        },
        month: MonthQuota {
            points: num(&v["month"]["points"]),
            total_tokens: v["month"]["totalTokens"].as_i64().unwrap_or(0),
            prompt_tokens: v["month"]["promptTokens"].as_i64().unwrap_or(0),
            completion_tokens: v["month"]["completionTokens"].as_i64().unwrap_or(0),
            requests: v["month"]["requests"].as_i64().unwrap_or(0),
            start_date: str_of(&v["month"]["startDate"]),
            end_date: str_of(&v["month"]["endDate"]),
        },
        plan: PlanQuota {
            effective_remaining: num(&v["plan"]["effective_remaining"]),
            next_boundary_at: str_of(&v["plan"]["next_boundary_at"]),
            windows: Vec::new(),
        },
        source: str_of(&v["source"]),
        err: str_of(&v["err"]),
        by_source: Vec::new(),
    };
    if let Some(arr) = v["bySource"].as_array() {
        for s in arr {
            out.by_source.push(SourceSpend {
                label: str_of(&s["label"]),
                points: num(&s["points"]),
                tokens: s["tokens"].as_i64().unwrap_or(0),
                requests: s["requests"].as_i64().unwrap_or(0),
            });
        }
    }
    if let Some(arr) = v["plan"]["windows"].as_array() {
        for w in arr {
            out.plan.windows.push(PlanWindow {
                r#type: str_of(&w["type"]),
                quota: num(&w["quota"]),
                used: num(&w["used"]),
                remaining: num(&w["remaining"]),
                exhausted: w["exhausted"].as_bool().unwrap_or(false),
                period_end: str_of(&w["period_end"]),
            });
        }
    }
    out
}

// ---------------------------------------------------------------- 缓存

/// 缓存键：`sha256(access_token)` 前 8 字节的十六进制 —— `quotaCacheKey`（`quota.go:113-116`）。
pub fn quota_cache_key(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(16);
    for b in &sum[..8] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

struct CacheEntry {
    q: Quota,
    at: Instant,
}

/// 按 token 哈希键控的积分缓存 —— `quotaCache`（`quota.go:100-105`）。
/// 官网对同 token 高频调用会 429 限流（2026-08-27 事故：GUI 每 3s 轮询 /quota ×
/// 无缓存 = 每分钟 20 次直打官网），故两条查询路径统一走这里。
pub struct QuotaCache {
    m: Mutex<std::collections::HashMap<String, CacheEntry>>,
}

impl Default for QuotaCache {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaCache {
    pub fn new() -> Self {
        QuotaCache {
            m: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, CacheEntry>> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 缓存命中判定 + 改写 —— `quota.go:121-140`。
    /// 命中且未过 TTL：复制一份返回；原值 `live` → 改写为 `cache`。
    pub fn get(&self, key: &str, now: Instant) -> Option<Quota> {
        let g = self.lock();
        let e = g.get(key)?;
        let ttl = if e.q.source == "none" {
            QUOTA_ERR_TTL
        } else {
            QUOTA_TTL
        };
        if now.duration_since(e.at) < ttl {
            let mut cp = e.q.clone();
            if cp.source == "live" {
                cp.source = "cache".to_string();
            }
            return Some(cp);
        }
        None
    }

    pub fn put(&self, key: &str, q: Quota, now: Instant) {
        self.lock().insert(
            key.to_string(),
            CacheEntry {
                q,
                at: now,
            },
        );
    }

    pub fn get_entry(&self, key: &str) -> Option<Quota> {
        self.lock().get(key).map(|e| e.q.clone())
    }
}

// ---------------------------------------------------------------- 拉取

/// 积分拉取的口径开关（测试缝：真上游是 `https://codely.tuanjie.cn`，单测走假上游）。
#[derive(Debug, Clone)]
pub struct QuotaEndpoint {
    pub url: String,
    pub timeout: Duration,
    pub user_agent: String,
}

impl Default for QuotaEndpoint {
    fn default() -> Self {
        QuotaEndpoint {
            url: BILLING_SUMMARY_URL.to_string(),
            timeout: BILLING_TIMEOUT,
            // Go 用包级 `cliUserAgent`（`client.go:54` 启动解析一次）——这里取同一份进程级 UA。
            user_agent: crate::ua::process_cli_user_agent().to_string(),
        }
    }
}

/// 建一个「不设客户端总超时」的裸客户端：超时由 [`fetch_quota_with_token`] 的**请求级**
/// 15s 兜住（对应 Go 的 `context.WithTimeout(ctx, billingTimeout)`）。
///
/// 与 Go 的一处已登记差异：Go 的 `defer cancel()` 只罩住 `client.Do`（拿到响应头即返回），
/// **读体不在 15s 预算内**；reqwest 的请求级 timeout 连读体一起罩（更严，不会少读）。
pub fn build_quota_client() -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .expect("reqwest Client::builder().build() 不会失败")
}

/// 官网积分接口的一次调用 —— `fetchQuotaWithToken`（`quota.go:188-253`）。
/// 返回 `Err(msg)` 时 `msg` 与 Go 的 `fmt.Errorf` 文案逐字同源。
pub async fn fetch_quota_with_token(
    client: &reqwest::Client,
    ep: &QuotaEndpoint,
    access_token: &str,
) -> Result<Quota, String> {
    let req = client
        .get(&ep.url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", ep.user_agent.clone())
        .timeout(ep.timeout);
    let resp = req
        .send()
        .await
        .map_err(|e| format!("官网积分接口请求失败: {e}"))?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("官网积分接口请求失败: {e}"))?;
    let body = &body[..body.len().min(QUOTA_MAX_BODY)];
    if status.as_u16() != 200 {
        return Err(format!(
            "官网积分接口 HTTP {}: {}",
            status.as_u16(),
            truncate(&String::from_utf8_lossy(body), 200)
        ));
    }
    let s: Value = serde_json::from_slice(body)
        .map_err(|e| format!("官网积分接口响应解析失败: {e}"))?;
    Ok(parse_summary(&s))
}

/// `summaryResp` 的解析（`quota.go:212-252`）：points 类是**字符串**（`ParseFloat`，
/// 失败取 0）；token/计数类是数字；`bySource` 同名合并后按积分降序。
pub fn parse_summary(s: &Value) -> Quota {
    let f = |v: &Value| -> f64 {
        match v.as_str() {
            // Go `strconv.ParseFloat` 不去首尾空白（含空白即整体报错→取 0），故这里也不 trim。
            Some(text) => text.parse::<f64>().unwrap_or(0.0),
            None => 0.0,
        }
    };
    let daily = &s["daily_allowance"];
    let mut q = Quota {
        source: "live".to_string(),
        daily: DailyQuota {
            quota: f(&daily["quota_points"]),
            used: f(&daily["used_points"]),
            remaining: f(&daily["remaining_points"]),
            period_end: daily["period_end_at"].as_str().unwrap_or("").to_string(),
        },
        gift: GiftQuota {
            remaining: f(&s["gift_credits"]["remaining_points"]),
        },
        plan: PlanQuota {
            effective_remaining: f(&s["coding_plan"]["effective_remaining_points"]),
            next_boundary_at: s["coding_plan"]["next_boundary_at"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            windows: Vec::new(),
        },
        ..Default::default()
    };
    if let Some(ws) = s["coding_plan"]["windows"].as_array() {
        for w in ws {
            q.plan.windows.push(PlanWindow {
                r#type: w["window_type"].as_str().unwrap_or("").to_string(),
                quota: f(&w["quota_points"]),
                used: f(&w["used_points"]),
                remaining: f(&w["remaining_points"]),
                exhausted: w["exhausted"].as_bool().unwrap_or(false),
                period_end: w["period"]["end_at"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    // 总积分 = 今日剩余 + 赠送剩余 + 有效赠送余额（billing.effective_available_points 是数字）
    q.total = q.daily.remaining + q.gift.remaining + s["billing"]["effective_available_points"].as_f64().unwrap_or(0.0);
    let totals = &s["totals"];
    q.month = MonthQuota {
        points: totals["recorded_points"].as_f64().unwrap_or(0.0),
        total_tokens: totals["total_tokens"].as_i64().unwrap_or(0),
        prompt_tokens: totals["prompt_tokens"].as_i64().unwrap_or(0),
        completion_tokens: totals["completion_tokens"].as_i64().unwrap_or(0),
        requests: totals["settlement_count"].as_i64().unwrap_or(0),
        start_date: s["period"]["start_date"].as_str().unwrap_or("").to_string(),
        end_date: s["period"]["end_date"].as_str().unwrap_or("").to_string(),
    };
    // 合并同名 label（后端按 source_type 拆多条，界面只按模型名显示）。
    // Go 用 map 迭代（**顺序不确定**）后按 points 降序排序；降序比较用 `>`，
    // 相等时顺序随 map 迭代而定 ⇒ 本实现按 label 排序保证同一输入同一输出
    // （见 README 差异：等分并列的次序在 Go 侧本身不确定）。
    use std::collections::BTreeMap;
    let mut merged: BTreeMap<String, SourceSpend> = BTreeMap::new();
    if let Some(arr) = totals["by_source"].as_array() {
        for src in arr {
            let label = src["label"].as_str().unwrap_or("").to_string();
            let points = src["recorded_points"].as_f64().unwrap_or(0.0);
            let tokens = src["total_tokens"].as_i64().unwrap_or(0);
            let requests = src["settlement_count"].as_i64().unwrap_or(0);
            merged
                .entry(label.clone())
                .and_modify(|m| {
                    m.points += points;
                    m.tokens += tokens;
                    m.requests += requests;
                })
                .or_insert(SourceSpend {
                    label,
                    points,
                    tokens,
                    requests,
                });
        }
    }
    let mut by_source: Vec<SourceSpend> = merged.into_values().collect();
    by_source.sort_by(|a, b| {
        b.points
            .partial_cmp(&a.points)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.cmp(&b.label))
    });
    q.by_source = by_source;
    q
}

/// 错误体截断 —— Go `client.go:557-562`：`len(s) <= n` 原样，否则**按字节**取前 n 个
/// 再追加 `"..."`。Rust 的 `String` 放不下 Go 切断多字节字符时产生的非法 UTF-8
/// （Go 的 JSON 编码器会把它写成 U+FFFD），故退到 n 以内最后的字符边界；
/// 纯 ASCII 输入（nginx 错误页是常态）与 Go 逐字节一致。
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// 带缓存的积分拉取 —— `fetchQuotaCached`（`quota.go:121-158`）。
///
/// 语义逐条对齐：
/// * `force=false` 且命中未过期 → 返回副本（live→cache）；
/// * 拉取失败：有旧 live 值 → 返回旧值副本（`source=cache` + 本次 `err`）；
///   否则写一条 `none` + 负缓存并返回**新的** `none`；
/// * 拉取成功 → 写 `live` 进缓存并返回；
/// * `force=true` 时**不读缓存**，但成功/失败仍照常写缓存。
pub async fn fetch_quota_cached(
    cache: &QuotaCache,
    client: &reqwest::Client,
    ep: &QuotaEndpoint,
    access_token: &str,
    force: bool,
) -> Quota {
    let key = quota_cache_key(access_token);
    let now = Instant::now();
    if !force {
        if let Some(q) = cache.get(&key, now) {
            return q;
        }
    }
    match fetch_quota_with_token(client, ep, access_token).await {
        Ok(q) => {
            cache.put(&key, q.clone(), Instant::now());
            q
        }
        Err(err) => {
            // 失败但有旧 live 值 → 回退旧值（Go 在**锁内**读缓存，这里读同一张表）
            if let Some(old) = cache.get_entry(&key) {
                if old.source == "live" {
                    let mut cp = old;
                    cp.source = "cache".to_string();
                    cp.err = err.clone();
                    return cp;
                }
            }
            let eq = Quota::none(err.clone());
            cache.put(&key, eq.clone(), Instant::now());
            Quota::none(err)
        }
    }
}

/// `/quota` 端点的装配口径 —— `handleQuota`（`server.go:1869-1895`）：
/// `?uid`（Go 侧读的是 `user_id`）为空走本地账号；指定 uid 时「账号不在池中」/
/// 「读取 token 失败: …」两条文案逐字同源。
pub struct QuotaView;

impl QuotaView {
    /// 账号不在池中 —— `server.go:1882`。
    pub fn not_in_pool() -> Quota {
        Quota::none("账号不在池中")
    }

    /// 取 token 失败 —— `server.go:1887`。
    pub fn token_error(err: &str) -> Quota {
        Quota::none(format!("读取 token 失败: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_sha256_truncated_hex() {
        // 与 Go `quotaCacheKey` 同值（sha256 前 8 字节 hex）
        assert_eq!(quota_cache_key("abc").len(), 16);
        assert_eq!(quota_cache_key("abc"), quota_cache_key("abc"));
        assert_ne!(quota_cache_key("abc"), quota_cache_key("abd"));
        // 已知答案：sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(quota_cache_key("abc"), "ba7816bf8f01cfea");
    }

    /// P8b 审查必修 #3：对齐 Go `truncate`（`client.go:557-562`）——`len(s) <= n`
    /// 原样，否则**按字节**取前 n 个再追加 `"..."`。上游 nginx 错误页常超 200 字节，
    /// 旧实现（按字符、无后缀）与 Go 的 err 文案不同。
    #[test]
    fn truncate_matches_go_byte_semantics() {
        assert_eq!(truncate("abc", 200), "abc", "短于上限原样");
        assert_eq!(truncate(&"x".repeat(200), 200), "x".repeat(200), "恰好上限原样");
        let long = "y".repeat(250);
        assert_eq!(truncate(&long, 200), format!("{}...", "y".repeat(200)));
        // 多字节字符不许被拦腰截断（Go 会产出非法 UTF-8 再由其 JSON 编码器写成
        // U+FFFD；Rust 的 String 放不下，故退到 n 以内最后的字符边界——纯 ASCII
        // 输入与 Go 逐字节一致，nginx 错误页即此形态）。
        let s = format!("{}中", "z".repeat(199));
        let got = truncate(&s, 200);
        assert!(got.ends_with("..."));
        assert!(got.starts_with(&"z".repeat(199)));
    }

    #[test]
    fn parse_summary_merges_labels_and_sorts_desc() {
        let body = br#"{
          "billing":{"effective_available_points":1234.5},
          "daily_allowance":{"quota_points":"10000.000000000","used_points":"10.5","remaining_points":"9989.5","period_end_at":"2099-01-01T00:00:00Z"},
          "gift_credits":{"remaining_points":"500.25"},
          "coding_plan":{"effective_remaining_points":"9989.5","next_boundary_at":"2099-01-02T00:00:00+08:00",
            "windows":[{"window_type":"usage_5h","quota_points":"800","used_points":"1","remaining_points":"799","exhausted":false,"period":{"end_at":"2099-01-01T05:00:00+08:00"}}]},
          "period":{"start_date":"2099-01-01","end_date":"2099-01-31"},
          "totals":{"recorded_points":42.5,"settlement_count":7,"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
            "by_source":[{"label":"codely-core","recorded_points":10,"settlement_count":2,"total_tokens":50},
                         {"label":"codely-core","recorded_points":5,"settlement_count":1,"total_tokens":30},
                         {"label":"kimi-k3","recorded_points":27.5,"settlement_count":4,"total_tokens":40}]}
        }"#;
        let v: Value = serde_json::from_slice(body).unwrap();
        let q = parse_summary(&v);
        assert_eq!(q.source, "live");
        assert_eq!(q.daily.quota, 10000.0);
        assert_eq!(q.daily.used, 10.5);
        assert_eq!(q.total, 9989.5 + 500.25 + 1234.5);
        assert_eq!(q.plan.windows.len(), 1);
        assert_eq!(q.plan.windows[0].r#type, "usage_5h");
        assert_eq!(q.by_source.len(), 2, "同名 label 合并");
        assert_eq!(q.by_source[0].label, "kimi-k3");
        assert_eq!(q.by_source[0].points, 27.5);
        assert_eq!(q.by_source[1].label, "codely-core");
        assert_eq!(q.by_source[1].points, 15.0);
        assert_eq!(q.by_source[1].tokens, 80);
        assert_eq!(q.by_source[1].requests, 3);
        assert_eq!(q.month.total_tokens, 120);
        assert_eq!(q.month.requests, 7);
    }

    #[test]
    fn quota_json_matches_go_field_order_and_omitempty() {
        let q = Quota {
            total: 1.5,
            daily: DailyQuota {
                quota: 2.0,
                used: 0.5,
                remaining: 1.5,
                period_end: String::new(),
            },
            gift: GiftQuota { remaining: 0.0 },
            month: MonthQuota {
                points: 0.0,
                total_tokens: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                requests: 0,
                start_date: String::new(),
                end_date: String::new(),
            },
            by_source: Vec::new(),
            plan: PlanQuota::default(),
            source: "live".to_string(),
            err: String::new(),
        };
        assert_eq!(
            q.to_go_json(),
            r#"{"total":1.5,"daily":{"quota":2,"used":0.5,"remaining":1.5},"gift":{"remaining":0},"month":{"points":0,"totalTokens":0,"promptTokens":0,"completionTokens":0,"requests":0},"plan":{"effective_remaining":0},"source":"live"}"#
        );
        let round = Quota::from_go_json(&q.to_go_json()).unwrap();
        assert_eq!(round.source, "live");
        assert_eq!(round.total, 1.5);
        assert_eq!(round.plan.windows.len(), 0);
    }

    #[test]
    fn none_quota_carries_err() {
        let q = Quota::none("x");
        assert_eq!(
            q.to_go_json(),
            r#"{"total":0,"daily":{"quota":0,"used":0,"remaining":0},"gift":{"remaining":0},"month":{"points":0,"totalTokens":0,"promptTokens":0,"completionTokens":0,"requests":0},"plan":{"effective_remaining":0},"source":"none","err":"x"}"#
        );
    }
}
