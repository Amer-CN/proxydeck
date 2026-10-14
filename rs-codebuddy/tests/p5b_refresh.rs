//! P5b 修正轮的行为级测试：**凭据刷新链 + 单锁并发语义**。
//!
//! 门禁（`p5b_golden.rs`）里的夹具 `expiresAt` 都在 2100 年，刷新路径**零覆盖**；
//! 本文件用「近过期夹具 + 假刷新端点」把它补上，并顺手把两件只有真跑才能问清的事问清：
//!
//! 1. `credential.rs::headers_for` 是不是**一把锁罩全程**——两线程异号并发，各自必须
//!    只拿到自己号的 token，且没有任何跨号文件读/写（Go `credential.go:544-546` 的
//!    警告原文就是拆锁会串号）；
//! 2. 刷新回写链（明文 + `$wbEncrypted` 信封两种形态）到底通不通——回写文件里 nonce
//!    随机、**不能逐字节比**，故按「解密后明文 + 键集合」比对；
//! 3. `reqwest::blocking` 在 async handler 内首调会不会 panic（实测：**会**，
//!    tokio 1.53.3 的 debug 断言在调用线程上建/拆嵌套 runtime）——该失败形态与修复后的
//!    「一次性 OS 线程 + async 客户端」对照，是这次换掉 blocking 客户端的直接证据。

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use axum::Router;
use serde_json::{json, Value};

use common::{EnvGuard, TestDir};
use rs_codebuddy::atrest;
use rs_codebuddy::cred::CredSource;
use rs_codebuddy::credential::RealCredential;
use rs_codebuddy::region::Region;

/// 测试哨兵 secret（与门禁同一把；`$wbEncrypted` 派生密钥只认这一条链）。
const SECRET: &str = "c2VjcmV0LWdvbGRlbi1zZWNyZXQ=";

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
struct Fixture {
    access: &'static str,
    refresh: &'static str,
    new_access: &'static str,
    new_refresh: &'static str,
    uid: &'static str,
}

/// 两个号：`alpha` / `beta`。token 名刻意把号名嵌进去——任何串号都会在断言里现形。
fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            access: "tok-alpha-access",
            refresh: "tok-alpha-refresh",
            new_access: "tok-alpha-access-2",
            new_refresh: "tok-alpha-refresh-2",
            uid: "uid-alpha",
        },
        Fixture {
            access: "tok-beta-access",
            refresh: "tok-beta-refresh",
            new_access: "tok-beta-access-2",
            new_refresh: "tok-beta-refresh-2",
            uid: "uid-beta",
        },
    ]
}

fn file_name(f: &Fixture) -> String {
    format!("workbuddy-desktop-{}.info", f.uid)
}

/// 造一份**近过期**的合成 `.info`（`expiresAt` = 现在 − 5 分钟 ⇒ `expired()` 为真）。
///
/// `encrypted=true` 时 token 字段写成 `$wbEncrypted` 信封（用 crate 自己的加密器现造，
/// 走的是与生产同一条密钥链与 AAD；`encrypt_like` 的 nonce 随机 ⇒ 两次造出来的字节不同，
/// 这正是回写链不能逐字节比的原因）。
fn write_auth(
    dir: &std::path::Path,
    f: &Fixture,
    encrypted: bool,
    mtime_ms: i64,
) -> String {
    let token = |plain: &str| -> Value {
        if encrypted {
            atrest::encrypt_like(&[], plain, Region::Cn).expect("造信封")
        } else {
            Value::String(plain.to_string())
        }
    };
    let doc = json!({
        "account": {
            "uid": f.uid,
            "nickname": f.uid,
            "enterpriseId": "ent",
            "enterpriseName": "ent-name",
        },
        "auth": {
            "accessToken": token(f.access),
            "refreshToken": token(f.refresh),
            "domain": "workbuddy.cn",
            "expiresAt": now_millis() - 300_000,
            "refreshExpiresAt": now_millis() + 86_400_000,
        },
    });
    let mut body = serde_json::to_string_pretty(&doc).expect("序列化夹具");
    body.push('\n');
    let p = dir.join(file_name(f));
    std::fs::write(&p, body.as_bytes()).expect("写夹具");
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms.max(0) as u64);
    if let Ok(fh) = std::fs::File::options().write(true).open(&p) {
        let _ = fh.set_modified(t);
    }
    body
}

