//! bridge_contract.rs —— P7a/P7b 的主门禁：**Go 宿主与 Rust 宿主的桥接层差分对照**。
//!
//! 参考值来源 `tests/golden/bridge-go-reference.json`：由
//! `.work/rust-rewrite/_p7b_collect_test.go`（P7a 采集器的扩表版，跑完从 `app/` 删除、
//! 只留档在 `.work/`）跑**真实 Go 闭包**采出来的——采集器给 `webview.WebView` 接口写了假
//! 实现，让生产 `app.bindAll(fake)` 把 30 个闭包原样交出来，再用 reflect 按位置调用
//! （不是对闭包体的转抄）。
//!
//! 本测试把黄金里的**同一串命令、同一套场景前置**再跑一遍（Rust 侧），逐条比对：
//!   * 字符串相等（归一化后逐字节）——键名、键序、文案、缺省值全在比对范围内
//!   * 落盘文件：**格式**（明文 / `PDAPI1` 密文）与**逻辑内容**（密文按 DPAPI 解回明文比）
//!   * 三组纯函数用例（退出原因 / 换血标记 / StampedWriter）按**值**比
//!
//! 插件场景（`PL*`）走**测试注册表**（端口 19788~19790）+ 假后端进程（`tests/fixtures/fake-backend.js`，
//! node）——绝不碰 8785~8791 与正在运行的 GUI/后端进程。
//!
//! 明示的豁免（黄金 `comparison_exemptions` 里也登记着，不做静默放过）：
//!   1. `ccKeyMapSet` 的 JSON 解析错误文案（Go `encoding/json` 与 serde_json 的文本不同）
//!   2. `appdir.log`（带时间戳、内容取决于 icacls 是否在 PATH 上）
//!   3. `PDAPI1` 密文字节本身（DPAPI 每次产出不同；按能否解回同一明文比）
//!   4. `ccLiveSnapshot` 的取值（随本机插件端口实时状态变化；只比键集与 ok 形状）
//!   5. `PL3_plugins_spawn_exit` 的 `plugins/zz-beta.log`（两边被 spawn 的子进程不同，
//!      日志正文必然不同；退出原因本身由 lastErr 逐字锁）
//!   6. 三组纯函数用例含非法 UTF-8 时期望值的字节形态（Go 写 `\ufffd` 转义、Rust 是同值字符）

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use rs_gui::appdir;
use rs_gui::bridge::{Bridge, NullClipboard, BINDINGS, PLUGIN_BINDINGS};
use rs_gui::host::NoWindowHost;
use rs_gui::plugins::{self, PluginDef, PluginHost, SpawnTarget};
use rs_gui::state::App;

const GOLDEN: &str = include_str!("golden/bridge-go-reference.json");

/// Rust 侧**已知新增**的探活目标（Go `pluginDefs` 里没有）。
///
/// 2026-10-20：新增 `qoder-intl`（Qoder 国际版，8791）。Go 侧插件表仍是 7 条，
/// 黄金基准 `ccLiveSnapshot.live` 因此少这个键。比对时**只扣除这一项**，
/// 其余任何键差异照旧判红（见 `go_and_rust_bridge_agree_command_by_command` 例外 4）。
/// 将来若 Go 侧也加了同名插件，把这里清空即可。
const RUST_ONLY_LIVE_KEYS: &[&str] = &["qoder-intl"];

/* ---------------- 归一化（与 Go 采集器同一套哨兵） ---------------- */

struct Normalizer {
    data_dir: String,
    appdata: String,
    exe: String,
    exe_dir: String,
    port: String,
}

impl Normalizer {
    /// 与 Go 采集器逐条同源的两条纪律：
    ///   1. **exe 先于 exeDir** 替换（exeDir 是 exe 路径的前缀，先换前缀会打碎 `$EXE`）；
    ///   2. 每条路径替换「原样」与「JSON 转义形态」两种写法（桥返回的是 JSON 文本）。
    fn norm(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (val, sentinel) in [
            (&self.exe, "$EXE"),
            (&self.exe_dir, "$EXEDIR"),
            (&self.data_dir, "$DATA"),
            (&self.appdata, "$APPDATA"),
        ] {
            if val.is_empty() {
                continue;
            }
            out = out.replace(val.as_str(), sentinel);
            out = out.replace(&val.replace('\\', "\\\\"), sentinel);
        }
        if !self.port.is_empty() {
            out = out.replace(&format!("127.0.0.1:{}", self.port), "127.0.0.1:$PORT");
        }
        out = replace_digits(&out, "\"pid\":", "0");
        out = replace_digits(&out, "\"uptime\":", "0");
        out = replace_digits(&out, "\"port\":\"", "$PORT");
        out = replace_digits(&out, "-port ", "$PORT");
        // `version` 是**有意分道**的字段：v4.0.0 起 Rust 即产品本体（v4.x），
        // 基准（采收自 Go 实现，该实现已于 2026-10-18 移除）冻结在 v3.20.11，两侧同形不同值。
// 归一成哨兵后，差分的其余字节照比。
        out = replace_quoted(&out, "\"version\":", "$VER");
        out
    }
}

