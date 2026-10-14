//! quota.rs —— WorkBuddy 积分（官方 `get-user-resource` 口径）查询，对应 Go
//! `internal/codebuddy/quota.go`。
//!
//! 数据源：`POST {base}/v2/billing/meter/get-user-resource`（认证与 chat 转发同一凭证）。
//! 缓存：**按区 + 号分键**（每个实例各一份 map，CN/INTL 天然隔离），uid 空用固定键
//! `_default`；TTL 5 分钟；失败回退**该 uid 自己的**缓存（绝不串号）。
//! 401/403 = 凭据失效：与聊天路径同一信号，调用方据此 `MarkDead`（手动刷新积分 = 免费验活）。

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{Map, Value};

use crate::cred::CredSource;
use crate::failover::format_full_display;
use crate::gjson;
use crate::gotime;
use crate::pool::AccountPool;

/// Go `quotaPath`。
pub const QUOTA_PATH: &str = "/v2/billing/meter/get-user-resource";
/// Go `quotaTimeout`。
pub const QUOTA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Go `quotaTTL`。
pub const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// Go `quotaDefaultKey`（默认号（不带 uid 的查询）的缓存键）。
pub const QUOTA_DEFAULT_KEY: &str = "_default";
/// Go `io.LimitReader(resp.Body, 2<<20)`。
pub const MAX_QUOTA_BODY: usize = 2 << 20;

/// 积分快照（GUI 链路监测积分卡用，官方口径）。
#[derive(Debug, Clone, Default)]
pub struct Quota {
    pub total: f64,
    pub packs: i64,
    pub active: ActivePack,
    pub nearest_end: NearestPack,
    pub uid: String,
    pub nickname: String,
    pub source: String,
    pub err: String,
}

/// 当前消耗中的包（`CycleEndTime` 最近且 `CapacityUsed>0`）。
#[derive(Debug, Clone, Default)]
pub struct ActivePack {
    pub name: String,
    pub used: f64,
    pub size: f64,
    pub end: String,
}

/// 未过期包里到期最早的包。
#[derive(Debug, Clone, Default)]
pub struct NearestPack {
    pub name: String,
    pub date: String,
    pub remain: f64,
}

/// 响应里的一条资源包（容量字段用 `f64` 宽松解析）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Account {
    #[serde(rename = "PackageName", default)]
    pub package_name: String,
    #[serde(rename = "CycleStartTime", default)]
    pub cycle_start_time: String,
    #[serde(rename = "CycleEndTime", default)]
    pub cycle_end_time: String,
    #[serde(rename = "CapacitySize", default)]
    pub capacity_size: f64,
    #[serde(rename = "CapacityUsed", default)]
    pub capacity_used: f64,
    #[serde(rename = "CapacityRemain", default)]
    pub capacity_remain: f64,
    #[serde(rename = "CycleCapacityRemain", default)]
    pub cycle_capacity_remain: f64,
    #[serde(rename = "CycleCapacitySize", default)]
    pub cycle_capacity_size: f64,
}

impl Quota {
    /// Go `writeJSON` 的字段名与嵌套形态（键排序交给 `gjson`）。
    pub fn to_json(&self) -> Value {
        let active = gjson::obj(vec![
            ("name", gjson::s(&self.active.name)),
            ("used", num(self.active.used)),
            ("size", num(self.active.size)),
            ("end", gjson::s(&self.active.end)),
        ]);
        let nearest = gjson::obj(vec![
            ("name", gjson::s(&self.nearest_end.name)),
            ("date", gjson::s(&self.nearest_end.date)),
            ("remain", num(self.nearest_end.remain)),
        ]);
        let mut pairs = vec![
            ("total", num(self.total)),
            ("packs", Value::Number(self.packs.into())),
            ("active", active),
            ("nearestEnd", nearest),
            ("uid", gjson::s(&self.uid)),
            ("nickname", gjson::s(&self.nickname)),
            ("source", gjson::s(&self.source)),
        ];
        if !self.err.is_empty() {
            pairs.push(("err", gjson::s(&self.err)));
        }
        gjson::obj(pairs)
    }
}

fn num(v: f64) -> Value {
    serde_json::Number::from_f64(v)
        .map(Value::Number)
        // Go 的 `json.Marshal(float64)` 对 Inf/NaN 写 `null`（`gjson` 已按 Go 规则处理，
        // 这里只需把「非有限」交给 Number 之外的分支）。
        .unwrap_or(Value::Null)
}

