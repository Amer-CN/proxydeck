//! dispatch.rs —— flag 分发骨架（对应 Go `app/main.go:210-241` 的 `main` 分发 +
//! `app/plugin_modes.go` 的各子模式分支）。
//!
//! 形态（用户裁决）：**一个 exe 多子模式**——
//!   * 无参                     → GUI（[`crate::webview::run_gui`]）
//!   * `--plugin-<id> --port N` → 对应 crate 的 `run(args)`（库依赖，进程内直接跑）
//!   * `-headless`              → `rs-proxy`（主代理核心）
//!   * `-version`               → 打印版本退出（Go 版同款文案）
//!
//! 两条实现纪律：
//!   1. **参数口径不重造**：子模式的 argv 原样交给各 crate 自己的 clap `Args` 解析器
//!      （参数名、默认值、校验与单独跑那个 crate 时同源）。
//!   2. Go 侧 `-flag` 与 `--flag` 两种写法都认（Go `flag` 包如此）；这里统一把
//!      「单横线长选项」规范化成 `--` 再交给 clap。

use std::future::Future;

use clap::Parser;

use crate::appdir;
use crate::plugins;

/// 插件启动失败诊断（Go `plugin_modes.go` 的 `pluginFailLog`）：带时间戳、追加写、
/// 按 1 MB × 3 轮转，落数据目录 `<id>-plugin-error.log`——GUI 的退出监视就是靠
/// 读这个文件给出「启动失败：…」的可读原因。
fn plugin_fail_log(id: &str, msg: &str) {
    let _ = appdir::append_log(&plugins::err_log_path_for_id(id), &format!("启动失败：{msg}"));
}

/// 分发目标。
#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    /// 无参启动：GUI 窗口
    Gui,
    /// `-headless`：主代理核心（rs-proxy）
    Proxy,
    /// `--plugin-<id>`：对应插件 crate
    Plugin(&'static str),
    /// `--fax`：注水专线独立浮窗（P7c，尚未落地）
    Fax,
    /// `-version`：打印版本退出（Go `main.go:216-219`）
    Version,
}

/// 插件 id → 承载它的 Rust crate 名（`None` = 该插件**没有** Rust 实现）。
pub fn plugin_crate(id: &str) -> Option<&'static str> {
    match id {
        "bai" => Some("rs-bai"),
        "codebuddy" | "codebuddy-intl" => Some("rs-codebuddy"),
        "comate" => Some("rs-comate"),
        "qoder" => Some("rs-qoder"),
        "vibex" => Some("rs-vibex"),
        // 团结（tuanjie / 8788）**还没有 Rust 实现**：Go 侧实现留在 internal/tuanjie，
        // Rust 侧没有对应 crate。子模式如实报「尚未实现」，绝不静默当成功。
        "tuanjie" => None,
        _ => None,
    }
}

/// 本分发器认得的插件子模式 flag（与 Go `plugin_modes.go` 的**分支顺序**逐个对齐：
/// vibex → tuanjie → codebuddy → codebuddy-intl → bai → comate → qoder。
/// 顺序只在多个 `--plugin-*` 并存时有意义（与 Go 同：vibex 优先）。}
pub const PLUGIN_FLAGS: [&str; 7] = [
    "plugin-vibex",
    "plugin-tuanjie",
    "plugin-codebuddy",
    "plugin-codebuddy-intl",
    "plugin-bai",
    "plugin-comate",
    "plugin-qoder",
];

/// 把 Go 风格的「单横线长选项」规范化为 clap 认的 `--flag`（短选项/负值原样保留）。
pub fn normalize(argv: &[String]) -> Vec<String> {
    argv.iter()
        .map(|a| {
            let bytes = a.as_bytes();
            if bytes.len() > 2 && bytes[0] == b'-' && bytes[1] != b'-' {
                format!("-{a}")
            } else {
                a.clone()
            }
        })
        .collect()
}

fn has_flag(norm: &[String], flag: &str) -> bool {
    let long = format!("--{flag}");
    norm.iter().any(|a| a == &long)
}

/// 判定分发目标（只看 flag 名，不解析值——值交给各 crate 的 clap）。
pub fn plan(argv: &[String]) -> Mode {
    let norm = normalize(argv);
    if has_flag(&norm, "version") {
        return Mode::Version;
    }
    for flag in PLUGIN_FLAGS {
        if has_flag(&norm, flag) {
            let id = flag.trim_start_matches("plugin-");
            return Mode::Plugin(id);
        }
    }
    // Go `main.go` 的先后：fax → headless（`--fax -headless` 并存时进浮窗分支）。
    if has_flag(&norm, "fax") {
        return Mode::Fax;
    }
    if has_flag(&norm, "headless") {
        return Mode::Proxy;
    }
    Mode::Gui
}