/// 把 `marker` 后紧跟的一串十进制数字换成 `sentinel`（Go 侧用正则做同一件事）。
fn replace_digits(s: &str, marker: &str, sentinel: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(marker) {
        out.push_str(&rest[..i + marker.len()]);
        let after = &rest[i + marker.len()..];
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            out.push_str(sentinel);
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// 把 `marker` 之后引号里的内容换成哨兵（`"version":"v4.0.1"` → `"version":"$VER"`）。
fn replace_quoted(s: &str, marker: &str, sentinel: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(marker) {
        let after_marker = i + marker.len();
        out.push_str(&rest[..after_marker]);
        let after = &rest[after_marker..];
        let Some(open) = after.find('"') else {
            rest = after;
            break;
        };
        if open > 0 {
            // marker 与引号之间有空白等原样保留
            out.push_str(&after[..open]);
        }
        let tail = &after[open + 1..];
        let Some(close) = tail.find('"') else {
            out.push_str(&after[open..]);
            rest = "";
            break;
        };
        out.push('"');
        out.push_str(sentinel);
        out.push('"');
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    out
}

/// 把 `"msg":"..."` 的内容换成哨兵（仅用于 JSON 解析错误文案那一处例外）。
fn replace_msg(s: &str, sentinel: &str) -> String {
    let Some(i) = s.find("\"msg\":\"") else {
        return s.to_string();
    };
    let start = i + "\"msg\":\"".len();
    let tail = &s[start..];
    let Some(end) = tail.find('"') else {
        return s.to_string();
    };
    format!("{}{}{}", &s[..start], sentinel, &tail[end..])
}

/// 时间戳 → `$TS`（StampedWriter 用例；与 Go 采集器的 `normStamp` 同规则）。
/// 走**字节**扫描（多字节 UTF-8 原样带过），否则 `b[i] as char` 会把中文打碎。
fn norm_stamp(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        let is_stamp = i + 19 <= b.len()
            && b[i..i + 4].iter().all(|c| c.is_ascii_digit())
            && b[i + 4] == b'-'
            && b[i + 7] == b'-'
            && b[i + 10] == b' '
            && b[i + 13] == b':'
            && b[i + 16] == b':'
            && [5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
                .iter()
                .all(|k| b[i + k].is_ascii_digit());
        if is_stamp {
            out.extend_from_slice(b"$TS");
            i += 19;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/* ---------------- 假核心（与 Go 采集器同一套固定响应） ---------------- */

fn fake_core_body(uri: &str) -> Option<&'static str> {
    Some(match uri {
        "/health" => r#"{"ok":true}"#,
        "/v1/stats" => r#"{"ok":true,"total":7,"today":3,"models":[]}"#,
        "/v1/models" => r#"{"data":[{"id":"m1"}],"ok":true}"#,
        "/v1/models?plan=go" => r#"{"data":[{"id":"m1","plan":"go"}],"ok":true}"#,
        "/v1/calibration" => r#"{"ok":true,"msg":"已校准"}"#,
        _ => return None,
    })
}

/// 起一个最小 HTTP 服务（只伺候上面那几条固定 URI），返回监听端口。
fn start_fake_core() -> u16 {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let ln = TcpListener::bind("127.0.0.1:0").expect("绑定假核心端口失败");
    let port = ln.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in ln.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => return,
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let uri = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let want: usize = head
                    .lines()
                    .find_map(|l| {
                        let l = l.to_ascii_lowercase();
                        l.strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let got = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|i| buf.len() - (i + 4))
                    .unwrap_or(0);
                let mut remaining = want.saturating_sub(got);
                while remaining > 0 {
                    match s.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => remaining = remaining.saturating_sub(n),
                        Err(_) => break,
                    }
                }
                let body = fake_core_body(&uri).unwrap_or("");
                let status = if body.is_empty() { "404 Not Found" } else { "200 OK" };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            });
        }
    });
    port
}

