//! pool.rs —— 多 token 账号池（对应 Go `internal/vibex/pool.go`）。
//!
//! C12~C18：401 标死、402/429 冷却、按 JWT exp 判过期；同号（同 `sub`）以新换旧；
//! `snapshot` 脱敏（绝不含 token 本体）；`daysLeft` 向上取整、过期给 −1。
//!
//! 时间格式：Go 用 `time.Format(time.RFC3339)`，而 `time.Now()`/`time.Unix()` 都是 **Local**
//! 位置，所以输出里带本机时区偏移（如 `+08:00`）。Rust 标准库没有时区，这里用 Win32
//! `SystemTimeToTzSpecificLocalTime` 现算偏移（非 Windows 退 UTC），做到与 Go 逐字一致。
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::gjson::{s, J};
use crate::jwt::decode_jwt;

/// 单个 Rh-Accesstoken 的运行时状态（Dead/冷却/调用计数是进程内存态，重启清零）。
#[derive(Debug, Clone)]
pub struct TokenState {
    pub value: String,
    pub dead: bool,
    pub cooldown_until_ms: i64,
    pub calls: i64,
    /// 由 JWT exp 解出；`None` 表示未知（对应 Go 的零值 `time.Time`）。
    pub exp_ms: Option<i64>,
    pub has_exp: bool,
    /// JWT sub（同号去重键；空表示未知，走串去重兜底）。
    pub sub: String,
}

impl TokenState {
    /// 用一个 token 串建 state（JWT exp 现解）——对应 Go `tokenStateOf` / `NewPool` 的构造。
    pub fn from_value(value: &str) -> TokenState {
        let mut st = TokenState {
            value: value.to_string(),
            dead: false,
            cooldown_until_ms: 0,
            calls: 0,
            exp_ms: None,
            has_exp: false,
            sub: String::new(),
        };
        if let Some(info) = decode_jwt(value) {
            st.exp_ms = info.exp.map(|e| e * 1000);
            st.has_exp = info.has_exp();
            st.sub = info.sub;
        }
        st
    }

    /// `usable`：`Dead` → false；冷却未过 → false；`HasExp && now.After(Exp)` → false。
    pub fn usable(&self, now_ms: i64) -> bool {
        if self.dead {
            return false;
        }
        if self.cooldown_until_ms > now_ms {
            return false;
        }
        if self.has_exp && now_ms > self.exp_ms.unwrap_or(i64::MAX) {
            return false;
        }
        true
    }
}

/// 池（线程安全）。
pub struct Pool {
    list: Mutex<Vec<TokenState>>,
    cooldown_ms: i64,
}

/// 当前 Unix 毫秒。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `addrOf`：TrimSpace 后取**首个 `;` 之前**那段（cookie 属性尾巴去重）。
pub fn addr_of(v: &str) -> String {
    let v = v.trim();
    match v.find(';') {
        Some(i) => v[..i].trim().to_string(),
        None => v.to_string(),
    }
}

impl Pool {
    /// 用给定顺序建池（去重）；`cooldown` 为 402/429 命中的冷却时长。
    pub fn new(values: Vec<String>, cooldown_ms: i64) -> Pool {
        let p = Pool {
            list: Mutex::new(Vec::new()),
            cooldown_ms,
        };
        for v in values {
            if v.trim().is_empty() {
                continue;
            }
            p.add_with_state(TokenState::from_value(&v));
        }
        p
    }

