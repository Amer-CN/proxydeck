//! command_dispatch.rs —— 分发骨架的**真实进程**门禁（P7a 形态：单 exe 多子模式）。
//!
//! 跑得动的那几条：`-version`、`--plugin-tuanjie`（**P7d-1 起真起服务**，测试端口 +
//! 隔离数据目录 + 假上游，见 `plugin_submode_really_starts_the_service`）、`-headless`
//! 的失败自写日志（P7c）、以及失败子模式的诊断落盘（P7b）。
//! **不跑** GUI 分支（会弹窗，干扰正在用界面的用户），也不跑任何会长期占端口的插件子模式
//! （团结那条起来、探活、立刻收）。
//!
//! ⚠ **`--fax` 刻意不在这条门禁里跑**（P7c 起它已真实现）：`run_fax_window` 会**真的建一个
//! 浮窗**，而且它会先抢 `Local\ProxyDeck-FaxWindow` 这个**进程名互斥体**——真跑一次就可能
//! 把用户正开着的注水浮窗挡在门外（或反过来让本进程的浮窗占住名字）。
//! 故 `--fax` 只做：① 单实例/标题/尺寸常量的差分对照（`tests/shell_contract.rs`）
//! ② 三绑定的差分（同上）③ 人工验收清单（`README.md`）。P7a~P7b 那条
//! 「`--fax` 如实报未实现」的用例已随实现落地而删除，不保留空转断言。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rs-gui")
}

/// 测试端口（与 rs-tuanjie 自己的集成测试同一档；**绝不碰 8788**）。
const TUANJIE_TEST_PORT: u16 = 18788;

/// 隔离数据目录：临时目录 + 合成账号，**不读也**不写真实 `%APPDATA%\ProxyDeck`。
struct IsolatedDir(std::path::PathBuf);

impl IsolatedDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rs-gui-p7d1-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".codely-cli")).unwrap();
        // 合成登录态（假 access_token，只有形状对）：真登录态一个字节都不读。
        std::fs::write(
            dir.join(".codely-cli").join("oauth_creds.json"),
            br#"{"access_token":"synthetic-access-token-for-rs-gui-gate"}"#,
        )
        .unwrap();
        IsolatedDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for IsolatedDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 假团结登录/换 key 域：只认 `GET /api/api-token/cli-api-key`，回 `{"cli_api_key":"…"}`。
