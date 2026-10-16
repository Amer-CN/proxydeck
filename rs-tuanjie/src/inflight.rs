//! 进行中请求注册表 + 实时动态日志 —— `internal/tuanjie/inflight.go` 的逐条对应物。
//!
//! Go 侧事实（`inflight.go`，只读核对）：
//! - `InflightReq`（`:12-20`）：字段与 JSON 名 `rid/model/user_id/stream/elapsed/idle/bytes`；
//! - `NewRegistry`（`:41-43`）：容量 **500**（`capacity: 500`，超限丢最老）；
//! - `Register`（`:46-60`）：`order` 是 FIFO；超容量时丢 `order[0]` 且从 map 删；
//! - `Finish`（`:73-85`）：从 map 删 + 从 order 里摘（只摘第一个命中）；
//! - `Inflight`（`:88-108`）：快照按 `Elapsed` **降序**（插入排序，稳定）；
//! - `LoadOf`（`:111-121`）：按 user_id 计数；
//! - `newRID`（`:123-141`）：进程级计数器 `n++`，把 `n & 0xf` 逐位右移写满 12 位十六进制
//!   （小写）—— 即 n 的 **低 48 位零填充 12 位十六进制**；
//! - `ActivityEvent`（`:144-154`）：`seq/time/kind/message/model,omitempty/user_id,omitempty/
//!   latency_ms,omitempty/tokens,omitempty/status,omitempty`；
//! - `ActivityLog.Add`（`:169-181`）：新的插最前、超 200 条截尾、`seq` 单调递增、
//!   `time` = 本机时区 `HH:MM:SS`；
//! - `List`（`:184-192`）：`limit<=0 || limit>len` 时取全部，否则取前 limit 条。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

/// 注册表容量 —— `inflight.go:42`。
pub const REGISTRY_CAPACITY: usize = 500;

/// 动态日志环形容量 —— `inflight.go:178`。
pub const ACTIVITY_CAPACITY: usize = 200;

/// 一条进行中请求的快照 —— `InflightReq`（`inflight.go:12-20`）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InflightReq {
    pub rid: String,
    pub model: String,
    pub user_id: String,
    pub stream: bool,
    /// 总耗时秒。
    pub elapsed: f64,
    /// 距上个 chunk 的静默秒数。
    pub idle: f64,
    /// 已收字节。
    pub bytes: i64,
}

struct RegistryEntry {
    rid: String,
    model: String,
    user_id: String,
    stream: bool,
    started: Instant,
    last_chunk: Instant,
    bytes: i64,
}

struct RegistryInner {
    reqs: HashMap<String, RegistryEntry>,
    order: Vec<String>,
}

/// 线程安全的进行中请求注册表 —— `RequestRegistry`（`inflight.go:33-38`）。
pub struct RequestRegistry {
    inner: Mutex<RegistryInner>,
}

impl Default for RequestRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestRegistry {
    /// `NewRegistry`（`inflight.go:41-43`）。
    pub fn new() -> Self {
        RequestRegistry {
            inner: Mutex::new(RegistryInner {
                reqs: HashMap::new(),
                order: Vec::new(),
            }),
        }
    }

    /// 登记一条新请求，返回 rid —— `Register`（`inflight.go:46-60`）。
    pub fn register(&self, model: &str, user_id: &str, stream: bool) -> String {
        let rid = next_rid();
        let mut g = self.inner.lock().unwrap();
        if g.order.len() >= REGISTRY_CAPACITY {
            let oldest = g.order.remove(0);
            g.reqs.remove(&oldest);
        }
        let now = Instant::now();
        g.reqs.insert(
            rid.clone(),
            RegistryEntry {
                rid: rid.clone(),
                model: model.to_string(),
                user_id: user_id.to_string(),
                stream,
                started: now,
                last_chunk: now,
                bytes: 0,
            },
        );
        g.order.push(rid.clone());
        rid
    }

    /// 记录一个 chunk 到达 —— `Touch`（`inflight.go:63-70`）。
    pub fn touch(&self, rid: &str, nbytes: i64) {
        let mut g = self.inner.lock().unwrap();
        if let Some(r) = g.reqs.get_mut(rid) {
            r.last_chunk = Instant::now();
            r.bytes += nbytes;
        }
    }

