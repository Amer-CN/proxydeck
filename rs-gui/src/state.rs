//! state.rs —— GUI 控制器与聚合状态（对应 Go `app/app.go` 的 `app` 结构 +
//! `app/bridge.go` 的 `stateMsg` / `state` / `start` / `stop` / `newApp`）。
//!
//! `ccGetState` 的全部键名与取值口径逐字段对齐 `bridge.go:337-406`：
//! ui.html 直接消费这些键（`running` / `phase` / `baseUrl` / `apiKey` / `startedOnce` …），
//! 少一个键或改名都会让界面静默失灵，故本文件是 P7a 的对齐重点。

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::appdir;
use crate::httpc;
use crate::keyprobe::KeyProbe;
use crate::proc;
use crate::update::{self, UpdFlow, UpdateState};

/// 发给界面的完整状态（字段顺序 = Go `stateMsg` 的声明顺序，不可调）。
#[derive(Serialize)]
pub struct StateMsg {
    pub running: bool,
    pub external: bool,
    /// idle | running | external | error
    pub phase: String,
    pub host: String,
    pub port: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub uptime: i64,
    pub autostart: bool,
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(rename = "noticeDismissed")]
    pub notice_dismissed: bool,
    #[serde(rename = "closeHintDismissed")]
    pub close_hint_dismissed: bool,
    pub version: String,
    /// stats.json 存在且 started>0：代理曾成功运行过
    #[serde(rename = "startedOnce")]
    pub started_once: bool,
    pub core: String,
    pub pid: i32,
    /// Go 侧是 `json:"lastErr,omitempty"`：空串**不出现在 JSON 里**
    #[serde(rename = "lastErr", skip_serializing_if = "String::is_empty")]
    pub last_err: String,
}

/// 控制器内部态（Go `app` 的字段同名同义）。
#[derive(Default)]
pub struct Inner {
    pub running: bool,
    pub external: bool,
    pub started: Option<Instant>,
    pub api_key: String,
    pub last_err: String,
}

/// GUI 与代理核心之间的控制器（桥接层的全部命令都挂在它上面）。
pub struct App {
    pub host: String,
    pub port: String,
    pub inner: Mutex<Inner>,

    /// 更新检查缓存与后台拉取通道（Go 的包级 `updateCache*` / `updateRefreshCh`）
    pub update: Mutex<UpdateState>,
    /// 一键更新流程状态（Go 的 `updFlowMu/upd*`）
    pub upd_flow: Mutex<UpdFlow>,
    /// 存储 key 探活缓存（Go 的 `keyProbe*`）
    pub key_probe: Mutex<KeyProbe>,
    /// 官网用量缓存（Go 的 `usageCache`）
    pub usage: Mutex<String>,
    /// 插件托管器（Go `app` 上的 `plugins` 状态表；换血流程要复用同一个实例）
    plugins: Arc<crate::plugins::PluginHost>,

    usage_tx: SyncSender<()>,
    usage_rx: Mutex<Option<Receiver<()>>>,
    update_tx: SyncSender<()>,
    update_rx: Mutex<Option<Receiver<()>>>,
}

impl App {
    /// 建控制器（对应 Go `newApp`）：启动时清一次腾位法残留的旧程序文件，
    /// 未显式给 key 时从数据目录的 `api-key.txt` 读。
    pub fn new(host: &str, port: &str, key: &str) -> Self {
        update::cleanup_old_exe();

        let mut api_key = key.to_string();
        if api_key.is_empty() {
            if let Ok(b) = appdir::read_file(&Self::key_file()) {
                api_key = String::from_utf8_lossy(&b).trim().to_string();
            }
        }
        let (usage_tx, usage_rx) = sync_channel(1);
        let (update_tx, update_rx) = sync_channel(1);
        Self {
            host: host.to_string(),
            port: port.to_string(),
            inner: Mutex::new(Inner {
                api_key,
                ..Default::default()
            }),
            update: Mutex::new(UpdateState::default()),
            upd_flow: Mutex::new(UpdFlow::default()),
            key_probe: Mutex::new(KeyProbe::default()),
            usage: Mutex::new(String::new()),
            plugins: std::sync::Arc::new(crate::plugins::PluginHost::production()),
            usage_tx,
            usage_rx: Mutex::new(Some(usage_rx)),
            update_tx,
            update_rx: Mutex::new(Some(update_rx)),
        }
    }

