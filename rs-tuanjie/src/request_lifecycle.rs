//! 团结请求生命周期：一次请求的「路由 → 注册/计数 → 收尾」归一个所有者。
//!
//! 计数口径按 Go 原样锁住（`server.go`）：
//! * 池路径 `inc_load` 绑定**初选账号**，且在响应交付前配平；换号不迁移 load 计数。
//! * `RequestRegistry` 的条目在非流式收尾时删除；流式随响应流走，流尽/客户端取消时删除。
//! * pacing 的进行中计数只在兜底实际启用时 +1，请求返回（含提前返回）时配平。
//!
//! 守卫只做同步配平；`Drop` 不 spawn 任务、不发网络请求。

use std::sync::Arc;

use crate::accounts::AccountPool;
use crate::inflight::RequestRegistry;
use crate::pacing::Pacer;
use crate::server::AppState;

/// 本请求的凭据来源：由**选号结果**决定，不按账号数量推断。
pub enum CredentialRoute {
    PoolAccount {
        account: crate::accounts::Account,
        token: String,
    },
    /// 选不出池号（全冷却/全失效/无池）：沿用本机凭据。
    LocalCredentialFallback,
}

impl CredentialRoute {
    pub fn pool_uid(&self) -> Option<&str> {
        match self {
            Self::PoolAccount { account, .. } => Some(account.user_id.as_str()),
            Self::LocalCredentialFallback => None,
        }
    }
}

/// 取本请求路由：池非空且当场能选出号才走池路径，否则回退本机凭据。
pub fn resolve_route(st: &AppState, model: &str) -> CredentialRoute {
    match st.pool.pick_with_token(model) {
        Some((account, token)) => CredentialRoute::PoolAccount { account, token },
        None => CredentialRoute::LocalCredentialFallback,
    }
}

/// 进行中注册条目的所有权；`finish` 幂等，`Drop` 兜底删除。
pub struct FinishGuard {
    registry: Arc<RequestRegistry>,
    rid: Option<String>,
}

impl FinishGuard {
    pub fn new(registry: Arc<RequestRegistry>, rid: String) -> Self {
        Self {
            registry,
            rid: Some(rid),
        }
    }

    pub fn rid(&self) -> &str {
        self.rid.as_deref().unwrap_or("")
    }

    pub fn finish(&mut self) {
        if let Some(rid) = self.rid.take() {
            self.registry.finish(&rid);
        }
    }
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

/// 一次性请求生命周期：注册号 + 初选账号 load 计数。
pub struct RequestLifecycle {
    registry: Arc<RequestRegistry>,
    rid: Option<String>,
    pool: Arc<AccountPool>,
    load_uid: Option<String>,
}

impl RequestLifecycle {
    /// 登记进行中请求；池路由同时给**初选账号**加 load（换号不迁移）。
    pub fn begin(
        st: &Arc<AppState>,
        model: &str,
        wants_stream: bool,
        route: &CredentialRoute,
    ) -> Self {
        let rid = st
            .registry
            .register(model, route.pool_uid().unwrap_or(""), wants_stream);
        let load_uid = route.pool_uid().map(str::to_string);
        if let Some(uid) = load_uid.as_deref() {
            st.pool.inc_load(uid);
        }
        Self {
            registry: st.registry.clone(),
            rid: Some(rid),
            pool: st.pool.clone(),
            load_uid,
        }
    }

    pub fn rid(&self) -> &str {
        self.rid.as_deref().unwrap_or("")
    }

    /// 配平初选账号 load（响应交付前调用；Drop 兜底）。
    pub fn release_load(&mut self) {
        if let Some(uid) = self.load_uid.take() {
            self.pool.dec_load(&uid);
        }
    }

    /// 结束进行中注册（非流式路径）。
    pub fn finish_registry(&mut self) {
        if let Some(rid) = self.rid.take() {
            self.registry.finish(&rid);
        }
    }

    /// 非流式收尾：注册与 load 一起配平。
    pub fn finish_all(&mut self) {
        self.finish_registry();
        self.release_load();
    }

    /// 流式收尾：把注册条目所有权交给响应流（load 已由调用方在交付前配平）。
    pub fn take_finish_guard(&mut self) -> FinishGuard {
        let rid = self.rid.take().unwrap_or_default();
        FinishGuard::new(self.registry.clone(), rid)
    }

    /// pacing 计数守卫：启用时 +1，离开作用域（含提前 return）时配平。
    pub fn pacing_guard(pacer: &Pacer, enabled: bool) -> PacingGuard<'_> {
        PacingGuard::new(pacer, enabled)
    }
}

impl Drop for RequestLifecycle {
    fn drop(&mut self) {
        self.release_load();
        self.finish_registry();
    }
}

/// pacing 进行中计数守卫（同步配平，不做网络/后台任务）。
pub struct PacingGuard<'a> {
    pacer: Option<&'a Pacer>,
}

impl<'a> PacingGuard<'a> {
    pub fn new(pacer: &'a Pacer, enabled: bool) -> Self {
        if enabled {
            pacer.pending_inc();
            Self { pacer: Some(pacer) }
        } else {
            Self { pacer: None }
        }
    }
}

impl Drop for PacingGuard<'_> {
    fn drop(&mut self) {
        if let Some(pacer) = self.pacer.take() {
            pacer.pending_dec();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_guard_balances_only_when_enabled() {
        let pacer = Pacer::detached();
        {
            let _guard = PacingGuard::new(&pacer, true);
            assert_eq!(pacer.pending(), 1);
        }
        assert_eq!(pacer.pending(), 0);
        {
            let _guard = PacingGuard::new(&pacer, false);
            assert_eq!(pacer.pending(), 0);
        }
        assert_eq!(pacer.pending(), 0);
    }

    #[test]
    fn finish_guard_is_idempotent_and_drop_safe() {
        let registry = Arc::new(RequestRegistry::new());
        let rid = registry.register("KIMI-K3", "uid-a", false);
        assert_eq!(registry.inflight().len(), 1);
        let mut guard = FinishGuard::new(registry.clone(), rid);
        guard.finish();
        guard.finish();
        assert!(registry.inflight().is_empty(), "重复 finish 不得 panic/重复计数");
    }
}