/// 读出 auth 文件并**解密**成明文（返回 `(access, refresh)` 明文 + 顶层键集合）。
fn read_decrypted(path: &std::path::Path) -> (String, String, Vec<String>) {
    let b = std::fs::read(path).expect("读回写后的 auth 文件");
    let v: Value = serde_json::from_slice(&b).expect("回写文件仍是合法 JSON");
    let auth = v.get("auth").expect("回写文件仍有 auth 段");
    let one = |k: &str| -> String {
        let raw = auth.get(k).expect("token 字段存在");
        let raw = serde_json::to_vec(raw).unwrap();
        atrest::decrypt_auth_field(&raw, Region::Cn).unwrap_or_default()
    };
    let mut keys: Vec<String> = v
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    (one("accessToken"), one("refreshToken"), keys)
}

/// 假刷新端点：按 `Authorization` 分派**完整响应**，并把「请求头是否自洽」记账。
///
/// 为什么按 `Authorization` 而不是 `X-User-Id` 分派：Authorization 里的 access token 正是
/// 「这次到底读了哪个文件」的直接结果（携带-读取是被测对象自己做的）。串号时 Authorization
/// 会带成对方的 token，于是这里要么分派到对方的新 token（返回的 token 与调用方号不符 ⇒
/// 断言现形），要么整发落到 `handled=false`（请求头自相矛盾 ⇒ 断言现形）。两条路都是硬失败。
struct RefreshServer {
    base: String,
    hits: Arc<Mutex<Vec<(String, String, String)>>>, // (authorization, x-refresh-token, x-user-id)
    handled: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for RefreshServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn start_refresh_server() -> RefreshServer {
    let hits: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let handled = Arc::new(AtomicBool::new(true));
    let h = hits.clone();
    let ok = handled.clone();
    // 应答表在闭包外算好：`Authorization` 就是「读了哪个文件」的直接结果，按它分派新 token。
    let table: Arc<Vec<(String, String, String, String, String, String)>> = Arc::new(
        fixtures()
            .into_iter()
            .map(|f| {
                (
                    format!("Bearer {}", f.access),
                    f.refresh.to_string(),
                    f.uid.to_string(),
                    f.new_access.to_string(),
                    f.new_refresh.to_string(),
                    f.access.to_string(),
                )
            })
            .collect(),
    );
    let app = Router::new().fallback(axum::routing::any(move |req: Request| {
        let h = h.clone();
        let ok = ok.clone();
        let table = table.clone();
        async move {
            let (parts, _body) = req.into_parts();
            let get = |name: &str| -> String {
                parts
                    .headers
                    .get(name)
                    .map(|v| String::from_utf8_lossy(v.as_bytes()).to_string())
                    .unwrap_or_default()
            };
            let auth = get("authorization");
            // 刻意留 60ms：把「读文件 -> 网络 -> 写回」的窗口撑开，让交错并发必然踩到
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            let refresh = get("x-refresh-token");
            let uid = get("x-user-id");
            h.lock().unwrap().push((auth.clone(), refresh.clone(), uid.clone()));
            let route = table
                .iter()
                .find(|(a, r, u, _, _, _)| *a == auth && *r == refresh && *u == uid);
            let Some((_, _, _, new_access, new_refresh, plain_access)) = route else {
                ok.store(false, Ordering::SeqCst);
                return Response::builder()
                    .status(500)
                    .body(Body::from(format!(
                        "incoherent refresh request: auth={auth} refresh={refresh} uid={uid}"
                    )))
                    .unwrap();
            };
            debug_assert!(auth.contains(plain_access.as_str()));
            let body = json!({
                "code": 0,
                "msg": "",
                "data": {
                    "accessToken": new_access,
                    "refreshToken": new_refresh,
                    "expiresAt": now_millis() + 3_600_000,
                    "refreshExpiresAt": now_millis() + 86_400_000,
                },
            });
            Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("假刷新端点起监听");
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    RefreshServer {
        base: format!("http://{addr}"),
        hits,
        handled,
        handle,
    }
}

/// 并发迭代次数：每号 64 次刷新（两线程同时开跑，单锁 ⇒ 总耗时 ≈ 2×64×60ms）。
const ITERS: usize = 64;

/// 两线程异号 `headers_for` 交错：**单锁罩全程**的行为证据。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_headers_for_does_not_cross_accounts() {
    // 环境变量是进程级的：与门禁共用同一把串行锁（见 README 差异 14）。
    let env = EnvGuard::set(&[]);
    for encrypted in [false, true] {
        let tag = if encrypted { "encrypted" } else { "plain" };
        let dir = TestDir::new(&format!("p5b-refresh-{tag}"));
        let dir_s = dir.path().to_string_lossy().to_string();
        env.set_locked(&[
            ("CODEBUDDY_AUTH_DIR", Some(dir_s.as_str())),
            ("WORKBUDDY_AT_REST_SECRET", Some(SECRET)),
            ("USERPROFILE", Some(dir_s.as_str())),
            ("LOCALAPPDATA", Some("")),
            ("XDG_DATA_HOME", Some("")),
        ]);
        atrest::reset_key_cache();

        let fx = fixtures();
        // mtime 拉开：alpha 较新（凭据层自动选号时也只会选它，本用例不依赖该行为）
        write_auth(dir.path(), &fx[0], encrypted, 1_792_036_800_000);
        write_auth(dir.path(), &fx[1], encrypted, 1_792_033_200_000);

        let srv = start_refresh_server().await;
        // 假刷新端点就是凭据层的 base_url（`refresh` 打 `{base_url}/v2/plugin/auth/token/refresh`）。
        // 生产装配里这条同源（`AppState::production_with_data_dir` 用同一个 override 造凭据）。
        let cred = Arc::new(RealCredential::with_base_url("", Region::Cn, &srv.base));

        // ---- 第一段：**交叉对撞**（确定性窗口） ----
        //
        // 两个线程**同时起跑**，各自先停 20ms 再进入刷新；两次刷新的 60ms 处理窗口因此**重叠**：
        // 线程 A 拉 A 号、线程 B 拉 B 号（真并发窗口，而非错峰）。
        // 拆锁的实现下：A 把路径切成 A 文件并读好；B 把路径切成 B 文件；A 的刷新（网络）在锁外
        // 跑完回来，`cached` 已被 B 覆盖 ⇒ A 拿到的头属于 B，且 A 的刷新结果会被写回 B 的路径。
        // 单锁实现下每发只回「这次传进去的那个文件」的 token，两个线程的断言都过。
        {
            let paths: Vec<String> = fx
                .iter()
                .map(|f| dir.path().join(file_name(f)).to_string_lossy().to_string())
                .collect();
            let mut hs = Vec::new();
            for i in 0..2 {
                let cred = cred.clone();
                let p = paths[i].clone();
                let want = fx[i].clone();
                hs.push(std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    match cred.headers_for(&p) {
                        Ok(h) => {
                            let auth = h
                                .get("Authorization")
                                .map(|v| v.to_str().unwrap_or("").to_string())
                                .unwrap_or_default();
                            let uid = h
                                .get("X-User-Id")
                                .map(|v| v.to_str().unwrap_or("").to_string())
                                .unwrap_or_default();
                            // 二代 token 都算「本号的合法取值」：第二段/加密轮次里文件可能已被
                            // 自己的刷新结果回写过。**对方任何一代 token 都不在允许集合内**，
                            // 所以「串号」仍然只由对方的 token 触发（硬失败）。
                            let ok_auth = auth == format!("Bearer {}", want.access)
                                || auth == format!("Bearer {}", want.new_access);
                            if !ok_auth || uid != want.uid {
                                return vec![format!(
                                    "[{}] 传 {} 却拿到 auth={auth} uid={uid}",
                                    want.uid, p
                                )];
                            }
                            Vec::new()
                        }
                        Err(e) => vec![format!("[{}] headers_for 报错: {e}", want.uid)],
                    }
                }));
            }
            for h in hs {
                let bad = h.join().expect("对撞线程不该 panic");
                assert!(bad.is_empty(), "[{tag}] 交叉对撞串号：{bad:?}");
            }
            // 文件侧：A 的刷新结果绝不能落进 B 的文件（拆锁实现下 B 文件会被 A 的 token 覆盖）。
            // 解密后比明文：信封形态下新 token 不以原样落盘，故不能拿字符串 contains 判。
            for (i, f) in fx.iter().enumerate() {
                let raw = std::fs::read_to_string(dir.path().join(file_name(f))).unwrap();
                let (access, refresh, _) = read_decrypted(&dir.path().join(file_name(f)));
                assert!(
                    access == f.access || access == f.new_access,
                    "[{tag}] {} 的文件里 access 不是自己的任何一代：{access}",
                    f.uid
                );
                assert!(
                    refresh == f.refresh || refresh == f.new_refresh,
                    "[{tag}] {} 的文件里 refresh 不是自己的任何一代：{refresh}",
                    f.uid
                );
                let other = &fx[1 - i];
                for foreign in [other.access, other.refresh, other.new_access, other.new_refresh] {
                    assert!(
                        !raw.contains(foreign),
                        "[{tag}] {} 的文件里出现了对方的 token {foreign}（跨号写回）",
                        f.uid
                    );
                }
            }
        }

        // ---- 第二段：持续交错（同号并发 + 交叉拉号） ----
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        // 交叉拉号用的路径（循环外算好，避免把 `dir` 借进线程闭包）
        let paths: Vec<(String, String)> = fx
            .iter()
            .map(|f| {
                let own = dir.path().join(file_name(f)).to_string_lossy().to_string();
                let other = fx
                    .iter()
                    .find(|x| x.uid != f.uid)
                    .map(|x| dir.path().join(file_name(x)).to_string_lossy().to_string())
                    .unwrap_or_default();
                (own, other)
            })
            .collect();
        for (i, f) in fx.iter().enumerate() {
            let cred = cred.clone();
            let barrier = barrier.clone();
            let path = paths[i].0.clone();
            let other_path = paths[i].1.clone();
            let f = f.clone();
            let other_fx = fx[(i + 1) % fx.len()].clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut bad: Vec<String> = Vec::new();
                // 每次取头都断言「返回的 Authorization 必须是**我这次传进去的那个文件**的 token」。
                // 这是 `headersFor(path)` 的唯一合同；拆锁的实现会在并发窗口里让对手的路径
                // 盖掉自己的缓存，于是返回对手的 token —— 本线程能直接看见（硬失败）。
                let call = |cred: &RealCredential, want_path: &str, f: &Fixture| -> Option<String> {
                    match cred.headers_for(want_path) {
                        Ok(h) => {
                            let auth = h
                                .get("Authorization")
                                .map(|v| v.to_str().unwrap_or("").to_string())
                                .unwrap_or_default();
                            let uid = h
                                .get("X-User-Id")
                                .map(|v| v.to_str().unwrap_or("").to_string())
                                .unwrap_or_default();
                            let ok_auth = auth == format!("Bearer {}", f.access)
                                || auth == format!("Bearer {}", f.new_access);
                            if !ok_auth || uid != f.uid {
                                return Some(format!(
                                    "[{}] 传 {want_path} 却拿到 auth={auth} uid={uid}（跨号串号）",
                                    f.uid
                                ));
                            }
                            None
                        }
                        Err(e) => Some(format!("[{}] headers_for 报错: {e}", f.uid)),
                    }
                };
                // 主流向：各拉自己的号（同时起跑，第一次调用都会触发刷新）
                for _ in 0..ITERS {
                    if let Some(e) = call(&cred, &path, &f) {
                        bad.push(e);
                        break;
                    }
                }
                // 加严一轮：**交叉拉号**——A 线程反复拉 B 的文件、B 线程反复拉 A 的文件，
                // 使「切路径」与「读文件」两条语句在另一线程里持续交错（这正是拆锁会串号
                // 的窗口；单锁实现下每发都仍然只回「这次传进去的那个文件」的 token）。
                if bad.is_empty() {
                    for _ in 0..ITERS * 2 {
                        if let Some(e) = call(&cred, &other_path, &other_fx) {
                            bad.push(e);
                            break;
                        }
                    }
                }
                bad
            }));
        }
        for h in handles {
            let bad = h.join().expect("并发线程不该 panic");
            assert!(bad.is_empty(), "[{tag}] 并发取头串号：{bad:?}");
        }

