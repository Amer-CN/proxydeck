//! 会话层——**红线 4**（`x-litellm-session-id` = 请求体 `litellm_session_id` =
//! `prompt_cache_key`，三处同值）的取值来源（`client.go:326-480`）。
//!
//! 语义对齐官方 CLI 的「一个 CLI 窗口」：窗口内所有请求复用同一 session id，
//! 窗口关闭（闲置/超龄清理）后消失。池参数与 Go 逐条相同：
//!
//! - 串行请求复用最近使用的空闲会话（单窗口连续对话）
//! - 全忙且未达上限（16）→ 新建（模拟再开一个窗口）
//! - 达上限 → 共享占用最少的会话（官方单窗口本来就允许并发共享同一 session）
//! - 每会话随机闲置阈值 20~40 分钟、最长存活 180~330 分钟
//! - 每次租借 `promptSeq += 1`（`metadata.prompt_id` 的 `########N` 计数）
//!
//! 会话 id 是 RFC 4122 v4（Go `uuid.New()`），`litellm_conversation_id` 是
//! `litellm_conversation_<ms>_<rand36>`（对齐官方 `VAt()`）。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 池上限——`client.go:357`（`sessionPoolMaxSize = 16`）。
pub const SESSION_POOL_MAX_SIZE: usize = 16;

/// `rand36` 的字符表——`client.go:450`。
const RAND36_CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// 一次请求用得着的会话三要素（Go `LitellmSession` 的对外可观察部分）。
///
/// Go 里会话对象带锁内可变状态（inUse/lastUsed/promptSeq），这里把「池内状态」放进
/// [`PoolEntry`]，租借时给出快照 + 一个 RAII 归还守卫（对应 Go 的 `defer Release`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LitellmSession {
    /// `x-litellm-session-id` / `litellm_session_id` / `prompt_cache_key` 三处同值。
    pub id: String,
    /// `metadata.litellm_conversation_id`。
    pub conversation_id: String,
    /// `metadata.prompt_id` 的 `########N` 计数（每次租借 +1）。
    pub prompt_seq: i64,
}

impl LitellmSession {
    /// `metadata.prompt_id` 取值——`reqshape.go:247`：`<会话ID>########<N>`。
    pub fn prompt_id(&self) -> String {
        format!("{}########{}", self.id, self.prompt_seq)
    }
}

/// 池内条目（Go `LitellmSession` 的锁内状态部分）。
struct PoolEntry {
    session: LitellmSession,
    in_use: usize,
    created_at: Instant,
    last_used: Instant,
    idle_ttl: Duration,
    max_age: Duration,
}

fn pool() -> &'static Mutex<Vec<PoolEntry>> {
    static POOL: OnceLock<Mutex<Vec<PoolEntry>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::new()))
}

fn lock_pool() -> std::sync::MutexGuard<'static, Vec<PoolEntry>> {
    // 池是纯内存态、不存在半写坏的中间态，毒化后继续用（等价 Go 的「无 panic 传播」）
    pool().lock().unwrap_or_else(|e| e.into_inner())
}

/// 会话租借守卫：持有期间 `in_use > 0`，`Drop` 时归还（对应 `ReleaseLitellmSession`）。
pub struct SessionLease {
    session: LitellmSession,
}

impl SessionLease {
    /// 本次请求的会话快照。
    pub fn session(&self) -> &LitellmSession {
        &self.session
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        release(&self.session.id);
    }
}

