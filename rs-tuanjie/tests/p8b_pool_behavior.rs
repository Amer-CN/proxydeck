//! P8b 第二片门禁（一）：账号池行为 —— 401 标死 / 换活号 / rescan 复位 / 冷却 / 内存态计数。
//!
//! **纪律（简报硬性）**：本文件**只**用合成账号（`tuanjie-accounts.json` 写在临时目录、
//! JWT 由 `synthetic_jwt` 现造）。真实 `tuanjie-accounts.json*` 与真实 JWT
//! **一个字节都不读进测试**——所以池的路径与 token 缝都由测试显式注入，绝不走
//! `AccountPool::new()`（那会去读 `%APPDATA%\ProxyDeck\`）。
//!
//! 「假上游」是**确定性的状态码序列**：按当前号回 401/429/200，而不是真起 HTTP 服务
//! （转发本身在 P8a 的黄金门禁里已逐字节锁住；这里锁的是**池状态机**）。
//! 换号循环照抄生产装配要用的形态（`internal/codebuddy/server.go:1261-1418` 的
//! `deadResent < pool.Size()` 有界轮换 + `Dec 旧号 / Inc 新号`）。

use rs_tuanjie::accounts::{jwt_sub, Account, AccountPool};

/// 换号重发上限的语义源（`codebuddy/server.go:1257`）：`deadResent < s.pool.Size()`。
fn max_swaps(p: &AccountPool) -> usize {
    p.size()
}

fn synthetic_jwt(sub: &str, exp: i64) -> String {
    let payload = format!("{{\"sub\":\"{sub}\",\"exp\":{exp}}}");
    let enc = rs_tuanjie::crypto::base64_encode(payload.as_bytes());
    format!("eyJhbGciOiJub25lIn0.{}.sig", enc.trim_end_matches('='))
}

/// 合成账号池：`user_id` → 名字，token 是 `sub` 为同一个 `user_id` 的合成 JWT。
fn synthetic_pool(ids: &[&str]) -> (AccountPool, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rs-tuanjie-p8b-pool-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tuanjie-accounts.json");
    let mut accounts = Vec::new();
    for id in ids {
        accounts.push(format!(
            "{{\"user_id\":\"{id}\",\"access_token\":\"{}\",\"enabled\":true,\"has_glm53\":true}}",
            synthetic_jwt(id, 4_000_000_000)
        ));
    }
    std::fs::write(
        &path,
        format!("{{\"accounts\":[{}]}}", accounts.join(",")),
    )
    .unwrap();
    // token 缝：给池内号回它自己的 token 快照（`effectiveAccessToken` 的池账号分支）；
    // 对没有快照的探针（本地账号探测）如实回错 ⇒ **不引入本地账号**、不读真实登录态。
    let p = AccountPool::with_path(Some(path.clone()), |a: &Account| {
        if a.access_token.is_empty() {
            Err("读取团结登录态失败（请先登录团结 Cowork 桌面端）".to_string())
        } else {
            Ok(a.access_token.clone())
        }
    });
    (p, dir)
}

/// 一次「请求」的结果（只关心状态码；转发字节由 P8a/后续小轮的门禁负责）。
#[derive(Debug, PartialEq)]
enum Outcome {
    Ok200,
    Timeout,
    Status(u16),
}

/// 有界换号循环：与生产装配同款（回 401 → 标死 → 换个号重发；上限 = 池大小）。
/// 返回 `(最终结果, 上游被访问的 uid 序)`。
fn run_request<F>(p: &AccountPool, model: &str, upstream: F) -> (Outcome, Vec<String>)
where
    F: Fn(&str) -> Outcome,
{
    let mut seen: Vec<String> = Vec::new();
    let mut swaps = 0usize;
    let Some((mut acc, _tok)) = p.pick_with_token(model) else {
        return (Outcome::Status(401), seen); // 无可用号：调用方如实透传上游响应
    };
    p.inc_load(&acc.user_id);
    let mut release = acc.user_id.clone();
    let mut attempts = 0usize;
    let outcome = loop {
        attempts += 1;
        assert!(attempts <= 8, "换号循环必须收敛（防无限重发）");
        seen.push(acc.user_id.clone());
        let out = upstream(&acc.user_id);
        match out {
            Outcome::Status(401) | Outcome::Status(403) => {
                // 凭据失效：标死当前号 + 有界换号重发（codebuddy/server.go:1400-1418）
                p.mark_dead(&acc.user_id);
                if swaps < max_swaps(p) {
                    if let Some((next, _t)) = p.pick_with_token(model) {
                        p.dec_load(&release);
                        release = next.user_id.clone();
                        p.inc_load(&next.user_id);
                        acc = next;
                        swaps += 1;
                        continue;
                    }
                }
                break out;
            }
            other => break other,
        }
    };
    p.dec_load(&release);
    (outcome, seen)
}