/// 产出交给某个插件 crate 的 argv（第 0 位是程序名，与 clap `parse_from` 口径一致）。
///
/// 三处翻译，其余原样透传：
///   * `--plugin-codebuddy-intl` → `--plugin-codebuddy --region INTL`（rs-codebuddy 用 region 区分）
///   * `--portable`：只留给接受它的 crate（rs-vibex）；其余 crate 由调用方改用
///     `PROXYDECK_DATA_DIR` 环境变量保证父子数据目录一致
///   * 其它未知 flag 原样带上——由 crate 自己的 clap 如实报错，不在这里静默丢弃
pub fn plugin_argv(prog: &str, plugin_id: &str, argv: &[String]) -> Vec<String> {
    let mut out = vec![prog.to_string()];
    let norm = normalize(argv);
    let accepts_portable = matches!(plugin_id, "vibex");
    for a in &norm {
        if a == "--plugin-codebuddy-intl" {
            out.push("--plugin-codebuddy".to_string());
            out.push("--region".to_string());
            out.push("INTL".to_string());
            continue;
        }
        if a == "--portable" && !accepts_portable {
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// 产出交给 `rs-proxy` 的 argv：去掉 Go 层的 `-headless`（那是「跑代理」的选择器，
/// 不是 rs-proxy 的参数），其余原样。
pub fn proxy_argv(prog: &str, argv: &[String]) -> Vec<String> {
    let mut out = vec![prog.to_string()];
    for a in normalize(argv) {
        if a == "--headless" {
            continue;
        }
        out.push(a);
    }
    out
}

/// 取 `-flag value` / `--flag value` / `--flag=value` 的值（Go `flag` 包的 `-port 55990`
/// 与 `-port=55990` 两种写法都认）。
pub fn flag_value(argv: &[String], name: &str) -> Option<String> {
    let norm = normalize(argv);
    let long = format!("--{name}");
    let eq = format!("--{name}=");
    let mut it = norm.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix(&eq) {
            return Some(v.to_string());
        }
        if a == &long {
            return it.next().cloned();
        }
    }
    None
}

fn block_on<F: Future<Output = Result<(), String>>>(f: F) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("创建 tokio 运行时失败: {e}"))?;
    rt.block_on(f)
}

