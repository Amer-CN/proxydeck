//! `internal/comate/zulu.go` 的 Rust 等价物：登录态读取、zulu CLI 探测、内部 zulu serve
//! 的探活/拉起/复用，以及 `list-model` 模型目录。
//!
//! 只用 std 的 `process`（与 Go 的 `exec.Command` 同层）+ reqwest 探活；
//! 子进程一律 `CREATE_NO_WINDOW`（Go `hideConsole`：`HideWindow` + `CREATE_NO_WINDOW`），
//! 否则 `zulu.cmd` 这种批处理每次 spawn 都会弹一个 cmd 窗口。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 登录态文件：`%USERPROFILE%\.comate\cli-login.json`（Go `licenseFile()`）。
pub fn license_path() -> PathBuf {
    home_dir().join(".comate").join("cli-login.json")
}

/// zulu serve 的 pid 文件：`%USERPROFILE%\.comate\zulu-serve.pid`（zulu 自身写，路径写死）。
pub fn serve_pid_path() -> PathBuf {
    home_dir().join(".comate").join("zulu-serve.pid")
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Comate IDE 的设置文件：`%APPDATA%\Comate\User\settings.json`。
pub fn ide_settings_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(appdata.join("Comate").join("User").join("settings.json"))
}

/// 从 IDE 设置里读 `baidu.comate.license`（Comate 登录态）。
///
/// 2026-11-15 实测：更新后 `cli-login.json`（CLI 登录）与 IDE 设置里是**两个不同账号**
/// （`de500679…`/勤奋的Amer 与 `875914f0…`/我比尼玛还愛尼）。用户在用的、买会员的是 IDE 那个，
/// 所以生产环境优先读它；`cli-login.json` 只作兜底。
pub fn ide_license_at(path: &Path) -> String {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    // IDE 的 settings.json 常带 UTF-8 BOM 与注释之外的普通 JSON；带 BOM 时先剥掉。
    let bytes = strip_utf8_bom(&bytes);
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let map = match value {
        serde_json::Value::Object(m) => m,
        _ => return String::new(),
    };
    match map.get("baidu.comate.license") {
        Some(serde_json::Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

fn strip_utf8_bom(b: &[u8]) -> &[u8] {
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &b[3..]
    } else {
        b
    }
}

/// 许可证来源（按顺序取第一个非空）。
#[derive(Debug, Clone)]
pub enum LicenseSource {
    /// 显式环境变量 `COMATE_LICENSE`（人工指定时最优先）。
    Env(String),
    /// Comate IDE 的 `settings.json`（当前在用账号）。
    IdeSettings(PathBuf),
    /// CLI 登录态 `~/.comate/cli-login.json`。
    File(PathBuf),
}

impl LicenseSource {
    pub fn read(&self) -> String {
        match self {
            LicenseSource::Env(name) => std::env::var(name).unwrap_or_default().trim().to_string(),
            LicenseSource::IdeSettings(p) => ide_license_at(p),
            LicenseSource::File(p) => read_license_at(p),
        }
    }
}

/// 按优先级解析登录态；全空返回 ""。
pub fn resolve_license(sources: &[LicenseSource]) -> String {
    for s in sources {
        let v = s.read();
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

/// pid 文件里我们关心的字段（zulu 写的 JSON：pid/port/host/zuluVersion/zuluBin/authMode）。
#[derive(Debug, Clone, Deserialize)]
pub struct ServePid {
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub host: Option<String>,
}

pub fn read_serve_pid() -> Option<ServePid> {
    let bytes = std::fs::read(serve_pid_path()).ok()?;
    serde_json::from_slice::<ServePid>(&bytes).ok()
}

/// 干掉 pid 文件记录的那个 zulu serve，并删掉 pid 文件。
///
/// 为什么需要：`zulu serve` 是**单例**——它会读 pid 文件，发现已有服务就「复用」并立刻返回，
/// **根本不看 `--port`/`--license`**（2026-11-15 实测：`serve --port 8892` 直接回了
/// `{"port":8792,"reused":true}`）。所以只要 Comate IDE 自己拉过一个 serve（例如 8779），
/// 我们 spawn 的那个就会原地退出、8792 永远不健康，随后整体超时。
///
/// 只在该 serve 不在我们期望的端口时调用（自己端口上的实例由健康检查直接复用）。
pub fn stop_serve_from_pid_file(reason: &str) -> bool {
    let Some(p) = read_serve_pid() else {
        return false;
    };
    let mut killed = false;
    if p.pid > 0 {
        #[cfg(windows)]
        {
            let status = std::process::Command::new("taskkill")
                .args(["/PID", &p.pid.to_string(), "/T", "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            killed = status.map(|s| s.success()).unwrap_or(false);
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &p.pid.to_string()])
                .status();
            killed = true;
        }
        eprintln!(
            "comate-plugin: 停掉既有 zulu serve（pid={} port={}，{reason}）→ killed={killed}",
            p.pid, p.port
        );
    }
    let _ = std::fs::remove_file(serve_pid_path());
    killed
}

/// Go `readLicense`：文件缺失/坏 JSON/license 非字符串 → ""；否则 TrimSpace。
pub fn read_license_at(path: &Path) -> String {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let map = match value {
        // Go 的 `json.Unmarshal(null, &struct)` 是 no-op → 零值 ""；其它非对象类型报错 → ""
        serde_json::Value::Object(m) => m,
        serde_json::Value::Null => return String::new(),
        _ => return String::new(),
    };
    let picked = match map.get("license") {
        Some(v) => Some(v),
        None => map
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("license"))
            .map(|(_, v)| v),
    };
    match picked {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.trim().to_string(),
        Some(_) => String::new(), // 类型不符：Go 的 Unmarshal 整个失败
    }
}

/// Go `findZulu`：`COMATE_ZULU`（存在且非目录）→ ProgramFiles(x86)/ProgramFiles → `D:` 候选。
pub fn discover_zulu(env_value: Option<&str>) -> Option<PathBuf> {
    if let Some(v) = env_value {
        let t = v.trim();
        if !t.is_empty() {
            let p = PathBuf::from(t);
            if is_file(&p) {
                return Some(p);
            }
        }
    }
    let rel = Path::new("Comate")
        .join("resources")
        .join("app")
        .join("extensions")
        .join("baiducomate.comate")
        .join("dist")
        .join("zulu-cli")
        .join("bin")
        .join("zulu.cmd");
    let mut candidates: Vec<PathBuf> = Vec::new();
    for env in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(pf) = std::env::var_os(env) {
            candidates.push(PathBuf::from(pf).join(&rel));
        }
    }
    candidates.push(PathBuf::from(r"D:\Program Files (x86)").join(&rel));
    candidates.into_iter().find(|c| is_file(c))
}

fn is_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| !m.is_dir()).unwrap_or(false)
}