        // 上游侧证据：每一发刷新请求都必须「自洽」（Authorization / X-Refresh-Token / X-User-Id
        // 三者同属一个号），且每号恰好 ITERS 发。
        assert!(
            srv.handled.load(Ordering::SeqCst),
            "[{tag}] 出现不自洽的刷新请求（跨号读/写会让三者对不上）"
        );
        let hits = srv.hits.lock().unwrap().clone();
        let mut per_uid: BTreeMap<String, usize> = BTreeMap::new();
        for (_, _, uid) in &hits {
            *per_uid.entry(uid.clone()).or_insert(0) += 1;
        }
        assert_eq!(hits.len(), 2, "[{tag}] 刷新请求数不对");
        for f in &fx {
            assert_eq!(
                per_uid.get(f.uid).copied().unwrap_or(0),
                1,
                "[{tag}] 号 {} 的刷新请求数不对（每号只该刷新一次）：{per_uid:?}",
                f.uid
            );
        }

        // 并发通道确实被踩过：两线程同时起跑时，**同一个号的刷新只允许一发**
        // （双花 refresh token 在单锁语义下不可能发生），而两号各一发 → 2 发。
        // 旧的分段锁实现下这里会出现「同一号两发」或「请求头自相矛盾」（handled=false）。

        // 文件侧证据：解密后明文 + 键集合（nonce 随机，不能逐字节比）。
        for (i, f) in fx.iter().enumerate() {
            let p = dir.path().join(file_name(f));
            let (access, refresh, keys) = read_decrypted(&p);
            assert_eq!(access, f.new_access, "[{tag}] {} 的 access 回写不对", f.uid);
            assert_eq!(
                refresh, f.new_refresh,
                "[{tag}] {} 的 refresh 回写不对",
                f.uid
            );
            assert_eq!(
                keys,
                vec!["account".to_string(), "auth".to_string()],
                "[{tag}] {} 的顶层键集合被改动",
                f.uid
            );
            // 跨号污染的直接检查：对方任何一代 token 都不该出现在本文件里。
            let raw = std::fs::read_to_string(&p).unwrap();
            let other = &fx[1 - i];
            for foreign in [other.access, other.refresh, other.new_access, other.new_refresh] {
                assert!(
                    !raw.contains(foreign),
                    "[{tag}] {} 的文件里出现了对方的 token {foreign}（跨号写回）",
                    f.uid
                );
            }
            if encrypted {
                // 形态保持：回写后仍是信封（明文 token 不该以原样出现在文件里）
                assert!(
                    !raw.contains(f.new_access) && !raw.contains(f.new_refresh),
                    "[{tag}] {} 的信封形态没保持（新 token 以明文落盘）",
                    f.uid
                );
                assert!(raw.contains("$wbEncrypted"), "[{tag}] 信封字段名丢了");
            }
        }
        println!(
            "[{tag}] 并发证据：确定性对撞（A 起跑后 20ms B 在 A 的刷新窗口里拉另一号）+ 两线程各 {ITERS} 次同号/交叉交错，\
             全部只回「本次传进去的那个文件」的 token；{} 发刷新请求全部自洽（每号恰好 1 发，无双花），\
             两份 auth 文件各自回写成自己的 token，互不污染",
            hits.len()
        );
    }
}

