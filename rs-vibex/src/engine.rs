//! engine.rs —— 编排层：provider 解析、沙箱项目复用、一轮对话时序、会话亲和。
//!
//! 对应 Go `internal/vibex/engine.go`（C49~C60）。
//!
//! **已知坑（v3.19.0 审查驳回 1 处）**：Go 的 `turn` 里那个「哨兵」事件是
//! `select { case out <- outEvent{}: case <-ctx.Done(): }` —— 消费方已退出时**不得阻塞**。
//! Rust 侧同样必须：`mpsc::Sender::send` 在接收端被丢弃时立刻返回 `Err`（不阻塞），
//! 且外面再套一层取消分支，绝不出现「生产方永久挂在哨兵上」。
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, watch};

use crate::client::{
    dial_ws, write_text_on, ws_auth_headers, ws_url, Client, DialErr, VibexError, WS_CLOSED,
    WS_READ_TIMEOUT,
};
use crate::config::{Config, WS_INIT_CWD};
use crate::gjson::J;
use crate::pool::Pool;
use crate::prompt::{claude_events, OutEvent};
use crate::util::{first_non_empty, init_root_meta, live_status, ports_ok, str_of, to_map};

/// 会话表空闲 TTL（Go `sessionIdleTTL = 30 * time.Minute`）。
pub const SESSION_IDLE_TTL_MS: i64 = 30 * 60 * 1000;
/// WS 读窗口（Go 里每片 5 秒）。
const WS_SLICE_MS: i64 = 5_000;
/// ping 保活间隔（Go 每 20 秒）。
const PING_INTERVAL: Duration = Duration::from_secs(20);

/// 取消标志（对应 Go 的 `context.Context`）。
///
/// 用 `watch` 而不是 `Notify`：`Notify::notify_waiters` 在没有等待者时会**丢掉**通知，
/// 「先检查后 await」的写法存在竞态窗口；`watch` 的 `changed()` 是幂等且无竞态的。
#[derive(Clone)]
pub struct Cancel {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Cancel {
    pub fn new() -> Cancel {
        let (tx, rx) = watch::channel(false);
        Cancel {
            tx: Arc::new(tx),
            rx,
        }
    }
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }
}

/// 会话表：记录每个 session_id 已经发过多少条 messages（下一轮只发增量）。
pub struct SessionTable {
    m: std::collections::HashMap<String, (i64, i64)>, // id -> (sent, last_used_ms)
}

impl SessionTable {
    pub fn new() -> SessionTable {
        SessionTable {
            m: std::collections::HashMap::new(),
        }
    }

    /// `begin`：决定本轮从第几条 message 开始打平、以及是否需要 new_session。
    pub fn begin(&mut self, id: &str, total: usize, now_ms: i64) -> (usize, bool) {
        if id.is_empty() {
            return (0, true);
        }
        if let Some((_, last)) = self.m.get(id) {
            if now_ms - last > SESSION_IDLE_TTL_MS {
                self.m.remove(id);
            }
        }
        match self.m.get(id) {
            None => (0, true),
            Some((sent, _)) => {
                if *sent >= total as i64 {
                    // 没有增量（重发了同一份历史，或换了更短的历史）：当新会话全量重发
                    (0, true)
                } else {
                    (*sent as usize, false)
                }
            }
        }
    }

    /// `commit`：记下本轮已发到第几条（只在本轮成功时调用）。
    pub fn commit(&mut self, id: &str, total: usize, now_ms: i64) {
        if id.is_empty() {
            return;
        }
        self.m.insert(id.to_string(), (total as i64, now_ms));
    }
}

/// 一轮所需的输入。
#[derive(Clone)]
pub struct TurnRequest {
    pub provider: Value,
    pub prompt: String,
    pub first_connect: bool,
    pub fresh_session: bool,
    pub session_id: String,
    pub total_msgs: usize,
}

/// 编排层。
pub struct Engine {
    cfg: Arc<Config>,
    client: Arc<Client>,
    pool: Arc<Pool>,

