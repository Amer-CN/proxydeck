//! P8b 第三片差分对照 runner（Rust 侧）—— 把固定请求序列打给**已启动的**一个服务实例，
//! 记录每次响应的 `{status, content_type, body}` 与假上游观测量，写成 JSON。
//!
//! 两侧（Rust 起本 crate 的 server；Go 起 `internal/tuanjie` 的 Server）用**同一份**
//! 用例（`tests/p8b3_scenario/mod.rs` 经 `#[path]` 复用）与同一串环境变量，产物由
//! `.work/p8b3/compare.mjs` 逐字节 diff。
//!
//! 纪律：只打 `127.0.0.1` 上的临时端口；不碰 8788；合成账号；不读真实凭据。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[path = "../../tests/p8b3_scenario/mod.rs"]
mod scenario;

use scenario::{cases, normalize_body, Env, FakeUpstream, Report, Resp, SeenReq};

const USAGE: &str = "\
用法: p8b3-diff-run --server-bin <exe> [--port N] --out <report.json> [--root DIR]
  --server-bin  rs-tuanjie 可执行文件（本 runner 会 spawn 它，--port N --data-dir <临时数据目录>）
  --port        服务监听端口（默认 18788，**不得用 8788**）
  --out         报告 JSON 落盘路径
";

#[tokio::main]
async fn main() {
    let mut server_bin: Option<String> = None;
    let mut port: u16 = 18788;
    let mut out: Option<String> = None;
    let mut root: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server-bin" => server_bin = args.next(),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(18788),
            "--out" => out = args.next(),
            "--root" => root = args.next(),
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                return;
            }
            other => {
                eprintln!("未知参数 {other}\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let (Some(server_bin), Some(out)) = (server_bin, out) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    if port == 8788 {
        eprintln!("拒绝：8788 是生产端口（本任务纪律：不得碰）");
        std::process::exit(2);
    }
    let root = root.map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!("p8b3-run-{}", std::process::id()))
    });
    // 每次跑都从**干净状态**起：账号池/统计/pacing 这几份落盘文件会被上一轮的 POST
    // 改过（`accounts-get` 因此与 Go 侧对不上——踩过一次），残留会让两侧从不同初始态出发。
    if root.exists() {
        let _ = std::fs::remove_dir_all(&root);
    }
    std::fs::create_dir_all(&root).expect("建 runner 根目录");

    // 1) 隔离环境：合成 HOME + 临时数据目录（真实凭据与账号池零命中）
    let env = Env::create(&root);
    let token = format!("Bearer {}", scenario::synthetic_jwt());

    // 2) 假上游：该 token 的计费应答固定为 200 + 摘要 JSON
    let up = FakeUpstream::start(vec![(token.clone(), 200, scenario::BILLING_OK_JSON.to_string())]);

    // 3) 起被测服务（子进程），把它指向假上游
    let addr = format!("127.0.0.1:{port}");
    // `/inflight` 的合成记录用「触发器文件」控制注入时机（`Setup::InsertInflight` 那一步
    // 才落文件）：对照序列里 /inflight 有两条用例（空 → 有一条），启动时就注入会让第一条变非空。
    let inflight_trigger = root.join("p8b3-inflight-trigger");
    let _ = std::fs::remove_file(&inflight_trigger);
    let mut child = Command::new(&server_bin)
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(&env.data)
        .env("USERPROFILE", &env.home)
        .env("PROXYDECK_DATA_DIR", &env.data)
        // 服务侧要把两个域都指到假上游（换 key / 计费 / LiteLLM 三条路径）
        .env("RS_TUANJIE_TEST_BASES", format!("{} {}", up.base(), up.base()))
        // 并给出 /inflight 合成记录的触发器文件路径
        .env("RS_TUANJIE_TEST_INFLIGHT", &inflight_trigger)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {server_bin} 失败: {e}"));

    // 4) 等就绪（探 /health，最多 15 秒）
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("建对照用 HTTP 客户端");
    let mut ready = false;
    for _ in 0..150 {
        if let Ok(r) = client
            .get(format!("http://{addr}/health"))
            .send()
            .await
        {
            if r.status().is_success() {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !ready {
        let _ = child.kill();
        panic!("服务 15 秒内未就绪（{addr}）");
    }

    // 5) 跑固定请求序列（原始字节 + 响应头，不做任何解码）
    let mut records: Vec<(String, Resp)> = Vec::new();
    for c in cases() {
        // 用例前置动作 —— 与 Go 侧采集器**同一份规格**（`Setup` 枚举）。
        if c.setup == scenario::Setup::InsertInflight {
            std::fs::write(&inflight_trigger, b"1").expect("落 /inflight 触发器");
        }
        let url = format!("http://{addr}{}", c.path);
        let mut rb = client.request(
            reqwest::Method::from_bytes(c.method.as_bytes()).expect("方法名"),
            &url,
        );
        if let Some(b) = c.body {
            rb = rb.header("Content-Type", "application/json").body(b);
        }
        let resp = rb.send().await.expect("请求失败（服务已就绪）");
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let raw = resp.bytes().await.unwrap_or_default();
        let body = String::from_utf8_lossy(&raw).into_owned();
        records.push((
            c.name.to_string(),
            Resp {
                status,
                content_type,
                body: normalize_body(&body),
            },
        ));
    }

    // 6) 收观测 + 收尾
    let mut billing_hits = std::collections::BTreeMap::new();
    for t in [token.clone(), "Bearer uid-not-in-pool".to_string()] {
        billing_hits.insert(t.clone(), up.billing_hits(&t));
    }
    let report = Report {
        cases: records,
        billing_hits,
        cli_key_hits: up.cli_key_hits(),
        models_hits: up.models_hits(),
        seen: up.seen().iter().map(|s| SeenReq { ..s.clone() }).collect(),
    };
    let _ = child.kill();
    let _ = child.wait();

    let json = serde_json::to_string_pretty(&report.to_json()).expect("序列化报告");
    std::fs::write(&out, json).unwrap_or_else(|e| panic!("写报告 {out} 失败: {e}"));
    eprintln!("报告已写：{out}");
}