/// 要一个当前空闲的端口（立刻释放，供「无监听」场景用）。
fn free_port() -> u16 {
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = ln.local_addr().unwrap().port();
    drop(ln);
    p
}

/* ---------------- 假后端进程（与 Go 采集器共用同一份脚本） ---------------- */

fn fixture_js() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-backend.js")
}

fn require_node() {
    let ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        ok,
        "插件场景要用 node 跑 tests/fixtures/fake-backend.js（假后端进程）；没有 node 就会把这些行为整批漏掉，故直接失败"
    );
}

/// 假后端进程句柄（Drop 时收掉）。
struct NodeBackend {
    child: Option<Child>,
}

impl NodeBackend {
    fn spawn(mode: &str, port: u16) -> Self {
        let mut cmd = std::process::Command::new("node");
        cmd.arg(fixture_js())
            .arg("--plugin-zz-x")
            .arg("--port")
            .arg(port.to_string())
            .env("RS_GUI_FAKE_MODE", mode);
        let child = cmd.spawn().expect("起假后端失败（node 起不来？）");
        let b = Self { child: Some(child) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let ready = if mode == "serve" {
                rs_gui::httpc::ok(&format!("http://127.0.0.1:{port}/health"))
            } else {
                std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
            };
            if ready {
                return b;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("假后端（{mode}:{port}）10s 内没就位");
    }
}

impl Drop for NodeBackend {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// 本进程占住的健康应答器（`killByPort` 排除自己 → 用它测「杀后复验」）。
/// 监听器由调用方持有，直到测试结束才释放——这正是 `PL4` 的语义（端口一直占着）。
fn start_inproc_healthy(port: u16) -> std::net::TcpListener {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let ln = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("绑测试端口 {port} 失败（被占？）: {e}"));
    let listener = ln.try_clone().expect("克隆监听失败");
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let Ok(mut sock) = s else { break };
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                );
                let _ = sock.flush();
            });
        }
    });
    ln
}

/* ---------------- 场景前置（与 Go 采集器逐条对齐） ---------------- */

/// 落盘文件的逻辑内容：明文原样，`PDAPI1` 密文解回明文。
fn logical_content(raw: &[u8]) -> (bool, Vec<u8>) {
    if rs_gui::dpapi::is_protected(raw) {
        (
            true,
            rs_gui::dpapi::unprotect_file_bytes(raw).unwrap_or_default(),
        )
    } else {
        (false, raw.to_vec())
    }
}

/// 黄金里的测试注册表 → Rust 侧的同名注册表。
fn test_defs_from_golden(v: &Value) -> Vec<PluginDef> {
    v.as_array()
        .expect("黄金缺 plugin_test_defs")
        .iter()
        .map(|d| PluginDef {
            id: d["id"].as_str().unwrap().to_string(),
            name: d["name"].as_str().unwrap().to_string(),
            dir: d["dir"].as_str().unwrap().to_string(),
            script: d["script"].as_str().unwrap().to_string(),
            args: d["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect(),
            port: d["port"].as_u64().unwrap() as u16,
            health: d["health"].as_str().unwrap().to_string(),
            native: d["native"].as_str().unwrap().to_string(),
        })
        .collect()
}

/// 写场景的前置文件（内容取黄金里的 `pre_files`，两侧同字节）。
fn write_pre_files(dir: &Path, files: &Value) {
    if let Some(map) = files.as_object() {
        for (rel, content) in map {
            let p = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content.as_str().unwrap().as_bytes()).unwrap();
        }
    }
}

/// 应用前置动作（与 Go 采集器的 `applyHooks` 同一套写法：`<kind>:<port>` / `node:<mode>:<port>`）。
fn apply_hooks(hooks: &Value, node_holders: &mut Vec<NodeBackend>) -> Vec<std::net::TcpListener> {
    let mut listeners = Vec::new();
    let Some(list) = hooks.as_array() else {
        return listeners;
    };
    for h in list {
        let h = h.as_str().unwrap();
        let (kind, port_str) = if let Some(p) = h.strip_prefix("inproc_healthy:") {
            ("inproc_healthy", p)
        } else if let Some(p) = h.strip_prefix("node:serve:") {
            ("node:serve", p)
        } else if let Some(p) = h.strip_prefix("node:raw:") {
            ("node:raw", p)
        } else {
            panic!("未知前置动作：{h}");
        };
        let port: u16 = port_str.parse().expect("前置动作的端口必须是数字");
        match kind {
            "inproc_healthy" => listeners.push(start_inproc_healthy(port)),
            "node:serve" => node_holders.push(NodeBackend::spawn("serve", port)),
            "node:raw" => node_holders.push(NodeBackend::spawn("raw", port)),
            _ => unreachable!(),
        }
    }
    listeners
}