    turn_mu: tokio::sync::Mutex<()>,
    conn_seq: AtomicI64,
    cur_prov: Mutex<String>,
    last_app: Mutex<Option<Value>>,
    sessions: Mutex<SessionTable>,
}

impl Engine {
    pub fn new(cfg: Arc<Config>, client: Arc<Client>, pool: Arc<Pool>) -> Engine {
        Engine {
            cfg,
            client,
            pool,
            turn_mu: tokio::sync::Mutex::new(()),
            conn_seq: AtomicI64::new(0),
            cur_prov: Mutex::new(String::new()),
            last_app: Mutex::new(None),
            sessions: Mutex::new(SessionTable::new()),
        }
    }

    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    pub fn sessions(&self) -> &Mutex<SessionTable> {
        &self.sessions
    }

    /// `nextFirstConnect`：`connect_mode=="auto"`（大小写不敏感）时首连 `init_root`。
    pub fn next_first_connect(&self) -> bool {
        if !self.cfg.connect_mode.eq_ignore_ascii_case("auto") {
            return false;
        }
        let n = self.conn_seq.fetch_add(1, Ordering::SeqCst);
        n == 0
    }

    /// `resolveModel`：请求指定 → 配置默认 → 首个 lite → 首个。
    pub async fn resolve_model(&self, requested: &str) -> Result<Value, VibexError> {
        let providers = self.client.providers().await?;
        let providers = match providers {
            Some(p) if !p.is_empty() => p,
            _ => {
                return Err(VibexError::new(
                    "未能获取 llm-providers 列表(为空)",
                    502,
                    "",
                ))
            }
        };
        if !requested.is_empty() {
            if let Some(p) = match_provider(&providers, requested) {
                return Ok(p);
            }
        }
        if !self.cfg.model.is_empty() {
            if let Some(p) = match_provider(&providers, &self.cfg.model) {
                return Ok(p);
            }
        }
        for raw in &providers {
            if let Some(m) = raw.as_object() {
                if str_of(m.get("billing_tier")) == "lite" {
                    return Ok(raw.clone());
                }
            }
        }
        if let Some(m) = providers[0].as_object() {
            if !m.is_empty() {
                return Ok(providers[0].clone());
            }
        }
        Err(VibexError::new("provider 列表元素非法", 502, ""))
    }

    /// `ensureApp`：复用/创建沙箱项目 → 启动 → 绑定 provider。
    pub async fn ensure_app(&self, provider: &Value) -> Result<Value, VibexError> {
        let pid = provider_id(provider);
        let name = self.cfg.app_name.clone();
        let apps = self.client.apps_list().await?;
        let mut target: Option<Value> = None;
        for raw in &apps {
            let m = match raw.as_object() {
                Some(m) => m,
                None => continue,
            };
            if str_of(m.get("name")) == name || str_of(m.get("app_id")) == name {
                target = Some(raw.clone());
                break;
            }
        }
        let mut target = match target {
            Some(t) => t,
            None => self.client.app_create(&name, &pid).await?,
        };
        let app_id = match target.as_object() {
            Some(m) => str_of(m.get("app_id")),
            None => String::new(),
        };
        if app_id.is_empty() {
            return Err(VibexError::new("无法从沙箱项目对象中确定 app_id", 502, ""));
        }
        let started = self.client.app_start(&app_id).await?;
        if let (Some(t), Some(s)) = (target.as_object_mut(), started.as_object()) {
            for (k, v) in s {
                t.insert(k.clone(), v.clone());
            }
        }
        let need_patch = {
            let cur = self.cur_prov.lock().unwrap();
            *cur != pid
        };
        if need_patch {
            match self.client.set_llm_provider(&app_id, &pid).await {
                Ok(()) => {
                    *self.cur_prov.lock().unwrap() = pid.clone();
                }
                Err(e) => crate::logf(&format!("set_llm_provider 失败(忽略): {e}")),
            }
        }
        *self.last_app.lock().unwrap() = Some(target.clone());
        Ok(target)
    }