    /// 测试缝：塞一条**指定 rid** 的进行中请求（`register` 用进程级计数器，多测试并行
    /// 时 rid 不可预期）。生产路径不用它；本片用它让 `/inflight` 的 `elapsed`/`idle`
    /// 两个 float64 字段在对照里有非空取值（否则字段形状差异看不出来）。
    #[doc(hidden)]
    pub fn insert_for_test(&self, rid: &str, model: &str, user_id: &str, stream: bool, bytes: i64) {
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        g.reqs.insert(
            rid.to_string(),
            RegistryEntry {
                rid: rid.to_string(),
                model: model.to_string(),
                user_id: user_id.to_string(),
                stream,
                started: now,
                last_chunk: now,
                bytes,
            },
        );
        g.order.push(rid.to_string());
    }

    /// 移除一条请求 —— `Finish`（`inflight.go:73-85`）。
    pub fn finish(&self, rid: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.reqs.remove(rid).is_some() {
            if let Some(i) = g.order.iter().position(|o| o == rid) {
                g.order.remove(i);
            }
        }
    }

    /// 进行中请求快照（按耗时降序）—— `Inflight`（`inflight.go:88-108`）。
    pub fn inflight(&self) -> Vec<InflightReq> {
        let g = self.inner.lock().unwrap();
        let now = Instant::now();
        let mut out: Vec<InflightReq> = g
            .reqs
            .values()
            .map(|r| InflightReq {
                rid: r.rid.clone(),
                model: r.model.clone(),
                user_id: r.user_id.clone(),
                stream: r.stream,
                elapsed: now.duration_since(r.started).as_secs_f64(),
                idle: now.duration_since(r.last_chunk).as_secs_f64(),
                bytes: r.bytes,
            })
            .collect();
        // 耗时降序（Go 用的是插入排序 —— 稳定；这里同样用相邻交换，保持稳定性）
        for i in 1..out.len() {
            let mut j = i;
            while j > 0 && out[j].elapsed > out[j - 1].elapsed {
                out.swap(j, j - 1);
                j -= 1;
            }
        }
        out
    }

    /// 某账号当前进行中请求数 —— `LoadOf`（`inflight.go:111-121`）。
    pub fn load_of(&self, user_id: &str) -> usize {
        let g = self.inner.lock().unwrap();
        g.reqs.values().filter(|r| r.user_id == user_id).count()
    }
}

static RID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `newRID`（`inflight.go:128-141`）：计数器 +1，取低 48 位零填充成 12 位小写十六进制。
pub fn next_rid() -> String {
    let n = RID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    rid_from_n(n)
}

/// `newRID` 的**纯格式面**：第 n 次取值（n 从 1 起）。
/// 拆出来是为了让「12 位零填充小写十六进制」这条形态能被黄金门禁**逐值锁死**——
/// 进程级计数器在多测试并行时不能被复位，只有纯函数才可比。
pub fn rid_from_n(n: u64) -> String {
    format!("{:012x}", n & 0xffff_ffff_ffff)
}

