//! `internal/proxy/usage.go`：`/v1/usage`——聚合 CommandCode 官网的四个用量端点。
//!
//! ⚠ `json.RawMessage` 的形态必须**原样内嵌**（Go 的 `json.Marshal` 对 RawMessage 是
//! `compact` + 复制原始字节，不是重新编码），所以本模块不走 `Value` 往返，而是直接
//! 拼接上游返回的字节。

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::gjson::{push_go_float, push_go_string, to_go, GoObj};
use crate::stats::plan_label;
use crate::Proxy;

/// 成功结果缓存 TTL（GUI 每 8s 轮询，官网数字更新慢）。
pub const USAGE_TTL: Duration = Duration::from_secs(20);

/// `itemTokens`（内部聚合用；**不参与 JSON 输出**——Go 里它的字段全未导出）。
#[derive(Debug, Clone, Default)]
pub struct ItemTokens {
    pub r#model: String,
    pub r#in: i64,
    pub r#out: i64,
    pub cache_read: i64,
    pub cost: f64,
    pub day: String,
}

/// `usageSummary` 里 RawMessage 字段的对（`Some("")` = nil，omitempty 不写）。
type Raw = Option<String>;

/// `HandleUsage` 的完整业务逻辑（返回下游体与缓存是否命中）。
pub struct UsageOutcome {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub cache_hit: bool,
}

impl UsageOutcome {
    fn err(status: u16, message: &str, err_type: &str) -> UsageOutcome {
        UsageOutcome {
            status,
            content_type: "application/json",
            body: crate::chat::openai_error_body(message, err_type).into_bytes(),
            cache_hit: false,
        }
    }
}