    /// `turn`：跑一轮；错误先写进 `state`，**哨兵必须是最后一个动作**。
    pub async fn turn(
        self: Arc<Self>,
        req: TurnRequest,
        tx: mpsc::Sender<OutEvent>,
        state: Arc<Mutex<String>>,
        cancel: Cancel,
    ) {
        let this = self.clone();
        let req2 = req.clone();
        let tx2 = tx.clone();
        let state2 = state.clone();
        let cancel2 = cancel.clone();
        let fut = async move {
            let _guard = this.turn_mu.lock().await;
            let app = match this.ensure_app(&req2.provider).await {
                Ok(a) => a,
                Err(e) => {
                    set_err(&state2, &e.message);
                    return;
                }
            };
            match this.pump(&app, &req2, &tx2, &cancel2).await {
                Ok(()) => {
                    let mut s = this.sessions.lock().unwrap();
                    s.commit(&req2.session_id, req2.total_msgs, crate::pool::now_ms());
                }
                Err(e) => set_err(&state2, &e.message),
            }
        };
        // 对齐 Go 的 `defer recover()`：内部错误也要落到 state（不静默丢）
        let _ = AssertUnwindSafe(fut).catch_unwind().await;
        // 哨兵：最后执行；接收端已被丢弃时不阻塞（mpsc send 返回 Err），
        // 外面再挂一条取消分支，绝不出现「生产方永久挂在哨兵上」。
        let sentinel = OutEvent::default();
        tokio::select! {
            _ = tx.send(sentinel) => {}
            _ = cancel.cancelled() => {}
        }
    }

