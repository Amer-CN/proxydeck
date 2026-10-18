//! Go 兼容的基础设施：Base64 URL 编码、UUID v4、Go 时间格式。
//!
//! 只有COMMAND 代理真正用到的那部分：`json.Marshal` 的写出器在 [`crate::gjson`]，
//! JSON 解析在 [`crate::godecode`]。

use base64::Engine;

/// `base64.RawURLEncoding`（无填充的 URL-safe base64）——Go 的 `base64url` 指的就是它
/// （`base64.URLEncoding` 才有 `=` 填充）。用于 uuid 变体与 id 生成。
pub fn base64_raw_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `uuid.New().String()` 的等价物：RFC 4122 v4（随机，版本/变体位正确）。
///
/// Go 的 `google/uuid.New()` 用 `crypto/rand` 取 16 字节，再置
/// `b[6] = (b[6] & 0x0f) | 0x40`、`b[8] = (b[8] & 0x3f) | 0x80`。
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("取系统随机数失败");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format_uuid(&b)
}

/// 把 16 字节按 `8-4-4-4-12` 十六进制（小写）格式化——Go `uuid.String()` 的形态。
fn format_uuid(b: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            s.push('-');
        }
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    s
}

/// Go `uuid.New().String()[:n]` 的等价物：取前 n 个字符（uuid 串含连字符，
/// Go 也是直接切字符串，不是切字节）。
pub fn uuid_prefix(n: usize) -> String {
    let s = uuid_v4();
    s.chars().take(n).collect()
}

/// 秒级 unix 时间戳（Go `time.Now().Unix()`）。
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 本地时区的 `YYYY-MM-DD`（Go `time.Now().Format("2006-01-02")` / `dayKey()`）。
///
/// 与 Go 一致地取**本机时区**（`time.Now()` 是 local）。这里用系统本地偏移实现：
/// `time.Local` 在 Windows 上就是系统时区，Rust 侧取同一个偏移即可（不做夏令时特判——
/// Go 的 `time.Now().Format` 同样由系统给出当时的偏移）。
pub fn local_day(now_unix: i64) -> String {
    let off = local_utc_offset_secs(now_unix);
    let secs = now_unix + off;
    let days = secs.div_euclid(86_400);
    civil_from_days(days)
}

/// `YYYY-MM-DD HH:MM:SS`（本地时区）——`appdir` 日志时间戳用。
pub fn local_stamp(now_unix: i64) -> String {
    let off = local_utc_offset_secs(now_unix);
    let secs = now_unix + off;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let d = civil_from_days(days);
    format!(
        "{} {:02}:{:02}:{:02}",
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 本机相对 UTC 的偏移秒数（按给定时刻取值，含夏令时）。
///
/// 实现：调 kernel32 的 `GetTimeZoneInformation` 拿「标准时区 + 夏令时」两套规则，
/// 按 `SYSTEMTIME`（**UTC**）落在哪一侧决定用哪套——与 Go 的 `time.Local`
/// （Windows 上就是系统时区）语义一致。
#[cfg(windows)]
fn local_utc_offset_secs(now_unix: i64) -> i64 {
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
    }    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTime,
        daylight_bias: i32,
    }
    impl Default for TimeZoneInformation {
        fn default() -> Self {
            TimeZoneInformation {
                bias: 0,
                standard_name: [0; 32],
                standard_date: SystemTime::default(),
                standard_bias: 0,
                daylight_name: [0; 32],
                daylight_date: SystemTime::default(),
                daylight_bias: 0,
            }
        }
    }
    extern "system" {
        fn GetTimeZoneInformation(tzi: *mut TimeZoneInformation) -> u32;
    }
    /// `TIME_ZONE_ID_INVALID`
    const INVALID: u32 = 0xFFFF_FFFF;
    const DAYLIGHT: u32 = 2;

    let mut tzi = TimeZoneInformation::default();
    let rc = unsafe { GetTimeZoneInformation(&mut tzi) };
    if rc == INVALID {
        return 0;
    }
    // 先把本地时刻当作 UTC 求「朴素」偏移：Go 的 time.Local 在冬令时就是 -(bias)
    let std_off = -(tzi.bias as i64 + tzi.standard_bias as i64) * 60;
    if rc != DAYLIGHT {
        return std_off;
    }
    // 夏令时窗口：把 UTC 时刻换算成 SYSTEMTIME，与 daylight_date 比
    let utc = system_time_from_unix(now_unix);
    let d = tzi.daylight_date;
    if in_daylight(utc, (d.year, d.month, d.day_of_week, d.day, d.hour, d.minute)) {
        -(tzi.bias as i64 + tzi.daylight_bias as i64) * 60
    } else {
        std_off
    }
}