/// `quotaCacheKey`：uid → 缓存键（空 uid 用固定键）。
pub fn quota_cache_key(uid: &str) -> String {
    if uid.is_empty() {
        QUOTA_DEFAULT_KEY.to_string()
    } else {
        uid.to_string()
    }
}

/// 官方积分接口的**非 200** 响应（typed error：上游状态码可判定）。
#[derive(Debug)]
pub struct QuotaHttpError {
    pub status: u16,
    pub body: String,
}

impl QuotaHttpError {
    pub fn message(&self) -> String {
        format!(
            "官方积分接口 HTTP {}: {}",
            self.status,
            crate::chat::truncate(&self.body, 200)
        )
    }
}

/// 积分缓存（按区 + 号分键；每实例一份）。
#[derive(Default)]
pub struct QuotaCache {
    inner: Mutex<BTreeMap<String, (Quota, Instant)>>,
}

impl QuotaCache {
    pub fn new() -> QuotaCache {
        QuotaCache::default()
    }

    fn get(&self, key: &str) -> Option<(Quota, Instant)> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.get(key).cloned()
    }

    fn put(&self, key: &str, q: Quota) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.insert(key.to_string(), (q, Instant::now()));
    }
}

/// `quotaResolve`：uid → auth 文件路径 + 账号信息。uid 空 = 默认号（池内 mtime 最新）。
///
/// `Ok(None)` = 零值 Server（无池）：沿用当前凭据文件（旧行为）。
pub fn quota_resolve(
    pool: Option<&AccountPool>,
    uid: &str,
) -> Result<Option<(String, String, String)>, String> {
    let Some(p) = pool else {
        return Ok(None);
    };
    if uid.is_empty() {
        return Ok(p.newest_path());
    }
    Ok(p.path_of(uid))
}

/// `FetchQuota`：带 5 分钟缓存；命中缓存返回 `source:"cache"`。
pub async fn fetch_quota(
    http: &reqwest::Client,
    base_url: &str,
    cred: &dyn CredSource,
    cache: &QuotaCache,
    pool: Option<&AccountPool>,
    uid: &str,
) -> Quota {
    let key = quota_cache_key(uid);
    if let Some((q, at)) = cache.get(&key) {
        if at.elapsed() < QUOTA_TTL {
            let mut q = q;
            q.source = "cache".to_string();
            return q;
        }
    }
    fetch_quota_fresh(http, base_url, cred, cache, pool, uid, &key).await
}

/// `FetchQuotaForce`：强制实时拉取（手动刷新按钮用），绕过缓存并回写。
pub async fn fetch_quota_force(
    http: &reqwest::Client,
    base_url: &str,
    cred: &dyn CredSource,
    cache: &QuotaCache,
    pool: Option<&AccountPool>,
    uid: &str,
) -> Quota {
    let key = quota_cache_key(uid);
    fetch_quota_fresh(http, base_url, cred, cache, pool, uid, &key).await
}

/// `fetchQuotaFresh`：实拉一次并回写缓存；失败回退该 key 自己的缓存。
pub async fn fetch_quota_fresh(
    http: &reqwest::Client,
    base_url: &str,
    cred: &dyn CredSource,
    cache: &QuotaCache,
    pool: Option<&AccountPool>,
    uid: &str,
    key: &str,
) -> Quota {
    let resolved = quota_resolve(pool, uid);
    let (path, nick, uid_out) = match resolved {
        Err(e) => {
            return Quota {
                source: "none".to_string(),
                err: e,
                ..Default::default()
            }
        }
        Ok(None) => (String::new(), String::new(), String::new()),
        Ok(Some((path, nick, uid_out))) => (path, nick, uid_out),
    };
    match fetch_quota_live(http, base_url, cred, &path).await {
        Err(Fail::Http(he)) => {
            // 积分路径 401/403 标死（与矩阵观感一致）：quota_resolve 在本入口把查询 uid
            // 解析成实际选中的号，上游 401/403 时 MarkDead 落到它——聊天之外的免费验活通道。
            if let Some(p) = pool {
                let status = he.status;
                if !uid_out.is_empty() && (status == 401 || status == 403) {
                    p.mark_dead(&uid_out);
                    println!(
                        "[codebuddy] 账号 {} 积分查询 HTTP {}，标记凭据失效（手动刷新积分=免费验活）",
                        crate::pool::uid_short(&uid_out),
                        status
                    );
                }
            }
            fallback_cache(cache, key, &he.message())
        }
        Err(Fail::Other(msg)) => fallback_cache(cache, key, &msg),
        Ok(mut q) => {
            q.uid = uid_out;
            q.nickname = nick;
            cache.put(key, q.clone());
            q
        }
    }
}