    /// `Len`（不区分死活）。
    pub fn len(&self) -> usize {
        self.list.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `add`：按 `addrOf` 去重，JWT exp 现解。返回 (下标, 是否已存在)。
    pub fn add(&self, v: &str) -> (Option<usize>, bool) {
        let v = v.trim();
        if v.is_empty() {
            return (None, false);
        }
        self.add_with_state(TokenState::from_value(v))
    }

    /// `addWithState` 两档去重：同串 → 已存在；同 `sub` 不同串 → 旧串让位。
    pub fn add_with_state(&self, st: TokenState) -> (Option<usize>, bool) {
        if st.value.trim().is_empty() {
            return (None, false);
        }
        let key = addr_of(&st.value);
        let mut list = self.list.lock().unwrap();
        for (i, t) in list.iter().enumerate() {
            if addr_of(&t.value) == key {
                return (Some(i), true);
            }
        }
        if !st.sub.is_empty() {
            let mut kept: Vec<TokenState> = Vec::with_capacity(list.len() + 1);
            for mut t in list.drain(..) {
                if t.sub.is_empty() {
                    if let Some(info) = decode_jwt(&t.value) {
                        t.has_exp = info.has_exp();
                        t.exp_ms = info.exp.map(|e| e * 1000);
                        t.sub = info.sub;
                    }
                }
                if t.sub.is_empty() || t.sub != st.sub {
                    kept.push(t);
                }
            }
            *list = kept;
        }
        list.push(st);
        (Some(list.len() - 1), false)
    }

    /// `acquire`：取第一个可用 token 的下标。
    pub fn acquire(&self) -> Option<usize> {
        let list = self.list.lock().unwrap();
        let now = now_ms();
        for (i, t) in list.iter().enumerate() {
            if t.usable(now) {
                return Some(i);
            }
        }
        None
    }

    /// 取某下标的 token 本体（仅供构造鉴权头，绝不落日志）。
    pub fn value_of(&self, i: usize) -> String {
        self.list
            .lock()
            .unwrap()
            .get(i)
            .map(|t| t.value.clone())
            .unwrap_or_default()
    }

    /// `markDead`（401/403/412）。
    pub fn mark_dead(&self, i: usize) {
        if let Some(t) = self.list.lock().unwrap().get_mut(i) {
            t.dead = true;
        }
    }

    /// `markCooldown`（402/429）。
    pub fn mark_cooldown(&self, i: usize) {
        let until = now_ms() + self.cooldown_ms;
        if let Some(t) = self.list.lock().unwrap().get_mut(i) {
            t.cooldown_until_ms = until;
        }
    }

    /// `incCalls`（成功调用计数）。
    pub fn inc_calls(&self, i: usize) {
        if let Some(t) = self.list.lock().unwrap().get_mut(i) {
            t.calls += 1;
        }
    }

    /// `snapshot`：脱敏状态（只给序号/死活/冷却/过期时间/调用数，绝不含 token 本体）。
    pub fn snapshot(&self) -> Vec<J> {
        let list = self.list.lock().unwrap();
        let now = now_ms();
        let mut out = Vec::with_capacity(list.len());
        for (i, t) in list.iter().enumerate() {
            let mut pairs: Vec<(String, J)> = vec![
                ("available".into(), J::Bool(t.usable(now))),
                ("calls".into(), J::Int(t.calls)),
                ("dead".into(), J::Bool(t.dead)),
                ("index".into(), J::Int(i as i64)),
            ];
            if t.cooldown_until_ms > now {
                pairs.push((
                    "cooldown_until".into(),
                    s(rfc3339_local(t.cooldown_until_ms)),
                ));
            }
            if t.has_exp {
                let exp = t.exp_ms.unwrap_or(0);
                pairs.push(("days_left".into(), J::Int(days_left(exp, now) as i64)));
                pairs.push(("exp".into(), s(rfc3339_local(exp))));
                pairs.push(("expired".into(), J::Bool(now > exp)));
            }
            out.push(J::Obj(pairs));
        }
        out
    }

    /// `maxDaysLeft`：池中所有带 exp 的 token 里最大的剩余天数；无 exp/无 token → `None`
    /// （调用方据此省略 `days_left*` 键）。
    pub fn max_days_left(&self) -> Option<i64> {
        let list = self.list.lock().unwrap();
        let now = now_ms();
        let mut best: Option<i64> = None;
        for t in list.iter() {
            if !t.has_exp {
                continue;
            }
            let d = days_left(t.exp_ms.unwrap_or(0), now) as i64;
            if best.is_none() || d > best.unwrap() {
                best = Some(d);
            }
        }
        best
    }
}

/// `daysLeft`：`0 < d < 24h` → 1；`d <= 0` → −1；否则 `ceil(d/24h)`。
pub fn days_left(exp_ms: i64, now_ms: i64) -> i32 {
    let d = exp_ms - now_ms;
    const DAY: i64 = 24 * 60 * 60 * 1000;
    if d > 0 && d < DAY {
        return 1;
    }
    if d <= 0 {
        return -1;
    }
    ((d + DAY - 1) / DAY) as i32
}

// ---------- RFC3339（Local） ----------

/// 把 Unix 毫秒格式化成 Go `time.Time.Format(time.RFC3339)` 的结果（带本机时区偏移）。
pub fn rfc3339_local(unix_ms: i64) -> String {
    let offset = local_offset_secs(unix_ms / 1000);
    let total = unix_ms.div_euclid(1000) + offset;
    let (y, mo, d, h, mi, sec) = civil_from_unix(total);
    let sign = if offset < 0 { '-' } else { '+' };
    let a = offset.abs();
    let (oh, om) = (a / 3600, (a % 3600) / 60);
    let zone = if offset == 0 {
        "Z".to_string()
    } else {
        format!("{sign}{oh:02}:{om:02}")
    };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}{zone}")
}