/// 从会话池租借一个会话（对应 `AcquireLitellmSession`，`client.go:365-411`）。
pub fn acquire() -> SessionLease {
    let now = Instant::now();
    let mut p = lock_pool();

    // 清理：空闲（in_use==0）且（闲置 > 本会话 idleTTL 或 年龄 > 本会话 maxAge）
    p.retain(|e| {
        !(e.in_use == 0 && (now.duration_since(e.last_used) > e.idle_ttl
            || now.duration_since(e.created_at) > e.max_age))
    });

    // 1) 最近使用的空闲会话
    let mut idle: Option<usize> = None;
    for (i, e) in p.iter().enumerate() {
        if e.in_use == 0 && idle.map_or(true, |j| e.last_used > p[j].last_used) {
            idle = Some(i);
        }
    }
    if let Some(i) = idle {
        p[i].in_use += 1;
        p[i].last_used = now;
        p[i].session.prompt_seq += 1;
        return SessionLease {
            session: p[i].session.clone(),
        };
    }

    // 2) 全忙：未达上限 → 新建
    if p.len() < SESSION_POOL_MAX_SIZE {
        let (session, idle_ttl, max_age) = new_pooled_session();
        let entry = PoolEntry {
            session: session.clone(),
            in_use: 1,
            created_at: now,
            last_used: now,
            idle_ttl,
            max_age,
        };
        let mut entry = entry;
        entry.session.prompt_seq += 1;
        let session = entry.session.clone();
        p.push(entry);
        return SessionLease { session };
    }

    // 3) 达上限：共享占用最少的
    let mut share = 0usize;
    for i in 1..p.len() {
        if p[i].in_use < p[share].in_use {
            share = i;
        }
    }
    p[share].in_use += 1;
    p[share].last_used = now;
    p[share].session.prompt_seq += 1;
    SessionLease {
        session: p[share].session.clone(),
    }
}

/// 归还会话（对应 `ReleaseLitellmSession`，`client.go:414-424`）。
pub fn release(session_id: &str) {
    let now = Instant::now();
    let mut p = lock_pool();
    if let Some(e) = p.iter_mut().find(|e| e.session.id == session_id) {
        if e.in_use > 0 {
            e.in_use -= 1;
        }
        e.last_used = now;
    }
}

/// 生成一个全新会话并使用它（对应 `newLitellmSession`，`client.go:428-436`）：
/// 随机 `idleTTL`/`maxAge` + 会话级 `litellm_conversation_id`。
fn new_pooled_session() -> (LitellmSession, Duration, Duration) {
    let session = LitellmSession {
        id: new_uuid_v4(),
        conversation_id: new_litellm_conversation_id(),
        prompt_seq: 0,
    };
    (
        session,
        rand_session_idle_ttl(),
        rand_session_max_age(),
    )
}

/// 不入池的一次性会话（对应 `newLitellmSession` 的「用后即弃」用法，
/// `client.go:439`/`495-498`/`88-92` 三处兜底路径）。`promptSeq` 从 0 起。
pub fn new_litellm_session() -> LitellmSession {
    let (s, _, _) = new_pooled_session();
    s
}

/// 不入池的一次性会话 id（对应 `newLitellmSessionID`，`client.go:439`）。
pub fn new_litellm_session_id() -> String {
    new_litellm_session().id
}

/// `litellm_conversation_<ms>_<rand36>`——`client.go:443-445`（对齐官方 `VAt()`）。
pub fn new_litellm_conversation_id() -> String {
    format!("litellm_conversation_{}_{}", unix_millis(), rand36())
}

/// `Date.now()` 的毫秒时间戳。
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// RFC 4122 v4 UUID（Go `uuid.New().String()`：小写、连字符分段）。
pub fn new_uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `rand36()`——7~11 位小写字母数字（`client.go:449-457`）。
pub fn rand36() -> String {
    let n = 7 + rand_int_n(5) as usize; // 7..=11
    let mut out = String::with_capacity(n);
    for _ in 0..n {
        out.push(RAND36_CHARSET[rand_int_n(RAND36_CHARSET.len() as u32) as usize] as char);
    }
    out
}

/// `randIntN`——crypto/rand 取 4 字节小端 u32 后取模；随机源异常时退化为时间派生
/// （`client.go:461-470`，退化只损失随机性、不影响正确性）。
pub fn rand_int_n(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let mut buf = [0u8; 4];
    if getrandom::fill(&mut buf).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        return (nanos % n as i64) as u32;
    }
    u32::from_le_bytes(buf) % n
}

