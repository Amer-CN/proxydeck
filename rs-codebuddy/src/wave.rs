//! 风控波保护（抄自社区教科书：workbuddy2api-hub / reidev workbuddy2api-panel）。
//!
//! 背景：2026-10-23 下午起国际版号成批被上游限制（11140「账号存在异常行为」）。
//! 逐个号硬撞只会把整池烧光——社区做法是**15 分钟内同一域有 2 个号被限，就把该域
//! 其余号暂停派发 30 分钟**，等上游风控波过去，面板可一键解除。
//!
//! 本项目一个插件进程 = 一个域（CN / INTL 各拉一个），所以状态放进程内存即可，
//! 不需要跨域协调。开关：环境变量 `CODEBUDDY_WAVE_PAUSE_SECS`（默认 1800；设 0 关闭）。
//! 解除：`POST /v1/wave` 或进程重启。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

/// 统计窗口：15 分钟内累计到阈值即判定为「风控波」。
pub const WAVE_WINDOW_MS: i64 = 15 * 60 * 1000;
/// 默认暂停时长：30 分钟（社区默认值，可通过环境变量覆盖）。
pub const WAVE_PAUSE_MS: i64 = 30 * 60 * 1000;
/// 触发阈值：窗口内 2 个号被账号级限制。
pub const WAVE_THRESHOLD: usize = 2;

/// 进程级风控波状态。
pub struct WaveGuard {
    /// 被账号级限制的时点（epoch 毫秒），只保留窗口内的。
    hits: Mutex<Vec<i64>>,
    /// 暂停到期时点（epoch 毫秒；`i64::MIN` = 未暂停）。
    paused_until: AtomicI64,
    /// 实际使用的暂停时长（毫秒；0 = 关闭）。
    pause_ms: i64,
}

impl Default for WaveGuard {
    fn default() -> Self {
        Self::new(WAVE_PAUSE_MS)
    }
}

impl WaveGuard {
    pub fn new(pause_ms: i64) -> Self {
        Self {
            hits: Mutex::new(Vec::new()),
            paused_until: AtomicI64::new(i64::MIN),
            pause_ms: pause_ms.max(0),
        }
    }

    /// 按环境变量构造：未设置/非法 → 默认 30 分钟；0 → 关闭。
    pub fn from_env() -> Self {
        let ms = match std::env::var("CODEBUDDY_WAVE_PAUSE_SECS") {
            Ok(v) => match v.trim().parse::<i64>() {
                Ok(secs) => secs.saturating_mul(1000),
                Err(_) => WAVE_PAUSE_MS,
            },
            Err(_) => WAVE_PAUSE_MS,
        };
        Self::new(ms)
    }

    pub fn enabled(&self) -> bool {
        self.pause_ms > 0
    }

    /// 记录一次「账号被限制」。返回 `true` = **本次刚好触发**风控波暂停
    /// （调用方据此立即停止派发并回 503，保护还活着的号）。
    pub fn note_restricted(&self, now_ms: i64) -> bool {
        if !self.enabled() {
            return false;
        }
        let mut hits = match self.hits.lock() {
            Ok(h) => h,
            Err(e) => e.into_inner(),
        };
        hits.retain(|t| now_ms.saturating_sub(*t) <= WAVE_WINDOW_MS);
        hits.push(now_ms);
        if hits.len() >= WAVE_THRESHOLD {
            let until = now_ms.saturating_add(self.pause_ms);
            let prev = self.paused_until.swap(until, Ordering::SeqCst);
            if prev <= now_ms {
                hits.clear();
                return true;
            }
        }
        false
    }

    /// 当前暂停到期时点；未暂停或已过期返回 `None`。
    pub fn active_until(&self, now_ms: i64) -> Option<i64> {
        let until = self.paused_until.load(Ordering::SeqCst);
        if until > now_ms {
            Some(until)
        } else {
            None
        }
    }

    /// 立即解除（GUI / `POST /v1/wave`）。
    pub fn resume(&self) {
        self.paused_until.store(i64::MIN, Ordering::SeqCst);
        if let Ok(mut hits) = self.hits.lock() {
            hits.clear();
        }
    }

    /// 窗口内的命中次数（/health 展示用）。
    pub fn hits(&self, now_ms: i64) -> usize {
        let hits = match self.hits.lock() {
            Ok(h) => h,
            Err(e) => e.into_inner(),
        };
        hits.iter()
            .filter(|t| now_ms.saturating_sub(**t) <= WAVE_WINDOW_MS)
            .count()
    }

    pub fn pause_ms(&self) -> i64 {
        self.pause_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_hits_inside_window_trip_the_pause() {
        let g = WaveGuard::new(WAVE_PAUSE_MS);
        assert!(!g.note_restricted(1_000));
        assert!(g.note_restricted(2_000), "第二个号被限必须触发");
        assert_eq!(g.active_until(2_500), Some(2_000 + WAVE_PAUSE_MS));
        assert!(!g.note_restricted(3_000), "已在暂停中再次命中不重复触发");
    }

    #[test]
    fn hits_outside_window_expire() {
        let g = WaveGuard::new(WAVE_PAUSE_MS);
        assert!(!g.note_restricted(1_000));
        // 窗口外再来一个：不算第二个
        assert!(!g.note_restricted(1_000 + WAVE_WINDOW_MS + 1));
        assert!(g.active_until(1_000 + WAVE_WINDOW_MS + 2).is_none());
        // 窗口内再来一个才算
        assert!(g.note_restricted(1_000 + WAVE_WINDOW_MS + 3));
    }

    #[test]
    fn pause_expires_and_resume_clears() {
        let g = WaveGuard::new(10_000);
        g.note_restricted(0);
        g.note_restricted(1);
        assert!(g.active_until(5_000).is_some());
        assert!(g.active_until(20_000).is_none(), "到期自动失效");
        g.note_restricted(20_001);
        g.note_restricted(20_002);
        assert!(g.active_until(20_003).is_some());
        g.resume();
        assert!(g.active_until(20_004).is_none());
        assert_eq!(g.hits(20_004), 0);
    }

    #[test]
    fn disabled_never_pauses() {
        let g = WaveGuard::new(0);
        assert!(!g.note_restricted(1));
        assert!(!g.note_restricted(2));
        assert!(g.active_until(3).is_none());
        assert!(!g.enabled());
    }

    #[test]
    fn from_env_parses_seconds_and_falls_back() {
        // 只测纯函数分派：直接构造等价参数，避免改动进程级环境变量。
        assert_eq!(WaveGuard::new(1800_000).pause_ms(), 1_800_000);
        assert_eq!(WaveGuard::new(-5).pause_ms(), 0, "负数当关闭");
    }
}