/// 让 `/health` 走到 `status:"ok"`（真换 key 会打 `codely.tuanjie.cn`，测试里绝不出网）。
struct FakeCodey {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeCodey {
    fn start() -> Self {
        let ln = TcpListener::bind("127.0.0.1:0").expect("假上游必须能绑端口");
        let port = ln.local_addr().unwrap().port();
        ln.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                match ln.accept() {
                    Ok((s, _)) => {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        serve_one(s);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        FakeCodey {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for FakeCodey {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // 叫醒 accept：丢一次连接即可（非阻塞 accept 已在 10ms 粒度轮询）
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_one(mut s: TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    // 读到请求头结束（\r\n\r\n）为止；client 不带体，够用。
    loop {
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let body = if head.contains("GET /api/api-token/cli-api-key") {
        r#"{"cli_api_key":"sk-synthetic-cli-api-key-for-gate"}"#.to_string()
    } else {
        // 其余路径（本用例不该打到）如实回 404，不假装成功。
        let resp = "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\ncontent-length: 9\r\nconnection: close\r\n\r\nnot here\n";
        let _ = s.write_all(resp.as_bytes());
        return;
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = s.write_all(resp.as_bytes());
}

/// 收掉子进程并等端口真空出（`Child::kill` 是 TerminateProcess，端口回收要一点时间）。
fn kill_and_wait_port_free(mut child: Child, port: u16) {
    let _ = child.kill();
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("进程已收，端口 {port} 仍未空出");
}

fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(3))).ok()?;
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n\r\n")
            .as_bytes(),
    )
    .ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let status: u16 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, text))
}

#[test]
fn version_flag_prints_the_go_compatible_line() {
    let out = Command::new(bin()).arg("-version").output().unwrap();
    assert!(out.status.success(), "-version 必须退出码 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim_end(),
        format!(
            "ProxyDeck {} (proxy core {})",
            rs_gui::APP_VERSION,
            rs_gui::CORE_VERSION
        ),
        "版本行必须与 Go `main.go:217` 的 `ProxyDeck %s (proxy core %s)` 同形"
    );
}

/// **P7d-1 的主门禁**：`rs-gui.exe --plugin-tuanjie --port <测试端口>` 必须**真起服务**并过健康检查。
///
/// 判据（全部是真实进程/真实 socket，不是进程内自证）：
///   1. 子进程起来后 `/health` 真应答 200，且体内 `"status":"ok"`（换了 key → 说明
///      `AppState::new()`、cli_api_key 换取链、axum 监听整条都活了）；
///   2. `"service":"tuanjie-go"`（与 Go 8788 同形）；
///   3. 子进程**不退出**（kill 前 `try_wait()` 为 `None`）——服务是真常驻，不是打完就退。
///
/// 隔离：临时数据目录 + 临时 HOME（合成 `oauth_creds.json`）+ 假上游（`RS_TUANJIE_TEST_BASES`，
/// 顺带关掉种子卫兵的 npm 出网）。**绝不碰 8788**，也绝不读真实登录态/账号文件。
#[test]
fn plugin_submode_really_starts_the_service() {
    let dir = IsolatedDir::new("tuanjie");
    let codey = FakeCodey::start();
    let data_dir = dir.path().join("data");
    let port = TUANJIE_TEST_PORT.to_string();

    let mut child = Command::new(bin())
        .args([
            "--plugin-tuanjie",
            "--port",
            &port,
            // 分发器要把 `--data-dir` 原样透传给 rs-tuanjie 自己的 clap（不重造参数口径）
            "--data-dir",
            &data_dir.to_string_lossy(),
        ])
        .env(rs_gui::appdir::DATA_DIR_ENV, &data_dir)
        .env("USERPROFILE", dir.path())
        .env("APPDATA", dir.path())
        .env("LOCALAPPDATA", dir.path())
        // 测试缝：两个域都指到假上游（顺带让种子卫兵循环不起，对照环境不出网）
        .env(
            "RS_TUANJIE_TEST_BASES",
            format!("{} {}", codey.base(), codey.base()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("起 rs-gui 的 --plugin-tuanjie 子模式");

    // 探活：最多 30 秒（首轮要构造客户端 + 换 key + 监听）。
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = String::new();
    let health = loop {
        if let Some((status, text)) = http_get(TUANJIE_TEST_PORT, "/health") {
            if status == 200 && text.contains("\"status\":\"ok\"") {
                break text;
            }
            last = text;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            panic!("30s 内未探到 /health status:ok。最后一次响应：{last:?}\n子进程 stderr：{err}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    assert!(
        child.try_wait().unwrap().is_none(),
        "服务必须常驻（kill 前不得自行退出）"
    );
    assert!(
        health.contains("\"service\":\"tuanjie-go\""),
        "/health 的 service 字段必须与 Go 8788 同形：{health:?}"
    );

    kill_and_wait_port_free(child, TUANJIE_TEST_PORT);
}

/// `--port 8788` + 测试缝在场 → **拒绝启动**（退出码 2），且不监听。
///
/// 这条在 `rs-gui` 分发器这一层再锁一遍（`rs-tuanjie` 自己也有一条同样的门禁）：
/// P7d-1 把团结接进分发器后，「测试缝会不会顺着生产端口漏上去」是多出来的一条风险面。
#[test]
fn plugin_tuanjie_refuses_test_seams_on_production_port() {
    let dir = IsolatedDir::new("tuanjie-8788");
    let out = Command::new(bin())
        .args(["--plugin-tuanjie", "--port", "8788"])
        .env(rs_gui::appdir::DATA_DIR_ENV, dir.path().join("data"))
        .env(
            "RS_TUANJIE_TEST_BASES",
            "http://127.0.0.1:1 http://127.0.0.1:1",
        )
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "测试缝 + 8788 必须在分发器这一层也被拒（退出码 2）"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("8788"), "拒绝文案应点名生产端口：{stderr}");
    // 拒绝路径**不写** `<id>-plugin-error.log`（用法错误 ≠ 启动失败，GUI 不该收到诊断）
    assert!(
        !dir.path()
            .join("data")
            .join("tuanjie-plugin-error.log")
            .exists(),
        "拒绝启动（退出码 2）不该落启动失败诊断文件"
    );
}

/// 失败子模式必须写 `<id>-plugin-error.log`（Go `plugin_modes.go` 的 `pluginFailLog`）：
/// GUI 的退出监视就是读这个文件给「启动失败：…」的，缺了它界面只会停在「启动中」。
///
/// 触发方式（P7d-1 起团结已能真起服务，故换一条确定性的失败路径）：给团结传一个
/// **clap 收不下的端口** `99999`（`u16` 溢出）→ `Mode::Plugin` 的解析失败分支 →
/// 与 `run` 失败走**同一个失败收尾**。
#[test]
fn plugin_submode_failure_writes_the_diagnostic_log() {
    let dir = std::env::temp_dir().join(format!("rs-gui-p7b-faillog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let out = Command::new(bin())
        .args(["--plugin-tuanjie", "--port", "99999"])
        .env(rs_gui::appdir::DATA_DIR_ENV, &dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "失败子模式必须非零退出");

    let log = dir.join("tuanjie-plugin-error.log");
    let body = std::fs::read_to_string(&log)
        .expect("子模式失败必须写 <id>-plugin-error.log（GUI 的退出监视读它）");
    assert!(
        body.contains(" 启动失败："),
        "AppendLog 的行头是 `YYYY-MM-DD HH:MM:SS `，正文以「启动失败：」开头：{body:?}"
    );
    assert!(
        body.contains("99999"),
        "诊断里要带上真实原因（clap 的端口报错原文）：{body:?}"
    );
    assert!(body.ends_with('\n'), "AppendLog 每行一个换行：{body:?}");
    // AppendLog 的行头形态（Go 同款 2006-01-02 15:04:05）
    assert_eq!(&body[4..5], "-");
    assert_eq!(&body[7..8], "-");
    assert_eq!(&body[10..11], " ");
    assert_eq!(&body[13..14], ":");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `-headless` 的**失败路径**必须自写 `headless-error.log`（Go `main.go:236-240`：
/// `appdir.AppendLog(appdir.Path("headless-error.log"), "headless 启动失败："+err)`）。
///
/// 怎么**确定性地**触发失败？用 `-port 99999`：`u16` 放不下 → clap 当场拒绝 →
/// `Mode::Proxy` 的解析失败分支走到 `headless-error.log`（端口被占那条路在 Go 里
/// 是「先杀再听」，正常不会走到失败，故只能拿参数错误这条确定路径做证据）。
/// Rust 的 `u16` 解析拒绝与 Go 的 `net.JoinHostPort` 报错都是「headless 起不来」，
/// 走的是**同一个失败收尾**——这正是本条要锁的东西。
#[test]
fn headless_failure_writes_its_own_log() {
    let dir = std::env::temp_dir().join(format!("rs-gui-p7c-headless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let out = Command::new(bin())
        .args(["-headless", "-port", "99999"])
        .env(rs_gui::appdir::DATA_DIR_ENV, &dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "headless 起不来必须非零退出");

    let log = dir.join("headless-error.log");
    let body = std::fs::read_to_string(&log)
        .expect("headless 失败必须自写 headless-error.log（Go AppendLog 同源）");
    assert!(
        body.contains("headless 启动失败："),
        "正文必须以 Go 同款前缀开头：{body:?}"
    );
    assert!(body.ends_with('\n'), "AppendLog 每行一个换行：{body:?}");
    // AppendLog 的行头形态（Go 同款 2006-01-02 15:04:05）
    assert_eq!(&body[4..5], "-");
    assert_eq!(&body[7..8], "-");
    assert_eq!(&body[10..11], " ");
    assert_eq!(&body[13..14], ":");
    let _ = std::fs::remove_dir_all(&dir);
}
