//! discover.rs —— 定位 opencode 可执行文件，以及读 attach 用的 service.json。
//!
//! 定位顺序（任务简报 §3）：
//! 1. 环境变量 `PROXYDECK_OPENCODE_EXE`（绝对路径，测试用，最高优先级）
//! 2. `%APPDATA%\ProxyDeck\opencode\` 下最新版本目录里的 `opencode.exe`（本插件自装的落位）
//! 3. 用户自己装过的常见位置：
//!    `%LOCALAPPDATA%\Programs\opencode\opencode.exe` →
//!    `%USERPROFILE%\.opencode\bin\opencode.exe` →
//!    `%APPDATA%\npm\node_modules\@opencode\cli\bin\opencode.exe` →
//!    `%APPDATA%\npm\opencode.cmd`（解析 .cmd 启动器指向的真实 exe，解析不出就跳过）
//! 4. PATH 上的 `opencode`
//! 5. 都没有 → 调用方把 `/health` 报 `opencode_not_installed`
//!
//! attach 模式（`--attach`）才读 `service.json`；默认永远自己 spawn，避免干扰用户的 TUI。
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::datadir;

/// 测试/高级用户覆盖 opencode 可执行文件路径。
pub const OPENCODE_EXE_ENV: &str = "PROXYDECK_OPENCODE_EXE";

/// 找到的 opencode 及其来源（日志用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundExe {
    pub path: PathBuf,
    /// `env` / `appdata` / `path`
    pub source: &'static str,
}

fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// 按顺序找 opencode。全部失败返回 None（不报错，调用方决定 `/health` 文案）。
pub fn find_opencode_exe() -> Option<FoundExe> {
    if let Some(raw) = env_nonempty(OPENCODE_EXE_ENV) {
        let path = PathBuf::from(&raw);
        if is_file(&path) {
            return Some(FoundExe {
                path,
                source: "env",
            });
        }
        // 优先级最高 ≠ 找不到就终止：环境变量写错时继续走下面的常规位置，
        // 但把这条线索打到日志里，避免"明明设了变量却仍报未安装"的困惑。
        crate::logf(&format!(
            "{OPENCODE_EXE_ENV}={raw} 指向的文件不存在，继续查找常规位置"
        ));
    }

    if let Some(path) = find_in_appdata() {
        return Some(FoundExe {
            path,
            source: "appdata",
        });
    }

    if let Some((path, source)) = find_user_install() {
        return Some(FoundExe { path, source });
    }

    if let Some(path) = find_on_path() {
        return Some(FoundExe {
            path,
            source: "path",
        });
    }
    None
}

/// `%APPDATA%\ProxyDeck\opencode\`：既认「版本子目录 + opencode.exe」，也认根目录直放。
fn find_in_appdata() -> Option<PathBuf> {
    let root = datadir::data_dir().join("opencode");
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                for name in ["opencode.exe", "opencode"] {
                    let exe = path.join(name);
                    if is_file(&exe) {
                        let dir_name = path
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        candidates.push((dir_name, exe));
                        break;
                    }
                }
            } else if is_file(&path) && is_opencode_name(&path) {
                // 便携用户把 opencode.exe 直接扔在 opencode\ 根下。
                candidates.push(("".to_string(), path));
            }
        }
    }

    latest_by_version(candidates)
}

/// 用户自己装过 opencode 的常见位置（按简报顺序），PATH 之前。
///
/// 只认真实存在的文件；`.cmd` 启动器解析不出目标 exe 就跳过，不瞎猜路径。
fn find_user_install() -> Option<(PathBuf, &'static str)> {
    if let Some(local) = env_nonempty("LOCALAPPDATA") {
        let exe = Path::new(&local)
            .join("Programs")
            .join("opencode")
            .join("opencode.exe");
        if is_file(&exe) {
            return Some((exe, "localappdata"));
        }
    }

    if let Some(home) = env_nonempty("USERPROFILE") {
        let exe = Path::new(&home)
            .join(".opencode")
            .join("bin")
            .join("opencode.exe");
        if is_file(&exe) {
            return Some((exe, "userprofile"));
        }
    }

    if let Some(appdata) = env_nonempty("APPDATA") {
        let npm = Path::new(&appdata).join("npm");
        let pkg_exe = npm
            .join("node_modules")
            .join("@opencode")
            .join("cli")
            .join("bin")
            .join("opencode.exe");
        if is_file(&pkg_exe) {
            return Some((pkg_exe, "npm"));
        }

        let shim = npm.join("opencode.cmd");
        if let Ok(text) = std::fs::read_to_string(&shim) {
            if let Some(exe) = parse_cmd_shim(&text, shim.parent().unwrap_or(Path::new("."))) {
                if is_file(&exe) {
                    return Some((exe, "npm-shim"));
                }
            }
        }
    }
    None
}

