//! gotime.rs —— Go `time.Time` 的 **JSON 形态**对齐（`/v1/accounts` 与池状态文件用）。
//!
//! 为什么需要它：Go 的 `AccountView.LimitedUntil` 是 `time.Time`，`json.Marshal` 写出的是
//! RFC3339 **带纳秒**（`2026-10-15T12:00:00.123+08:00`，尾随零裁掉，无小数就不带小数点），
//! `Account.LastUsed` 则显式走 `Format(time.RFC3339)`（**秒精度**，先转本地时区、再丢小数）。
//! 两点都由真 Go 采收（见黄金 `accounts-*` 场景的响应体），Rust 侧逐字节对齐。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Go `time.Time{}`（零值）在 JSON 里写出的形态。
pub const GO_ZERO_RFC3339: &str = "0001-01-01T00:00:00Z";

/// `SystemTime` → epoch 毫秒；早于 epoch 或取不到给 `i64::MIN`（当 Go 零值处理）。
pub fn to_millis(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => i64::MIN,
    }
}

/// epoch 毫秒 → `SystemTime`。
pub fn from_millis(ms: i64) -> SystemTime {
    if ms <= 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

/// 现在（epoch 毫秒）。
pub fn now_millis() -> i64 {
    to_millis(SystemTime::now())
}

/// 本机时区相对 UTC 的秒偏移（Go 的 `time.Now()` 带本地 Location）。
#[cfg(windows)]
pub fn local_offset_secs() -> i32 {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct WinSystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: WinSystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: WinSystemTime,
        daylight_bias: i32,
    }
    extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }
    let mut info: TimeZoneInformation = unsafe { std::mem::zeroed() };
    let r = unsafe { GetTimeZoneInformation(&mut info) };
    // `bias` 的语义是 UTC = local + bias（分钟），故本地偏移 = -bias。
    // 1=TIME_ZONE_ID_STANDARD，2=TIME_ZONE_ID_DAYLIGHT。
    let mut total = info.bias;
    if r == 2 {
        total += info.daylight_bias;
    } else if r == 1 {
        total += info.standard_bias;
    }
    -total * 60
}

/// 非 Windows：本 crate 的目标平台是 Windows，其它平台按 UTC 处理并如实登记。
#[cfg(not(windows))]
pub fn local_offset_secs() -> i32 {
    0
}

/// 分解 epoch 毫秒（按给定秒偏移）为 (year, month, day, hour, min, sec, millis)。
fn civil(ms: i64, offset_secs: i32) -> (i64, u32, u32, u32, u32, u32, u32) {
    let shifted = ms + (offset_secs as i64) * 1000;
    let days = shifted.div_euclid(86_400_000);
    let mut rem = shifted.rem_euclid(86_400_000);
    if rem < 0 {
        rem += 86_400_000;
    }
    let hour = (rem / 3_600_000) as u32;
    rem %= 3_600_000;
    let min = (rem / 60_000) as u32;
    rem %= 60_000;
    let sec = (rem / 1000) as u32;
    let millis = (rem % 1000) as u32;
    // days since 1970-01-01 → civil date（Howard Hinnant 的 civil_from_days）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    (y, m as u32, d as u32, hour, min, sec, millis)
}

fn format_offset(offset_secs: i32) -> String {
    if offset_secs == 0 {
        return "Z".to_string();
    }
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let a = offset_secs.abs();
    format!("{}{:02}:{:02}", sign, a / 3600, (a % 3600) / 60)
}

/// Go `time.RFC3339`（**秒精度**）：`Account.LastUsed.Format(time.RFC3339)` 的形态。
pub fn format_rfc3339_seconds(ms: i64, offset_secs: i32) -> String {
    if ms == i64::MIN {
        return GO_ZERO_RFC3339.to_string();
    }
    let (y, mo, d, h, mi, s, _) = civil(ms, offset_secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}",
        y,
        mo,
        d,
        h,
        mi,
        s,
        format_offset(offset_secs)
    )
}

/// `time.Time` 的 `MarshalJSON` 形态（**纳秒**，尾随零裁掉；无小数则不带小数点）。
/// `SystemTime` 只有毫秒精度，故最多 3 位小数——与 Rust 侧存的东西一致。
pub fn marshal_time(ms: i64, offset_secs: i32) -> String {
    if ms == i64::MIN {
        return GO_ZERO_RFC3339.to_string();
    }
    let (y, mo, d, h, mi, s, milli) = civil(ms, offset_secs);
    let frac = if milli == 0 {
        String::new()
    } else {
        let f = format!("{milli:03}");
        format!(".{}", f.trim_end_matches('0'))
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{}",
        y,
        mo,
        d,
        h,
        mi,
        s,
        frac,
        format_offset(offset_secs)
    )
}

/// 解析 RFC3339（纳秒截到毫秒）。失败给 `None`（Go `time.Parse` 的错误路径）。
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let mut i = 19;
    let mut millis = 0i64;
    if i < b.len() && b[i] == b'.' {
        let start = i + 1;
        let mut end = start;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        let digits = &s[start..end];
        if digits.is_empty() {
            return None;
        }
        let take = digits.len().min(3);
        millis = digits[..take].parse::<i64>().ok()?;
        for _ in take..3 {
            millis *= 10;
        }
        i = end;
    }
    let off = if i < b.len() && b[i] == b'Z' {
        0i32
    } else if i + 6 <= b.len() && (b[i] == b'+' || b[i] == b'-') {
        let hh = num(i + 1, i + 3)? as i32;
        let mm = num(i + 4, i + 6)? as i32;
        let v = hh * 3600 + mm * 60;
        if b[i] == b'-' {
            -v
        } else {
            v
        }
    } else {
        return None;
    };
    let days = days_from_civil(year, month, day)?;
    let local = days * 86_400_000 + hour * 3_600_000 + min * 60_000 + sec * 1000 + millis;
    Some(local - (off as i64) * 1000)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marshal_time_trims_fraction_like_go() {
        let base = parse_rfc3339("2026-10-15T04:46:15Z").unwrap();
        assert_eq!(marshal_time(base, 0), "2026-10-15T04:46:15Z");
        assert_eq!(
            marshal_time(base + 500, 0),
            "2026-10-15T04:46:15.5Z",
            "尾随零裁掉：.500 → .5（Go time.Time.MarshalJSON）"
        );
        assert_eq!(marshal_time(base + 123, 0), "2026-10-15T04:46:15.123Z");
        assert_eq!(marshal_time(base, 8 * 3600), "2026-10-15T12:46:15+08:00");
        assert_eq!(marshal_time(i64::MIN, 8 * 3600), GO_ZERO_RFC3339);
    }

    #[test]
    fn rfc3339_seconds_format_matches_go_format_call() {
        let t = parse_rfc3339("2026-10-15T04:46:15.987Z").unwrap();
        assert_eq!(format_rfc3339_seconds(t, 0), "2026-10-15T04:46:15Z");
        assert_eq!(
            format_rfc3339_seconds(t, 8 * 3600),
            "2026-10-15T12:46:15+08:00"
        );
    }

    #[test]
    fn parse_accepts_go_forms_and_rejects_junk() {
        for s in [
            "2026-10-15T04:46:15Z",
            "2026-10-15T04:46:15.123456789+08:00",
            "2026-10-15T04:46:15-05:30",
        ] {
            assert!(parse_rfc3339(s).is_some(), "应能解析 {s}");
        }
        assert!(parse_rfc3339("not-a-time").is_none());
        assert!(
            parse_rfc3339("2026-10-15T04:46:15").is_none(),
            "缺时区要报错（Go time.Parse 同）"
        );
    }
}