fn fallback_cache(cache: &QuotaCache, key: &str, err: &str) -> Quota {
    if let Some((q, _)) = cache.get(key) {
        let mut cp = q;
        cp.source = "cache".to_string();
        cp.err = err.to_string();
        return cp;
    }
    Quota {
        source: "none".to_string(),
        err: err.to_string(),
        ..Default::default()
    }
}

enum Fail {
    Http(QuotaHttpError),
    Other(String),
}

/// `fetchQuotaLive`：实拉一次官方积分接口。
async fn fetch_quota_live(
    http: &reqwest::Client,
    base_url: &str,
    cred: &dyn CredSource,
    path: &str,
) -> Result<Quota, Fail> {
    let hdr = cred
        .headers_for(path)
        .map_err(|e| Fail::Other(e.to_string()))?;
    // 固定模板：时间区间 2024-12-01 到现在
    let body = gjson::obj(vec![
        ("PageNumber", Value::Number(1.into())),
        ("PageSize", Value::Number(100.into())),
        ("ProductCode", gjson::s("p_tcaca")),
        (
            "Status",
            Value::Array(vec![Value::Number(0.into()), Value::Number(3.into())]),
        ),
        (
            "PackageStartTimeRangeBegin",
            gjson::s("2024-12-01 21:25:00"),
        ),
        (
            "PackageStartTimeRangeEnd",
            gjson::s(format_full_display(gotime::now_millis())),
        ),
    ]);
    let url = format!("{base_url}{QUOTA_PATH}");
    let req = http
        .post(&url)
        .headers(hdr)
        .body(gjson::to_go_bytes(&body));
    let resp = match tokio::time::timeout(QUOTA_TIMEOUT, req.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(Fail::Other(format!("官方积分接口请求失败: {e}"))),
        Err(_) => return Err(Fail::Other("官方积分接口请求失败: 超时".to_string())),
    };
    let status = resp.status().as_u16();
    let raw = read_capped(resp, MAX_QUOTA_BODY).await;
    if status != 200 {
        return Err(Fail::Http(QuotaHttpError {
            status,
            body: String::from_utf8_lossy(&raw).to_string(),
        }));
    }
    let out: Value = serde_json::from_slice(&raw)
        .map_err(|e| Fail::Other(format!("官方积分接口响应解析失败: {e}")))?;
    let code = out.get("code").and_then(|x| x.as_i64()).unwrap_or(0);
    if code != 0 {
        let msg = out.get("msg").and_then(|x| x.as_str()).unwrap_or("");
        return Err(Fail::Other(format!(
            "官方积分接口 code={}: {}",
            code,
            crate::chat::truncate(msg, 200)
        )));
    }
    let accounts: Vec<Account> = out
        .get("data")
        .and_then(|d| d.get("Response"))
        .and_then(|r| r.get("Data"))
        .and_then(|d| d.get("Accounts"))
        .map(|a| serde_json::from_value(a.clone()).unwrap_or_default())
        .unwrap_or_default();
    Ok(build_quota(&accounts))
}

async fn read_capped(resp: reqwest::Response, max: usize) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        if out.len() >= max {
            break;
        }
        let remain = max - out.len();
        if chunk.len() <= remain {
            out.extend_from_slice(&chunk);
        } else {
            out.extend_from_slice(&chunk[..remain]);
            break;
        }
    }
    out
}

/// `buildQuota` 按实测口径汇总：
/// * 包的剩余 = `CycleCapacityRemain`（**不** fallback 到 `CapacityRemain`——两者语义不同）；
/// * 总剩余 = 所有过期时间未到的包剩余合计；
/// * 当前包 = 未过期包里 `CycleEndTime` 最近且 `0 < CapacityUsed < CapacitySize` 的；
/// * 最近过期 = 未过期包里 `CycleEndTime` 最早的。
pub fn build_quota(accounts: &[Account]) -> Quota {
    let now = gotime::now_millis();
    let mut unexpired: Vec<(Account, f64, i64)> = Vec::new();
    for a in accounts {
        if a.cycle_end_time.is_empty() {
            continue;
        }
        // Go：time.ParseInLocation("2006-01-02 15:04:05", CycleEndTime, time.Local)
        let Some(end) = parse_local_datetime(&a.cycle_end_time) else {
            continue;
        };
        if !(now < end) {
            continue;
        }
        unexpired.push((a.clone(), a.cycle_capacity_remain, end));
    }
    let mut q = Quota {
        source: "live".to_string(),
        ..Default::default()
    };
    for (_, rem, _) in &unexpired {
        q.total += *rem;
    }
    q.packs = unexpired.len() as i64;
    unexpired.sort_by(|a, b| a.2.cmp(&b.2));
    if let Some((first, rem, _)) = unexpired.first() {
        q.nearest_end = NearestPack {
            name: first.package_name.clone(),
            date: first.cycle_end_time.clone(),
            remain: *rem,
        };
    }
    for (p, _, _) in &unexpired {
        // 跳过已用满的包（used>=size 无剩余额度）——否则「当前消耗包」显示 100/100 已满，
        // 与总剩余（其他包还有额度）矛盾，用户看着像积分识别错误。
        if p.capacity_used <= 0.0 || p.capacity_used >= p.capacity_size {
            continue;
        }
        q.active = ActivePack {
            name: p.package_name.clone(),
            used: p.capacity_used,
            size: p.capacity_size,
            end: p.cycle_end_time.clone(),
        };
        break;
    }
    q
}