/// 进程入口（`main` 只做 `std::process::exit(dispatch::run(...))`）。
pub fn run(argv: Vec<String>) -> i32 {
    let prog = argv
        .first()
        .cloned()
        .unwrap_or_else(|| "rs-gui".to_string());
    let args: Vec<String> = argv.iter().skip(1).cloned().collect();

    let mode = plan(&args);
    // 便携模式必须在任何状态文件读写之前定下来（Go `runDataDirInit` 同位置）
    appdir::init(args.iter().any(|a| a == "-portable" || a == "--portable"));

    match mode {
        Mode::Version => {
            println!("ProxyDeck {} (proxy core {})", crate::APP_VERSION, crate::CORE_VERSION);
            0
        }

        Mode::Plugin(id) => {
            let Some(krate) = plugin_crate(id) else {
                let msg = format!(
                    "rs-gui: --plugin-{id} 尚无 Rust 实现（该插件仍由 Go 侧 internal/{id} 承载），本 exe 无法提供该子模式"
                );
                // 与 Go 侧同源：子模式起不来就把原因写进诊断文件，GUI 的退出监视读它，
                // 界面显示「启动失败：…」而不是停在「启动中」。
                plugin_fail_log(id, &msg);
                eprintln!("{msg}");
                return 1;
            };
            // 便携模式下强制把数据目录钉给子模式进程内的各 crate（它们多数没有 --portable）
            if appdir::portable() {
                std::env::set_var(appdir::DATA_DIR_ENV, appdir::data_dir());
            }
            let sub = plugin_argv(&prog, id, &args);
            let r = match krate {
                "rs-bai" => match rs_bai::Args::try_parse_from(&sub) {
                    Ok(a) => block_on(rs_bai::run(a)),
                    Err(e) => Err(e.to_string()),
                },
                "rs-codebuddy" => match rs_codebuddy::Args::try_parse_from(&sub) {
                    Ok(a) => block_on(rs_codebuddy::run(a)),
                    Err(e) => Err(e.to_string()),
                },
                "rs-comate" => match rs_comate::Args::try_parse_from(&sub) {
                    Ok(a) => block_on(rs_comate::run(a)),
                    Err(e) => Err(e.to_string()),
                },
                "rs-qoder" => match rs_qoder::Args::try_parse_from(&sub) {
                    Ok(a) => block_on(rs_qoder::run(a)),
                    Err(e) => Err(e.to_string()),
                },
                "rs-vibex" => match rs_vibex::Args::try_parse_from(&sub) {
                    Ok(a) => {
                        // 与单独跑 rs-vibex 时同一条护栏与文案（退出码 2）
                        if !a.plugin_vibex {
                            eprintln!("请用 --plugin-vibex 启动（VibeX 插件服务模式）");
                            return 2;
                        }
                        block_on(rs_vibex::run(a))
                    }
                    Err(e) => Err(e.to_string()),
                },
                other => Err(format!("未接入的 crate: {other}")),
            };
            if let Err(e) = r {
                plugin_fail_log(id, &e);
                eprintln!("{krate}: 启动失败: {e}");
                return 1;
            }
            0
        }

        Mode::Proxy => {
            let sub = proxy_argv(&prog, &args);
            match rs_proxy::Args::try_parse_from(&sub) {
                Ok(a) => match block_on(rs_proxy::run(a)) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("Server failed: {e}");
                        1
                    }
                },
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }

        Mode::Fax => {
            eprintln!("rs-gui: --fax（注水专线独立浮窗）尚未实现（P7c）");
            1
        }

        Mode::Gui => {
            // 监听口径与 Go `main.go` 的 flag 缺省值一致：-port 55990 / -host 127.0.0.1 / -api-key ""
            let host = flag_value(&args, "host").unwrap_or_else(|| "127.0.0.1".to_string());
            let port = flag_value(&args, "port").unwrap_or_else(|| "55990".to_string());
            let key = flag_value(&args, "api-key").unwrap_or_default();
            let app = std::sync::Arc::new(crate::state::App::new(&host, &port, &key));
            // GUI 启动前先解析数据目录（Go `runDataDirInit` 的位置）
            if appdir::portable() {
                std::env::set_var(appdir::DATA_DIR_ENV, appdir::data_dir());
            }
            match crate::webview::run_gui(app) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("rs-gui: {e}");
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plan_matches_go_dispatch_order() {
        assert_eq!(plan(&v(&[])), Mode::Gui);
        assert_eq!(plan(&v(&["-version"])), Mode::Version);
        assert_eq!(plan(&v(&["-headless"])), Mode::Proxy);
        assert_eq!(plan(&v(&["--headless", "-port", "55990"])), Mode::Proxy);
        assert_eq!(plan(&v(&["--plugin-bai", "--port", "8891"])), Mode::Plugin("bai"));
        assert_eq!(
            plan(&v(&["--plugin-codebuddy-intl", "--port", "8789"])),
            Mode::Plugin("codebuddy-intl")
        );
        assert_eq!(plan(&v(&["--plugin-tuanjie", "--port", "8788"])), Mode::Plugin("tuanjie"));
        assert_eq!(plan(&v(&["--fax"])), Mode::Fax);
        // Go 侧 `-plugin-*` 单横线写法同样要认
        assert_eq!(plan(&v(&["-plugin-qoder", "-port", "18785"])), Mode::Plugin("qoder"));
        // 并存时的先后与 Go 一致（fax → headless；vibex 优先于其它插件 flag）
        assert_eq!(plan(&v(&["--fax", "-headless"])), Mode::Fax);
        assert_eq!(
            plan(&v(&["--plugin-bai", "--plugin-vibex"])),
            Mode::Plugin("vibex")
        );
    }

    #[test]
    fn normalize_turns_single_dash_long_options_into_double() {
        assert_eq!(
            normalize(&v(&["-port", "8788", "--host", "127.0.0.1", "-h"])),
            v(&["--port", "8788", "--host", "127.0.0.1", "-h"])
        );
    }

    #[test]
    fn plugin_argv_translates_intl_and_portable() {
        let a = plugin_argv("rs-gui", "codebuddy-intl", &v(&["--plugin-codebuddy-intl", "--port", "18789", "--desensitize"]));
        assert_eq!(
            a,
            v(&["rs-gui", "--plugin-codebuddy", "--region", "INTL", "--port", "18789", "--desensitize"])
        );
        // 不带 --portable 的 crate：丢掉该参数（改用 PROXYDECK_DATA_DIR 保证同目录）
        let b = plugin_argv("rs-gui", "bai", &v(&["--plugin-bai", "--port", "18891", "--portable"]));
        assert_eq!(b, v(&["rs-gui", "--plugin-bai", "--port", "18891"]));
        // vibex 接受 --portable：原样保留
        let c = plugin_argv("rs-gui", "vibex", &v(&["--plugin-vibex", "--port", "18790", "--portable"]));
        assert_eq!(c, v(&["rs-gui", "--plugin-vibex", "--port", "18790", "--portable"]));
    }

    #[test]
    fn proxy_argv_drops_the_headless_selector() {
        assert_eq!(
            proxy_argv("rs-gui", &v(&["-headless", "-port", "18788", "-api-key", "x"])),
            v(&["rs-gui", "--port", "18788", "--api-key", "x"])
        );
    }

    /// 最强的一层：翻译后的 argv 必须能被**各 crate 自己的 clap 解析器**吃下去，
    /// 且关键字段落对位置（参数口径不重造的证据）。
    #[test]
    fn translated_argv_parses_with_each_crates_own_parser() {
        use clap::Parser;

        let a = rs_bai::Args::try_parse_from(plugin_argv(
            "rs-gui",
            "bai",
            &v(&["--plugin-bai", "--port", "18891"]),
        ))
        .expect("rs-bai 必须接受分发器翻译出的 argv");
        assert_eq!(a.port, 18891);
        assert!(a.plugin_bai);

        let a = rs_codebuddy::Args::try_parse_from(plugin_argv(
            "rs-gui",
            "codebuddy-intl",
            &v(&["--plugin-codebuddy-intl", "--port", "18789", "--desensitize"]),
        ))
        .expect("rs-codebuddy 必须接受翻译后的 argv");
        assert_eq!(a.port, 18789);
        assert_eq!(a.region, "INTL");
        assert!(a.desensitize);

        let a = rs_comate::Args::try_parse_from(plugin_argv(
            "rs-gui",
            "comate",
            &v(&["--plugin-comate", "--port", "18786"]),
        ))
        .expect("rs-comate 必须接受翻译后的 argv");
        assert_eq!(a.port, 18786);

        let a = rs_qoder::Args::try_parse_from(plugin_argv(
            "rs-gui",
            "qoder",
            &v(&["--plugin-qoder", "--port", "18785"]),
        ))
        .expect("rs-qoder 必须接受翻译后的 argv");
        assert_eq!(a.port, 18785);

        let a = rs_vibex::Args::try_parse_from(plugin_argv(
            "rs-gui",
            "vibex",
            &v(&["--plugin-vibex", "--port", "18790", "--portable"]),
        ))
        .expect("rs-vibex 必须接受翻译后的 argv");
        assert_eq!(a.port, 18790);
        assert!(a.portable);

        let a = rs_proxy::Args::try_parse_from(proxy_argv(
            "rs-gui",
            &v(&["-headless", "-port", "18788", "-debug"]),
        ))
        .expect("rs-proxy 必须接受翻译后的 argv");
        assert_eq!(a.port, 18788);
        assert!(a.debug);
    }

    #[test]
    fn flag_value_accepts_both_go_forms() {
        assert_eq!(
            flag_value(&v(&["-port", "18788"]), "port").as_deref(),
            Some("18788")
        );
        assert_eq!(
            flag_value(&v(&["--port=18788"]), "port").as_deref(),
            Some("18788")
        );
        assert_eq!(
            flag_value(&v(&["--host", "127.0.0.2"]), "host").as_deref(),
            Some("127.0.0.2")
        );
        assert_eq!(flag_value(&v(&["-headless"]), "port"), None);
        assert_eq!(
            flag_value(&v(&["-api-key", "K"]), "api-key").as_deref(),
            Some("K")
        );
    }

    #[test]
    fn tuanjie_has_no_rust_crate_yet() {
        assert_eq!(plugin_crate("tuanjie"), None, "团结还没有 Rust 实现，必须如实报");
        assert_eq!(plugin_crate("bai"), Some("rs-bai"));
        assert_eq!(plugin_crate("codebuddy"), Some("rs-codebuddy"));
        assert_eq!(plugin_crate("codebuddy-intl"), Some("rs-codebuddy"));
        assert_eq!(plugin_crate("vibex"), Some("rs-vibex"));
    }
}
