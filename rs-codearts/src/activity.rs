//! activity.rs —— 码道「活动积分」领取（签到 / 新手 / 邀请 / 学生…全部 claimable 活动）。
//!
//! 上游接口（2026-11 实测口径，抄自社区参考实现 HITZY2002/codearts2api 与
//! Darkstarrd-dev/tinylab 的 `codearts_credits.go`）：
//!
//! ```text
//! GET  /v1/ops/delivery?channel=IDE   → {code:0, data:{items:[{campaignId,type,title,
//!                                        status,claimable,benefitUnit,benefitAmount,
//!                                        pendingTotalAmount,extra:{startTime,endTime}}]}}
//! POST /v1/ops/claim   {campaignId, channel:"IDE", idempotentKey}
//! POST /v1/ops/confirm {campaignId}   ← claim 响应里 data.id 非空时才需要
//! ```
//!
//! 纪律（各家一致）：
//! * 按活动 `type` 选，不按 campaignId（ID 随投放批次变）；唯一例外是种子里的 ID 1/4；
//! * `campaignId` 可能是**数字**，读的时候数字/字符串都认；
//! * 只有 `claimable=true` 且 `status` 属于可领态才 claim；`CLAIMED/CONFIRMED/CONSUMED`
//!   一律视为已领（CLAIMED 只补 confirm）；
//! * 领取失败**不**影响推理账号健康（不冷却、不标死）；
//! * 本地账本按自然日（UTC+8）记录，重启不重复领。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// 上游活动条目。
#[derive(Clone, Debug, Default)]
pub struct Campaign {
    /// 原始 campaignId（数字/字符串都保留原值，回传时按原类型发）。
    pub id: Value,
    pub title: String,
    /// 活动类型：`USER_LOGIN`（每日签到）、`NEW_USER_REGISTER`（新手）、
    /// `INVITE_*`（邀请）、`STUDENT_*`（学生）等，未知类型照常按 claimable 处理。
    pub kind: String,
    pub status: String,
    pub claimable: bool,
    pub unit: String,
    pub amount: f64,
    pub pending: f64,
    pub start: Option<String>,
    pub end: Option<String>,
}

impl Campaign {
    /// 账本键：优先 type+campaignId（ID 变也不丢），没有 type 时退回 ID。
    pub fn key(&self) -> String {
        let id = self.id.to_string();
        if self.kind.is_empty() {
            id
        } else {
            format!("{}#{}", self.kind, id)
        }
    }
    pub fn id_text(&self) -> String {
        match &self.id {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            other => other.to_string(),
        }
    }
    pub fn is_login(&self) -> bool {
        self.kind.eq_ignore_ascii_case("USER_LOGIN") || self.id_text() == "1"
    }
    pub fn is_new_user(&self) -> bool {
        self.kind.eq_ignore_ascii_case("NEW_USER_REGISTER") || self.id_text() == "4"
    }
    /// 领取时回传的 campaignId：数字原样回数字，字符串回字符串（HITZY 实测 ID 是数字）。
    pub fn id_json(&self) -> Value {
        self.id.clone()
    }
}

/// 解析 `data.items`。
pub fn parse_campaigns(v: &Value) -> Vec<Campaign> {
    let items = v
        .get("data")
        .and_then(|d| d.get("items"))
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for it in items {
        let id = it.get("campaignId").cloned().unwrap_or(Value::Null);
        if id.is_null() {
            continue;
        }
        let num = |k: &str| it.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        out.push(Campaign {
            id,
            title: it.get("title").and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
            kind: it.get("type").and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
            status: it.get("status").and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
            claimable: it.get("claimable").and_then(|x| x.as_bool()).unwrap_or(false),
            unit: it.get("benefitUnit").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            amount: num("benefitAmount"),
            pending: num("pendingTotalAmount"),
            start: it
                .get("extra")
                .and_then(|e| e.get("startTime"))
                .and_then(|x| x.as_str())
                .map(str::to_string),
            end: it
                .get("extra")
                .and_then(|e| e.get("endTime"))
                .and_then(|x| x.as_str())
                .map(str::to_string),
        });
    }
    out
}

/// 结果是否「已了结」（以后不用再领）。
pub fn is_settled_status(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "CLAIMED" | "CONFIRMED" | "CONSUMED"
    )
}