/// 解析 npm 生成的 Windows `.cmd` 启动器：抽取引号里的 `.exe` 路径，
/// 把 `%dp0%` / `%~dp0` 展开成 `.cmd` 所在目录。解析不出返回 None。
///
/// npm 的 shim 形如：
/// ```text
/// endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\...\opencode.exe" %*
/// ```
/// 也见过 `"%~dp0%\node_modules\...\opencode.exe"`；两种都认。
fn parse_cmd_shim(text: &str, shim_dir: &Path) -> Option<PathBuf> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("REM ")
            || trimmed.starts_with("rem ")
            || trimmed.starts_with("::")
        {
            continue;
        }
        for quoted in quoted_segments(trimmed) {
            let expanded = expand_dp0(&quoted, shim_dir);
            let path = PathBuf::from(&expanded);
            let file_name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if file_name.starts_with("opencode") && file_name.ends_with(".exe") {
                return Some(path);
            }
        }
    }
    None
}

/// 取一行里所有双引号包裹的片段（未闭合的尾巴忽略）。
fn quoted_segments(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut inside = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                if inside {
                    out.push(std::mem::take(&mut current));
                }
                inside = !inside;
            }
            _ if inside => current.push(ch),
            _ => {}
        }
    }
    out
}

/// 展开批处理里的 `%~dp0` / `%dp0%`（不区分这几种常见大小写写法）。
fn expand_dp0(token: &str, shim_dir: &Path) -> String {
    let dir = shim_dir.display().to_string();
    let mut out = token.to_string();
    for pattern in ["%~dp0", "%dp0%", "%~DP0", "%DP0%"] {
        out = out.replace(pattern, &dir);
    }
    out
}

/// 目录名按版本号排序（`2.0.32` > `2.0.9`），非版本名排在版本名之后；同分按名字倒序。
pub fn latest_by_version(candidates: Vec<(String, PathBuf)>) -> Option<PathBuf> {
    sort_latest_by_version(candidates).map(|(_, path)| path)
}

/// 同 [`latest_by_version`]，但把版本名一起返回（安装状态扫描用）。
pub fn sort_latest_by_version(mut candidates: Vec<(String, PathBuf)>) -> Option<(String, PathBuf)> {
    candidates.sort_by(|a, b| {
        let va = version_key(&a.0);
        let vb = version_key(&b.0);
        vb.cmp(&va).then_with(|| b.0.cmp(&a.0))
    });
    candidates.into_iter().next()
}

/// 把目录名里的连续数字取出来做版本比较（不解析点号语义，够用且不 panic）。
fn version_key(name: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur: Option<u64> = None;
    for ch in name.chars() {
        if let Some(d) = ch.to_digit(10) {
            cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add(d as u64));
        } else if let Some(n) = cur.take() {
            out.push(n);
        }
    }
    if let Some(n) = cur {
        out.push(n);
    }
    out
}

fn is_opencode_name(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name == "opencode" || name == "opencode.exe"
}

/// PATH 查找：Windows 认 `opencode.exe` / `opencode`，非 Windows 认 `opencode`。
fn find_on_path() -> Option<PathBuf> {
    let path_var = env_nonempty("PATH")?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    let names: &[&str] = if cfg!(windows) {
        &["opencode.exe", "opencode"]
    } else {
        &["opencode"]
    };
    for dir in path_var.split(sep) {
        if dir.trim().is_empty() {
            continue;
        }
        for name in names {
            let candidate = Path::new(dir.trim()).join(name);
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// `service.json` 里 attach 需要的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRegistration {
    pub url: String,
    pub password: String,
    pub pid: Option<u32>,
    pub version: Option<String>,
}

/// 读 `service.json`（`%LOCALAPPDATA%\state\opencode\` 或 `~/.local/state/opencode/`）。
///
/// 这里**不判断 PID 是否活着**（无 unsafe 就调不到 OpenProcess）；attach 时由调用方
/// 探活 `/api/info`，探不通就退回自己 spawn。这样 stale service.json 不会把插件拖死。
pub fn read_service_registration() -> Option<ServiceRegistration> {
    for path in service_json_paths() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(reg) = parse_service_json(&text) {
                return Some(reg);
            }
            crate::logf(&format!("service.json 解析失败：{}", path.display()));
        }
    }
    None
}

