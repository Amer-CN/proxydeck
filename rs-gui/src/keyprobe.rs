//! keyprobe.rs —— 存储 key 探活（对应 Go `bridge.go:543-549` 的缓存与
//! `bridge.go:778-812` 的 `ccProbeKey`，以及 `internal/proxy/probe.go` 的 `ProbeKey`）。
//!
//! 口径（逐条对齐）：
//!   * 结果缓存 25 分钟（略短于前端 30 分钟复查周期，保证每轮复查都真探）
//!   * 缓存命中直接回原串；否则**后台线程**真探、立即回 `{"status":"pending"}`
//!   * 探测打 `https://api.commandcode.ai/alpha/whoami`（免费端点、零生成计费）
//!   * 只回状态字与状态码：不回 key、不回上游响应体
//!   * `ccSaveKey` 换 key/清 key 时立即失效缓存

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 探活结果保鲜期（Go `25*time.Minute`）。
pub const PROBE_TTL: Duration = Duration::from_secs(25 * 60);
/// CommandCode 上游默认地址（Go `proxy.DefaultBaseURL`）。
pub const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";

/// 探活缓存（Go 的 `keyProbeCached` / `keyProbeAt` / `keyProbeBusy`）。
#[derive(Default)]
pub struct KeyProbe {
    pub cached: String,
    pub at: Option<Instant>,
    pub busy: bool,
}

impl KeyProbe {
    /// 缓存是否新鲜（Go：`cached != "" && time.Since(at) < 25min`）。
    pub fn fresh(&self) -> bool {
        !self.cached.is_empty() && self.at.map(|t| t.elapsed() < PROBE_TTL).unwrap_or(false)
    }

    /// 立即失效（Go `ccSaveKey` 里清 `keyProbeCached`）。
    pub fn invalidate(&mut self) {
        self.cached.clear();
    }
}

/// `ccProbeKey` 的立即返回：返回 (回给前端的串, 是否该踢一次后台真探)。
/// 语义与 Go 逐行一致——`start` 取的是**改忙标记之前**的 busy。
pub fn probe_step(cache: &Mutex<KeyProbe>) -> (String, bool) {
    let mut c = cache.lock().unwrap();
    let fresh = c.fresh();
    let busy = c.busy;
    if !fresh && !busy {
        c.busy = true;
    }
    if fresh {
        let cached = c.cached.clone();
        drop(c);
        return (cached, false);
    }
    drop(c);
    (r#"{"status":"pending"}"#.to_string(), !busy)
}

/// 后台探完写回缓存（Go 探测 goroutine 结尾的三字段赋值）。
pub fn store_result(cache: &Mutex<KeyProbe>, json: String) {
    let mut c = cache.lock().unwrap();
    c.cached = json;
    c.at = Some(Instant::now());
    c.busy = false;
}

/// 探活一个 key（Go `proxy.ProbeKey`）：返回 (status, httpCode, 网络错误)。
/// status = ok / unauthorized / error；网络错误时 httpCode 为 0。
pub fn probe_key(base_url: &str, api_key: &str) -> (String, i32, Option<String>) {
    let url = format!("{base_url}/alpha/whoami");
    let client = crate::httpc::client_with_timeout(Duration::from_secs(10));
    match client.get(&url).bearer_auth(api_key).send() {
        Ok(r) => {
            let code = r.status().as_u16() as i32;
            match code {
                200 => ("ok".to_string(), code, None),
                401 => ("unauthorized".to_string(), code, None),
                _ => ("error".to_string(), code, None),
            }
        }
        Err(e) => ("error".to_string(), 0, Some(e.to_string())),
    }
}

/// 组装探测结果的 JSON（Go：
/// `{"status":..,"httpCode":..}` + 网络错误时补 `"detail":"网络错误: ..."`，键排序）。
pub fn probe_result_json(status: &str, http_code: i32, net_err: Option<&str>) -> String {
    let mut pairs: Vec<(&str, serde_json::Value)> = Vec::new();
    if let Some(e) = net_err {
        pairs.push((
            "detail",
            serde_json::Value::String(format!("网络错误: {e}")),
        ));
    }
    pairs.push(("httpCode", serde_json::json!(http_code)));
    pairs.push(("status", serde_json::Value::String(status.to_string())));
    crate::jsons::marshal(&crate::jsons::obj(&pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_requires_both_cache_and_recent_time() {
        let mut c = KeyProbe::default();
        assert!(!c.fresh(), "空缓存不算新鲜");
        c.cached = r#"{"status":"ok","httpCode":200}"#.to_string();
        assert!(!c.fresh(), "没有时间戳不算新鲜");
        c.at = Some(Instant::now());
        assert!(c.fresh());
        c.at = Some(Instant::now() - Duration::from_secs(26 * 60));
        assert!(!c.fresh(), "超过 25 分钟必须重探");
    }

    #[test]
    fn probe_step_matches_go_busy_flag_semantics() {
        let cache = Mutex::new(KeyProbe::default());
        // 第一次：忙标记由 false 变 true → 该踢后台探测，立即回 pending
        let (out, start) = probe_step(&cache);
        assert_eq!(out, r#"{"status":"pending"}"#);
        assert!(start, "首次调用必须踢一次真探");
        assert!(cache.lock().unwrap().busy);
        // 第二次（上一次还没探完）：busy 已经是 true → 不再重复踢
        let (out, start) = probe_step(&cache);
        assert_eq!(out, r#"{"status":"pending"}"#);
        assert!(!start, "在探中不得重复踢");
        // 探完写回 → 命中缓存
        store_result(
            &cache,
            r#"{"httpCode":200,"status":"ok"}"#.to_string(),
        );
        let (out, start) = probe_step(&cache);
        assert_eq!(out, r#"{"httpCode":200,"status":"ok"}"#);
        assert!(!start);
    }

    #[test]
    fn result_json_matches_go_key_order_and_detail() {
        assert_eq!(
            probe_result_json("nokey", 0, None),
            r#"{"httpCode":0,"status":"nokey"}"#
        );
        assert_eq!(
            probe_result_json("error", 0, Some("boom")),
            r#"{"detail":"网络错误: boom","httpCode":0,"status":"error"}"#
        );
    }
}