    /// `pump`：在 token 池上跑一轮 WS 对话（401/403/412 标死换号，402/429 冷却换号）。
    async fn pump(
        &self,
        app: &Value,
        req: &TurnRequest,
        tx: &mpsc::Sender<OutEvent>,
        cancel: &Cancel,
    ) -> Result<(), VibexError> {
        let app_id = match app.as_object() {
            Some(m) => str_of(m.get("app_id")),
            None => String::new(),
        };
        if app_id.is_empty() {
            return Err(VibexError::new("app 对象缺少 app_id", 502, ""));
        }
        let n = self.pool.len();
        if n == 0 {
            return Err(VibexError::new(
                crate::client::TOKEN_HINT,
                401,
                "TOKEN_NOT_CONFIGURED",
            ));
        }
        let mut last_err: Option<VibexError> = None;
        for _ in 0..n {
            let idx = match self.pool.acquire() {
                Some(i) => i,
                None => break,
            };
            let token = self.pool.value_of(idx);
            match self
                .pump_with_token(&token, &app_id, app, req, tx, cancel)
                .await
            {
                Ok(()) => return Ok(()),
                Err(PumpErr::Ws(he)) => match he.status {
                    401 | 403 | 412 => {
                        self.pool.mark_dead(idx);
                        last_err = Some(VibexError::new(he.to_string(), he.status, ""));
                        continue;
                    }
                    402 | 429 => {
                        self.pool.mark_cooldown(idx);
                        last_err = Some(VibexError::new(he.to_string(), he.status, ""));
                        continue;
                    }
                    _ => return Err(VibexError::new(he.to_string(), 502, "")),
                },
                Err(PumpErr::Fatal(e)) => return Err(e),
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Err(VibexError::new(
                format!("token 池中没有可用 token: {}", crate::client::TOKEN_HINT),
                401,
                "TOKEN_EXHAUSTED",
            )),
        }
    }

    /// `pumpWithToken`：用指定 token 跑完一轮。
    async fn pump_with_token(
        &self,
        token: &str,
        app_id: &str,
        app: &Value,
        req: &TurnRequest,
        tx: &mpsc::Sender<OutEvent>,
        cancel: &Cancel,
    ) -> Result<(), PumpErr> {
        let raw_url = ws_url(&self.cfg.base_url(), app_id).map_err(|e| PumpErr::Fatal(VibexError::new(e, 502, "")))?;
        let headers = ws_auth_headers(token, &self.cfg.base_url());
        let mut ws = match dial_ws(&raw_url, &headers, crate::client::WS_DIAL_TIMEOUT).await {
            Ok(w) => w,
            Err(DialErr::Handshake(he)) => return Err(PumpErr::Ws(he)),
            Err(DialErr::Other(e)) => {
                return Err(PumpErr::Fatal(VibexError::new(
                    format!("WS 连接失败: {e}"),
                    502,
                    "",
                )))
            }
        };

        // ping 保活（每 20s）+ 收尾：先停 ping，再关连接。
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let ws_writer = ws.writer_handle();
        let ping_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PING_INTERVAL);
            ticker.tick().await; // 第一次 tick 立即返回，跳过
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if write_text_on(&ws_writer, r#"{"type":"ping"}"#).await.is_err() {
                            return;
                        }
                    }
                    _ = stop_rx.changed() => return,
                }
            }
        });

        let result = self
            .pump_ws_inner(&mut ws, app_id, app, req, tx, cancel)
            .await;

        let _ = stop_tx.send(true);
        let _ = ping_task.await;
        ws.close().await;
        result
    }

    async fn pump_ws_inner(
        &self,
        ws: &mut crate::client::WsConn,
        app_id: &str,
        app: &Value,
        req: &TurnRequest,
        tx: &mpsc::Sender<OutEvent>,
        cancel: &Cancel,
    ) -> Result<(), PumpErr> {
        // (1) init_root（首次）或 init
        let init_msg = if req.first_connect {
            let empty = Map::new();
            let app_obj = app.as_object().unwrap_or(&empty);
            J::Obj(vec![
                ("cwd".into(), J::Str(WS_INIT_CWD.to_string())),
                (
                    "meta".into(),
                    J::from_value(&init_root_meta(app_obj)),
                ),
                ("replay".into(), J::Bool(false)),
                ("type".into(), J::Str("init_root".to_string())),
            ])
            .to_go()
        } else {
            J::Obj(vec![
                ("cwd".into(), J::Str(WS_INIT_CWD.to_string())),
                ("replay".into(), J::Bool(false)),
                ("type".into(), J::Str("init".to_string())),
            ])
            .to_go()
        };
        ws.write_text(&init_msg)
            .await
            .map_err(|e| PumpErr::Fatal(VibexError::new(format!("WS 发送 init 失败: {e}"), 502, "")))?;

        // (2) 等 ready
        let ready_deadline = crate::pool::now_ms() + self.cfg.ready_timeout_ms();
        loop {
            let remain = ready_deadline - crate::pool::now_ms();
            if remain <= 0 {
                return Err(PumpErr::Fatal(VibexError::new(
                    format!("WS 未在 {:.0}s 内就绪", self.cfg.ready_timeout_ms() as f64 / 1000.0),
                    504,
                    "",
                )));
            }
            let msg = match ws.read_message(remain.min(WS_SLICE_MS)).await {
                Ok(m) => m,
                Err(e) if e == WS_READ_TIMEOUT => continue,
                Err(e) if e == WS_CLOSED => {
                    return Err(PumpErr::Fatal(VibexError::new("WS 在就绪前关闭", 502, "")))
                }
                Err(e) => {
                    return Err(PumpErr::Fatal(VibexError::new(
                        format!("WS 读取失败: {e}"),
                        502,
                        "",
                    )))
                }
            };
            let m = match crate::util::json_unmarshal(msg.as_bytes()) {
                Value::Object(m) => m,
                _ => continue,
            };
            match str_of(m.get("type")).as_str() {
                "ready" => break,
                "error" => {
                    return Err(PumpErr::Fatal(VibexError::new(
                        format!(
                            "连接失败: {}",
                            first_non_empty(&[
                                &str_of(m.get("message")),
                                &str_of(m.get("code")),
                                "unknown"
                            ])
                        ),
                        502,
                        "",
                    )))
                }
                _ => {}
            }
        }

        // (3) ensure-llm-settings（失败忽略）
        self.client.ensure_llm_settings(app_id).await;
        // (4) new_session（fresh 时）
        if req.fresh_session {
            ws.write_text(r#"{"type":"new_session"}"#)
                .await
                .map_err(|e| {
                    PumpErr::Fatal(VibexError::new(
                        format!("WS 发送 new_session 失败: {e}"),
                        502,
                        "",
                    ))
                })?;
        }
        // (5) prompt
        let prompt_msg = J::Obj(vec![
            ("permission_mode".into(), J::Str("dontAsk".to_string())),
            ("text".into(), J::Str(req.prompt.clone())),
            ("type".into(), J::Str("prompt".to_string())),
        ])
        .to_go();
        ws.write_text(&prompt_msg).await.map_err(|e| {
            PumpErr::Fatal(VibexError::new(format!("WS 发送 prompt 失败: {e}"), 502, ""))
        })?;

        // (6) 事件泵
        let deadline = crate::pool::now_ms() + self.cfg.turn_timeout_ms();
        loop {
            if cancel.is_cancelled() {
                return Err(PumpErr::Fatal(VibexError::new("已取消", 499, "")));
            }
            let remain = deadline - crate::pool::now_ms();
            if remain <= 0 {
                let _ = ws.write_text(r#"{"type":"cancel"}"#).await;
                emit(
                    tx,
                    cancel,
                    OutEvent::new("note", "已达到单轮超时上限, 已发送 cancel"),
                )
                .await?;
                let mut e = OutEvent::new("done", "");
                e.flags = Some(crate::util::json_unmarshal(br#"{"timeout":true}"#));
                emit(tx, cancel, e).await?;
                return Ok(());
            }
            let msg = match ws.read_message(remain.min(WS_SLICE_MS)).await {
                Ok(m) => m,
                Err(e) if e == WS_READ_TIMEOUT => continue,
                Err(e) => {
                    if cancel.is_cancelled() {
                        return Err(PumpErr::Fatal(VibexError::new("已取消", 499, "")));
                    }
                    let _ = e;
                    emit(tx, cancel, OutEvent::new("note", "连接关闭")).await?;
                    let mut ev = OutEvent::new("done", "");
                    ev.flags = Some(crate::util::json_unmarshal(br#"{"ws_closed":true}"#));
                    emit(tx, cancel, ev).await?;
                    return Ok(());
                }
            };
            let m = match crate::util::json_unmarshal(msg.as_bytes()) {
                Value::Object(m) => m,
                _ => continue,
            };
            match str_of(m.get("type")).as_str() {
                "claude_event" => {
                    // Go 侧返回 nil slice = 本轮不投任何事件（与空切片同效）
                    if let Some(events) =
                        claude_events(m.get("event").unwrap_or(&Value::Null), self.cfg.expose_tools)
                    {
                        for ev in events {
                            emit(tx, cancel, ev).await?;
                        }
                    }
                }
                "done" => {
                    let mut flags = Map::new();
                    for k in [
                        "context_overflow",
                        "session_poison",
                        "balance_insufficient",
                        "rate_limit_failover",
                        "failover_model",
                        "primary_model",
                    ] {
                        flags.insert(k.to_string(), m.get(k).cloned().unwrap_or(Value::Null));
                    }
                    let mut ev = OutEvent::new("done", "");
                    ev.flags = Some(Value::Object(flags));
                    emit(tx, cancel, ev).await?;
                    return Ok(());
                }
                "error" => {
                    // ready 之后的 error = 轮内错误，记下继续泵
                    let text = first_non_empty(&[
                        &str_of(m.get("message")),
                        &str_of(m.get("code")),
                        "unknown error",
                    ]);
                    emit(tx, cancel, OutEvent::new("error", &text)).await?;
                }
                _ => {}
            }
        }
    }
}

/// `pumpWithToken` 的错误：握手级（可换号）或致命。
enum PumpErr {
    Ws(crate::client::WsHandshakeError),
    Fatal(VibexError),
}

fn set_err(state: &Arc<Mutex<String>>, msg: &str) {
    if msg.is_empty() {
        return;
    }
    let mut s = state.lock().unwrap();
    if s.is_empty() {
        *s = msg.to_string();
    }
}

async fn emit(
    tx: &mpsc::Sender<OutEvent>,
    cancel: &Cancel,
    ev: OutEvent,
) -> Result<(), PumpErr> {
    tokio::select! {
        r = tx.send(ev) => {
            if r.is_err() {
                return Err(PumpErr::Fatal(VibexError::new("消费方已退出", 499, "")));
            }
            Ok(())
        }
        _ = cancel.cancelled() => Err(PumpErr::Fatal(VibexError::new("已取消", 499, ""))),
    }
}

/// `matchProvider`：先按 id 精确匹配，再按 display_name/label/name。
pub fn match_provider(providers: &[Value], want: &str) -> Option<Value> {
    for raw in providers {
        if let Some(m) = raw.as_object() {
            if provider_id(raw) == want {
                return Some(raw.clone());
            }
            let _ = m;
        }
    }
    for raw in providers {
        let m = match raw.as_object() {
            Some(m) => m,
            None => continue,
        };
        for k in ["display_name", "label", "name"] {
            if !want.is_empty() && want == str_of(m.get(k)) {
                return Some(raw.clone());
            }
        }
    }
    None
}

/// `providerID`：`id` 优先，其次 `provider_id`。
pub fn provider_id(p: &Value) -> String {
    let m = match p.as_object() {
        Some(m) => m,
        None => return String::new(),
    };
    first_non_empty(&[&str_of(m.get("id")), &str_of(m.get("provider_id"))])
}

/// 便捷入口：状态里当前错误。
pub fn state_err(state: &Arc<Mutex<String>>) -> String {
    state.lock().unwrap().clone()
}

/// 供 `pump` 判断用的「项目是否 running 且端口就绪」（与 client 内部同口径）。
pub fn app_is_ready(app: &Value) -> bool {
    let m = match app.as_object() {
        Some(m) => m,
        None => return false,
    };
    live_status(Some(m)) == "running" && ports_ok(m)
}

use std::panic::AssertUnwindSafe;

/// 便于日志：把 provider 对象的关键字段描述出来（不含 token）。
pub fn describe_provider(p: &Value) -> String {
    match p.as_object() {
        Some(m) => format!(
            "id={} tier={}",
            str_of(m.get("id")),
            to_map(m.get("billing_tier").unwrap_or(&Value::Null))
                .map(|_| "<obj>".to_string())
                .unwrap_or_else(|| str_of(m.get("billing_tier")))
        ),
        None => "<nil>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_table_rules() {
        let mut t = SessionTable::new();
        assert_eq!(t.begin("", 3, 0), (0, true), "无 id → 新会话");
        assert_eq!(t.begin("s1", 3, 0), (0, true), "未知 id → 新会话");
        t.commit("s1", 3, 0);
        assert_eq!(t.begin("s1", 4, 100), (3, false), "有增量 → 只发增量");
        assert_eq!(t.begin("s1", 3, 100), (0, true), "无增量 → 全量重发当新会话");
        assert_eq!(
            t.begin("s1", 4, SESSION_IDLE_TTL_MS + 1),
            (0, true),
            "过期 → 新会话"
        );
    }

    #[test]
    fn match_provider_id_then_names() {
        let providers = vec![
            serde_json::json!({"id": "lite-1", "display_name": "Lite One"}),
            serde_json::json!({"id": "pro-1", "label": "Pro"}),
        ];
        assert_eq!(provider_id(&match_provider(&providers, "lite-1").unwrap()), "lite-1");
        assert_eq!(provider_id(&match_provider(&providers, "Lite One").unwrap()), "lite-1");
        assert_eq!(provider_id(&match_provider(&providers, "Pro").unwrap()), "pro-1");
        assert!(match_provider(&providers, "nope").is_none());
    }
}