fn service_json_paths() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = env_nonempty("XDG_STATE_HOME") {
        roots.push(PathBuf::from(xdg));
    }
    if let Some(local) = env_nonempty("LOCALAPPDATA") {
        roots.push(Path::new(&local).join("state"));
    }
    if let Some(home) = env_nonempty("USERPROFILE").or_else(|| env_nonempty("HOME")) {
        roots.push(Path::new(&home).join(".local").join("state"));
    }
    roots
        .into_iter()
        .map(|r| r.join("opencode").join("service.json"))
        .collect()
}

/// 解析 service.json。`url` 缺失时用 hostname+port 拼；密码缺失按空串（无鉴权）。
pub fn parse_service_json(text: &str) -> Option<ServiceRegistration> {
    let v: Value = serde_json::from_str(text).ok()?;
    let url = match v.get("url").and_then(Value::as_str) {
        Some(u) if !u.trim().is_empty() => u.trim().trim_end_matches('/').to_string(),
        _ => {
            let host = v
                .get("hostname")
                .and_then(Value::as_str)
                .unwrap_or("127.0.0.1");
            let port = v.get("port").and_then(Value::as_u64)?;
            format!("http://{host}:{port}")
        }
    };
    let password = v
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pid = v
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok());
    let version = v
        .get("version")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    Some(ServiceRegistration {
        url,
        password,
        pid,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_sort_picks_latest() {
        let c = vec![
            ("2.0.9".to_string(), PathBuf::from("a")),
            ("2.0.32".to_string(), PathBuf::from("b")),
            ("2.0.10".to_string(), PathBuf::from("c")),
            ("beta".to_string(), PathBuf::from("d")),
        ];
        assert_eq!(latest_by_version(c), Some(PathBuf::from("b")));
    }

    #[test]
    fn cmd_shim_resolves_opencode_exe_and_ignores_node() {
        let dir = std::env::temp_dir().join(format!(
            "rs-opencode-shim-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        let pkg_bin = dir
            .join("node_modules")
            .join("@opencode")
            .join("cli")
            .join("bin");
        std::fs::create_dir_all(&pkg_bin).unwrap();
        let exe = pkg_bin.join("opencode.exe");
        std::fs::write(&exe, b"exe").unwrap();

        let shim = "@ECHO off\r\n\
                    GOTO start\r\n\
                    :start\r\n\
                    SETLOCAL\r\n\
                    endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@opencode\\cli\\bin\\opencode.exe\" %*\r\n";
        assert_eq!(parse_cmd_shim(shim, &dir), Some(exe.clone()));

        // 只有 node.exe 的 node 包 shim：不能当成 opencode。
        let node_shim = "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%~dp0%\\node_modules\\foo\\bin\\foo.js\" %*\r\n";
        assert_eq!(parse_cmd_shim(node_shim, &dir), None);
        let exe_shim = "start \"\" \"%dp0%\\node.exe\" %*\r\n";
        assert_eq!(parse_cmd_shim(exe_shim, &dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_service_json_full_and_minimal() {
        let r = parse_service_json(
            r#"{"id":"x","version":"2.0.32","url":"http://127.0.0.1:45278/","pid":19680,"password":"pw"}"#,
        )
        .unwrap();
        assert_eq!(r.url, "http://127.0.0.1:45278");
        assert_eq!(r.password, "pw");
        assert_eq!(r.pid, Some(19680));
        assert_eq!(r.version.as_deref(), Some("2.0.32"));

        let r = parse_service_json(r#"{"hostname":"127.0.0.1","port":1234}"#).unwrap();
        assert_eq!(r.url, "http://127.0.0.1:1234");
        assert_eq!(r.password, "");
        assert_eq!(r.pid, None);

        assert!(parse_service_json("not json").is_none());
        assert!(parse_service_json(r#"{"password":"x"}"#).is_none());
    }
}