/// 一条实时动态事件 —— `ActivityEvent`（`inflight.go:144-154`）。
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEvent {
    pub seq: i64,
    /// `HH:MM:SS`（本机时区）。
    pub time: String,
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user_id: String,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub latency_ms: i64,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub tokens: i64,
    #[serde(skip_serializing_if = "is_zero_i32")]
    pub status: i32,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

struct ActivityInner {
    events: Vec<ActivityEvent>,
    seq: i64,
}

/// 内存环形动态日志 —— `ActivityLog`（`inflight.go:157-161`）。
pub struct ActivityLog {
    inner: Mutex<ActivityInner>,
}

impl Default for ActivityLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityLog {
    /// `NewActivityLog`（`inflight.go:164-166`）。
    pub fn new() -> Self {
        ActivityLog {
            inner: Mutex::new(ActivityInner {
                events: Vec::with_capacity(64),
                seq: 0,
            }),
        }
    }

    /// 记一条事件（新的插最前，超 200 丢最老）—— `Add`（`inflight.go:169-181`）。
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        kind: &str,
        message: &str,
        model: &str,
        user_id: &str,
        latency_ms: i64,
        tokens: i64,
        status: i32,
    ) {
        let mut g = self.inner.lock().unwrap();
        g.seq += 1;
        let ev = ActivityEvent {
            seq: g.seq,
            time: local_hms(),
            kind: kind.to_string(),
            message: message.to_string(),
            model: model.to_string(),
            user_id: user_id.to_string(),
            latency_ms,
            tokens,
            status,
        };
        g.events.insert(0, ev);
        g.events.truncate(ACTIVITY_CAPACITY);
    }

    /// 最近 limit 条（`limit<=0` 取全部）—— `List`（`inflight.go:184-192`）。
    pub fn list(&self, limit: usize) -> Vec<ActivityEvent> {
        let g = self.inner.lock().unwrap();
        let n = if limit == 0 || limit > g.events.len() {
            g.events.len()
        } else {
            limit
        };
        g.events[..n].to_vec()
    }
}

/// 本机时区 `HH:MM:SS` —— Go `time.Now().Format("15:04:05")`（`inflight.go:174`）。
///
/// Windows 走 `GetLocalTime`（对齐 Go 的本地时区语义）；非 Windows 退到 UTC，
/// 并**在 crate README 的差异清单里登记**（本 crate 只跑 Windows）。
pub fn local_hms() -> String {
    let (h, m, s) = local_hms_parts();
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(windows)]
fn local_hms_parts() -> (u16, u16, u16) {
    #[repr(C)]
    #[derive(Default)]
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
        fn GetLocalTime(lp_system_time: *mut SystemTime);
    }
    let mut st = SystemTime::default();
    unsafe { GetLocalTime(&mut st) };
    (st.hour, st.minute, st.second)
}

#[cfg(not(windows))]
fn local_hms_parts() -> (u16, u16, u16) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (
        ((secs / 3600) % 24) as u16,
        ((secs / 60) % 60) as u16,
        (secs % 60) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rid_is_twelve_lowercase_hex() {
        let r = next_rid();
        assert_eq!(r.len(), 12, "{r}");
        assert!(r.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{r}");
    }

    #[test]
    fn capacity_evicts_oldest_and_finish_removes() {
        let reg = RequestRegistry::new();
        let mut rids = Vec::new();
        for _ in 0..(REGISTRY_CAPACITY + 1) {
            rids.push(reg.register("m", "u1", true));
        }
        let snap = reg.inflight();
        assert_eq!(snap.len(), REGISTRY_CAPACITY, "容量上限");
        assert!(!snap.iter().any(|r| r.rid == rids[0]), "最老被丢");
        assert!(snap.iter().any(|r| r.rid == rids[REGISTRY_CAPACITY]), "最新在");
        reg.finish(&rids[REGISTRY_CAPACITY]);
        assert_eq!(reg.inflight().len(), REGISTRY_CAPACITY - 1);
    }

    #[test]
    fn load_of_counts_by_user() {
        let reg = RequestRegistry::new();
        for _ in 0..3 {
            reg.register("m", "uA", true);
        }
        reg.register("m", "uB", false);
        assert_eq!(reg.load_of("uA"), 3);
        assert_eq!(reg.load_of("uB"), 1);
        assert_eq!(reg.load_of("nobody"), 0);
    }

    #[test]
    fn activity_caps_and_orders_like_go() {
        let al = ActivityLog::new();
        for i in 0..(ACTIVITY_CAPACITY + 10) {
            al.add("ok", &format!("msg-{i}"), "m", "u", i as i64, i as i64 * 2, 200);
        }
        let all = al.list(0);
        assert_eq!(all.len(), ACTIVITY_CAPACITY);
        assert_eq!(all[0].message, format!("msg-{}", ACTIVITY_CAPACITY + 9));
        assert_eq!(
            all[ACTIVITY_CAPACITY - 1].message,
            "msg-10".to_string()
        );
        assert_eq!(al.list(3).len(), 3);
    }
}