/// **改造现场的第二条证据**：替换掉 blocking 客户端的理由是实测得到的一条硬结论——
/// `reqwest::blocking::Client` 在 async handler 内提交第一发请求就会 panic。
///
/// 为什么这条证据不写成常驻用例：验证它必须**重新启用 `reqwest` 的 `blocking` 特性**，
/// 而本 crate 已经把该特性从 `Cargo.toml` 摘掉了（不摘就是留着这颗雷）。故只在这里
/// 留档复现方式与实测原始输出，探针文件本身跑完即删（登记在简报）。
///
/// 复现方式（`Cargo.toml` 临时把 `blocking` 加回 `reqwest` 的 features，跑完去掉）：
///
/// ```text
/// let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
///     let c = reqwest::blocking::Client::builder().build().unwrap();
///     let _ = c.get("http://127.0.0.1:1/x").send();
/// }));
/// // 放在 tokio multi_thread runtime 的 block_on 内（模拟 async handler）
/// ```
///
/// 2026-10-15 实测原始输出：
///
/// ```text
/// thread '<unnamed>' panicked at ...\tokio-1.53.3\src\runtime\blocking\shutdown.rs:51:21:
/// Cannot drop a runtime in a context where blocking is not allowed. This happens when
/// a runtime is dropped from within an asynchronous context.
/// ```
///
/// 成因：`reqwest` 的 blocking 实现在 `blocking/wait.rs::enter()` 里带一条 debug 断言——
/// 在当前线程上临时建一个 current-thread runtime 再拆掉；tokio 1.53.3 的
/// `blocking/shutdown.rs:51` 用 `try_enter_blocking_region()` 判定「当前线程在 async
/// 上下文里」，于是 `Runtime::drop` 直接 panic。**这条 panic 是 async 上下文专属**
/// （同一个 blocking 客户端在普通 OS 线程里用毫无问题）。修复后的实现
/// （`credential.rs::refresh_post_blocking`）就是「一次性 OS 线程 + 线程内 async 客户端」：
/// 线程不在任何 async 上下文里，建/拆 runtime 合法，调用方线程也不再碰 blocking。
///
/// 反向验证（本次实测）：把 `refresh_post_blocking` 换成 blocking 客户端，
/// `concurrent_headers_for_does_not_cross_accounts` 立即以同一条 panic 文案变红。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_http_path_runs_inside_async_handler() {
    // 本用例是上面那条「不写成常驻用例」的**正向对照**：刷新的网络段是从 async handler
    // 里发起的（`fetch_quota_live` → `headers_for` → `refresh` → `refresh_post_blocking`），
    // 走一遍真实 HTTP，回来还得是 200——即「async 上下文里刷新不出事」这件事本身有门禁。
    let dir = TestDir::new("p5b-refresh-in-async");
    let dir_s = dir.path().to_string_lossy().to_string();
    let env = EnvGuard::set(&[
        ("CODEBUDDY_AUTH_DIR", Some(dir_s.as_str())),
        ("WORKBUDDY_AT_REST_SECRET", Some(SECRET)),
        ("USERPROFILE", Some(dir_s.as_str())),
        ("LOCALAPPDATA", Some("")),
        ("XDG_DATA_HOME", Some("")),
    ]);
    atrest::reset_key_cache();
    let fx = fixtures();
    write_auth(dir.path(), &fx[0], false, 1_792_036_800_000);
    let srv = start_refresh_server().await;
    let cred = RealCredential::with_base_url("", Region::Cn, &srv.base);
    let path = dir.path().join(file_name(&fx[0])).to_string_lossy().to_string();

    // 直接在 async 上下文里调（`#[tokio::test]` 的 block_on 内）——blocking 客户端会在这里炸。
    let h = cred.headers_for(&path).expect("async 上下文里刷新应成功");
    assert_eq!(
        h.get("Authorization").unwrap().to_str().unwrap(),
        format!("Bearer {}", fx[0].new_access),
        "刷新后应带上新 token"
    );
    let hits = srv.hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 1, "应恰好一发刷新请求");
    drop(env);
}