/// 该活动现在能不能领。
/// `day` 是今天（UTC+8，`YYYY-MM-DD`）；`settled` 是本地账本里已了结的键集合。
pub fn claimable_now(c: &Campaign, settled: &BTreeMap<String, String>, day: &str, now_unix: i64) -> bool {
    if is_settled_status(&c.status) {
        return false;
    }
    if !c.claimable {
        return false;
    }
    // 单位只认 CREDIT / 空（token 类活动不领，避免把 token 当积分记账）。
    if !c.unit.is_empty() && !c.unit.eq_ignore_ascii_case("CREDIT") {
        return false;
    }
    // 时间窗（RFC3339，容忍带 Z/偏移）；解析失败就不判，交给上游。
    if let Some(end) = &c.end {
        if let Some(t) = parse_rfc3339_secs(end) {
            if now_unix >= t {
                return false;
            }
        }
    }
    if let Some(start) = &c.start {
        if let Some(t) = parse_rfc3339_secs(start) {
            if now_unix < t {
                return false;
            }
        }
    }
    let key = c.key();
    if let Some(prev_day) = settled.get(&key) {
        if c.is_login() {
            // 每日签到按自然日，跨零点失效。
            return prev_day != day;
        }
        // 其他活动（新手/邀请/学生…）每个账号一次。
        return false;
    }
    true
}

fn parse_rfc3339_secs(s: &str) -> Option<i64> {
    // 形如 2026-11-15T23:59:59+08:00 / Z；只取到秒。
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    let num = |a: usize, n: usize| -> Option<i64> {
        std::str::from_utf8(&b[a..a + n]).ok()?.parse::<i64>().ok()
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, se) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let days = days_from_civil(y, mo, d);
    let mut ts = days * 86_400 + h * 3600 + mi * 60 + se;
    // 时区偏移（+08:00/-05:00）→ 转 UTC 秒。
    if s.len() >= 25 {
        let tz = &s[19..];
        let sign = if tz.starts_with('+') { -1 } else if tz.starts_with('-') { 1 } else { 0 };
        if sign != 0 {
            let hh: i64 = tz[1..3].parse().unwrap_or(0);
            let mm: i64 = tz[4..6].parse().unwrap_or(0);
            ts += sign * (hh * 3600 + mm * 60);
        }
    }
    Some(ts)
}

