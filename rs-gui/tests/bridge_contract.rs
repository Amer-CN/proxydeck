//! bridge_contract.rs —— P7a 的主门禁：**Go 宿主与 Rust 宿主的桥接层差分对照**。
//!
//! 参考值来源 `tests/golden/bridge-go-reference.json`：由
//! `.work/rust-rewrite/_p7a_collect_test.go`（同内容留档）跑**真实 Go 闭包**采出来的——
//! 采集器给 `webview.WebView` 接口写了假实现，让生产 `app.bindAll(fake)` 把 26 个闭包
//! 原样交出来，再用 reflect 按位置调用（不是对闭包体的转抄）。
//!
//! 本测试把黄金里的**同一串命令、同一套场景前置**再跑一遍（Rust 侧），逐条比对：
//!   * 字符串相等（归一化后逐字节）——键名、键序、文案、缺省值全在比对范围内
//!   * 落盘文件：**格式**（明文 / `PDAPI1` 密文）与**逻辑内容**（密文按 DPAPI 解回明文比）
//!
//! 四条例外（在黄金的 `comparison_exemptions` 里也登记着，不做静默豁免）：
//!   1. `ccKeyMapSet` 的 JSON 解析错误文案（Go `encoding/json` 与 serde_json 的文本不同）
//!   2. `appdir.log`（带时间戳、内容取决于 icacls 是否在 PATH 上）
//!   3. `PDAPI1` 密文字节本身（DPAPI 每次产出不同；按能否解回同一明文比）
//!   4. `ccLiveSnapshot` 的取值（随本机插件端口实时状态变化；只比键集与 ok 形状）

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use rs_gui::appdir;
use rs_gui::bridge::{Bridge, NullClipboard, BINDINGS};
use rs_gui::host::NoWindowHost;
use rs_gui::state::App;

const GOLDEN: &str = include_str!("golden/bridge-go-reference.json");

/* ---------------- 归一化（与 Go 采集器同一套哨兵） ---------------- */

struct Normalizer {
    data_dir: String,
    appdata: String,
    exe: String,
    port: String,
}

impl Normalizer {
    fn norm(&self, s: &str) -> String {
        let mut out = s.to_string();
        out = out.replace(&self.exe, "$EXE");
        out = out.replace(&self.data_dir, "$DATA");
        out = out.replace(&self.appdata, "$APPDATA");
        if !self.port.is_empty() {
            out = out.replace(&format!("127.0.0.1:{}", self.port), "127.0.0.1:$PORT");
        }
        out = replace_digits(&out, "\"pid\":", "0");
        out = replace_digits(&out, "\"uptime\":", "0");
        out = replace_digits(&out, "\"port\":\"", "$PORT");
        out = replace_digits(&out, "-port ", "$PORT");
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
                // 读到请求头结束
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => return,
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let uri = head
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                // 有 body 就接着读干净（不然客户端可能看到连接被重置）
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

/* ---------------- 场景前置（与 Go 采集器逐条对齐） ---------------- */

fn scenario_pre_files(scen: &str) -> Vec<(&'static str, &'static str)> {
    match scen {
        "S3_files_present" => vec![
            ("close_mode.txt", "tray"),
            (
                "keymap.json",
                r#"[["cmd"],["tj","comate"],["cb","qoder"],["ext"]]"#,
            ),
            ("notice_dismissed.flag", "1"),
            ("close_hint_dismissed.flag", "1"),
            ("stats.json", r#"{"started":42,"total":9}"#),
            ("api-key.txt", "SAVED-KEY"),
        ],
        "S5_keymap_bad_file" => vec![("keymap.json", r#"[["cmd"],["nope"]]"#)],
        _ => Vec::new(),
    }
}

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

/* ---------------- 主门禁 ---------------- */

#[test]
fn go_and_rust_bridge_agree_command_by_command() {
    let golden: Value = serde_json::from_str(GOLDEN).expect("黄金 JSON 解析失败");

    // (0) 26 个绑定名与顺序必须与 Go 侧一致
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

    let scenarios = golden["scenarios"].as_object().expect("黄金缺 scenarios");
    for (scen, data) in scenarios {
        let dir = root.join(scen);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);

        for (name, content) in scenario_pre_files(scen) {
            std::fs::write(dir.join(name), content.as_bytes()).unwrap();
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
            port: port.to_string(),
        };

        let app = Arc::new(App::new("127.0.0.1", &port.to_string(), ""));
        let bridge = Bridge::new(Arc::clone(&app), Arc::new(NoWindowHost))
            .with_clipboard(Arc::new(NullClipboard));

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
                let gk: BTreeSet<String> = g["live"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect();
                let rk: BTreeSet<String> = r["live"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect();
                assert_eq!(gk, rk, "ccLiveSnapshot 的探活目标键集必须一致");
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
        if let Some(files) = data["files"].as_object() {
            for (rel, go_raw) in files {
                // 例外 2：诊断日志（带时间戳、内容取决于 icacls 是否在 PATH 上）
                if rel.ends_with("appdir.log") {
                    continue;
                }
                let go_text = go_raw.as_str().unwrap();
                let (go_protected, go_plain) = logical_content(go_text.as_bytes());
                let path: PathBuf = if let Some(stripped) = rel.strip_prefix("$APPDATA/") {
                    appdata.join(stripped)
                } else {
                    dir.join(rel)
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

    // ---- 存在性补跑：把 26 条都真调一遍（黄金场景只覆盖了除 ccCopy 以外的 25 条）----
    // 用独立数据目录 + 假核心端口；ccProbeKey 在「无 key」态先跑（不产生真实网络请求）。
    let pres_dir = root.join("presence");
    std::fs::create_dir_all(&pres_dir).unwrap();
    std::env::set_var(appdir::DATA_DIR_ENV, &pres_dir);
    let pres_port = core_port;
    let app = Arc::new(App::new("127.0.0.1", &pres_port.to_string(), ""));
    let bridge = Bridge::new(Arc::clone(&app), Arc::new(NoWindowHost))
        .with_clipboard(Arc::new(NullClipboard));
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
    ];
    for (cmd, args) in &presence {
        covered.insert((*cmd).to_string());
        if let Err(e) = bridge.dispatch(cmd, args) {
            failures.push(format!("[presence] {cmd} 未实现：{e}"));
        }
    }

    // 覆盖度断言：26 个绑定名全部被真调用过（黄金场景 + 存在性补跑）
    let all: BTreeSet<String> = BINDINGS.iter().map(|s| s.to_string()).collect();
    let missing: Vec<&String> = all.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "这些绑定没有在门禁里被真调用过：{missing:?}"
    );

    std::env::remove_var(appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        failures.is_empty(),
        "Go↔Rust 桥接差分不一致（{} 处，共比 {} 步）：\n{}\n（ccLiveSnapshot 只比键集：{live_snapshot_keys:?}）",
        failures.len(),
        checked_steps,
        failures.join("\n")
    );
}

/// 落盘目录不得被门禁污染（防手滑把数据目录指到用户真实目录）。
#[test]
fn golden_pins_the_expected_go_versions() {
    let golden: Value = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(golden["app_version"].as_str().unwrap(), rs_gui::APP_VERSION);
    assert_eq!(
        golden["core_version"].as_str().unwrap(),
        rs_gui::CORE_VERSION
    );
    assert!(
        Path::new("ui.html").exists(),
        "rs-gui 根下必须有逐字节复制的 ui.html"
    );
}
