//! P8b 第四片门禁：`handleChat` 的池路径与单账号路径（`internal/tuanjie/server.go:938-1430`）。
//!
//! ## 纪律（简报硬性）
//!
//! * **只**用合成账号：`HOME`/`USERPROFILE` 指向临时目录（合成 JWT），
//!   `PROXYDECK_DATA_DIR` 指向临时数据目录；真实 `tuanjie-accounts.json*` 与真实 JWT
//!   **一个字节都不读进测试**（真实账号池文件在 `%APPDATA%\ProxyDeck\`，本测试的
//!   数据目录被显式改指，且进程不碰 8788）。
//! * 假上游只监听 `127.0.0.1` 的临时端口；**确定性序列**由
//!   [`scenario::FakeUpstream::set_chat_script`] 逐次给定（不是随机、不是真上游）。
//! * 进程级环境变量（`USERPROFILE`/`PROXYDECK_DATA_DIR`）**必须串行**：本文件用一条
//!   同步 `Mutex` 把「改环境 + 跑请求」整段罩住，**并在锁内自建 current-thread 运行时**
//!   （`#[tokio::test]` 的 `test-threads` 并行 / 跨 await 丢锁都会让两个用例抢环境——
//!   本片第一次就这么跑，10 个用例全挂死；这是本 crate 的 `env_lock` 惯例要补的那条）。
//!
//! 用例分四类：**确定性状态码序列**（429/402/502/400/200 空体，走假上游）、
//! **单账号路径**（清空池后走无池分支）、**纯函数对表**（与 Go 的判据逐条对齐）、
//! **请求体上限**（35 MiB 体实测未被截断到 32 MiB）。

#[path = "p8b3_scenario/mod.rs"]
mod scenario;

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use rs_tuanjie::server::{router, AppState};

/// 进程级环境变量的串行锁（**持锁期间不得跨 await 丢锁**：见文件头注释）。
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 在锁内建一个 current-thread 运行时跑完整个用例（请求全程不释放锁）。
fn run_locked<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 current-thread 运行时");
    let out = rt.block_on(f());
    drop(guard);
    out
}

/// 本文件共用的假上游：`client::set_test_bases` 是 `OnceLock`（进程内首次生效），
/// 所以所有用例必须打**同一个**假上游；各用例用不同的合成 key/脚本来隔离。
fn shared_upstream() -> &'static scenario::FakeUpstream {
    static UP: OnceLock<scenario::FakeUpstream> = OnceLock::new();
    UP.get_or_init(|| scenario::FakeUpstream::start(vec![]))
}

/// 造一次隔离运行：临时根目录 + 合成登录态 + 合成账号池 + `AppState`。
struct Run {
    st: Arc<AppState>,
    root: std::path::PathBuf,
    tokens: Vec<(String, String)>,
    /// 用例标签（token 的盐 / 假上游 key 的登记名；见 [`Run::new`]）。留着便于失败现场定位。
    #[allow(dead_code)]
    tag: String,
}

impl Run {
    /// `accounts`：本条用例要在 `tuanjie-accounts.json` 里落几个号（名字即 `user_id`）；
    /// 第一个号同时是**本地号**（`oauth_creds.json` 的 `sub` 与它一致，保证池里不出现
    /// 身份不明的额外成员）。
    ///
    /// **token 带盐**（`salt = tag`）：同一个 `user_id` 在不同用例里必须得到不同的 token
    /// 字符串——假上游按 token 分辨账号，撞串就等于两个用例互打对方的脚本。
    fn new(tag: &str, accounts: &[&str]) -> Run {
        let root = std::env::temp_dir().join(format!(
            "rs-tuanjie-p8b4-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时根目录");
        let env = scenario::Env::create_with_salt(&root, accounts[0], tag);
        env.apply();

        let mut tokens: Vec<(String, String)> = Vec::new();
        let mut entries: Vec<String> = Vec::new();
        for (i, id) in accounts.iter().enumerate() {
            let token = scenario::synthetic_jwt_for(id, tag);
            let source = if i == 0 { "local" } else { "pool" };
            // 本地号在池里 token 恒空（实时读登录态）；池号带快照。
            let at = if i == 0 { "" } else { token.as_str() };
            entries.push(format!(
                "{{\"user_id\":\"{id}\",\"access_token\":\"{at}\",\"enabled\":true,\
                  \"has_glm53\":true,\"source\":\"{source}\"}}"
            ));
            tokens.push((id.to_string(), token));
        }
        std::fs::write(
            root.join("data").join("tuanjie-accounts.json"),
            format!("{{\"accounts\":[{}]}}", entries.join(",")),
        )
        .expect("写合成账号池");

        let up = shared_upstream();
        for (id, token) in &tokens {
            up.set_token_key(token, &format!("sk-key-{id}"));
        }
        rs_tuanjie::client::set_test_bases(up.base(), up.base());
        let st = Arc::new(AppState::new().expect("构造 AppState"));
        Run {
            st,
            root,
            tokens,
            tag: tag.to_string(),
        }
    }

    fn key_of(&self, uid: &str) -> String {
        // 与 Go 采集器的 key 命名**逐字同源**（`sk-key-<uid>`）：token 本身已带盐、按 key
        // 不会跨用例相撞，而差分对照要的正是「同一个 key 名，两侧比命中数」。
        format!("sk-key-{uid}")
    }

    /// 取某个账号的合成 access_token（断言出站 `Authorization` 用）。
    #[allow(dead_code)]
    fn token_of(&self, uid: &str) -> String {
        self.tokens
            .iter()
            .find(|(id, _)| id == uid)
            .map(|(_, t)| t.clone())
            .expect("用例账号必须存在")
    }

    /// 把池清空 ⇒ 下次请求走**单账号路径**（`st.pool.size() == 0` 的分支）。
    ///
    /// 为什么需要它：本地号一进池就算「有池」，于是只有一个号的部署其实走**池路径**
    /// （Go 同：`accountTokenFor` 只看 `pool.Size() != 0`）。要测单账号路径就得让池真的空。
    fn force_single_account(&self) {
        for id in self.tokens.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>() {
            assert!(self.st.pool.remove(&id), "清池失败：{id}");
        }
        assert_eq!(self.st.pool.size(), 0, "池应已清空");
    }

    /// 打开 pacing 开关（落盘走临时数据目录，不碰真实 `%APPDATA%`）。
    fn enable_pacing(&self) {
        self.st.pacer.set_enabled(true);
    }

    fn chat_timing(&self) -> &rs_tuanjie::chat::ChatTiming {
        &self.st.chat_timing
    }

    /// 把 429 兜底的**总预算**压到 2 秒（生产 30 分钟）。
    ///
    /// 为什么需要这个缝：Go 侧没有它，它靠**取消请求 ctx** 让第二层 `select` 立刻退出
    /// （`pacing_pool_test.go:122-137`）。Rust 的 `handle_chat` 是本进程内直接 await，
    /// 没有等价的中断点，所以要让「预算耗尽 ⇒ 循环退出」这条分支在秒级门禁里可走到，
    /// 只能把预算本身调小。**生产构造恒为 30 分钟**（`ChatTiming::default`）。
    fn short_pacing_budget(&self) {
        self.chat_timing()
            .set_pacing_max_wait_for_test(std::time::Duration::from_secs(2));
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn post_chat(st: Arc<AppState>, body: Vec<u8>) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("构造请求");
    let resp = router(st).oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), 1 << 24)
        .await
        .expect("读响应体");
    (status, String::from_utf8_lossy(&b).into_owned())
}