/// 每会话独立的闲置阈值：20~40 分钟（`client.go:473-475`）。
pub fn rand_session_idle_ttl() -> Duration {
    Duration::from_secs((20 + rand_int_n(21) as u64) * 60)
}

/// 每会话独立的最长存活：180~330 分钟（`client.go:478-480`）。
pub fn rand_session_max_age() -> Duration {
    Duration::from_secs((180 + rand_int_n(151) as u64) * 60)
}

#[cfg(test)]
pub(crate) fn reset_pool_for_test() {
    lock_pool().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 会话池是**进程级全局状态**，池相关用例必须串行——否则一个用例的 `reset_pool_for_test`
    /// 会把另一个用例建好的会话清掉（首版就这么踩过：并发例断言「共享已有会话」随机失败）。
    static POOL_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn pool_guard() -> std::sync::MutexGuard<'static, ()> {
        POOL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn uuid_is_rfc4122_v4_shape() {
        let id = new_uuid_v4();
        let b = id.as_bytes();
        assert_eq!(b.len(), 36);
        assert_eq!(b[8], b'-');
        assert_eq!(b[13], b'-');
        assert_eq!(b[18], b'-');
        assert_eq!(b[23], b'-');
        assert_eq!(b[14], b'4', "版本位应为 4: {id}");
        assert!(matches!(b[19], b'8' | b'9' | b'a' | b'b'), "变体位: {id}");
    }

    #[test]
    fn conversation_id_shape() {
        let c = new_litellm_conversation_id();
        let rest = c
            .strip_prefix("litellm_conversation_")
            .expect("前缀固定");
        let (ms, rand) = rest.split_once('_').expect("有下划线分隔");
        assert!(ms.parse::<u64>().is_ok(), "毫秒时间戳: {c}");
        assert!((7..=11).contains(&rand.len()), "rand36 长度: {c}");
        assert!(rand
            .bytes()
            .all(|b| RAND36_CHARSET.contains(&b)));
    }

    #[test]
    fn rand36_length_and_charset() {
        for _ in 0..200 {
            let s = rand36();
            assert!((7..=11).contains(&s.len()));
            assert!(s.bytes().all(|b| RAND36_CHARSET.contains(&b)));
        }
    }

    #[test]
    fn ttl_ranges_match_go() {
        for _ in 0..200 {
            let t = rand_session_idle_ttl().as_secs();
            assert!((20 * 60..=40 * 60).contains(&t), "idleTTL={t}s");
            let m = rand_session_max_age().as_secs();
            assert!((180 * 60..=330 * 60).contains(&m), "maxAge={m}s");
        }
    }

    #[test]
    fn prompt_seq_starts_at_one_and_increments() {
        let _g = pool_guard();
        reset_pool_for_test();
        let a = acquire();
        let id = a.session().id.clone();
        assert_eq!(a.session().prompt_seq, 1);
        assert_eq!(a.session().prompt_id(), format!("{id}########1"));
        drop(a); // 归还
        let b = acquire();
        assert_eq!(b.session().id, id, "串行请求应复用同一空闲会话");
        assert_eq!(b.session().prompt_seq, 2);
        drop(b);
        reset_pool_for_test();
    }

    #[test]
    fn concurrent_borrows_create_new_sessions_until_cap() {
        let _g = pool_guard();
        reset_pool_for_test();
        let mut leases = Vec::new();
        for _ in 0..SESSION_POOL_MAX_SIZE {
            leases.push(acquire());
        }
        let ids: std::collections::HashSet<_> = leases.iter().map(|l| l.session().id.clone()).collect();
        assert_eq!(ids.len(), SESSION_POOL_MAX_SIZE, "未达上限前应各建一个会话");
        // 第 17 个：达上限 → 共享占用最少的（全部 in_use==1 → 取第一个）
        let extra = acquire();
        assert!(ids.contains(&extra.session().id), "达上限应共享已有会话");
        drop(extra);
        drop(leases);
        reset_pool_for_test();
    }
}