/// `time.ParseInLocation("2006-01-02 15:04:05", s, time.Local)`：本地时区解释。
fn parse_local_datetime(s: &str) -> Option<i64> {
    let off = gotime::local_offset_secs();
    // 把本地字面量当作「带本地偏移的 RFC3339」解析，再减掉偏移得 UTC 纪元。
    let rfc = format!("{}T{}", &s[..10.min(s.len())], s.get(11..).unwrap_or("00:00:00"));
    let with_off = if off == 0 {
        format!("{rfc}Z")
    } else {
        let sign = if off < 0 { '-' } else { '+' };
        let a = off.abs();
        format!("{rfc}{}{:02}:{:02}", sign, a / 3600, (a % 3600) / 60)
    };
    gotime::parse_rfc3339(&with_off)
}

/// `handleQuota` 的响应体（`/quota?uid=&refresh=1`）。
pub fn handle_quota_body(q: &Quota) -> Map<String, Value> {
    match q.to_json() {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_matches_go() {
        assert_eq!(quota_cache_key(""), "_default");
        assert_eq!(quota_cache_key("u-golden"), "u-golden");
    }

    #[test]
    fn build_quota_follows_measured_conventions() {
        // 三条包：一条已过期、一条用满、一条在用；另有一条空 CycleEndTime。
        let accounts = vec![
            Account {
                package_name: "expired".into(),
                cycle_end_time: "2020-01-01 00:00:00".into(),
                cycle_capacity_remain: 999.0,
                capacity_used: 1.0,
                capacity_size: 100.0,
                ..Default::default()
            },
            Account {
                package_name: "full".into(),
                cycle_end_time: "2099-01-01 00:00:00".into(),
                cycle_capacity_remain: 0.0,
                capacity_used: 100.0,
                capacity_size: 100.0,
                ..Default::default()
            },
            Account {
                package_name: "active".into(),
                cycle_end_time: "2099-06-01 00:00:00".into(),
                cycle_capacity_remain: 42.5,
                capacity_used: 7.5,
                capacity_size: 50.0,
                capacity_remain: 500.0,
                ..Default::default()
            },
            Account {
                package_name: "no-end".into(),
                ..Default::default()
            },
        ];
        let q = build_quota(&accounts);
        assert_eq!(q.source, "live");
        assert_eq!(q.packs, 2, "只数未过期包");
        assert!((q.total - 42.5).abs() < 1e-9, "用 CycleCapacityRemain，不 fallback");
        assert_eq!(q.nearest_end.name, "full");
        assert!((q.nearest_end.remain - 0.0).abs() < 1e-9);
        assert_eq!(q.active.name, "active", "用满的包跳过");
        assert!((q.active.used - 7.5).abs() < 1e-9);
        assert!((q.active.size - 50.0).abs() < 1e-9);
    }

    #[test]
    fn quota_json_shape_and_optional_err() {
        let q = Quota {
            total: 1569.69,
            packs: 3,
            source: "live".to_string(),
            ..Default::default()
        };
        let s = crate::gjson::to_go(&q.to_json());
        assert_eq!(
            s,
            r#"{"active":{"end":"","name":"","size":0,"used":0},"nearestEnd":{"date":"","name":"","remain":0},"nickname":"","packs":3,"source":"live","total":1569.69,"uid":""}"#
        );
        let q2 = Quota {
            source: "none".to_string(),
            err: "账号不在池中: u-gold".to_string(),
            ..Default::default()
        };
        let s2 = crate::gjson::to_go(&q2.to_json());
        assert!(s2.contains(r#""err":"账号不在池中: u-gold""#));
        assert!(s2.contains(r#""packs":0"#));
    }
}
