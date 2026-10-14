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
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".comate").join("cli-login.json")
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