/// Unix 秒 → (年, 月, 日, 时, 分, 秒)，UTC（Howard Hinnant 的 civil_from_days）。
fn civil_from_unix(t: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

#[cfg(windows)]
fn local_offset_secs(unix_secs: i64) -> i64 {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    extern "system" {
        fn SystemTimeToTzSpecificLocalTime(
            tz: *const core::ffi::c_void,
            utc: *const SystemTime,
            local: *mut SystemTime,
        ) -> i32;
    }
    // Unix 秒 → FILETIME 的 100ns 计数 → SYSTEMTIME（用同一个 civil 算法，避免再引 API）
    let (y, mo, d, h, mi, sec) = civil_from_unix(unix_secs);
    let utc = SystemTime {
        year: y as u16,
        month: mo as u16,
        day_of_week: 0,
        day: d as u16,
        hour: h as u16,
        minute: mi as u16,
        second: sec as u16,
        milliseconds: 0,
    };
    let mut local = SystemTime::default();
    let ok = unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) };
    if ok == 0 {
        return 0;
    }
    let local_as_utc = days_from_civil(local.year as i64, local.month as u32, local.day as u32) * 86400
        + (local.hour as i64) * 3600
        + (local.minute as i64) * 60
        + local.second as i64;
    local_as_utc - unix_secs
}

#[cfg(not(windows))]
fn local_offset_secs(_unix_secs: i64) -> i64 {
    0
}

/// (年, 月, 日) → 自 1970-01-01 的天数（civil_to_days）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_left_boundaries() {
        let day = 24 * 60 * 60 * 1000;
        let now = 1_800_000_000_000i64;
        assert_eq!(days_left(now + 10 * day, now), 10);
        assert_eq!(days_left(now + 9 * day + 12 * 60 * 60 * 1000, now), 10);
        assert_eq!(days_left(now + 60 * 60 * 1000, now), 1);
        assert_eq!(days_left(now, now), -1);
        assert_eq!(days_left(now - day, now), -1);
        assert_eq!(days_left(now + 2 * day, now), 2);
    }

    #[test]
    fn addr_of_trims_cookie_attrs() {
        assert_eq!(addr_of("a.b.c"), "a.b.c");
        assert_eq!(addr_of(" a.b.c ; Path=/; HttpOnly"), "a.b.c");
        assert_eq!(addr_of(""), "");
        assert_eq!(addr_of("x;"), "x");
    }

    #[test]
    fn rfc3339_has_zone_suffix() {
        let s = rfc3339_local(1_800_000_000_000);
        assert!(s.starts_with("2027-01-1"), "{s}");
        assert!(s.ends_with('Z') || s.contains('+') || s.contains('-'), "{s}");
        assert_eq!(s.len(), 25, "RFC3339：2027-01-15T08:00:00+08:00 / ...Z: {s}");
    }
}