/// Go `modelInfo`（`modelId` / `displayName`）。
///
/// `Option` 是为了对齐 Go 的容忍度：缺字段与 `null` 都落成零值 ""，**类型不符**则整份
/// 解析失败（黄金基准 `models-empty-and-null-entry` / `models-type-error` 锁住这两条）。
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ModelInfo {
    #[serde(rename = "modelId", default)]
    pub model_id: Option<String>,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
}

impl ModelInfo {
    pub fn id(&self) -> &str {
        self.model_id.as_deref().unwrap_or("")
    }
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or("")
    }
}

fn hidden_command(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Go `listModels`：`cmd /c <zulu> list-model -l <license>`，输出 TrimSpace 后解析 JSON 数组。
///
/// 返回 `Ok(None)` 表示输出是 JSON `null`（Go 的 `json.Unmarshal` 把切片留成 nil，
/// 缓存门槛 `modelsCache != nil` 因此永远不命中——见 `ModelsCache` 的注释）。
pub async fn list_models(zulu_path: &Path, license: &str) -> Result<Option<Vec<ModelInfo>>, String> {
    let zulu = zulu_path.to_path_buf();
    let license = license.to_string();
    let out = tokio::task::spawn_blocking(move || {
        hidden_command("cmd")
            .arg("/c")
            .arg(&zulu)
            .arg("list-model")
            .arg("-l")
            .arg(&license)
            .env("PLATFORM", "saas") // 外部版必须是 saas（默认 internal 会打 comate.baidu-int.com）
            .output()
    })
    .await
    .map_err(|e| format!("list-model 失败: {e}"))?
    .map_err(|e| format!("list-model 失败: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "list-model 失败: exit status {}",
            out.status.code().unwrap_or(-1)
        ));
    }
    let trimmed = trim_space_bytes(&out.stdout);
    serde_json::from_slice::<Option<Vec<ModelInfo>>>(&trimmed)
        .map_err(|e| format!("list-model 输出解析失败: {e}"))
}

/// Go `bytes.TrimSpace` 的等价物（Unicode White_Space）。返回拷贝，避免在借用上做指针算术。
fn trim_space_bytes(b: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(b).trim().as_bytes().to_vec()
}