fn ok_body(model: &str) -> String {
    format!(
        "{{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"model\":\"{model}\",\
          \"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"hi\"}},\
          \"finish_reason\":\"stop\"}}],\
          \"usage\":{{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}}}"
    )
}

const KIMI_REQ: &str = r#"{"model":"KIMI-K3","messages":[{"role":"user","content":"hi"}]}"#;

// ---------------------------------------------------------------- 入站：非法 UTF-8 本地 400

/// 非法 UTF-8 请求体：**本地 400**，一个字节都不打上游（`server.go:924-932`）。
#[test]
fn invalid_utf8_body_is_rejected_locally_without_touching_upstream() {
    run_locked(|| async {
        let run = Run::new("utf8", &["uid-a"]);
        let up = shared_upstream();
        let before = up.chat_hits();

        // 「你好」的 GBK 字节（坑 14 的实测复现形态）：C4 E3 BA C3
        let base = KIMI_REQ.as_bytes();
        let pos = base
            .windows(3)
            .position(|w| w == b"hi\"")
            .expect("请求体里有 hi");
        let body = [
            &base[..pos],
            &[0xC4u8, 0xE3, 0xBA, 0xC3],
            &base[pos + 2..],
        ]
        .concat();

        let (status, text) = post_chat(run.st.clone(), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "非法 UTF-8 必须本地 400");
        assert_eq!(
            up.chat_hits(),
            before,
            "本地拒绝的场景**绝不许**打上游（一次都不行）"
        );
        let v: serde_json::Value = serde_json::from_str(&text).expect("400 体是合法 JSON");
        assert_eq!(
            v["error"]["message"],
            serde_json::Value::String(rs_tuanjie::chat::INVALID_UTF8_BODY_MESSAGE.to_string()),
            "文案要与 Go 的 invalidUTF8BodyMessage 逐字一致"
        );
        assert_eq!(v["error"]["type"], "api_error");
        let events = run.st.activity.list(5);
        assert!(
            events.iter().any(|e| e.status == 400 && e.kind == "error"),
            "本地拒绝要写一条 400 的 error 事件：{events:?}"
        );
    })
}

// ---------------------------------------------------------------- 池路径 200

/// 池路径 200：按号换 key（**每个号一把**），响应整块透传，usage 入账。
#[test]
fn pool_path_200_uses_the_picked_account_key_and_passes_body_through() {
    run_locked(|| async {
        let run = Run::new("pool200", &["uid-a"]);
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        up.set_chat_script(&key_a, vec![(200, "", &ok_body("KIMI-K3"))]);

        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(text, ok_body("KIMI-K3"), "池路径 200 原样透传上游体");
        assert_eq!(up.chat_calls(&key_a), 1, "恰好一次上游调用");
        // 共用假上游：只取本用例这把 key 的命中（别的用例的调用被过滤掉）
        assert_eq!(
            up.chat_order().iter().filter(|k| **k == key_a).count(),
            1,
            "只用被选中那把 key：{:?}",
            up.chat_order()
        );
        // 出站体是**重排后**的（红线 4/8）：session id 三处同值
        let sent = String::from_utf8_lossy(&up.last_chat_body()).into_owned();
        let v: serde_json::Value = serde_json::from_str(&sent).expect("出站体是合法 JSON");
        let sid = v["litellm_session_id"]
            .as_str()
            .expect("有 litellm_session_id");
        assert_eq!(v["prompt_cache_key"].as_str(), Some(sid));
        assert!(!sid.is_empty());
        // usage 入账（池路径非流式含 usage 才记）
        let stats = run.st.stats.snapshot();
        let st_kimi = stats.get("KIMI-K3").expect("KIMI-K3 应入账");
        assert_eq!(st_kimi.calls, 1);
        assert_eq!(st_kimi.total_tokens, 10);
    })
}