/// `HandleUsage`。
pub async fn handle_usage(
    proxy: &Proxy,
    method: &str,
    auth: Option<&str>,
    query: &std::collections::HashMap<String, String>,
) -> UsageOutcome {
    if method != "GET" {
        return UsageOutcome::err(405, "Method not allowed", "invalid_request_error");
    }
    // 缓存命中：直接返回上次成功结果（官网数字更新慢，20s 延迟可接受）
    {
        let guard = proxy.usage_cache.lock().expect("usage 缓存锁");
        if let (Some(data), Some(at)) = (&guard.data, guard.at) {
            if at.elapsed() < USAGE_TTL {
                return UsageOutcome {
                    status: 200,
                    content_type: "application/json",
                    body: data.clone(),
                    cache_hit: true,
                };
            }
        }
    }

    let api_key = match auth {
        Some(h) if !h.is_empty() => trim_bearer(h),
        _ => trim_bearer(&proxy.api_key),
    };
    if api_key.is_empty() {
        return UsageOutcome::err(
            401,
            "API key required. Set Authorization header.",
            "authentication_error",
        );
    }

    // 1. whoami → org id
    let whoami_raw = match get(proxy, &api_key, "/alpha/whoami", &[]).await {
        Ok(b) => b,
        Err(e) => {
            let mut errors = vec![e];
            let mut body = encode_summary(
                &None,
                &None,
                &None,
                &None,
                &mut errors,
                "",
                "",
                0.0,
                0.0,
                0.0,
                0.0,
                0,
                "https://commandcode.ai/usage",
                &None,
                "",
                0,
                0,
                0,
                0,
                None,
                0,
                None,
                None,
            );
            body.push('\n');
            return UsageOutcome {
                status: 200,
                content_type: "application/json",
                body: body.into_bytes(),
                cache_hit: false,
            };
        }
    };
    let whoami_txt = String::from_utf8_lossy(&whoami_raw).to_string();
    let org_id = crate::godecode::parse_value(&whoami_raw)
        .ok()
        .and_then(|v| {
            v.get("org")
                .and_then(|o| o.get("id"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    let params: Vec<(&str, String)> = if org_id.is_empty() {
        Vec::new()
    } else {
        vec![("orgId", org_id.clone())]
    };

    // 2-4. credits / subscription / summary 并行拉取
    let (credits, sub, summary) = {
        let f1 = get(proxy, &api_key, "/alpha/billing/credits", &params);
        let f2 = get(proxy, &api_key, "/alpha/billing/subscriptions", &params);
        let f3 = get(proxy, &api_key, "/alpha/usage/summary", &params);
        tokio::join!(f1, f2, f3)
    };
    let credits = credits.ok();
    let sub = sub.ok();
    let summary = summary.ok();

    let sub_plan_id = sub
        .as_ref()
        .and_then(|b| crate::godecode::parse_value(b).ok())
        .and_then(|v| {
            v.get("data")
                .and_then(|d| d.get("planId"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    let sub_period_end = sub
        .as_ref()
        .and_then(|b| crate::godecode::parse_value(b).ok())
        .and_then(|v| {
            v.get("data")
                .and_then(|d| d.get("currentPeriodEnd"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    let summary_v = summary
        .as_ref()
        .and_then(|b| crate::godecode::parse_value(b).ok())
        .unwrap_or(serde_json::Value::Null);
    let total_cost = summary_v
        .get("totalCost")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let total_count = summary_v.get("totalCount").and_then(crate::godecode::go_int).unwrap_or(0);
    let total_tokens = summary_v
        .get("totalTokens")
        .and_then(crate::godecode::go_int)
        .unwrap_or(0);
    let total_in = summary_v
        .get("totalTokensIn")
        .and_then(crate::godecode::go_int)
        .unwrap_or(0);
    let total_out = summary_v
        .get("totalTokensOut")
        .and_then(crate::godecode::go_int)
        .unwrap_or(0);

    let credits_v = credits
        .as_ref()
        .and_then(|b| crate::godecode::parse_value(b).ok())
        .unwrap_or(serde_json::Value::Null);
    let num = |path: &[&str]| -> f64 {
        let mut cur = &credits_v;
        for p in path {
            cur = match cur.get(*p) {
                Some(v) => v,
                None => return 0.0,
            };
        }
        cur.as_f64().unwrap_or(0.0)
    };
    let win = |path: &[&str]| -> Option<(f64, f64, i64)> {
        let mut cur = &credits_v;
        for p in path {
            cur = match cur.get(*p) {
                Some(v) => v,
                None => return None,
            };
        }
        if cur.is_null() {
            return None;
        }
        Some((
            cur.get("used").and_then(|x| x.as_f64()).unwrap_or(0.0),
            cur.get("cap").and_then(|x| x.as_f64()).unwrap_or(0.0),
            cur.get("resetAt").and_then(crate::godecode::go_int).unwrap_or(0),
        ))
    };
    let five_hour = win(&["windowLimits", "fiveHour"]);
    let weekly = win(&["windowLimits", "weekly"]);

    let credits_avl = num(&["credits", "monthlyCredits"])
        + num(&["credits", "purchasedCredits"])
        + num(&["credits", "freeCredits"]);
    let credits_tot = credits_avl + total_cost;
    let percent = if credits_tot > 0.0 {
        let p = total_cost / credits_tot * 100.0;
        if p > 100.0 {
            100.0
        } else {
            p
        }
    } else {
        0.0
    };
    let days_left = match parse_rfc3339(&sub_period_end) {
        Some(t) => days_left_from(t, crate::util::unix_now()),
        None => 0,
    };

    // 5. 明细列表（`/internal/usage`，仅组织账号可拉）
    let mut items_raw: Raw = None;
    let mut list_cursor = String::new();
    let mut model_breakdown: BTreeMap<String, ItemTokens> = BTreeMap::new();
    let mut today_tokens = 0i64;
    if !org_id.is_empty() {
        let mut lp: Vec<(&str, String)> = vec![
            ("limit", query.get("limit").cloned().unwrap_or_else(|| "500".to_string())),
            ("orgId", org_id.clone()),
        ];
        if let Some(c) = query.get("cursor") {
            lp.push(("cursor", c.clone()));
        }
        if let Ok(list_raw) = get(proxy, &api_key, "/internal/usage", &lp).await {
            if let Ok(v) = crate::godecode::parse_value(&list_raw) {
                if let Some(items) = v.get("items") {
                    if !items.is_null() {
                        // ⚠ 必须内嵌**上游原文**（Go 的 `Items json.RawMessage` 原样转发），
                        // 不能 `to_go(items)` 重新编码：那会把 items[] 里每个对象的键按字节序重排
                        // （实测 `usage-endpoints #0`：黄金是 `model,inputTokens,outputTokens,…`，
                        // 重编码后变成 `cacheReadTokens,cost,createdAt,inputTokens,…`）。
                        items_raw = Some(
                            raw_member_of(&String::from_utf8_lossy(&list_raw), "items")
                                .unwrap_or_else(|| to_go(items)),
                        );
                    }
                    model_breakdown = model_breakdown_of(items);
                    today_tokens = today_tokens_of(items);
                }
                if let Some(c) = v.get("cursor").and_then(|x| x.as_str()) {
                    list_cursor = c.to_string();
                }
            }
        }
    }

    let mut errors: Vec<String> = Vec::new();
    let credits_raw: Raw = credits.as_ref().map(|b| String::from_utf8_lossy(b).to_string());
    let sub_raw: Raw = sub.as_ref().map(|b| String::from_utf8_lossy(b).to_string());
    let summary_raw: Raw = summary
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).to_string());
    let mut body = encode_summary(
        &Some(whoami_txt),
        &credits_raw,
        &sub_raw,
        &summary_raw,
        &mut errors,
        &plan_label(&sub_plan_id),
        &sub_plan_id,
        total_cost,
        credits_avl,
        credits_tot,
        percent,
        days_left,
        "https://commandcode.ai/usage",
        &items_raw,
        &list_cursor,
        total_tokens,
        total_count,
        total_in,
        total_out,
        Some(&model_breakdown),
        today_tokens,
        five_hour,
        weekly,
    );

    // ⚠ 缓存口径**逐字照抄 Go**：Go 是 `p.writeJSON(w, sum)`（`json.NewEncoder` ⇒ 带末尾换行）
    // 之后才 `json.Marshal(sum)` 落进 `usageData`，所以**缓存里存的是不带换行的形态**；
    // 命中缓存时 `p.usageData` 直接 `w.Write`，于是**缓存命中的响应体没有末尾换行**。
    // 黄金实测：`usage-endpoints` 两步看似同体，长度 1298 / 1297 —— 差的正是这一个字节。
    // 这里同样先给下游响应补换行、再缓存未补换行的形态，两个分支才都对得上。
    if errors.is_empty() && total_tokens > 0 {
        let mut g = proxy.usage_cache.lock().expect("usage 缓存锁");
        g.data = Some(body.clone().into_bytes());
        g.at = Some(Instant::now());
    }
    body.push('\n');

    UsageOutcome {
        status: 200,
        content_type: "application/json",
        body: body.into_bytes(),
        cache_hit: false,
    }
}

/// 上游 GET（Go 的 `get` 闭包）：非 200 → `"<endpoint> returned <code>: <body 截断>"`。
async fn get(
    proxy: &Proxy,
    api_key: &str,
    endpoint: &str,
    params: &[(&str, String)],
) -> Result<Vec<u8>, String> {
    let mut url = format!("{}{endpoint}", proxy.base_url);
    if !params.is_empty() {
        let q: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect();
        url.push('?');
        url.push_str(&q.join("&"));
    }    let resp = proxy
        .client
        .raw()
        .get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
    if status != 200 {
        return Err(format!(
            "{endpoint} returned {status}: {}",
            crate::truncate_log(&String::from_utf8_lossy(&body))
        ));
    }
    Ok(body)
}

/// `url.Values.Encode()` 的等价物（Go 用 `QueryEscape`：空格→`+`，其余按 RFC 3986 百分号编码）。
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `trimBearer`：去 `Bearer ` 前缀（**不做大小写规范化**——会破坏 `user_` 前缀的 key）。
pub fn trim_bearer(key: &str) -> String {
    let k = key.trim();
    if k.len() > 7 && k[..7].eq_ignore_ascii_case("Bearer ") {
        return k[7..].trim().to_string();
    }
    k.to_string()
}

/// RFC3339 解析（Go `time.Parse(time.RFC3339, …)`）；失败返回 None。
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let t = crate::util::parse_rfc3339(s)?;
    Some(t)
}

/// `daysLeft`：`int(time.Until(period_end).Hours()/24) + 1`，负数钳到 0。
///
/// **两步都必须逐字对齐 Go**（`usage.go:241-246`）：
/// * `time.Until(t).Hours()` 是 `float64(d) / float64(time.Hour)`，`d` 是 `time.Duration`
///   （纳秒，**整数**）。本 crate 的时间戳单位是秒，故等价式为
///   `float64(period_end_unix - now_unix) / 3600.0`（纳秒在本路径上必然是 1e9 的整数倍，
///   先 ×1e9 再 ÷1e9 只会白白多两次舍入，**不做**）；
/// * 除法在 `int()` **之外** ⇒ Go 先算 `float64` 的小时数、**再**截断到整数；
///   Rust 的 `as i64` 同样是向零截断（`trunc`），**不是** `floor`。
///
/// 原先写成 `.floor()` 且把「天」直接当除法单位，两者在「周期已结束但不足一天」时给出
/// 不同答案：负的分数天经 `trunc` 得 `-0 → 0`（+1 后 `daysLeft=1`），经 `floor` 得 `-1`
/// （+1 后 `0`）——而 `daysLeft=0` 本该只出现在「周期还没开始」的形态里。
fn days_left_from(period_end_unix: i64, now_unix: i64) -> i64 {
    let hours = (period_end_unix - now_unix) as f64 / 3600.0;
    // Go：`int(time.Until(t).Hours()/24) + 1` —— 先在 **f64** 里除 24，再向零截断。
    // Rust 的 `as i64` 与 Go 的 `int()` 同为向零截断（故负数也是 trunc 而非 floor）。
    let d = (hours / 24.0).trunc() as i64 + 1;
    if d < 0 {
        0
    } else {
        d
    }
}

/// `modelBreakdown`：按模型聚合 items 的 token（字段缺失即忽略）。
pub fn model_breakdown_of(items: &serde_json::Value) -> BTreeMap<String, ItemTokens> {
    let mut out: BTreeMap<String, ItemTokens> = BTreeMap::new();
    let arr = match items.as_array() {
        Some(a) => a,
        None => return out,
    };
    for it in arr {
        let m = match it.as_object() {
            Some(m) => m,
            None => continue,
        };
        let mut model = m
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if model.is_empty() {
            model = "unknown".to_string();
        }
        let mut e = out.get(&model).cloned().unwrap_or_default();
        e.r#model = model.clone();
        for (dst, keys) in [
            (&mut e.r#in, ["inputTokens", "input_tokens", "tokensIn"]),
            (&mut e.r#out, ["outputTokens", "output_tokens", "tokensOut"]),
            (
                &mut e.cache_read,
                ["cacheReadTokens", "cache_read_tokens", ""],
            ),
        ] {
            for k in keys {
                if k.is_empty() {
                    continue; // 占位（数组长度必须一致）
                }
                if let Some(v) = m.get(k) {
                    if let Some(n) = crate::godecode::go_int(v) {
                        *dst += n;
                        break;
                    }
                }
            }
        }
        for k in ["cost", "amount"] {
            if let Some(v) = m.get(k) {
                if let Some(n) = v.as_f64() {
                    e.cost += n;
                    break;
                }
            }
        }
        for k in ["createdAt", "created_at", "time", "timestamp"] {
            if let Some(v) = m.get(k) {
                if let Some(s) = v.as_str() {
                    if !s.is_empty() {
                        e.day = ts_to_day(s);
                        break;
                    }
                }
            }
        }
        out.insert(model, e);
    }
    out
}

/// `todayTokens`：今天（本地时区）所有模型的 token 合计。
pub fn today_tokens_of(items: &serde_json::Value) -> i64 {
    let today = crate::stats::day_key();
    let mut total = 0i64;
    for mb in model_breakdown_of(items).values() {
        if mb.day == today {
            total += mb.r#in + mb.r#out + mb.cache_read;
        }
    }
    total
}

/// `tsToDay`：RFC3339 → 本地日期；解析失败返回空串。
pub fn ts_to_day(ts: &str) -> String {
    match crate::util::parse_rfc3339(ts) {
        Some(t) => crate::util::local_day(t),
        None => String::new(),
    }
}

/// 把 `usageSummary` 编码成 JSON（键序 = Go 结构体声明序 + `omitempty` 规则）。
#[allow(clippy::too_many_arguments)]
fn encode_summary(
    whoami: &Raw,
    credits: &Raw,
    sub: &Raw,
    summary: &Raw,
    errors: &mut Vec<String>,
    plan_label: &str,
    plan_id: &str,
    cost_usd: f64,
    credits_avl: f64,
    credits_tot: f64,
    percent: f64,
    days_left: i64,
    usage_url: &str,
    items: &Raw,
    list_cursor: &str,
    total_tokens: i64,
    total_runs: i64,
    tokens_in: i64,
    tokens_out: i64,
    model_breakdown: Option<&BTreeMap<String, ItemTokens>>,
    today_tokens: i64,
    five_hour: Option<(f64, f64, i64)>,
    weekly: Option<(f64, f64, i64)>,
) -> String {
    let mut o = GoObj::new();
    // RawMessage + omitempty：nil 不写
    if let Some(v) = whoami {
        o.raw("whoami", &raw_of(v));
    }
    if let Some(v) = credits {
        o.raw("credits", &raw_of(v));
    }
    if let Some(v) = sub {
        o.raw("subscription", &raw_of(v));
    }
    if let Some(v) = summary {
        o.raw("summary", &raw_of(v));
    }
    if !errors.is_empty() {
        let mut s = String::from("[");
        for (i, e) in errors.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            push_go_string(&mut s, e);
        }
        s.push(']');
        o.raw("errors", &s);
    }
    o.opt_str("planLabel", plan_label);
    o.opt_str("planId", plan_id);
    o.field_f64("costUSD", cost_usd);
    o.field_f64("creditsAvailable", credits_avl);
    o.field_f64("creditsTotal", credits_tot);
    o.field_f64("percentUsed", percent);
    o.field_i64("daysLeft", days_left);
    o.opt_str("usageUrl", usage_url);
    if let Some(v) = items {
        o.raw("items", &raw_of(v));
    }
    o.opt_str("listCursor", list_cursor);
    o.field_i64("totalTokens", total_tokens);
    o.field_i64("totalRuns", total_runs);
    o.field_i64("tokensIn", tokens_in);
    o.field_i64("tokensOut", tokens_out);
    // Go 的字段带 `json:"modelBreakdown,omitempty"`（`usage.go:36`）⇒ **空表整个键都不写**；
    // Rust 没有 omitempty，必须自己判空（原先恒写 `"modelBreakdown":{}`，与 Go 的字节不同）。
    if let Some(mb) = model_breakdown {
        if !mb.is_empty() {
            let mut s = String::from("{");
            for (i, (k, _)) in mb.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                push_go_string(&mut s, k);
                s.push(':');
                // ⚠ `itemTokens` 的字段**全未导出** ⇒ Go 编出 `{}`（这是实测形状，不是简化）
                s.push_str("{}");
            }
            s.push('}');
            o.raw("modelBreakdown", &s);
        }
    }
    o.field_i64("todayTokens", today_tokens);
    if let Some((u, c, r)) = five_hour {
        o.raw("fiveHour", &win_json(u, c, r));
    }
    if let Some((u, c, r)) = weekly {
        o.raw("weekly", &win_json(u, c, r));
    }
    o.finish()
}

fn win_json(used: f64, cap: f64, reset_at: i64) -> String {
    let mut o = GoObj::new();
    o.field_f64("used", used)
        .field_f64("cap", cap)
        .field_i64("resetAt", reset_at);
    o.finish()
}

/// `json.RawMessage` 的写出：原样内嵌（空 → `null`，与 Go 的 `appendCompact(nil, nil)` 同）。
fn raw_of(s: &str) -> String {
    if s.is_empty() {
        "null".to_string()
    } else {
        s.to_string()
    }
}

/// 从 JSON 文本里取某个**顶层**成员的值原文（含内部结构，去掉它前面的空白）。
///
/// 对齐 Go 的 `json.RawMessage`：进的是上游那串字节走完 `unmarshal → marshal` 的结果。
/// ⚠ 逐字节：全程按 `u8` 走字符串/转义扫描，**不做 `&str` 下标切片**（会把多字节字符切一半 panic）。
/// 找不到、或值不完整时返回 `None`（调用方回落重新编码，并保留原有行为）。
pub fn raw_member_of(json: &str, key: &str) -> Option<String> {
    let b = json.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
                continue;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
                continue;
            }
            b'"' => {
                let (lit_start, next) = scan_string(b, i)?;
                let lit = String::from_utf8_lossy(&b[lit_start..next]).to_string();
                i = next;
                // 只认**顶层对象的成员名**（`{ "items": … }` 里的 `items`），
                // 不认数组元素或更深层对象里的同名键。
                if depth != 1 {
                    continue;
                }
                let decoded = decode_string_literal(&lit);
                if decoded != key {
                    continue;
                }
                let mut j = i;
                while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                    j += 1;
                }
                if j >= b.len() || b[j] != b':' {
                    continue;
                }
                j += 1;
                while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                    j += 1;
                }
                let end = scan_value(b, j)?;
                return Some(String::from_utf8_lossy(&b[j..end]).to_string());
            }
            _ => i += 1,
        }
    }
    None
}

/// 从 `start`（指向 `"`）扫过一个字符串字面量，返回（含引号的片段起点..终点的**下一字节**）。
fn scan_string(b: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some((start, i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// 从 `start` 起扫过一个 JSON 值，返回它的结束偏移（不含）。
fn scan_value(b: &[u8], start: usize) -> Option<usize> {
    if start >= b.len() {
        return None;
    }
    match b[start] {
        b'"' => scan_string(b, start).map(|(_, n)| n),
        b'{' | b'[' => {
            let (open, close) = if b[start] == b'{' { (b'{', b'}') } else { (b'[', b']') };
            let mut depth = 0usize;
            let mut i = start;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        let (_, n) = scan_string(b, i)?;
                        i = n;
                        continue;
                    }
                    c if c == open => depth += 1,
                    c if c == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            // 数字 / true / false / null：吃到分隔符
            let mut i = start;
            while i < b.len()
                && b[i] != b','
                && b[i] != b'}'
                && b[i] != b']'
                && !(b[i] as char).is_ascii_whitespace()
            {
                i += 1;
            }
            Some(i)
        }
    }
}

/// 把一段 `"..."` 字面量解成字符串（只处理 `\"`/`\\`/`\/`/常见转义与 `\uXXXX` 的 ASCII 面）。
fn decode_string_literal(lit: &str) -> String {
    let inner = lit.trim_matches('"');
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                    if let Some(ch) = char::from_u32(n) {
                        out.push(ch);
                    }
                }
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

/// `percentUsed` 的浮点写出走 Go 规则（供门禁核对格式）。
pub fn percent_json(v: f64) -> String {
    let mut s = String::new();
    push_go_float(&mut s, v);
    s
}

/// 供测试：把 `Mutex` 里的缓存清空。
pub fn clear_cache_guard(cache: &Mutex<crate::UsageCache>) {
    let mut g = cache.lock().expect("usage 缓存锁");
    g.data = None;
    g.at = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trim_bearer_ignores_case_but_keeps_key_case() {
        assert_eq!(trim_bearer("Bearer abc"), "abc");
        assert_eq!(trim_bearer("bearer abc"), "abc");
        assert_eq!(trim_bearer("  abc  "), "abc");
        assert_eq!(trim_bearer("Bearer "), "Bearer");
        assert_eq!(trim_bearer("user_aBcD"), "user_aBcD");
    }

    #[test]
    fn days_left_truncates_toward_zero_like_go() {
        // 参考实现：逐字照抄 Go `usage.go:242` 的 `int(time.Until(t).Hours()/24) + 1`，
        // 用 f64→i64 的向零截断（`as` 与 Go `int()` 同语义），再钳负。
        // 入参是**离周期结束的小时数**（`time.Duration.Hours()` 的量纲），换算成天再截断。
        let go = |hours: f64| -> i64 {
            let d = (hours / 24.0) as i64 + 1;
            if d < 0 {
                0
            } else {
                d
            }
        };
        // 每条用例给出「距周期结束的小时数」；`period_end` 按该小时数换算成 epoch 秒
        // （基准 now = 0），这样左右的输入单位一致。
        for (hours, why) in [
            (26_667.5 * 24.0, "远期分数天（黄金里 2099-11-01 那一族的形态）"),
            (48.5, "正的分数天"),
            (-0.5, "周期过去 30 分钟：trunc→0(+1=1)；floor→-1(+1=0) ← 曾经的 floor 分歧"),
            (0.0, "恰好在周期结束点"),
            (-24.0 * 3.25, "过去 3 天 6 小时（两条规则都得 -3，再钳到 0）"),
        ] {
            let period_end = (hours * 3600.0) as i64;
            let got = days_left_from(period_end, 0);
            // 参考值直接用同一小时数（`days_left_from` 内部也是「小时」量纲）
            assert_eq!(
                got,
                go(hours),
                "days_left_from(period_end={period_end}, now=0) 与 Go 不一致（{why}）"
            );
        }
        // 正向形状自检：黄金里那条 2099-11-01 的周期，在「今天」必须是一个**正整数天**
        // （若哪天算成 0，说明截断/取整口径又坏了，而不是「周期没开始」）。
        let end_2099 = crate::util::parse_rfc3339("2099-11-01T00:00:00Z").unwrap();
        let now = crate::util::unix_now();
        let d = days_left_from(end_2099, now);
        assert!(d > 20000 && d < 30000, "2099-11-01 距今应在 2~3 万天，实得 {d}");
        // 与同口径的朴素算式一致（同一 now，误差 0 天）
        assert_eq!(d, ((end_2099 - now) as f64 / 86400.0) as i64 + 1);
    }

    #[test]
    fn model_breakdown_ignores_string_token_fields() {
        // Go 的 `json.Unmarshal(v, &n int64)` 对字符串失败 → 该字段不计入；
        // 于是即便模型出现了，itemTokens 也是零值。
        let items = json!([
            {"model":"m1","inputTokens":"100","outputTokens":50},
            {"model":"m1","inputTokens":7}
        ]);
        let mb = model_breakdown_of(&items);
        assert_eq!(mb["m1"].r#in, 7, "字符串形态的 inputTokens 不应计入");
        assert_eq!(mb["m1"].r#out, 50);
    }

    #[test]
    fn model_breakdown_buckets_unknown_and_reads_camel_aliases() {
        let items = json!([
            {"inputTokens":1},
            {"model":"m2","tokensIn":3,"tokensOut":4,"cache_read_tokens":5,"amount":0.5}
        ]);
        let mb = model_breakdown_of(&items);
        assert!(mb.contains_key("unknown"));
        assert_eq!(mb["unknown"].r#in, 1);
        assert_eq!(mb["m2"].r#in, 3);
        assert_eq!(mb["m2"].r#out, 4);
        assert_eq!(mb["m2"].cache_read, 5);
        assert!((mb["m2"].cost - 0.5).abs() < 1e-12);
    }

    #[test]
    fn model_breakdown_non_empty_is_written_in_key_order() {
        // 反向面：非空时仍要按 Go 的 map 键字节序写（`omitempty` 只影响「空表」这一种形态）。
        let mb = BTreeMap::from([
            (
                "zz".to_string(),
                ItemTokens {
                    r#model: "zz".into(),
                    ..Default::default()
                },
            ),
            (
                "aa".to_string(),
                ItemTokens {
                    r#model: "aa".into(),
                    ..Default::default()
                },
            ),
        ]);
        let body = encode_summary(
            &None,
            &None,
            &None,
            &None,
            &mut Vec::new(),
            "Pro",
            "pro",
            0.0,
            0.0,
            0.0,
            0.0,
            0,
            "",
            &None,
            "",
            0,
            0,
            0,
            0,
            Some(&mb),
            0,
            None,
            None,
        );
        assert!(
            body.contains("\"modelBreakdown\":{\"aa\":{},\"zz\":{}}"),
            "非空表要按字节序写：{body}"
        );
    }

    #[test]
    fn usage_summary_key_order_and_rawwmessage_shape() {
        let body = encode_summary(
            &Some("{\"a\":1}".to_string()),
            &Some("{\"b\":2}".to_string()),
            &None,
            &None,
            &mut Vec::new(),
            "Pro",
            "pro",
            5.5,
            15.0,
            20.5,
            26.82926829268293,
            26679,
            "https://commandcode.ai/usage",
            &Some("[]".to_string()),
            "c1",
            100000,
            12,
            60000,
            40000,
            Some(&BTreeMap::new()),
            0,
            Some((1.5, 5.0, 1790000000000)),
            None,
        );
        assert!(body.starts_with("{\"whoami\":{\"a\":1},\"credits\":{\"b\":2},"), "{body}");
        assert!(body.contains("\"percentUsed\":26.82926829268293"), "{body}");
        // Go 的 `modelBreakdown` 带 omitempty ⇒ 空表连键都不写（原先恒写 `{}` 是分歧）
        assert!(
            !body.contains("modelBreakdown"),
            "空表必须按 omitempty 省略：{body}"
        );
        assert!(body.contains("\"fiveHour\":{\"used\":1.5,\"cap\":5,\"resetAt\":1790000000000}"), "{body}");
        assert!(!body.contains("subscription"), "nil RawMessage 应被 omitempty 跳过：{body}");
        assert!(!body.contains("weekly"));
        // 末尾换行由 handler 补（Go 的 `p.writeJSON` 走 json.NewEncoder），不在 encode_summary 里
        assert!(!body.ends_with('\n'), "encode_summary 本身不带换行：{body}");
    }

    /// `raw_member_of`：`/internal/usage` 的 `items` 必须**原文内嵌**（Go 的 `json.RawMessage`）。
    ///
    /// 这一条锁的是「不许重编码」：`to_go(items)` 会把每个对象的键按字节序重排，
    /// 实测 `usage-endpoints #0` 的黄金是 `model,inputTokens,outputTokens,cacheReadTokens,cost,createdAt`，
    /// 重编码后变成 `cacheReadTokens,cost,createdAt,inputTokens,model,outputTokens`。
    #[test]
    fn raw_member_of_keeps_upstream_byte_order() {
        let upstream = r#"{"items":[{"model":"m1","inputTokens":100,"outputTokens":50,"cacheReadTokens":10,"cost":0.25,"createdAt":"2099-10-16T01:02:03Z"},{"inputTokens":1}],"cursor":"c1","nested":{"items":[9]}}"#;
        let got = raw_member_of(upstream, "items").expect("items 应能取到");
        assert_eq!(
            got,
            r#"[{"model":"m1","inputTokens":100,"outputTokens":50,"cacheReadTokens":10,"cost":0.25,"createdAt":"2099-10-16T01:02:03Z"},{"inputTokens":1}]"#,
            "键序必须与上游原文一致"
        );
        // 顶层成员名才算；数组元素/内层同名键不算（`nested.items` 不能被选中）
        assert!(raw_member_of(r#"{"a":{"items":[1]}}"#, "items").is_none());
        assert!(raw_member_of(r#"[{"items":[1]}]"#, "items").is_none());
        // 字符串字面量里的 `items:` 不算成员名
        assert!(raw_member_of(r#"{"s":"items: [1]"}"#, "items").is_none());
        // 值形态：字符串 / 数字 / null
        assert_eq!(raw_member_of(r#"{"items":"x"}"#, "items").unwrap(), "\"x\"");
        assert_eq!(raw_member_of(r#"{"items": 1e3 }"#, "items").unwrap(), "1e3");
        assert_eq!(raw_member_of(r#"{"items":null}"#, "items").unwrap(), "null");
        // 多字节字符不得 panic（曾因 `&str` 下标切片踩过 char boundary panic）
        assert_eq!(
            raw_member_of(r#"{"s":"你好","items":["好"]}"#, "items").unwrap(),
            r#"["好"]"#
        );
    }

    #[test]
    fn nil_raw_message_encodes_as_null_when_present() {
        // 非 nil 的 RawMessage 承载空串（json.RawMessage("")）时 Go 编出 `null`
        let body = encode_summary(
            &Some(String::new()),
            &None,
            &None,
            &None,
            &mut Vec::new(),
            "",
            "",
            0.0,
            0.0,
            0.0,
            0.0,
            0,
            "",
            &None,
            "",
            0,
            0,
            0,
            0,
            None,
            0,
            None,
            None,
        );
        assert!(body.starts_with("{\"whoami\":null"), "{body}");
    }
}