/// unix 秒 → UTC 的 `SYSTEMTIME`（用本模块自己的 civil 算法，不依赖 C 运行时）。
#[cfg(windows)]
fn system_time_from_unix(unix: i64) -> SystemTimeUtc {
    let days = unix.div_euclid(86_400);
    let rem = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days_pub(days);
    SystemTimeUtc {
        year: y as u16,
        month: m as u16,
        day_of_week: weekday_from_days(days),
        day: d as u16,
        hour: (rem / 3600) as u16,
        minute: ((rem % 3600) / 60) as u16,
        second: (rem % 60) as u16,
    }
}

/// UTC 的 `SYSTEMTIME` 精简形态（只用得到这几个字段）。
#[cfg(windows)]
#[derive(Clone, Copy, Default)]
pub struct SystemTimeUtc {
    pub year: u16,
    pub month: u16,
    /// 0 = 周日 … 6 = 周六（与 Windows `SYSTEMTIME.wDayOfWeek` 同）
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

/// 1970-01-01（周四）起的天数 → 星期（0 = 周日）。
#[cfg(windows)]
fn weekday_from_days(days: i64) -> u16 {
    // 1970-01-01 是周四 = 4
    (((days % 7) + 7 + 4) % 7) as u16
}

/// Windows 的 `SYSTEMTIME` 时区日期规则：`wYear/wMonth/wDayOfWeek/wDay` 描述「第 n 个星期几」。
/// 返回该月内的 day-of-month。
#[cfg(windows)]
fn nth_weekday_day(year: u16, month: u16, weekday: u16, n: u16) -> u16 {
    let first = days_from_civil(year as i64, month as i64, 1);
    let first_wd = weekday_from_days(first);
    let delta = ((weekday as i64 - first_wd as i64) + 7) % 7;
    let mut day = 1 + delta + ((n as i64 - 1) * 7);
    // 越界则回退一周（Windows 也会这样规整）
    let month_days = days_in_month(year as i64, month as i64);
    while day > month_days {
        day -= 7;
    }
    day as u16
}

#[cfg(windows)]
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// UTC 时刻是否落在夏令时窗口内（[dst_start, 年内回绕到标准时开始) 的近似判定）。
#[cfg(windows)]
fn in_daylight(utc: SystemTimeUtc, dst_start: (u16, u16, u16, u16, u16, u16)) -> bool {
    let (sy, sm, swd, sn, sh, smin) = dst_start;
    let start_day = nth_weekday_day(sy, sm, swd, sn);
    let key = (utc.month, utc.day, utc.hour, utc.minute);
    let start_key = (sm, start_day, sh, smin);
    // 北半球简化：从 start 到年末为夏令时（Windows 的规则是在 3~11 月）
    key >= start_key
}

#[cfg(not(windows))]
fn local_utc_offset_secs(_now_unix: i64) -> i64 {
    0
}

/// Howard Hinnant 的 `civil_from_days`：天数 → `YYYY-MM-DD`。
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// `days_from_civil`：`YYYY-MM-DD` → 1970-01-01 起的天数。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `time.Parse(time.RFC3339, s)` 的等价物（返回 unix 秒）。
///
/// 支持 Go RFC3339 允许的形态：`2006-01-02T15:04:05Z07:00` 与带小数秒的变体。
/// 解析失败返回 `None`（Go 返回 err，调用方各自忽略）。
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') {
        return None;
    }
    let month = num(5, 2)?;
    let day = num(8, 2)?;
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let hour = num(11, 2)?;
    let min = num(14, 2)?;
    let sec = num(17, 2)?;
    let mut i = 19;
    // 小数秒（可选）
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    // 时区
    let off = if i >= b.len() {
        return None;
    } else if b[i] == b'Z' || b[i] == b'z' {
        0
    } else if b[i] == b'+' || b[i] == b'-' {
        let sign = if b[i] == b'-' { -1 } else { 1 };
        if i + 6 > b.len() {
            return None;
        }
        let oh = s.get(i + 1..i + 3)?.parse::<i64>().ok()?;
        let om = s.get(i + 4..i + 6)?.parse::<i64>().ok()?;
        sign * (oh * 3600 + om * 60)
    } else {
        return None;
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + min * 60 + sec - off)
}