/// Go `ensureZuluServe`：已健康则复用（不 spawn）；否则 spawn
/// `cmd /c <zulu> serve --port <port> --host 127.0.0.1 -l <license>` 并轮询 health。
///
/// 返回 `Ok(None)` = 复用外部实例（不负责 kill）；`Ok(Some(child))` = 自己 spawn 的。
pub async fn ensure_zulu_serve(
    zulu_path: &Path,
    license: &str,
    port: &str,
    health_url: &str,
    timeout: Duration,
) -> Result<Option<std::process::Child>, String> {
    if health_ok(health_url).await {
        return Ok(None);
    }
    let zulu = zulu_path.to_path_buf();
    let port = port.to_string();
    let license = license.to_string();
    let child = tokio::task::spawn_blocking(move || {
        // stdio 全部接空设备：Go 的 `exec.Cmd` 在 Stdout/Stderr/Stdin 为 nil 时也是这么做的
        // （`os/exec` 文档：nil 即 null device），子进程不会把输出打到插件进程的控制台上。
        hidden_command("cmd")
            .arg("/c")
            .arg(&zulu)
            .arg("serve")
            .arg("--port")
            .arg(&port)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("-l")
            .arg(&license)
            // ⚠ 2026-11-15 实测：zulu 1.8.1 的 `PLATFORM` 默认 `internal`，会去打
            // `comate.baidu-int.com`（百度内网，外部解析不了），会话直接以
            // 「您当前会话执行遇到了异常，请检测网络后重试！」失败（内核拿到 taskId=-1）。
            // 外部版必须显式设 `saas`（取值只能是 saas/internal/poc，大写无效）。
            .env("PLATFORM", "saas")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    })
    .await
    .map_err(|e| format!("启动 zulu serve 失败: {e}"))?
    .map_err(|e| format!("启动 zulu serve 失败: {e}"))?;

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if health_ok(health_url).await {
            return Ok(Some(child));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut child = child;
    let _ = child.kill();
    // 文案与 Go 逐字一致（Go 里 20s 是常量）
    Err("zulu serve 启动超时（20 秒未就绪）".to_string())
}

async fn health_ok(url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(url).send().await {
        Ok(r) => r.status() == reqwest::StatusCode::OK,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_license_like_go() {
        let dir = std::env::temp_dir().join(format!("rs-comate-lic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cli-login.json");
        let w = |c: &str| std::fs::write(&p, c).unwrap();

        w(r#"{"license":"  sent  "}"#);
        assert_eq!(read_license_at(&p), "sent");
        w(r#"{"license":"   "}"#);
        assert_eq!(read_license_at(&p), "");
        w(r#"{"license":null}"#);
        assert_eq!(read_license_at(&p), "");
        w(r#"{"license":123}"#);
        assert_eq!(read_license_at(&p), "");
        w("{not json");
        assert_eq!(read_license_at(&p), "");
        w("null");
        assert_eq!(read_license_at(&p), "");
        w(r#"[1,2]"#);
        assert_eq!(read_license_at(&p), "");
        std::fs::remove_file(&p).unwrap();
        assert_eq!(read_license_at(&p), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_list_model_like_go() {
        let ok = br#"[{"modelId":"deepseek-v4-flash_a866","displayName":"DeepSeek-V4-Flash"},{"modelId":"glm-5.3_x"},{"modelId":null},{"displayName":null},{}]"#;
        let v: Option<Vec<ModelInfo>> = serde_json::from_slice(ok).unwrap();
        let v = v.unwrap();
        assert_eq!(v[0].id(), "deepseek-v4-flash_a866");
        assert_eq!(v[0].display(), "DeepSeek-V4-Flash");
        assert_eq!(v[1].display(), "");
        assert_eq!(v[2].id(), "");
        assert_eq!(v[3].id(), "");
        assert_eq!(v[4].id(), "");

        // 空数组 → 非 nil 空切片；null → None（缓存语义不同，见 ModelsCache 注释）
        let empty: Option<Vec<ModelInfo>> = serde_json::from_slice(b"[]").unwrap();
        assert_eq!(empty, Some(Vec::new()));
        let null: Option<Vec<ModelInfo>> = serde_json::from_slice(b"null").unwrap();
        assert_eq!(null, None);

        // 类型不符/坏 JSON/空输入 → 整份失败（Go 同）
        assert!(serde_json::from_slice::<Option<Vec<ModelInfo>>>(br#"[{"modelId":123}]"#).is_err());
        assert!(serde_json::from_slice::<Option<Vec<ModelInfo>>>(b"not json at all").is_err());
        assert!(serde_json::from_slice::<Option<Vec<ModelInfo>>>(b"").is_err());
        assert!(serde_json::from_slice::<Option<Vec<ModelInfo>>>(br#"{"modelId":"x"}"#).is_err());
    }
}
