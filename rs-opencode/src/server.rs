//! server.rs —— 托管 `opencode serve` 子进程（挑端口、设密码、等就绪、退出时 taskkill）。
//!
//! 设计取舍：
//! * **端口自己挑**：实测 `--port 0` 不可靠，先在 20000–29999 里找空闲端口再显式传。
//! * **就绪判定用 `/api/info` 轮询**，而不是死等 stdout 里那行 "server listening on"。
//!   端口是我们给的，stdout 只用来打日志；轮询能同时覆盖"端口没起来 / 进程秒退"。
//! * **数据目录不动**：opencode 自己用 `~/.local/share/opencode`，登录态/Go 订阅都在这。
//! * **密码只进环境变量**，不写 argv、不落盘。
//! * **父进程被强杀也要带走子进程**（Windows）：`taskkill /F` / `TerminateProcess`
//!   不跑 `Drop`，光靠 `Drop` 清理必留孤儿。spawn 后立刻把子进程挂进一个
//!   `KILL_ON_JOB_CLOSE` 的匿名 Job Object，`Job` 句柄活到 `ManagedServer` 结束；
//!   父进程无论怎么死，内核关闭最后一个 job 句柄时都会由系统终止 job 内的进程。
//!   `Drop` + `taskkill` 的优雅停止路径保留（正常退出立刻杀，不等 job 兜底）。
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use tokio::process::{Child, Command};
use tokio::time::{sleep, Instant};

/// 挑端口的范围（任务简报 §4.3）。
pub const PORT_RANGE_START: u16 = 20000;
pub const PORT_RANGE_END: u16 = 29999;
/// 等 `opencode serve` 就绪的上限。
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// 就绪轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Windows `CREATE_NO_WINDOW`：不弹黑窗（与 `rs-qoder` 的 hideConsole 同值）。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 一个由本进程拉起的 `opencode serve`。
pub struct ManagedServer {
    child: Child,
    pid: Option<u32>,
    port: u16,
    base: String,
    password: String,
    stopped: bool,
    /// Windows Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）。
    ///
    /// **必须活到本结构被 drop**：`kill_on_job_close` 只在**最后一个** job 句柄
    /// 关闭时生效。父进程被 `TerminateProcess` 时不会跑 `Drop`，但进程销毁会由
    /// 内核关闭它持有的句柄——这就是"父进程怎么死都带走子进程"的兜底。
    /// 绝不把当前进程 assign 进这个 job（否则关句柄会先杀掉自己）。
    #[cfg(windows)]
    job: Option<win32job::Job>,
}

impl ManagedServer {
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// 显式停止（taskkill /F），幂等。
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if let Some(pid) = self.pid {
            kill_pid(pid);
        }
        // taskkill 之后顺手收尸；拿不到也无所谓，进程已死。
        let _ = self.child.start_kill();
        let _ = self.child.try_wait();
        // 关掉 job 的最后一个句柄：内核立刻终止 job 内残余进程（taskkill 失败时的兜底）。
        // 父进程被强杀时走的是同一机制，只是由内核在进程销毁时替我们关句柄。
        #[cfg(windows)]
        drop(self.job.take());
    }
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        if !self.stopped {
            self.stopped = true;
            if let Some(pid) = self.pid {
                kill_pid(pid);
            }
            let _ = self.child.start_kill();
            let _ = self.child.try_wait();
        }
    }
}

/// Windows：`taskkill /F /PID`（与 `plugins.rs` 的杀法一致）；其它平台退 `kill -9`。
fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Windows：新建一个 `KILL_ON_JOB_CLOSE` 的匿名 Job Object，并把 `handle`
///（进程句柄，`isize` 形态）对应的进程挂进去。
///
/// 返回的 `Job` 就是兜底本身：调用方必须把它持有到子进程生命周期结束。
/// 只 assign 子进程，**绝不 assign 当前进程**——否则父进程关句柄时会先杀掉自己。
#[cfg(windows)]
fn job_guard_for_process(handle: isize) -> Result<win32job::Job, String> {
    let mut info = win32job::ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&mut info)
        .map_err(|e| format!("创建 Job Object 失败（{e}）"))?;
    job.assign_process(handle)
        .map_err(|e| format!("把 opencode serve 加入 Job Object 失败（{e}）"))?;
    Ok(job)
}

/// 32 字节随机密码 → base64url（无填充）。失败时用时间/PID 兜底，不 panic。
pub fn random_password() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        fill_fallback(&mut bytes);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn fill_fallback(buf: &mut [u8]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ ((std::process::id() as u64) << 32)
        ^ 0x9e37_79b9_7f4a_7c15;
    for b in buf.iter_mut() {
        // xorshift64*，仅作熵源失败时的兜底（正常路径走 getrandom）。
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
}

fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        fill_fallback(&mut b);
    }
    u32::from_le_bytes(b)
}

fn port_free(port: u16) -> bool {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => {
            drop(l);
            true
        }
        Err(_) => false,
    }
}

/// 在 20000–29999 里挑一个当前可绑定的端口。随机试 64 次，再顺序兜底。
pub fn pick_free_port() -> Option<u16> {
    let span = (PORT_RANGE_END - PORT_RANGE_START + 1) as u32;
    for _ in 0..64 {
        let port = PORT_RANGE_START + (random_u32() % span) as u16;
        if port_free(port) {
            return Some(port);
        }
    }
    (PORT_RANGE_START..=PORT_RANGE_END).find(|p| port_free(*p))
}