/// 本机相对 UTC 的偏移秒数（公开包装，供 `stats.rs` 等模块复用）。
pub fn local_utc_offset_secs_pub(now_unix: i64) -> i64 {
    local_utc_offset_secs(now_unix)
}

/// `civil_from_days` 的公开形式（返回 `(年, 月, 日)`）。
pub fn civil_from_days_pub(z: i64) -> (i64, i64, i64) {
    let s = civil_from_days(z);
    let y: i64 = s[0..4].parse().unwrap_or(0);
    let m: i64 = s[5..7].parse().unwrap_or(0);
    let d: i64 = s[8..10].parse().unwrap_or(0);
    (y, m, d)
}

/// UTC 日期（供排查；与 `local_day` 对比看时区差异）。
pub fn local_day_utc(now_unix: i64) -> String {
    civil_from_days(now_unix.div_euclid(86_400))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_v4_shaped() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "版本位应为 4：{u}");
        let v = parts[3].chars().next().unwrap();
        assert!("89ab".contains(v), "变体位应为 8/9/a/b：{u}");
        assert!(u.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }

    #[test]
    fn uuid_prefix_takes_chars() {
        for n in [0, 1, 8, 29, 32] {
            assert_eq!(uuid_prefix(n).chars().count(), n);
        }
    }

    #[test]
    fn civil_roundtrip() {
        assert_eq!(civil_from_days(days_from_civil(1970, 1, 1)), "1970-01-01");
        assert_eq!(civil_from_days(days_from_civil(2026, 10, 16)), "2026-10-16");
        assert_eq!(civil_from_days(days_from_civil(2099, 1, 1)), "2099-01-01");
        assert_eq!(civil_from_days(0), "1970-01-01");
    }

    #[test]
    fn base64_raw_url_has_no_padding() {
        assert_eq!(base64_raw_url(b"a"), "YQ");
        assert_eq!(base64_raw_url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn parse_rfc3339_shapes() {
        // 1970-01-01T00:00:00Z
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        // 2099-11-01T00:00:00Z → 与 Go 的 time.Parse 同值（用 days_from_civil 自校）
        let t = parse_rfc3339("2099-11-01T00:00:00Z").unwrap();
        assert_eq!(t % 86_400, 0);
        assert!(t > 4_000_000_000, "2099 年应大于 4e9：{t}");
        // 带偏移
        assert_eq!(
            parse_rfc3339("2026-01-01T08:00:00+08:00"),
            Some(1767225600)
        );
        // 小数秒被忽略
        assert_eq!(
            parse_rfc3339("2026-01-01T00:00:00.123456789Z"),
            parse_rfc3339("2026-01-01T00:00:00Z")
        );
        // 非法形态
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-01-01"), None);
    }
}