// ---------------------------------------------------------------- 429 即时换号（有界 ≤3）

/// 429 即时换号：两号互相 429 之后**换到第三个健康号**、上游恰好 4 次、
/// 换前退避生效（`server.go:1130-1160`；对应 Go `TestPacingPoolSwitchBackoffAndLimit`）。
#[test]
fn pool_429_switches_three_times_at_most_and_recovers_on_a_healthy_account() {
    run_locked(|| async {
        let run = Run::new("switch", &["uid-a", "uid-b", "uid-c"]);
        run.enable_pacing();
        run.short_pacing_budget();
        // Go 侧同款注入（`pacing_pool_test.go:42` 是 120ms）：3 次换号真等 1s 太久。
        let injected = std::time::Duration::from_millis(120);
        run.chat_timing().set_switch_backoff_for_test(injected);

        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        let key_b = run.key_of("uid-b");
        let key_c = run.key_of("uid-c");
        let before_events = run.st.activity.list(200).len();
        up.set_chat_script(
            &key_a,
            vec![(429, "1", r#"{"error":{"message":"rate limited"}}"#)],
        );
        up.set_chat_script(
            &key_b,
            vec![(429, "1", r#"{"error":{"message":"rate limited"}}"#)],
        );
        up.set_chat_script(&key_c, vec![(200, "", &ok_body("KIMI-K3"))]);

        let t0 = std::time::Instant::now();
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        let elapsed = t0.elapsed();

        assert_eq!(status, StatusCode::OK, "换到健康号后应 200");
        assert_eq!(text, ok_body("KIMI-K3"));
        // 共用假上游：只数本用例三个号的命中（别的用例的调用被过滤掉）
        let mine: Vec<String> = up
            .chat_order()
            .into_iter()
            .filter(|k| *k == key_a || *k == key_b || *k == key_c)
            .collect();
        assert_eq!(
            mine, vec![key_a, key_b, key_c],
            "首次 A + 换 B + 换 C = 3 次上游访问，实得 {mine:?}"
        );
        // 换号退避生效：两次换号各等 ≥注入值（留时钟粒度容差）
        let floor = injected - std::time::Duration::from_millis(40);
        assert!(
            elapsed >= floor * 2,
            "两次换号退避应至少 {floor:?}×2，实测耗时 {elapsed:?}"
        );
        // 事件里能看到换号 1/3、2/3
        let events = run.st.activity.list(200);
        let fresh = &events[..events.len().saturating_sub(before_events)];
        let msgs: Vec<String> = fresh.iter().map(|e| e.message.clone()).collect();
        assert_eq!(
            msgs.iter().filter(|m| m.contains("429 换号")).count(),
            2,
            "两次换号各一条事件：{msgs:?}"
        );
    })
}

/// 429 换号**上限 3 次**：两号互撞 + 第三号仍 429 ⇒ 换号止于 3 次、上游 4 次、
/// 第二层等一次撞上预算即收手，最后如实透传 429（`server.go:1130-1183`）。
#[test]
fn pool_429_switch_budget_stops_at_three_and_the_budget_stops_the_wait_layer() {
    run_locked(|| async {
        let run = Run::new("switchcap", &["uid-a", "uid-b", "uid-solo"]);
        run.enable_pacing();
        run.short_pacing_budget();
        run.chat_timing()
            .set_switch_backoff_for_test(std::time::Duration::from_millis(5));
        let up = shared_upstream();
        let keys: Vec<String> = ["uid-a", "uid-b", "uid-solo"]
            .iter()
            .map(|id| run.key_of(id))
            .collect();
        for k in &keys {
            up.set_chat_script(
                k,
                vec![(429, "1", r#"{"error":{"message":"rate limited"}}"#)],
            );
        }
        let before_events = run.st.activity.list(200).len();
        let t0 = std::time::Instant::now();
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        let elapsed = t0.elapsed();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "如实透传 429");
        // 全池 429 之后池里的号都被 `BudgetExceeded` 无关（429 只走冷却/轮转），
        // 但**本轮循环结束时选号会退化为 uid-a**，而 uid-a 是本地号 ⇒ 池路径的非 200
        // 透传用的是 `writeErr(429, 上游体)`。Go 侧同一：`server.go:1194` 的
        // `writeErr(w, resp.StatusCode, string(poolErrBody))`。
        let v: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("rate limited"),
            "429 文案要带上游原文：{text:?}"
        );
        let mine: Vec<String> = up
            .chat_order()
            .into_iter()
            .filter(|k| keys.contains(k))
            .collect();
        assert!(
            (5..=6).contains(&mine.len()),
            "首次 1 + 换号 3 + 第二层 1~2（2s 预算 / Retry-After 1s）实得 {mine:?}"
        );
        // 逐号看：循环有界（换号最多把某个号打到 2 次，第二层再补 ≤2 次 ⇒ 上限 4）
        for k in &keys {
            assert!(
                up.chat_calls(k) <= 4,
                "{k} 被访问超过 4 次（换号预算没兜住）"
            );
        }
        // 预算耗尽：绝不会跑满 30 分钟（这里最长 2s 预算 + 余量）
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "预算 2s 下不该耗太久，实测 {elapsed:?}"
        );
        let events = run.st.activity.list(200);
        let fresh = &events[..events.len().saturating_sub(before_events)];
        let msgs: Vec<String> = fresh.iter().map(|e| e.message.clone()).collect();
        assert_eq!(
            msgs.iter().filter(|m| m.contains("429 换号")).count(),
            3,
            "换号事件数必须恰为 3（上限）：{msgs:?}"
        );
        assert!(!msgs.iter().any(|m| m.contains("（4/3）")), "不得出现 4/3");
    })
}

/// **换号预算有界性**：池越大越能撞，但循环永远收敛（`switches < 3` 是硬闸门）。
///
/// 反向验证：把 `MAX_POOL_SWITCHES` 改成 `usize::MAX` 会让本用例在上游恒 429 时
/// 无限重发（3 次换号的上界断言立即失败，用例不收敛）。
#[test]
fn switch_budget_is_bounded_for_any_pool_size() {
    run_locked(|| async {
        for n in 1..=3usize {
            let ids: Vec<String> = (0..n).map(|i| format!("uid-n{i}")).collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let run = Run::new(&format!("bounded{n}"), &refs);
            run.enable_pacing();
            run.short_pacing_budget();
            run.chat_timing()
                .set_switch_backoff_for_test(std::time::Duration::from_millis(5));
            let up = shared_upstream();
            for id in &ids {
                up.set_chat_script(
                    &run.key_of(id),
                    vec![(429, "1", r#"{"error":{"message":"rate limited"}}"#)],
                );
            }
            let (status, _) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "池 {n}");
            // 共用假上游 + 逐用例独立的 key：命中数用绝对值（锁内只有本请求在打上游）。
            // **有界性**是本条要锁的：池再大也不会打穿。
            //   池 1 个号：无号可换（`PickWithToken` 返回同号）⇒ 只有首次 + 第二层若干次；
            //   池 ≥2：首次 1 + 换号 3 + 第二层若干次。
            // 第二层次数由 2s 预算 / `Retry-After: 1s` 决定（≥1、≤4）。
            let mine: usize = ids.iter().map(|id| up.chat_calls(&run.key_of(id))).sum();
            assert!(
                (3..=8).contains(&mine),
                "池 {n} 个号：上游访问次数应有界（3..=8），实得 {mine}"
            );
            // 逐号看：任何一个号都不该被连打超过 4 次（换号轮转 2 次 + 第二层 2 次）
            for id in &ids {
                assert!(
                    up.chat_calls(&run.key_of(id)) <= 4,
                    "池 {n}：{id} 被访问超过 4 次（换号预算没兜住）"
                );
            }
        }
    })
}

// ---------------------------------------------------------------- 429 第二层：Retry-After 排队

/// 第二层：全池撞墙后按 `Retry-After` 等原号重发，等到 200 即恢复
/// （`server.go:1161-1183`），并补一条「限流兜底完成」事件。
#[test]
fn pool_429_waits_retry_after_then_recovers_with_200() {
    run_locked(|| async {
        let run = Run::new("retryafter", &["uid-a"]);
        run.enable_pacing();
        run.chat_timing()
            .set_switch_backoff_for_test(std::time::Duration::from_millis(5));
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        // 第一次 429 且 `Retry-After: 1`（秒）→ 第二层等 1s → 重发（第二次）拿 200。
        up.set_chat_script(
            &key_a,
            vec![
                (429, "1", r#"{"error":{"message":"rate limited"}}"#),
                (200, "", &ok_body("KIMI-K3")),
            ],
        );

        let t0 = std::time::Instant::now();
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        let elapsed = t0.elapsed();
        assert_eq!(status, StatusCode::OK, "等到窗口后必须恢复 200");
        assert_eq!(text, ok_body("KIMI-K3"));
        assert_eq!(up.chat_calls(&key_a), 2, "429 后重发一次");
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "第二层应真等 Retry-After=1s，实测 {elapsed:?}"
        );
        let msgs: Vec<String> = run.st.activity.list(20).iter().map(|e| e.message.clone()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("429 全池限流，等待")),
            "要有第二层等待事件：{msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("限流兜底完成，重发成功")),
            "流式成功没有 ok 事件承载，兜底恢复要自发上报：{msgs:?}"
        );
    })
}

/// 单账号路径的 429 **只有等待层**（无池可换）：按 Retry-After 等完重发。
#[test]
fn single_account_429_only_waits_and_has_no_switch_layer() {
    run_locked(|| async {
        let run = Run::new("single429", &["uid-a"]);
        run.force_single_account();
        run.enable_pacing();
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        up.set_chat_script(
            &key_a,
            vec![
                (429, "1", r#"{"error":{"message":"rate limited"}}"#),
                (200, "", &ok_body("KIMI-K3")),
            ],
        );

        let t0 = std::time::Instant::now();
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        let elapsed = t0.elapsed();
        assert_eq!(status, StatusCode::OK, "单账号路径等窗口后恢复");
        assert_eq!(text, ok_body("KIMI-K3"));
        assert_eq!(up.chat_calls(&key_a), 2, "一次原号重发");
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "等待层真等 Retry-After=1s，实测 {elapsed:?}"
        );
        let msgs: Vec<String> = run.st.activity.list(20).iter().map(|e| e.message.clone()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("429 限流，等待")),
            "单账号路径只应有等待层事件：{msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| m.contains("429 换号")),
            "单账号路径**不许**出现换号事件：{msgs:?}"
        );
    })
}

// ---------------------------------------------------------------- 402 换号 + 耗尽换号

/// 402 配额耗尽：**换下一个号重发**，并且该号被标记预算超额（`server.go:1071-1097`）。
///
/// 这是团结侧与 14018 同族的「额度耗尽」分支（Go 侧无 14018 业务码；见 README 登记）。
#[test]
fn pool_402_swaps_account_and_marks_budget_exceeded() {
    run_locked(|| async {
        let run = Run::new("402", &["uid-a", "uid-b"]);
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        let key_b = run.key_of("uid-b");
        // 402 之后 Go 会异步查官网 5h 窗口拿重置时刻——假上游的计费口未登记 ⇒ 404
        // （查询失败路径），于是零值 = 不参与自动恢复，与 Go 的失败分支一致。
        up.set_chat_script(
            &key_a,
            vec![(402, "", r#"{"error":{"message":"budget_exceeded"}}"#)],
        );
        up.set_chat_script(&key_b, vec![(200, "", &ok_body("KIMI-K3"))]);

        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::OK, "换号后应成功");
        assert_eq!(text, ok_body("KIMI-K3"));
        assert_eq!(up.chat_calls(&key_a), 1, "402 的号只被打一次（然后被禁用）");
        assert_eq!(up.chat_calls(&key_b), 1, "换到 B 号重发一次");

        // 402 标记是**落盘**的（Go `MarkBudgetExceededAt` 内就 saveLocked）——异步任务里做，
        // 故轮询等它落地（最多 3s）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !run
            .st
            .pool
            .get("uid-a")
            .map(|a| a.budget_exceeded)
            .unwrap_or(false)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "402 后该号必须被 MarkBudgetExceeded"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        for _ in 0..3 {
            let picked = run.st.pool.pick("KIMI-K3").expect("B 号仍可用");
            assert_eq!(picked.user_id, "uid-b", "被 402 禁用的号不再被选");
        }
    })
}

/// 全池 402：池里没人可换时，**如实透传 402**（不伪造、不无限循环）。
#[test]
fn all_accounts_402_passes_the_error_through_without_looping() {
    run_locked(|| async {
        let run = Run::new("402all", &["uid-a", "uid-b"]);
        let up = shared_upstream();
        for id in ["uid-a", "uid-b"] {
            up.set_chat_script(
                &run.key_of(id),
                vec![(402, "", r#"{"error":{"message":"budget_exceeded"}}"#)],
            );
        }
        let before = up.chat_hits();
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "全池 402 如实透传");
        // 两个号都被 402 禁掉后，**池路径**已无号可换、如实透传上游报文；而本用例里的
        // uid-a 同时是**本地号** ⇒ 走单账号路径的 402 包装（可读中文 + 上游原文尾部）。
        // 断言按实际分支写死，别把「本地账号包装」当成上游原文。
        let v: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("budget_exceeded"),
            "402 文案要带上游原文：{text:?}"
        );
        let mine: usize = ["uid-a", "uid-b"]
            .iter()
            .map(|id| up.chat_calls(&run.key_of(id)))
            .sum();
        assert_eq!(mine, 2, "池里两个号各恰好一次，实得 {mine}");
        let _ = before;
    })
}

// ---------------------------------------------------------------- 单账号 402 标记链 + 选号回退（P8b 审查 P2/P3）

/// P8b 审查 P2 的门禁锁：单账号路径 402 必须走异步标记链（取 token 失败→零值标记；
/// 成功→查 5h 窗口→带时刻标记），而不仅是改文案/记事件。
///
/// 构造：本地号先标死（pick 选不出 → 回退单账号路径，但 `local_account()` 仍在，
/// 标记链找得到它）；假上游回 402；断言 `budget_exceeded` 最终落地。
/// 计费口未登记 ⇒ 查询失败 ⇒ 零值标记（与 Go 的失败分支一致）。
#[test]
fn single_path_402_marks_local_account_via_async_chain() {
    run_locked(|| async {
        let run = Run::new("single402mark", &["uid-a"]);
        run.st.pool.mark_dead("uid-a");
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        up.set_chat_script(
            &key_a,
            vec![(402, "", r#"{"error":{"message":"budget_exceeded"}}"#)],
        );
        let (status, _text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        // 异步标记链落地（最多等 3s，与池路径 402 用例同口径）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !run
            .st
            .pool
            .get("uid-a")
            .map(|a| a.budget_exceeded)
            .unwrap_or(false)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "单账号 402 后本地号必须被标记 budget_exceeded"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
}

/// P8b 审查 P3 的门禁锁：池非空但当场选不出号时**回退单账号路径**，
/// 而不是回"账号池无可用账号"（Go `accountTokenFor` 语义；该文案只留给池内竞态）。
#[test]
fn dead_pool_falls_back_to_single_path() {
    run_locked(|| async {
        let run = Run::new("fallback", &["uid-a", "uid-b"]);
        run.st.pool.mark_dead("uid-a");
        run.st.pool.mark_dead("uid-b");
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        up.set_chat_script(&key_a, vec![(200, "", &ok_body("KIMI-K3"))]);
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::OK, "全死池应回退单账号路径并成功");
        assert_eq!(text, ok_body("KIMI-K3"));
    })
}

// ---------------------------------------------------------------- 200 空响应重放

/// 200 空响应：**重放一次**（单账号路径的 `ensureNonEmpty`），第二次有内容即透传。
#[test]
fn single_account_empty_200_is_replayed_once() {
    run_locked(|| async {
        let run = Run::new("empty", &["uid-a"]);
        run.force_single_account();
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        let empty = r#"{"id":"x","object":"chat.completion","model":"KIMI-K3","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}"#;
        up.set_chat_script(
            &key_a,
            vec![(200, "", empty), (200, "", &ok_body("KIMI-K3"))],
        );

        let t0 = std::time::Instant::now();
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::OK, "重放后拿到内容");
        assert_eq!(text, ok_body("KIMI-K3"));
        assert_eq!(up.chat_calls(&key_a), 2, "空响应触发恰好一次重放");
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(1400),
            "空响应重放前应等 1500ms（Go `server.go:2119`），实测 {:?}",
            t0.elapsed()
        );
    })
}

/// 恒空响应：重放一次仍空 → 502 + Go 的原话（`server.go:1373`）。
#[test]
fn single_account_always_empty_returns_502_with_go_message() {
    run_locked(|| async {
        let run = Run::new("empty2", &["uid-a"]);
        run.force_single_account();
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        let empty = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":""}}]}"#;
        up.set_chat_script(&key_a, vec![(200, "", empty)]);

        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let v: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        assert_eq!(
            v["error"]["message"],
            serde_json::Value::String(rs_tuanjie::chat::EMPTY_UPSTREAM_MESSAGE.to_string())
        );
        assert_eq!(
            up.chat_calls(&key_a),
            3,
            "首发 1 + 嗅探循环里的两次重放 = 3（Go `ensureNonEmpty` 是 `attempt < 2`）"
        );
    })
}

/// 单账号路径的上游错误体也必须人性化。
///
/// 2026-10-18 收尾任务①的**第二个调用点**：池路径先修，单账号路径后补——
/// 原来 `finish_non_200` 把上游 body 整串塞进我们的 `error.message`，客户端拿到
/// `{"error":{"message":"{\"error\":{\"message\":\"…\"}}"}}` 这种转义 JSON。
/// 单账号又是多数用户的默认路径，所以必须单独钉一条。
///
/// 同时守住：**判定告警仍按原始 body 计数**（人性化只影响回给客户端的文案，
/// 不能把 judge 的模式匹配喂成半截人话）。
#[test]
fn single_account_upstream_json_error_body_is_humanized() {
    run_locked(|| async {
        let run = Run::new("humanize-single", &["uid-a"]);
        run.force_single_account();
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        let upstream_body =
            r#"{"error":{"message":"team not allowed to access model","type":"auth_error"}}"#;
        up.set_chat_script(&key_a, vec![(400, "", upstream_body)]);

        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "上游 400 原样透传状态码");

        let v: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        assert_eq!(
            v["error"]["message"], "team not allowed to access model",
            "单账号路径必须抽出内层人话"
        );
        assert!(
            !text.contains("\\\"error\\\""),
            "响应里不应再出现转义的 error 片段：{text}"
        );
    })
}

// ---------------------------------------------------------------- 瞬时错误重发（≤3）

/// 上游 5xx（可重试报文）：**清 key 缓存 + 800ms + 重发**，上限 3 次
/// （`server.go:1099-1120` 池路径 / `:1299-1314` 单账号）。
#[test]
fn transient_5xx_is_retried_up_to_three_times_then_passed_through() {
    run_locked(|| async {
        let run = Run::new("retry5xx", &["uid-a"]);
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        up.set_chat_script(
            &key_a,
            vec![(502, "", r#"{"error":{"message":"bad gateway"}}"#)],
        );

        let t0 = std::time::Instant::now();
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        let elapsed = t0.elapsed();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            text,
            // 池路径非 200 ⇒ `write_err(reply.status, humanize_upstream_error(body))`：
            // 上游 `{"error":{"message":"bad gateway"}}` 抽出内层人话，不再整串转义
            // （2026-10-18 收尾任务①）。
            "{\"error\":{\"message\":\"bad gateway\",\"type\":\"api_error\"}}\n"
        );
        assert_eq!(
            up.chat_calls(&key_a),
            4,
            "1 次首发 + 3 次重发 = 4（`attempt < 3`）"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(2400),
            "3 次重发各等 800ms（下界 2.4s），实测 {elapsed:?}"
        );
    })
}

/// 不可重试的错误（`invalid model name` 这类确定性 400）：**一次都不重发**。
#[test]
fn non_retriable_400_is_not_retried() {
    run_locked(|| async {
        let run = Run::new("noretry", &["uid-a"]);
        let up = shared_upstream();
        let key_a = run.key_of("uid-a");
        up.set_chat_script(
            &key_a,
            vec![(
                400,
                "",
                r#"{"error":{"message":"Invalid model name passed in model=None"}}"#,
            )],
        );
        let (status, text) = post_chat(run.st.clone(), KIMI_REQ.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // 池里只有 1 个号 ⇒ 单账号路径的 `writeErr` 包装形态
        let v: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("Invalid model name"),
            "错误体要带上游原文：{text:?}"
        );
        assert_eq!(
            up.chat_calls(&key_a),
            1,
            "确定性 400 不重发（Go `retriableUpstream` 的 invalid model name 分支）"
        );
    })
}

// ---------------------------------------------------------------- 请求体上限（32MB vs 64MB）

/// **请求体上限分歧的实测**：Go `handleChat` 用 `io.LimitReader(r.Body, 64<<20)`；
/// axum 的 `DefaultBodyLimit` 缺省只有 **32MB**。本用例发一个 **>32MB 但 <64MB** 的体：
/// * 若 axum 的 32MB 上限先生效 → 请求被截断 → 体内 JSON 解析失败 → `model` 回落 `"?"`
///   → 没有任何账号认领该模型 ⇒ 502「账号池无可用账号」（或 413）。
/// * 若按 Go 的 64MB 口径收全 → `model` 正常解析 → 命中假上游 → 200。
///
/// 用途：把「两侧的体上限是否同口径」这件事**测出来**，而不是靠读文档。
#[test]
fn request_body_above_32mb_but_below_64mb_is_not_truncated() {
    run_locked(|| async {
        let run = Run::new("bigbody", &["uid-a"]);
        let up = shared_upstream();
        // 语句路径要的是「模型能被认出来」：假上游对 `sk-key-uid-a` 恒回 200
        up.set_chat_script("sk-key-uid-a", vec![(200, "", &ok_body("KIMI-K3"))]);
        // 35 MiB 填充（>32MiB 的 axum 缺省上限，<64MiB 的 Go 上限），
        // **`model` 放在填充之后**——一被截断就解析不出模型名。
        let fill = "x".repeat(35 * 1024 * 1024);
        let payload = format!(
            "{{\"messages\":[{{\"role\":\"user\",\"content\":\"{fill}\"}}],\"model\":\"KIMI-K3\"}}"
        );
        assert!(payload.len() > 32 * 1024 * 1024, "体必须超过 32MiB");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑临时端口");
        let addr = listener.local_addr().expect("local_addr");
        let server = {
            let st = run.st.clone();
            tokio::spawn(async move {
                let _ = axum::serve(listener, router(st)).await;
            })
        };
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("建 HTTP 客户端");
        let resp = client
            .post(format!("http://{addr}/v1/chat/completions"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .expect("打本地服务");
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        server.abort();
        assert_eq!(
            status, 200,
            ">32MiB 且 <64MiB 的请求体应被完整读入（Go 的 64MiB 口径）；\
             实得 {status} body={}",
            &body[..body.len().min(200)]
        );
        assert_eq!(up.chat_calls("sk-key-uid-a"), 1, "完整体应被转发到我方账号");
    })
}

// ---------------------------------------------------------------- 纯函数门面（与 Go 对表）

#[test]
fn retriable_upstream_matches_the_go_table() {
    use rs_tuanjie::chat::retriable_upstream as r;
    for (body, want) in [
        ("", false),
        ("Invalid model name passed in model=None", false),
        ("INVALID MODEL NAME", false),
        ("model not mapped", true),
        ("rate limited", true),
        ("HTTP 429", true),
        ("502 Bad Gateway", true),
        ("503", true),
        ("504", true),
        ("bad gateway", true),
        ("internal server error", true),
        ("something else", false),
        ("额度耗尽", false),
    ] {
        assert_eq!(r(body), want, "retriableUpstream({body:?})");
    }
}

#[test]
fn sniff_helpers_match_the_go_predicates() {
    use rs_tuanjie::chat::{has_stream_data_line, non_stream_has_content, stream_has_content};
    assert!(has_stream_data_line(b"data: {}\n"));
    assert!(has_stream_data_line(b"event: x\ndata: {}\n"));
    assert!(!has_stream_data_line(b": keepalive\n"));

    assert!(stream_has_content(
        br#"data: {"choices":[{"delta":{"content":"a"}}]}"#
    ));
    assert!(stream_has_content(
        br#"data: {"choices":[{"delta":{"reasoning_content":"r"}}]}"#
    ));
    assert!(!stream_has_content(
        br#"data: {"choices":[{"delta":{"content":""}}]}"#
    ));
    assert!(!stream_has_content(b"data: [DONE]\n"));
    assert!(!stream_has_content(b"data: {not json}\n"));

    assert!(non_stream_has_content(
        br#"{"choices":[{"message":{"content":"x"}}]}"#
    ));
    assert!(non_stream_has_content(
        br#"{"choices":[{"message":{"reasoning_content":"x"}}]}"#
    ));
    assert!(non_stream_has_content(
        br#"{"choices":[{"message":{"tool_calls":[{"id":"1"}]}}]}"#
    ));
    assert!(!non_stream_has_content(
        br#"{"choices":[{"message":{"content":"","reasoning_content":""}}]}"#
    ));
    // Go `len(json.RawMessage) > 0`：键缺席才是 nil（0 字节）；显式 `null` 落成 4 字节
    // `"null"`、`[]` 落成 2 字节——两者都判有内容（真机实测 `encoding/json`，非推测）。
    assert!(non_stream_has_content(
        br#"{"choices":[{"message":{"content":"","tool_calls":null}}]}"#
    ));
    assert!(non_stream_has_content(
        br#"{"choices":[{"message":{"content":"","tool_calls":[]}}]}"#
    ));
    assert!(!non_stream_has_content(
        br#"{"choices":[{"message":{"content":""}}]}"#
    ));
    assert!(non_stream_has_content(b"not json but has bytes"));
    assert!(!non_stream_has_content(b"   "));
}

#[test]
fn parse_model_and_stream_is_strict_like_go_json_unmarshal() {
    use rs_tuanjie::chat::parse_model_and_stream as p;
    assert_eq!(
        p(br#"{"model":"KIMI-K3","stream":true}"#),
        ("KIMI-K3".to_string(), true)
    );
    assert_eq!(p(br#"{"model":"x"}"#), ("x".to_string(), false));
    assert_eq!(p(br#"{"model":""}"#), ("?".to_string(), false));
    assert_eq!(p(b"{}"), ("?".to_string(), false));
    assert_eq!(p(b"{not json"), ("?".to_string(), false));
    // Go `json.Unmarshal` 要求整串合法：尾随垃圾即失败
    assert_eq!(p(br#"{"model":"x"}x"#), ("?".to_string(), false));
}

#[test]
fn switch_budget_constant_is_three() {
    // 有界性的硬闸门：Go `server.go:1140` 的 `switches < 3`
    assert_eq!(rs_tuanjie::chat::MAX_POOL_SWITCHES, 3);
    assert_eq!(rs_tuanjie::chat::POOL_RETRY_ATTEMPTS, 3);
    assert_eq!(rs_tuanjie::chat::SINGLE_RETRY_ATTEMPTS, 3);
}

/// 合成的 `Account` 形态（保证 `accounts` 模块的池成员语义在门禁里可见）。
#[test]
fn synthetic_account_shape_is_pool_sourced() {
    let a = rs_tuanjie::accounts::Account {
        user_id: "uid-synthetic".into(),
        access_token: "tok".into(),
        source: "pool".into(),
        enabled: true,
        ..Default::default()
    };
    assert_eq!(a.source, "pool");
    assert!(a.enabled);
}

/// P8b 审查 P1 的门禁锁：池路径流式必须**逐块**转发（首字节时延远小于整包时延）。
///
/// 上游是本机裸 TCP（不用共享 FakeUpstream——它只会整包立即回）：发响应头 + 第一块，
/// 停 600ms，再发剩余并关连接。若实现是整块缓冲，首字节最早也要 600ms 后才到；
/// 逐块转发则首块在几十毫秒内到达。断言首块 < 400ms，且重组体逐字节等于预期。
/// （差分 `chat-stream-1` 只比组装后字节，证明不了"真在流"——这条才证明。）
#[test]
fn pool_streaming_forwards_chunk_by_chunk() {
    run_locked(|| async {
        // `run` 只留作清理守卫（Drop 清临时目录）与 token 盐；服务的 state 另建（见下）。
        let _run = Run::new("stream-timing", &["uid-s"]);
        let c1 = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n";
        let c2 = "data: {\"choices\":[{\"delta\":{\"content\":\"，世界\"}}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":6,\"total_tokens\":18}}\n\ndata: [DONE]\n\n";
        // 1) 裸 TCP SSE 上游：头 + 块1 → 600ms → 块2+DONE → 关连接。
        let ln = std::net::TcpListener::bind("127.0.0.1:0").expect("绑上游端口");
        let up_port = ln.local_addr().expect("读上游端口").port();
        let c1s = c1.to_string();
        let c2s = c2.to_string();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (sock, _) = ln.accept().expect("等上游连接");
            let mut r = std::io::BufReader::new(sock.try_clone().expect("克隆句柄"));
            // 读掉请求头（读到空行；小请求体在 TCP 缓冲里，不用等）。
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let mut sock = r.into_inner();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .expect("写响应头");
            sock.write_all(c1s.as_bytes()).expect("写块1");
            sock.flush().expect("刷块1");
            std::thread::sleep(std::time::Duration::from_millis(600));
            sock.write_all(c2s.as_bytes()).expect("写块2");
            // 关连接（drop 即 EOF）。
        });
        // 2) litellm 指到裸上游；codey（换 key）仍走共享假上游。
        // 注意顺序：`Client` 在构造时快照 base，故必须先覆写缝、**再**新建 AppState
        // （`Run::new` 里建的那个快照的是全共享假上游，用它 chat 会打到 404）。
        let shared = shared_upstream();
        rs_tuanjie::client::set_test_bases(
            shared.base(),
            format!("http://127.0.0.1:{up_port}"),
        );
        let st = Arc::new(rs_tuanjie::server::AppState::new().expect("重建 AppState"));
        // 3) 起真服务，打 stream 请求，计时首块。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑服务端口");
        let addr = listener.local_addr().expect("读服务端口");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(st)).await;
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("建 HTTP 客户端");
        let t0 = std::time::Instant::now();
        let resp = client
            .post(format!("http://{addr}/v1/chat/completions"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"model":"KIMI-K3","stream":true,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .expect("发请求");
        let status = resp.status().as_u16();
        assert_eq!(status, 200);
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let first = stream
            .next()
            .await
            .expect("首块必须到达")
            .expect("首块无错");
        let first_elapsed = t0.elapsed();
        assert!(
            first_elapsed < std::time::Duration::from_millis(400),
            "首块应在 400ms 内到达（整块缓冲要等 600ms 后的第二块），实耗 {first_elapsed:?}"
        );
        assert!(!first.is_empty(), "首块非空");
        // 4) 收完，重组体逐字节等于预期。
        let mut body = first.to_vec();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk.expect("后续块无错"));
        }
        assert_eq!(
            String::from_utf8_lossy(&body),
            format!("{c1}{c2}"),
            "重组体必须逐字节等于上游原文"
        );
    })
}