    /* ---------------- 状态文件路径（Go bridge.go:49-54） ---------------- */

    /// 插件托管器（与 `Bridge` 用的是同一个实例——Go 侧也是 `app` 上的同一份状态表）。
    /// 换血流程（`rollover`）从 GUI 启动路径进来，拿的就是它。
    pub fn plugins(&self) -> &std::sync::Arc<crate::plugins::PluginHost> {
        &self.plugins
    }

    pub fn key_file() -> std::path::PathBuf {
        appdir::path("api-key.txt")
    }
    pub fn stats_file() -> std::path::PathBuf {
        appdir::path("stats.json")
    }
    pub fn notice_file() -> std::path::PathBuf {
        appdir::path("notice_dismissed.flag")
    }
    pub fn close_hint_file() -> std::path::PathBuf {
        appdir::path("close_hint_dismissed.flag")
    }
    pub fn close_mode_file() -> std::path::PathBuf {
        appdir::path("close_mode.txt")
    }
    pub fn key_map_file() -> std::path::PathBuf {
        appdir::path("keymap.json")
    }

    /// `net.JoinHostPort` 同口径：host 里含冒号（IPv6）要加方括号。
    pub fn base_url(&self) -> String {
        if self.host.contains(':') {
            format!("http://[{}]:{}", self.host, self.port)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }

    pub fn health_url(&self) -> String {
        format!("{}/health", self.base_url())
    }

    /* ---------------- 启停（Go bridge.go:238-335） ---------------- */

    /// 启动代理：端口已有健康代理 → 直接接管观察；否则清掉占端口的残留进程后
    /// spawn 本 exe 的 `-headless` 子进程并等就绪（最多 ~5s）。
    pub fn start(&self, key: &str) -> Result<String, String> {
        let mut inner = self.inner.lock().unwrap();

        let key = if key.is_empty() {
            inner.api_key.clone()
        } else {
            key.to_string()
        };
        let mut key_saved = true;
        if !key.is_empty() {
            inner.api_key = key.clone();
            key_saved = appdir::write_file(&Self::key_file(), key.as_bytes()).is_ok();
        }

        // 端口已有健康代理（可能是上次遗留的 headless 子进程）→ 直接接管观察。
        if httpc::ok(&self.health_url()) {
            inner.running = true;
            inner.external = false;
            inner.started = Some(Instant::now());
            inner.last_err.clear();
            return Ok(format!("检测到 {} 代理已在运行，已接入", self.base_url()));
        }

        // 端口被非健康进程占用（僵死/残留）→ 先清理，再启动。
        if proc::port_busy(&self.port) {
            let _ = proc::kill_by_port(&self.port); // 清理失败不阻塞：子进程绑定端口时若仍冲突会再报错
            std::thread::sleep(Duration::from_millis(300));
        }

        let exe = std::env::current_exe().map_err(|e| format!("无法定位自身可执行文件: {e}"))?;
        let mut args: Vec<String> = vec!["-headless".to_string()];
        args.extend(appdir::portable_args());
        /* key 已在上面落盘（api-key.txt），子进程会自行读取——正常路径不把密钥放上 argv：
           进程命令行对本机任意进程可读。仅落盘失败才退回 argv。 */
        if !key.is_empty() && !key_saved {
            args.push("-api-key".to_string());
            args.push(key.clone());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut cmd = proc::hidden_cmd(&exe.to_string_lossy(), &arg_refs);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动后台代理失败: {e}"))?;
        // headless 的 stdout/stderr 落数据目录的 headless-error.log（带时间戳行头、1MB×3 轮转）
        let log_path = appdir::path("headless-error.log");
        for pipe in [
            child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let path = log_path.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(pipe);
                for line in reader.lines() {
                    match line {
                        Ok(l) => {
                            if !l.is_empty() {
                                let _ = appdir::append_log(&path, &l);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // 等待代理就绪（最多 ~5s）
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if httpc::ok(&self.health_url()) {
                inner.running = true;
                inner.external = false;
                inner.started = Some(Instant::now());
                inner.last_err.clear();
                return Ok(format!(
                    "点火成功 · {} 已就绪（后台常驻，关窗不影响）",
                    self.base_url()
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(format!(
            "后台代理启动超时，请检查端口 {} 是否被占用",
            self.port
        ))
    }

    /// 停堆：探活 → 若有代理在跑就结束占用端口的进程（本 GUI 自身不持有代理）。
    pub fn stop(&self) -> Result<String, String> {
        let was_external = self.inner.lock().unwrap().external;

        if !httpc::ok(&self.health_url()) {
            let mut inner = self.inner.lock().unwrap();
            inner.running = false;
            inner.external = false;
            return Ok("代理未在运行".to_string());
        }

        proc::kill_by_port(&self.port)?;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.running = false;
            inner.external = false;
        }
        if was_external {
            Ok("后台代理已停止".to_string())
        } else {
            Ok("代理核心已停堆（后台进程已结束）".to_string())
        }
    }

    /* ---------------- 状态聚合（Go bridge.go:357-406） ---------------- */

    pub fn state(&self) -> StateMsg {
        let (mut running, external, mut started, mut last_err, key) = {
            let inner = self.inner.lock().unwrap();
            (
                inner.running,
                inner.external,
                inner.started,
                inner.last_err.clone(),
                inner.api_key.clone(),
            )
        };

        // 探活：端口上有健康代理即视为运行中（可能是本 GUI 起的 headless 子进程，
        // 也可能是开机自启/上次遗留的后台实例）。
        if httpc::ok(&self.health_url()) && !running {
            running = true;
            let mut inner = self.inner.lock().unwrap();
            inner.running = true;
            if inner.started.is_none() {
                inner.started = Some(Instant::now());
            }
            started = inner.started;
            last_err = inner.last_err.clone();
        }

        let phase = if running {
            "running"
        } else if !last_err.is_empty() {
            "error"
        } else {
            "idle"
        };

        let uptime = if running {
            started.map(|s| s.elapsed().as_secs() as i64).unwrap_or(0)
        } else {
            0
        };

        // 曾成功运行过？读 stats.json 的 started 字段（>0 即曾运行）。
        let mut started_once = false;
        if let Ok(b) = appdir::read_file(&Self::stats_file()) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
                if v.get("started").and_then(|x| x.as_i64()).unwrap_or(0) > 0 {
                    started_once = true;
                }
            }
        }

        StateMsg {
            running,
            external,
            phase: phase.to_string(),
            host: self.host.clone(),
            port: self.port.clone(),
            base_url: format!("{}/v1", self.base_url()),
            uptime,
            autostart: crate::bridge::autostart_installed(),
            api_key: key,
            notice_dismissed: appdir::file_exists(&Self::notice_file()),
            close_hint_dismissed: appdir::file_exists(&Self::close_hint_file()),
            version: crate::APP_VERSION.to_string(),
            started_once,
            core: crate::CORE_VERSION.to_string(),
            pid: std::process::id() as i32,
            last_err,
        }
    }

    /* ---------------- 后台刷新通道（Go bindAll 里起的两个常驻 goroutine） ---------------- */

    /// 起两个常驻后台循环（等价 Go `bindAll` 里的 `go func(){ for range ch }`）：
    /// 更新检查拉取、官网用量刷新。**只有带窗口的宿主该调**——测试与子模式调了
    /// 就会产生网络请求（门禁刻意不调，见 README「后台刷新的启动点」）。
    pub fn start_background(self: &std::sync::Arc<Self>) {
        if let Some(rx) = self.usage_rx.lock().unwrap().take() {
            let app = std::sync::Arc::clone(self);
            std::thread::spawn(move || {
                for _ in rx {
                    if let Some(s) = httpc::get_slow(&format!("{}/v1/usage", app.base_url())) {
                        *app.usage.lock().unwrap() = s;
                    }
                }
            });
        }
        if let Some(rx) = self.update_rx.lock().unwrap().take() {
            let app = std::sync::Arc::clone(self);
            std::thread::spawn(move || {
                for _ in rx {
                    update::refresh_releases(&app);
                }
            });
        }
    }

    /// 非阻塞踢一次更新拉取（Go `tryKickUpdateRefresh`）。
    pub fn kick_update(&self) -> bool {
        update::try_kick_update_refresh(&self.update, &self.update_tx)
    }

    /// 无条件踢一次更新拉取（Go `checkUpdateForce` 用）。
    pub fn kick_update_unconditional(&self) {
        let _ = self.update_tx.try_send(());
    }

    /// 非阻塞踢一次官网用量刷新（Go `ccUsageRefresh`）。
    pub fn kick_usage(&self) {
        let _ = self.usage_tx.try_send(());
    }
}