/// 拉起 `opencode serve` 并等到 `/api/info` 200。
pub async fn spawn(exe: &Path, startup_timeout: Duration) -> Result<ManagedServer, String> {
    let password = random_password();
    let port = pick_free_port().ok_or_else(|| "没有可用的本地端口（20000-29999）".to_string())?;
    let base = format!("http://127.0.0.1:{port}");

    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--hostname")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("OPENCODE_SERVER_PASSWORD", &password)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // tokio::process::Command 自带同名方法（内部转 std 的 CommandExt）。
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 {} 失败: {e}", exe.display()))?;
    let pid = child.id();

    // 趁子进程刚起来、还没跑远，立刻把它挂进 `KILL_ON_JOB_CLOSE` 的 Job Object。
    // 这一步失败就不托管：宁可启动失败（日志可见），也不留一个强杀父进程后无法
    // 回收的孤儿。拿不到句柄（spawn 后瞬间已退出）同样按失败处理。
    #[cfg(windows)]
    let job = match child.raw_handle() {
        Some(handle) => match job_guard_for_process(handle as isize) {
            Ok(job) => job,
            Err(e) => {
                if let Some(pid) = pid {
                    kill_pid(pid);
                }
                let _ = child.start_kill();
                let _ = child.try_wait();
                return Err(e);
            }
        },
        None => {
            if let Some(pid) = pid {
                kill_pid(pid);
            }
            let _ = child.start_kill();
            let _ = child.try_wait();
            return Err("opencode serve 刚启动就退出，拿不到进程句柄".to_string());
        }
    };

    // 必须持续排空 stdout/stderr，否则管道写满会把 opencode 卡住。
    if let Some(out) = child.stdout.take() {
        tokio::spawn(drain_lines(out, "serve"));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(drain_lines(err, "serve.err"));
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("构造健康检查客户端失败: {e}"))?;

    let deadline = Instant::now() + startup_timeout;
    loop {
        if info_ok(&client, &base, &password).await {
            return Ok(ManagedServer {
                child,
                pid,
                port,
                base,
                password,
                stopped: false,
                #[cfg(windows)]
                job: Some(job),
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!("opencode serve 提前退出（{status}）"));
            }
            Ok(None) => {}
            Err(e) => return Err(format!("检查 opencode serve 状态失败: {e}")),
        }
        if Instant::now() >= deadline {
            if let Some(pid) = pid {
                kill_pid(pid);
            }
            let _ = child.start_kill();
            let _ = child.try_wait();
            return Err(format!(
                "等待 opencode serve 就绪超时（{}s）",
                startup_timeout.as_secs()
            ));
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn info_ok(client: &reqwest::Client, base: &str, password: &str) -> bool {
    let mut req = client
        .get(format!("{base}/api/info"))
        .header(reqwest::header::ACCEPT, "application/json");
    if !password.is_empty() {
        req = req.basic_auth("opencode", Some(password));
    }
    match req.send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// 逐行把子进程输出打到日志；单行过长时交给 `BufReader` 的上限保护（不会无限涨）。
async fn drain_lines<R>(reader: R, tag: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if !line.is_empty() {
                    crate::logf(&format!("{tag}: {line}"));
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_url_safe_and_long_enough() {
        let p = random_password();
        assert!(p.len() >= 40, "32 字节 base64url 应至少 43 字符：{p}");
        assert!(
            p.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "密码含非 base64url 字符：{p}"
        );
        assert_ne!(p, random_password(), "两次随机密码不应相同");
    }

    #[test]
    fn free_port_is_in_range() {
        let p = pick_free_port().expect("本机应至少有一个空闲测试端口");
        assert!(
            (PORT_RANGE_START..=PORT_RANGE_END).contains(&p),
            "端口 {p} 不在范围"
        );
    }

    /// Windows 兜底机制自检：job 的最后一句柄关闭时，job 内进程必须被系统终止。
    ///
    /// 父进程被 `TerminateProcess` / `taskkill /F` 时不会跑 `Drop`，但内核会关闭
    /// 它持有的 job 句柄——本用例 `drop(job)` 模拟的正是这一步。用 `cmd /c ping`
    /// 当替身，不依赖本机安装 opencode。
    #[cfg(windows)]
    #[test]
    fn job_object_kills_child_when_last_handle_closes() {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use std::process::{Command as StdCommand, Stdio};
        use std::time::{Duration, Instant};

        let mut child = StdCommand::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1 > nul"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("应能启动 cmd 替身进程");
        let job = job_guard_for_process(child.as_raw_handle() as isize)
            .expect("应能创建 Job Object 并挂接子进程");
        assert!(
            job.query_process_id_list()
                .expect("应能查询 job 内进程")
                .contains(&(child.id() as usize)),
            "子进程没有进 job"
        );

        // 关闭最后一个 job 句柄——父进程被强杀时由内核替我们做的事。
        drop(job);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    let _ = child.kill();
                    panic!("job 句柄关闭后子进程 5s 仍未退出：KILL_ON_JOB_CLOSE 未生效");
                }
                Err(e) => panic!("查询子进程状态失败：{e}"),
            }
        }
    }
}