/// 等某插件的退出监视收敛（Go 采集器在同一个收敛点记录输出）。
fn wait_plugin_idle(host: &PluginHost, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let entry = host
            .list()
            .into_iter()
            .find(|e| e["id"] == Value::String(id.to_string()));
        if let Some(e) = entry {
            if e["running"] == Value::Bool(false) && e["lastErr"].as_str().map(|s| !s.is_empty()).unwrap_or(false) {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!("插件 {id} 的退出监视 10s 内没收敛");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/* ---------------- 主门禁 ---------------- */

#[test]
fn go_and_rust_bridge_agree_command_by_command() {
    let golden: Value = serde_json::from_str(GOLDEN).expect("黄金 JSON 解析失败");

    // (0) 绑定名与顺序必须与 Go 侧一致：26 个 bridge.go + 4 个插件托管
    let go_bindings: Vec<String> = golden["bindings26"]
        .as_array()
        .expect("黄金缺 bindings26")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        go_bindings,
        BINDINGS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "Rust 侧 26 个绑定名/顺序必须与 Go 黄金逐个一致"
    );
    assert_eq!(go_bindings.len(), 26);
    let go_plugin: Vec<String> = golden["bindings_plugin"]
        .as_array()
        .expect("黄金缺 bindings_plugin")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        go_plugin,
        PLUGIN_BINDINGS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "插件托管 4 个绑定的名字/顺序必须与 Go 黄金逐个一致"
    );

    // 隔离根目录：数据目录与 APPDATA 都指到这里，绝不碰用户真实状态
    let root = std::env::temp_dir().join(format!("rs-gui-p7a-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let appdata = root.join("appdata");
    std::fs::create_dir_all(&appdata).unwrap();
    std::env::set_var("APPDATA", &appdata);
    std::env::set_var("USERNAME", std::env::var("USERNAME").unwrap_or_else(|_| "tester".into()));

    // 假核心：S2 用它、存在性补跑也用它（ccStart 走「探活接管」分支，不起子进程）
    let core_port = start_fake_core();

    let mut failures: Vec<String> = Vec::new();
    let mut checked_steps = 0usize;
    let mut covered: BTreeSet<String> = BTreeSet::new();
    let mut live_snapshot_keys: BTreeSet<String> = BTreeSet::new();
    let mut node_holders: Vec<NodeBackend> = Vec::new();
    // PL4 的端口由本进程一直占着（这就是该场景的语义）：监听器活到测试结束
    let mut held_listeners: Vec<std::net::TcpListener> = Vec::new();
    // PL3 会 spawn 假后端：让它把 argv 记到这里，事后断言
    let spawn_argv_log = root.join("spawn-argv.json");

    // 有插件场景时要假后端进程：先确认 node 在，免得跑到一半才炸
    let needs_node = golden["scenarios"]
        .as_object()
        .unwrap()
        .values()
        .any(|s| {
            s["pre_hooks"]
                .as_array()
                .map(|hs| {
                    hs.iter()
                        .any(|h| h.as_str().unwrap_or("").starts_with("node:"))
                })
                .unwrap_or(false)
        });
    if needs_node {
        require_node();
    }

    let scenarios = golden["scenarios"].as_object().expect("黄金缺 scenarios");
    for (scen, data) in scenarios {
        let dir = root.join(scen);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);

        let plugins_test = data["uses_plugin_test_defs"].as_bool().unwrap_or(false);
        write_pre_files(&dir, &data["pre_files"]);
        held_listeners.extend(apply_hooks(&data["pre_hooks"], &mut node_holders));
        // PL3 会 spawn 假后端：让它把 argv 记下来，事后断言「spawn 出去的 argv 逐位正确」
        if scen == "PL3_plugins_spawn_exit" {
            let _ = std::fs::remove_file(&spawn_argv_log);
            std::env::set_var("RS_GUI_FAKE_LOG", spawn_argv_log.to_string_lossy().to_string());
        }
        // S4：Go 的 setAutostart 不建目录，采集器预建了启动目录让成功路径进门禁
        if scen == "S4_autostart" {
            std::fs::create_dir_all(
                appdata
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
                    .join("Programs")
                    .join("Startup"),
            )
            .unwrap();
        }

        let port = if scen == "S2_healthy_core" {
            core_port
        } else {
            free_port()
        };
        let norm = Normalizer {
            data_dir: dir.to_string_lossy().to_string(),
            appdata: appdata.to_string_lossy().to_string(),
            exe: std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            exe_dir: std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            port: port.to_string(),
        };

        let app = Arc::new(App::new("127.0.0.1", &port.to_string(), ""));
        let host = if plugins_test {
            // 插件场景走**测试注册表** + **假后端**作为 spawn 目标：
            //   * 生产目标（本 exe）由单测 `production_spawn_target_is_this_exe` 与
            //     `start_spawns_self_submode_with_go_arg_order` 锁住（进程形态、argv 逐位）；
            //   * 这里的差分比的是「spawn 之后的行为」：端口清理、退出监视、失败原因文案。
            //     Go 采集器 spawn 的是测试二进制（未定义 flag → 退出码 2），Rust 侧 spawn
            //     的是假后端（同参数、同退出码 2），两侧被观测的输出因此逐字节可比。
            let seam = SpawnTarget {
                exe: Some(PathBuf::from("node")),
                prefix: vec![fixture_js().to_string_lossy().to_string()],
            };
            std::env::set_var("RS_GUI_FAKE_MODE", "exit");
            std::env::set_var("RS_GUI_FAKE_EXIT", "2");
            Arc::new(PluginHost::new(
                test_defs_from_golden(&golden["plugin_test_defs"]),
                seam,
            ))
        } else {
            Arc::new(PluginHost::production())
        };
        let bridge = Bridge::new(Arc::clone(&app), Arc::new(NoWindowHost))
            .with_clipboard(Arc::new(NullClipboard))
            .with_plugins(Arc::clone(&host));

        // S1 的两条前置与 Go 采集器逐条一致：
        //   ① 先探一次 ccGetState（吞掉首次探活的时序差，结果丢弃）
        //   ② 把「最近一次踢取」置为现在（等价 GUI 刚启动拉过一轮，不触发后台拉取）
        if scen == "S1_fresh" {
            let _ = bridge.dispatch("ccGetState", &[]);
            app.update.lock().unwrap().last_fetch = Some(Instant::now());
        }

        for step in data["steps"].as_array().expect("场景缺 steps") {
            let cmd = step["cmd"].as_str().unwrap().to_string();
            let args: Vec<Value> = match step["args"].as_array() {
                Some(a) => a.clone(),
                None => Vec::new(),
            };
            // 退出监视是异步的：黄金在收敛点记录，这边也等同一个点再派发
            if let Some(id) = step["wait_plugin_idle"].as_str() {
                wait_plugin_idle(&host, id);
            }
            covered.insert(cmd.clone());
            let want_raw = step["out"].as_str().unwrap();
            let got_raw = bridge
                .dispatch(&cmd, &args)
                .unwrap_or_else(|e| panic!("{scen}/{cmd} 派发失败: {e}"));

            // 例外 4：ccLiveSnapshot 的取值随本机端口状态变化 —— 只比键集与 ok 形状
            if cmd == "ccLiveSnapshot" {
                let g: Value = serde_json::from_str(want_raw).unwrap();
                let r: Value = serde_json::from_str(&got_raw).unwrap();
                assert_eq!(g["ok"], r["ok"], "ccLiveSnapshot 的 ok 必须一致");
                let gk: BTreeSet<String> = g["live"].as_object().unwrap().keys().cloned().collect();
                let mut rk: BTreeSet<String> = r["live"].as_object().unwrap().keys().cloned().collect();
                // 2026-10-20：Rust 侧新增 `qoder-intl`（Qoder 国际版，8791），而 Go 侧
                // `pluginDefs` 仍是 7 条，黄金基准因此少一个键。这里**只放行这一个已知新增键**，
                // 其余任何键差异照旧判红——不放宽标准，只是把「Rust 侧已知扩展」显式登记出来。
                rk.remove(RUST_ONLY_LIVE_KEYS[0]);
                assert_eq!(gk, rk, "ccLiveSnapshot 的探活目标键集必须一致（已扣除 Rust 侧已知新增键）");
                live_snapshot_keys = gk;
                checked_steps += 1;
                continue;
            }

            let mut want = norm.norm(want_raw);
            let mut got = norm.norm(&got_raw);

            // 例外 1：JSON 解析错误文案（仅 ccKeyMapSet 的非法 JSON 输入那一步）
            if cmd == "ccKeyMapSet" {
                let raw = args.first().and_then(|v| v.as_str()).unwrap_or("");
                if serde_json::from_str::<Value>(raw).is_err() {
                    want = replace_msg(&want, "$JSONERR");
                    got = replace_msg(&got, "$JSONERR");
                }
            }

            checked_steps += 1;
            if want != got {
                failures.push(format!(
                    "[{scen}] {cmd}({args:?})\n    Go   : {want}\n    Rust : {got}"
                ));
            }
        }

        // 落盘对照：格式（明文/密文）与逻辑内容都要一致
        let excludes: Vec<String> = data["files_exclude"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default();
        if let Some(files) = data["files"].as_object() {
            for (rel, go_raw) in files {
                // 例外 2：诊断日志（带时间戳、内容取决于 icacls 是否在 PATH 上）
                if rel.ends_with("appdir.log") || excludes.contains(rel) {
                    continue;
                }
                let go_text = go_raw.as_str().unwrap();
                let (go_protected, go_plain) = logical_content(go_text.as_bytes());
                let path: PathBuf = if let Some(stripped) = rel.strip_prefix("$APPDATA/") {
                    appdata.join(stripped)
                } else {
                    dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
                };
                let Ok(rust_raw) = std::fs::read(&path) else {
                    failures.push(format!("[{scen}] 文件 {rel} 在 Rust 侧不存在"));
                    continue;
                };
                let (rust_protected, rust_plain) = logical_content(&rust_raw);
                if rust_protected != go_protected {
                    failures.push(format!(
                        "[{scen}] 文件 {rel} 落盘形态不一致：Go protected={go_protected}，Rust protected={rust_protected}"
                    ));
                    continue;
                }
                let go_norm = norm.norm(&String::from_utf8_lossy(&go_plain));
                let rust_norm = norm.norm(&String::from_utf8_lossy(&rust_plain));
                if go_norm != rust_norm {
                    failures.push(format!(
                        "[{scen}] 文件 {rel} 逻辑内容不一致：\n    Go   : {go_norm:?}\n    Rust : {rust_norm:?}"
                    ));
                }
            }
        }
    }

    // ---- 存在性补跑：把 30 条都真调一遍（黄金场景覆盖不全的补上）----
    // 用独立数据目录 + 假核心端口；ccProbeKey 在「无 key」态先跑（不产生真实网络请求）。
    let pres_dir = root.join("presence");
    std::fs::create_dir_all(&pres_dir).unwrap();
    std::env::set_var(appdir::DATA_DIR_ENV, &pres_dir);
    let pres_port = core_port;
    let app = Arc::new(App::new("127.0.0.1", &pres_port.to_string(), ""));
    let pres_host = Arc::new(PluginHost::new(
        test_defs_from_golden(&golden["plugin_test_defs"]),
        SpawnTarget {
            exe: Some(PathBuf::from("node")),
            prefix: vec![fixture_js().to_string_lossy().to_string()],
        },
    ));
    let bridge = Bridge::new(Arc::clone(&app), Arc::new(NoWindowHost))
        .with_clipboard(Arc::new(NullClipboard))
        .with_plugins(Arc::clone(&pres_host));
    let presence: Vec<(&str, Vec<Value>)> = vec![
        ("ccGetState", vec![]),
        ("ccLiveSnapshot", vec![]),
        ("ccProbeKey", vec![]),
        ("ccSaveKey", vec![Value::String("PRESENCE-KEY".into())]),
        ("ccStart", vec![Value::String("".into())]),
        ("ccStop", vec![]),
        ("ccStats", vec![]),
        ("ccUsage", vec![]),
        ("ccUsageRefresh", vec![]),
        ("ccCheckUpdate", vec![]),
        ("ccCheckUpdateForce", vec![]),
        ("ccOneKeyUpdate", vec![Value::String("status".into())]),
        ("ccCalib", vec![Value::String("".into()), Value::String("".into())]),
        ("ccModels", vec![Value::String("".into())]),
        ("ccAutostart", vec![Value::Bool(false)]),
        ("ccOpen", vec![Value::String("file:///x".into())]),
        ("ccCopy", vec![Value::String("clip".into())]),
        ("ccDismiss", vec![]),
        ("ccDismissCloseHint", vec![]),
        ("ccWindowCmd", vec![Value::String("min".into())]),
        ("ccRequestClose", vec![]),
        ("ccCloseChoice", vec![Value::String("tray".into()), Value::Bool(false)]),
        ("ccCloseModeGet", vec![]),
        ("ccCloseModeSet", vec![Value::String("ask".into())]),
        ("ccKeyMapGet", vec![]),
        ("ccKeyMapSet", vec![Value::String(r#"[["cmd"]]"#.into())]),
        // 插件托管 4 条（用测试注册表；start/stop 只走「未知 id」与「无监听」分支，
        // 绝不 spawn、绝不碰现行端口）
        ("ccPluginList", vec![]),
        ("ccPluginLog", vec![Value::String("zz-alpha".into()), Value::from(5)]),
        ("ccPluginStart", vec![Value::String("zz-nope".into())]),
        ("ccPluginStop", vec![Value::String("zz-nope".into())]),
    ];
    for (cmd, args) in &presence {
        covered.insert((*cmd).to_string());
        if let Err(e) = bridge.dispatch(cmd, args) {
            failures.push(format!("[presence] {cmd} 未实现：{e}"));
        }
    }

    // 覆盖度断言：30 个绑定名全部被真调用过（黄金场景 + 存在性补跑）
    let mut all: BTreeSet<String> = BINDINGS.iter().map(|s| s.to_string()).collect();
    all.extend(PLUGIN_BINDINGS.iter().map(|s| s.to_string()));
    let missing: Vec<&String> = all.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "这些绑定没有在门禁里被真调用过：{missing:?}"
    );

    // ---- spawn 出去的 argv：假后端记下来的必须逐位正确 --------------------------
    // （Go 侧 spawn 的是测试二进制、argv 无法被观测；这一条是 Rust 侧的补强，
    //   与单测 `start_spawns_self_submode_with_go_arg_order` 同一判据。）
    std::env::remove_var("RS_GUI_FAKE_MODE");
    std::env::remove_var("RS_GUI_FAKE_EXIT");
    std::env::remove_var("RS_GUI_FAKE_LOG");
    match std::fs::read_to_string(&spawn_argv_log) {
        Ok(raw) => {
            let v: Value = serde_json::from_str(&raw).expect("假后端记录的 argv 不是 JSON");
            let args: Vec<String> = v["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect();
            assert_eq!(
                args,
                vec![
                    "--plugin-zz-beta".to_string(),
                    "--port".to_string(),
                    "19789".to_string()
                ],
                "被 spawn 的子进程 argv 必须逐位是 --plugin-<native> --port <n>（zz-beta 无附加参数）"
            );
            checked_steps += 1;
        }
        Err(e) => failures.push(format!(
            "[PL3] 假后端没记下 spawn argv（{e}）：spawn 路径没真跑起来"
        )),
    }

    // ---- 换血标记语义（需要数据目录，故并进本测试，避免两个用例抢进程级 env）----
    let marker_dir = root.join("marker");
    std::fs::create_dir_all(&marker_dir).unwrap();
    std::env::set_var(appdir::DATA_DIR_ENV, &marker_dir);
    for case in golden["marker_cases"].as_array().expect("黄金缺 marker_cases") {
        let name = case["name"].as_str().unwrap();
        let path = rs_gui::update::update_marker_path();
        let _ = std::fs::remove_file(&path);
        if let Some(pre) = case["pre"].as_str() {
            std::fs::write(&path, pre.as_bytes()).unwrap();
        }
        let (to, ok) = rs_gui::update::consume_update_marker();
        let gone = !path.exists();
        let want_to = case["want_to"].as_str().unwrap();
        let want_ok = case["want_ok"].as_bool().unwrap();
        let want_gone = case["want_file_gone"].as_bool().unwrap();
        if to != want_to || ok != want_ok || gone != want_gone {
            failures.push(format!(
                "[marker/{name}] 期望 ({want_to:?},{want_ok},gone={want_gone}) 实得 ({to:?},{ok},gone={gone})"
            ));
        }
        checked_steps += 1;
    }

    std::env::remove_var(appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&root);
    drop(held_listeners);
    drop(node_holders);

    assert!(
        failures.is_empty(),
        "Go↔Rust 桥接差分不一致（{} 处，共比 {} 步）：\n{}\n（ccLiveSnapshot 只比键集：{live_snapshot_keys:?}）",
        failures.len(),
        checked_steps,
        failures.join("\n")
    );
}

/// 退出原因（`pluginExitReason`）的纯函数用例：逐条按 Go 采集的期望值比。
/// 这批用例含超 300 字节/多字节截断/非法 UTF-8 的期望值——按**值**比（见黄金的豁免 6），
/// 故这里用 `serde_json` 解出的字符串直接比。
#[test]
fn exit_reason_cases_match_go() {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    let defs = test_defs_from_golden(&golden["plugin_test_defs"]);
    let def = defs.first().expect("黄金缺测试注册表").clone();
    let dir = std::env::temp_dir().join(format!("rs-gui-p7b-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (i, case) in golden["exit_reason_cases"]
        .as_array()
        .expect("黄金缺 exit_reason_cases")
        .iter()
        .enumerate()
    {
        let name = case["name"].as_str().unwrap();
        let err_log = dir.join(format!("case-{i}.log"));
        match case["err_log"].as_str() {
            Some(content) => std::fs::write(&err_log, content.as_bytes()).unwrap(),
            None => {
                let _ = std::fs::remove_file(&err_log);
            }
        }
        let wait_err = case["wait_err"].as_str().filter(|s| !s.is_empty());
        let want = case["want"].as_str().unwrap();
        let got = plugins::plugin_exit_reason(&err_log, wait_err, &def);
        checked += 1;
        if got != want {
            failures.push(format!("[exit_reason/{name}]\n    Go   : {want:?}\n    Rust : {got:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "退出原因文案与 Go 不一致（{checked} 条）：\n{}",
        failures.join("\n")
    );
    assert!(checked >= 8, "退出原因用例条数不该少（实际 {checked}）");
}

/// StampedWriter 的分行/打戳口径（Go `appdir.StampedWriter` 同款用例）。
#[test]
fn stamped_writer_cases_match_go() {
    use std::io::Write;

    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    let dir = std::env::temp_dir().join(format!("rs-gui-p7b-stamp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (i, case) in golden["stamped_writer_cases"]
        .as_array()
        .expect("黄金缺 stamped_writer_cases")
        .iter()
        .enumerate()
    {
        let name = case["name"].as_str().unwrap();
        let path = dir.join(format!("stamp-{i}.log"));
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut w = appdir::StampedWriter::new(path.clone(), Some(f));
        for chunk in case["writes"].as_array().unwrap() {
            w.write_all(chunk.as_str().unwrap().as_bytes()).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        let got = norm_stamp(&String::from_utf8_lossy(&std::fs::read(&path).unwrap()));
        let want = case["want"].as_str().unwrap();
        checked += 1;
        if got != want {
            failures.push(format!("[stamped_writer/{name}]\n    Go   : {want:?}\n    Rust : {got:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "StampedWriter 与 Go 不一致（{checked} 条）：\n{}",
        failures.join("\n")
    );
    assert!(checked >= 6, "StampedWriter 用例条数不该少（实际 {checked}）");
}

/// 落盘目录不得被门禁污染（防手滑把数据目录指到用户真实目录）。
#[test]
fn golden_pins_the_expected_go_versions() {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    // 金标是**Go 侧**采收的，里面的 `app_version` 是采收当时的 Go 值（钉字面量：
    // 金标一重采就该有人来看一眼）。Rust 侧 v4.0.0 起即产品本体、版本号与 Go
    // 有意分道，故这里**不再**要求两侧同值——只要求 Rust 侧是个正常版本串。
    assert_eq!(golden["app_version"].as_str().unwrap(), "v3.20.10");
    assert!(
        rs_gui::APP_VERSION.starts_with('v') && rs_gui::APP_VERSION.len() > 2,
        "Rust 侧版本串形态不对：{:?}",
        rs_gui::APP_VERSION
    );
    assert_eq!(
        golden["core_version"].as_str().unwrap(),
        rs_gui::CORE_VERSION
    );
    assert!(
        Path::new("ui.html").exists(),
        "rs-gui 根下必须有逐字节复制的 ui.html"
    );
    assert!(
        fixture_js().exists(),
        "插件场景的假后端脚本必须随仓库带着"
    );
}

/// 假后端是 Spawn/差分证据链的**测量仪器**：它的源码一改，所有建立在它上面的
/// argv 记录与 serve 行为观测都不再可比。黄金里记下采集时那份源码的 SHA-256
/// （`fake_backend_sha256`），这里对**入库的那份**做真校验——仪器漂移必红。
/// （rs-qoder P3 补修第 2 条立下的本项目纪律，P7b 审查点名补上。）
#[test]
fn fake_backend_source_hash_matches_golden() {
    let p = fixture_js();
    let body = std::fs::read(&p).unwrap_or_else(|e| panic!("读不到假后端 {}: {e}", p.display()));
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    let want = golden["fake_backend_sha256"]
        .as_str()
        .expect("黄金缺 fake_backend_sha256")
        .to_ascii_lowercase();
    assert_eq!(
        sha256_hex(&body),
        want,
        "入库假后端的源码哈希与黄金记录不一致（改了仪器就得连带重采基准，见 tests/golden/README.md）"
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