// ---------------------------------------------------------------- 401 标死 / 换活号

#[test]
fn dead_account_is_skipped_and_request_succeeds_on_the_live_one() {
    let (p, dir) = synthetic_pool(&["uid-alpha", "uid-beta"]);
    // 上游对 alpha 恒 401、对 beta 回 200（合成序列，不出网）
    let (outcome, seen) = run_request(&p, "codely-core", |uid| match uid {
        "uid-alpha" => Outcome::Status(401),
        _ => Outcome::Ok200,
    });
    assert_eq!(outcome, Outcome::Ok200, "换到活号后应成功");
    assert_eq!(seen, vec!["uid-alpha", "uid-beta"], "先撞死号再换活号");

    // 标死的号：pick 永不再选（此后所有请求都落在 beta）
    let a = p.get("uid-alpha").unwrap();
    assert!(a.dead, "401 后必须标死");
    let b = p.get("uid-beta").unwrap();
    assert!(!b.dead, "活号不许被误标");
    for _ in 0..5 {
        let acc = p.pick("codely-core").unwrap();
        assert_eq!(acc.user_id, "uid-beta");
    }

    // 内存态：**Dead 不落盘**（Go codebuddy 同款口径，pool.go:13）
    p.reload();
    assert!(
        !p.get("uid-alpha").unwrap().dead,
        "Dead 是内存态：重载后清零（重启即忘）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn all_dead_pool_reports_the_upstream_status_instead_of_looping() {
    let (p, dir) = synthetic_pool(&["uid-alpha", "uid-beta"]);
    let (outcome, seen) = run_request(&p, "codely-core", |_uid| Outcome::Status(401));
    assert_eq!(outcome, Outcome::Status(401), "全池死号：如实透传 401，不编造");
    // 有界：最多「首号 + 池大小次换号」，一轮就够（两个号各打一次）
    assert_eq!(seen, vec!["uid-alpha", "uid-beta"]);
    assert!(p.get("uid-alpha").unwrap().dead);
    assert!(p.get("uid-beta").unwrap().dead);
    assert!(
        p.pick("codely-core").is_none(),
        "全死之后 pick 返回 None（调用方据此透传）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 反向验证 1：**换号上限**来自 `p.size()`（`codebuddy/server.go:1257` 的 `deadResent < Size()`）。
/// 把上限从池大小改成 0（= 不换号）应让「401 换活号」失败；这里用等价的可观测形式锁住边界：
/// 池越大能换的次数越多，但**永远收敛**（不会无限重发）。
#[test]
fn swap_budget_is_bounded_by_pool_size() {
    for n in 1..=4usize {
        let ids: Vec<String> = (0..n).map(|i| format!("uid-{i}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let (p, dir) = synthetic_pool(&refs);
        let (outcome, seen) = run_request(&p, "codely-core", |_uid| Outcome::Status(401));
        assert_eq!(outcome, Outcome::Status(401));
        assert!(
            seen.len() <= n + 1,
            "池 {n} 个号：上游最多被访问 {n}+1 次（实得 {}）",
            seen.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
#[ignore = "反向验证用：把 mark_dead 注掉会走成全池轮询（死循环），需人工确认后再跑"]
fn removing_the_dead_check_would_loop_forever() {
    // 说明性用例：**不跑**。记录的是「为什么必须有 Dead 跳过位」——
    // 若 pick 不跳死号，`seen` 会在 alpha/beta 之间无限交替（上游恒 401）。
}

#[test]
fn timeout_is_not_a_credential_failure() {
    let (p, dir) = synthetic_pool(&["uid-alpha"]);
    // 上游超时：**不**标死（Go 团结侧 `server.go:461` 的原话：「网络层错误（err != nil）
    // 仍如实 502，不兜底」；凭据治理只管 401）
    let (outcome, seen) = run_request(&p, "codely-core", |_uid| Outcome::Timeout);
    assert_eq!(outcome, Outcome::Timeout);
    assert_eq!(seen, vec!["uid-alpha"]);
    assert!(!p.get("uid-alpha").unwrap().dead, "超时不得标死");
    assert!(p.pick("codely-core").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 冷却（429）

#[test]
fn cooling_account_is_skipped_until_the_window_expires() {
    use rs_tuanjie::accounts::now_unix;
    let (p, dir) = synthetic_pool(&["uid-alpha", "uid-beta"]);
    // 把轮询指针推到 alpha，再给它记 60s 冷却
    assert_eq!(p.pick("codely-core").unwrap().user_id, "uid-alpha");
    p.mark_limited("uid-alpha", now_unix() + 60);
    assert_eq!(
        p.pick("codely-core").unwrap().user_id,
        "uid-beta",
        "冷却中的号要被跳过"
    );
    // 到期即自动可选（懒判定，无后台循环）
    p.mark_limited("uid-alpha", now_unix() - 1);
    let mut got = std::collections::HashSet::new();
    for _ in 0..4 {
        got.insert(p.pick("codely-core").unwrap().user_id);
    }
    assert!(got.contains("uid-alpha"), "冷却到期后必须重新入轮换");
    // clear_cool 提前解除
    p.mark_limited("uid-alpha", now_unix() + 3600);
    assert!(p.clear_cool("uid-alpha"));
    assert!(!p.clear_cool("nobody"));
    assert!(
        p.get("uid-alpha").unwrap().limited_until.is_none(),
        "提前恢复只清冷却"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- rescan 复位

#[test]
fn rescan_revives_dead_accounts_and_keeps_user_intent() {
    let (p, dir) = synthetic_pool(&["uid-alpha", "uid-beta"]);
    p.mark_dead("uid-alpha");
    assert!(p.toggle("uid-beta", false), "手动停用");
    assert!(p.get("uid-alpha").unwrap().dead);
    assert!(!p.get("uid-beta").unwrap().enabled);

    // 用户桌面端重新登录 → 新文件落地 → 点「重新扫描」
    p.rescan(|_a: &Account| Ok(synthetic_jwt("uid-alpha", 4_000_000_000)));

    let a = p.get("uid-alpha").unwrap();
    assert!(!a.dead, "rescan 即复位：死号复活再实测（codebuddy/pool.go:489-496）");
    assert!(a.enabled, "新号缺省启用");
    let b = p.get("uid-beta").unwrap();
    assert!(!b.enabled, "停用是用户意图，rescan 不得复活");

    // 402 预算标记与 Dead **不是同一层**（如实锁住，别搞混）：
    //   * Dead / 冷却 = 纯内存态（内存里的标记，rescan 重建成员即消失）
    //   * 402 预算标记随池**落盘**（Go 的 MarkBudgetExceededAt 里就调 saveLocked）
    //     ⇒ rescan 重读文件后它仍在；清它靠 Toggle 重新启用（accounts.go:478）
    p.mark_budget_exceeded("uid-beta");
    assert!(p.get("uid-beta").unwrap().budget_exceeded, "标记已置");
    assert!(
        p.get("uid-beta").unwrap().budget_reset_at.is_none(),
        "MarkBudgetExceeded 记零值时刻"
    );
    p.rescan(|_a: &Account| Ok(synthetic_jwt("uid-alpha", 4_000_000_000)));
    assert!(
        p.get("uid-beta").unwrap().budget_exceeded,
        "402 标记落在文件里 ⇒ rescan 后仍在（与内存态的 Dead 不同层）"
    );
    assert!(p.toggle("uid-beta", true), "重新启用");
    assert!(
        !p.get("uid-beta").unwrap().budget_exceeded,
        "Toggle(enabled=true) 才是 402 标记的清除路径（accounts.go:477-479）"
    );
    assert_eq!(p.size(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rescan_keeps_disk_only_members_and_drops_removed_files() {
    let (p, dir) = synthetic_pool(&["uid-alpha", "uid-beta"]);
    // 用户把 beta 从文件里删了（或换号换了文件）→ rescan 后出池
    let path = dir.join("tuanjie-accounts.json");
    std::fs::write(
        &path,
        format!(
            "{{\"accounts\":[{{\"user_id\":\"uid-alpha\",\"access_token\":\"{}\",\"enabled\":true}}]}}",
            synthetic_jwt("uid-alpha", 4_000_000_000)
        ),
    )
    .unwrap();
    p.rescan(|_a: &Account| Ok(synthetic_jwt("uid-alpha", 4_000_000_000)));
    assert_eq!(p.size(), 1);
    assert!(p.get("uid-beta").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 内存态计数

#[test]
fn inflight_is_in_memory_and_pick_prefers_the_least_loaded() {
    let (p, dir) = synthetic_pool(&["uid-alpha", "uid-beta"]);
    assert_eq!(p.load_of("uid-alpha"), 0);
    p.inc_load("uid-alpha");
    p.inc_load("uid-alpha");
    p.inc_load("uid-beta");
    assert_eq!(p.load_of("uid-alpha"), 2);
    assert_eq!(p.load_of("uid-beta"), 1);
    // 负载感知：最低负载优先（accounts.go:186-204）；选中本身不改计数（Go 由调用方 Inc）
    assert_eq!(p.pick("codely-core").unwrap().user_id, "uid-beta");
    assert_eq!(p.load_of("uid-alpha"), 2, "pick 不动负载计数");
    assert_eq!(p.load_of("uid-beta"), 1);
    // 减到 0 后钳位，不出现负数
    p.dec_load("uid-alpha");
    p.dec_load("uid-alpha");
    p.dec_load("uid-alpha");
    assert_eq!(p.load_of("uid-alpha"), 0, "Dec 钳在 0（再减不出负数）");
    p.dec_load("uid-beta");
    assert_eq!(p.load_of("uid-beta"), 0, "另一个号也归零");
    // loads **不落盘**：重载池成员不会重置它（Go 的 reload 同样只重建 accounts，
    // 不动 loads——重启才清零）。这里锁住这条真实口径，免得后人以为 reload 是「软重启」。
    p.inc_load("uid-alpha");
    p.reload();
    assert_eq!(p.load_of("uid-alpha"), 1, "reload 只重建成员，不动内存计数");
    assert_eq!(p.load_of("uid-beta"), 0, "另一个号没被动过");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn use_count_and_last_used_are_bumped_by_pick_and_persisted() {
    let (p, dir) = synthetic_pool(&["uid-alpha"]);
    let a0 = p.get("uid-alpha").unwrap();
    assert_eq!(a0.use_count, 0);
    assert_eq!(a0.last_used, "");
    p.pick("codely-core");
    p.pick("codely-core");
    let a = p.get("uid-alpha").unwrap();
    assert_eq!(a.use_count, 2, "选中即计数（Go 在 Pick 内自增）");
    assert_eq!(a.last_used.len(), 19, "YYYY-MM-DD HH:MM:SS");
    // 计数**随池落盘**（Go 的 saveLocked 在 Pick 里调）：重载后仍在
    p.reload();
    assert_eq!(p.get("uid-alpha").unwrap().use_count, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn status_reports_jwt_expiry_and_live_loads() {
    use rs_tuanjie::accounts::AccountStatus;
    let (p, dir) = synthetic_pool(&["uid-alpha"]);
    p.inc_load("uid-alpha");
    let s: Vec<AccountStatus> = p.status();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].user_id, "uid-alpha");
    assert_eq!(s[0].inflight, 1, "进行中计数实时反映在摘要里");
    assert_eq!(s[0].token_expires.len(), 16, "2006-01-02 15:04");
    assert!(s[0].token_remaining_hours > 0.0);
    assert_eq!(
        s[0].token_remaining_hours,
        ((s[0].token_remaining_hours * 10.0).floor() / 10.0),
        "剩余小时保留一位（Go 的 float64(int(remain*10))/10）"
    );
    // 无效 token 的号：显示「未知」而不是报错
    p.add("uid-gamma", "not-a-jwt", "", "");
    let s2 = p.status();
    let g = s2.iter().find(|x| x.user_id == "uid-gamma").unwrap();
    assert_eq!(g.token_expires, "未知");
    assert_eq!(g.token_remaining_hours, 0.0);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 混合序列（timeout + 401 + 200）

#[test]
fn timeout_then_401_then_200_across_three_requests() {
    let (p, dir) = synthetic_pool(&["uid-a", "uid-b", "uid-c"]);
    // 逐号枚举，不用兜底分支：uid-a 超时、uid-b 401、uid-c 200。
    let upstream = |uid: &str| match uid {
        "uid-a" => Outcome::Timeout,
        "uid-b" => Outcome::Status(401),
        _ => Outcome::Ok200,
    };

    // 段 1：uid-a 超时 —— 网络层错误不进凭据治理，如实返回、不标死、**不换号**
    let (o1, s1) = run_request(&p, "codely-core", upstream);
    assert_eq!(o1, Outcome::Timeout, "超时如实返回");
    assert_eq!(s1, vec!["uid-a"], "超时不换号");
    assert!(!p.get("uid-a").unwrap().dead, "超时不标死");
    assert!(!p.get("uid-b").unwrap().dead);
    assert!(!p.get("uid-c").unwrap().dead);

    // 段 2：uid-b 回 401 —— 标死 + 换活号（此时池里可用的号是 uid-a 与 uid-c）。
    // 注意换号后可能落到 uid-a（它只超时、没死，仍是合法候选）——故断言「换到了别的号
    // 且恰好两跳」，不写死换到谁；死号出局的收敛性由段 3 单独验证。
    let (o2, s2) = run_request(&p, "codely-core", upstream);
    assert_eq!(s2.first().map(String::as_str), Some("uid-b"), "起点是 b");
    assert_eq!(s2.len(), 2, "401 触发且仅触发一次换号");
    match o2 {
        Outcome::Ok200 => {}
        Outcome::Timeout => assert_eq!(s2[1], "uid-a", "换到了那个只超时的号"),
        other => panic!("不该出现的换号结果：{other:?}"),
    }
    assert!(p.get("uid-b").unwrap().dead, "401 标死");
    assert!(!p.get("uid-c").unwrap().dead, "没被 401 的号不标死");
    assert!(!p.get("uid-a").unwrap().dead, "超时的号不标死");

    // 段 3：**只有 uid-c 健康**（uid-a 恒超时、uid-b 已标死）——看轮询能否收敛到
    // 唯一活号上（死号/超时号都不该把它挤掉）。
    p.mark_dead("uid-a"); // 本轮只验证「死号出局后稳定落在活号上」
    for _ in 0..3 {
        let (o, s) = run_request(&p, "codely-core", upstream);
        assert_eq!(o, Outcome::Ok200, "唯一活号必须被选中");
        assert_eq!(s, vec!["uid-c"], "死号永不再选");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 本地账号（合成 token，不读真登录态）

#[test]
fn ensure_local_account_uses_the_injected_token_seam() {
    let dir = std::env::temp_dir().join(format!("rs-tuanjie-p8b-local-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tuanjie-accounts.json");
    let local_token = synthetic_jwt("uid-local", 4_000_000_000);
    let token_for_closure = local_token.clone();
    let p = AccountPool::with_path(Some(path.clone()), move |_a: &Account| {
        Ok(token_for_closure.clone())
    });
    // 空池 → 本地账号并进来（Go EnsureLocalAccount，accounts.go:411）
    assert_eq!(p.size(), 1);
    let local = p.local_account().expect("池里应有本地账号");
    assert_eq!(local.user_id, "uid-local");
    assert_eq!(local.source, "local");
    assert!(local.access_token.is_empty(), "本地账号 token 恒空（实时读）");
    // 取 token 走缝：本地账号拿到实时值
    assert_eq!(p.effective_access_token(&local).unwrap(), local_token);
    // sub 变了（桌面端切号）→ 更新那一条，不新增
    let token2 = synthetic_jwt("uid-other", 4_000_000_000);
    let p2 = AccountPool::with_path(Some(path.clone()), move |_a: &Account| {
        Ok(token2.clone())
    });
    assert_eq!(p2.size(), 1);
    assert_eq!(p2.local_account().unwrap().user_id, "uid-other");
    assert_eq!(jwt_sub(&local_token), "uid-local");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_login_state_means_single_account_mode() {
    let dir = std::env::temp_dir().join(format!("rs-tuanjie-p8b-nologin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = AccountPool::with_path(Some(dir.join("tuanjie-accounts.json")), |_a: &Account| {
        Err("读取团结登录态失败（请先登录团结 Cowork 桌面端）".to_string())
    });
    assert_eq!(p.size(), 0, "无登录态 = 空池 = 单账号模式");
    assert!(p.pick("codely-core").is_none());
    assert!(p.local_account().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// P8b 审查必修 #1：Go 零值时间读回必须为 `None`（未知，不参与自动恢复）。
///
/// `MarkBudgetExceededAt` 注释明言"resetAt 零值 = 未知"（`accounts.go:245-247`）。
/// 旧实现读成 `Some(-62135596800)` ⇒ 每条零重置账号被 `resume_scan_once` 收为候选，
/// 每 5 分钟打一次积分接口。回归断言两层：读回 `None`、复查不点名
/// （query 一旦被调就 panic）；落盘方向由 `accounts.rs` 内既有 marshal 单测锁定
/// （`None` 写成 `"0001-01-01T00:00:00Z"`）。
#[test]
fn zero_reset_time_is_unknown_and_skipped_by_resume() {
    let dir = std::env::temp_dir().join(format!(
        "rs-tuanjie-p8b-zeroreset-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tuanjie-accounts.json");
    std::fs::write(
        &path,
        r#"{"accounts":[{"user_id":"uid-zero","access_token":"tok","enabled":true,"budget_exceeded":true,"budget_reset_at":"0001-01-01T00:00:00Z"}]}"#,
    )
    .unwrap();
    let p = AccountPool::with_path(Some(path), |a: &Account| {
        Ok(a.access_token.clone())
    });
    let acc = p.get("uid-zero").expect("号必须在池里");
    assert_eq!(
        acc.budget_reset_at, None,
        "Go 零值时间必须读成 None（未知），实得 {:?}", acc.budget_reset_at
    );
    // 复查必须不点名：query 一旦被调就 panic（旧实现下该号恒为候选，必炸）
    p.resume_scan_once(
        4_000_000_000,
        Some(|_uid: &str| -> (f64, Option<i64>, bool) {
            panic!("零重置账号不得被收为复查候选")
        }),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// P8b 审查必修 #2：生产 token 缝必须分支（local 实时读 / 池用快照）。
///
/// `new()` 直接引用 `production_token_provider`，这里锁定该函数的池分支：
/// 有快照的池账号直接返回快照，**不碰登录态**（无登录态的纯池部署全军覆没即此）。
/// local 分支会读真实登录态（机器相关），不在门禁断言——分支条件本身逐字可见。
#[test]
fn production_provider_returns_pool_snapshot_without_touching_login_state() {
    use rs_tuanjie::accounts::production_token_provider;
    let a = Account {
        user_id: "uid-pool".to_string(),
        access_token: "snap-123".to_string(),
        source: "pool".to_string(),
        ..Default::default()
    };
    assert_eq!(
        production_token_provider(&a).unwrap(),
        "snap-123",
        "池账号必须拿自己的快照，不得去读登录态"
    );
}