/// Howard Hinnant 的 civil-from-days 逆运算：返回 1970-01-01 起的天数。
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// UTC+8 自然日字符串。
pub fn local_day_utc8(now_unix: i64) -> String {
    let days = (now_unix + 8 * 3600).div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// UTC+8 的小时（0-23），用于「9 点/21 点轮次」判断。
pub fn local_hour_utc8(now_unix: i64) -> i64 {
    ((now_unix + 8 * 3600).rem_euclid(86_400)) / 3600
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 单账号账本。
#[derive(Clone, Debug, Default)]
pub struct Entry {
    /// 最近一次运行（epoch 秒）。
    pub last_run: i64,
    pub last_message: String,
    pub last_credits: f64,
    /// 已了结的「活动键 → 领取日（UTC+8）」。
    pub settled: BTreeMap<String, String>,
}

/// 活动账本（落数据目录 `codearts-activity.json`）。
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    pub entries: BTreeMap<String, Entry>,
    path: PathBuf,
}

impl Ledger {
    pub fn load(path: &Path) -> Self {
        let mut out = Ledger {
            entries: BTreeMap::new(),
            path: path.to_path_buf(),
        };
        if let Ok(b) = std::fs::read(path) {
            if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                if let Some(accs) = v.get("accounts").and_then(|a| a.as_object()) {
                    for (k, e) in accs {
                        let mut en = Entry {
                            last_run: e.get("last_run").and_then(|x| x.as_i64()).unwrap_or(0),
                            last_message: e
                                .get("last_message")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            last_credits: e
                                .get("last_credits")
                                .and_then(|x| x.as_f64())
                                .unwrap_or(0.0),
                            settled: BTreeMap::new(),
                        };
                        if let Some(s) = e.get("settled").and_then(|x| x.as_object()) {
                            for (ck, cv) in s {
                                if let Some(t) = cv.as_str() {
                                    en.settled.insert(ck.clone(), t.to_string());
                                }
                            }
                        }
                        out.entries.insert(k.clone(), en);
                    }
                }
            }
        }
        out
    }

    pub fn save(&self) {
        let mut accounts = serde_json::Map::new();
        for (k, e) in &self.entries {
            let mut settled = serde_json::Map::new();
            for (sk, sv) in &e.settled {
                settled.insert(sk.clone(), Value::String(sv.clone()));
            }
            accounts.insert(
                k.clone(),
                serde_json::json!({
                    "last_run": e.last_run,
                    "last_message": e.last_message,
                    "last_credits": e.last_credits,
                    "settled": settled,
                }),
            );
        }
        let body = serde_json::json!({ "version": 1, "accounts": accounts });
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&body).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn entry(&self, id: &str) -> Entry {
        self.entries.get(id).cloned().unwrap_or_default()
    }

    pub fn update<F: FnOnce(&mut Entry)>(&mut self, id: &str, f: F) {
        let e = self.entries.entry(id.to_string()).or_default();
        f(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn campaign(id: Value, kind: &str, status: &str, claimable: bool) -> Campaign {
        Campaign {
            id,
            title: "t".into(),
            kind: kind.into(),
            status: status.into(),
            claimable,
            unit: "CREDIT".into(),
            amount: 1000.0,
            pending: 0.0,
            start: None,
            end: None,
        }
    }

    #[test]
    fn parses_numeric_campaign_id_and_fields() {
        let v = json!({"code":0,"data":{"items":[
            {"campaignId":1,"type":"USER_LOGIN","title":"每日签到","status":"ELIGIBLE","claimable":true,
             "benefitUnit":"CREDIT","benefitAmount":1000,"extra":{"startTime":"2026-10-01T00:00:00+08:00","endTime":"2026-12-31T23:59:59+08:00"}},
            {"campaignId":"7","type":"INVITE_REGISTER","title":"邀请注册","status":"ELIGIBLE","claimable":true,"benefitUnit":"CREDIT","benefitAmount":1000}
        ]}});
        let cs = parse_campaigns(&v);
        assert_eq!(cs.len(), 2);
        assert!(cs[0].id.is_number());
        assert_eq!(cs[0].id_text(), "1");
        assert_eq!(cs[0].kind, "USER_LOGIN");
        assert!(cs[0].is_login());
        assert_eq!(cs[1].id_text(), "7");
        assert!(!cs[1].is_login());
        assert_eq!(cs[0].key(), "USER_LOGIN#1");
    }

    #[test]
    fn login_campaign_is_daily_others_once() {
        let login = campaign(json!(1), "USER_LOGIN", "ELIGIBLE", true);
        let invite = campaign(json!(7), "INVITE_REGISTER", "ELIGIBLE", true);
        let mut settled = BTreeMap::new();
        assert!(claimable_now(&login, &settled, "2026-11-13", 1_700_000_000));
        settled.insert(login.key(), "2026-11-13".to_string());
        assert!(!claimable_now(&login, &settled, "2026-11-13", 1_700_000_000), "当天已领");
        assert!(claimable_now(&login, &settled, "2026-11-14", 1_700_000_000), "跨天可再领");
        settled.insert(invite.key(), "2026-11-13".to_string());
        assert!(!claimable_now(&invite, &settled, "2026-11-14", 1_700_000_000), "邀请类只一次");
    }

    #[test]
    fn skips_not_claimable_and_token_units() {
        let mut c = campaign(json!(2), "USER_LOGIN", "ELIGIBLE", false);
        let settled = BTreeMap::new();
        assert!(!claimable_now(&c, &settled, "d", 0));
        c.claimable = true;
        c.unit = "TOKEN".into();
        assert!(!claimable_now(&c, &settled, "d", 0));
        c.unit = "CREDIT".into();
        c.status = "CONFIRMED".into();
        assert!(!claimable_now(&c, &settled, "d", 0));
    }

    #[test]
    fn honors_activity_window() {
        let mut c = campaign(json!(3), "INVITE_REGISTER", "ELIGIBLE", true);
        c.end = Some("2026-11-15T23:59:59+08:00".into());
        let settled = BTreeMap::new();
        // 2026-11-15 23:59:59 +08 = 2026-11-15T15:59:59Z = 1794844799
        let before = 1_794_758_398;
        let after = 1_794_758_400;
        assert!(claimable_now(&c, &settled, "d", before));
        assert!(!claimable_now(&c, &settled, "d", after));
    }

    #[test]
    fn local_day_and_hour_are_utc8() {
        // 2026-11-13T11:35:00Z = 19:35 +08
        let ts = 1_794_569_700;
        assert_eq!(local_day_utc8(ts), "2026-11-13");
        assert_eq!(local_hour_utc8(ts), 19);
        // 跨零点：2026-11-13T16:30:00Z = 2026-11-14 00:30 +08
        let ts2 = 1_794_587_400;
        assert_eq!(local_day_utc8(ts2), "2026-11-14");
        assert_eq!(local_hour_utc8(ts2), 0);
    }

    #[test]
    fn ledger_roundtrip() {
        let dir = std::env::temp_dir().join("rs-codearts-ledger-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("codearts-activity.json");
        let mut l = Ledger::load(&path);
        l.update("u-1", |e| {
            e.last_run = 42;
            e.settled.insert("USER_LOGIN#1".into(), "2026-11-13".into());
        });
        l.save();
        let back = Ledger::load(&path);
        assert_eq!(back.entry("u-1").last_run, 42);
        assert_eq!(
            back.entry("u-1").settled.get("USER_LOGIN#1").map(String::as_str),
            Some("2026-11-13")
        );
        // 覆盖保存：Windows 上 rename 必须替换已有文件（防止账本永远停在第一版）
        let mut back = Ledger::load(&path);
        back.update("u-1", |e| {
            e.last_run = 43;
            e.settled.insert("USER_LOGIN#1".into(), "2026-11-14".into());
        });
        back.save();
        let again = Ledger::load(&path);
        assert_eq!(again.entry("u-1").last_run, 43);
        assert_eq!(
            again.entry("u-1").settled.get("USER_LOGIN#1").map(String::as_str),
            Some("2026-11-14")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
