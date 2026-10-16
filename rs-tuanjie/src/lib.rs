//! rs-tuanjie —— 团结（Codely）插件 Rust 重写，**P8a 小轮：凭证链与请求整形**。
//!
//! 逐字节复刻 `internal/tuanjie/client.go` + `reqshape.go`（Go 侧只读不改）。八项红线
//! 的落点见 `README.md` 的「八项红线对照表」；每一项都有 Go 行号 + Rust 位置 + 黄金门禁。
//!
//! 本小轮**不含**服务与转发编排（`server.go`/`handlers_extra.go` 等，属 P8b）；
//! 出站 TLS 指纹的取舍见 `README.md`「uTLS 路线取证结论」（P8a 用 rustls，差异已登记）。
//!
//! **P8b 切片（本轮）**已迁入 `pacing`（KIMI-K3 429 兜底）/ `inflight`（进行中注册表 +
//! 实时动态）/ `opencode_session`（`x-opencode-session`）/ `dist`（分布指纹纯函数）/
//! `channels`（注水渠道静态表）/ `accounts`（账号池）/ `quota`（积分）/
//! `stats`（按模型累计 + 落盘）/ `server`（**服务骨架与路由表**，本片新增）；
//! 剩余 P8b 面（`handleChat` 池路径 / `handlers_extra.go` 的注水与媒体 / `providers.go` /
//! 两条转发 / `media.go` / `probes.go` / `model/info` 聚合）见 `README.md` 的未覆盖清单。

pub mod accounts;
pub mod appdir;
pub mod channels;
pub mod client;
pub mod credentials;
pub mod crypto;
pub mod dist;
pub mod gojson;
pub mod headers;
pub mod inflight;
pub mod opencode_session;
pub mod pacing;
pub mod quota;
pub mod reqshape;
pub mod server;
pub mod session;
pub mod sign;
pub mod stats;
pub mod transport;
pub mod ua;

use std::time::Duration;

/// 团结侧的日志行（Go 用标准 `log`，带 `YYYY/MM/DD HH:MM:SS` 行头）。
/// 本 crate 的其余模块也用同一格式，便于与 8788 的日志逐行并排看。
pub fn logf(msg: &str) {
    eprintln!(
        "{} {}",
        chrono::Local::now().format("%Y/%m/%d %H:%M:%S"),
        msg
    );
}

/// 团结账号域（登录、换 key、dashboard、埋点）——`client.go:35`。
pub const CODEY_API_BASE: &str = "https://codely.tuanjie.cn";

/// 推理域（LiteLLM）——`client.go:36`。官方 CLI 的 chat 请求也直连这里。
pub const LITELLM_API_BASE: &str = "https://codely-litellm.tuanjie.cn";

/// 换 key 端点——`client.go:37`。**红线 5**：不换源、不加 `?teamId`。
pub const CLI_API_KEY_URL: &str = "https://codely.tuanjie.cn/api/api-token/cli-api-key";

/// cli_api_key 缓存时长——`client.go:38`。
pub const KEY_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 转发客户端总超时——`client.go:39`。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// 换 key 请求超时——`client.go:40`。
pub const KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// 统一的错误类型。文案尽量与 Go 侧一致（P8b 要把它翻成下游可读的响应）。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("读取团结登录态失败（请先登录团结 Cowork 桌面端）")]
    NoLoginState,
    #[error("读取团结登录态失败（请先登录团结 Cowork 桌面端）: {0}")]
    ReadLoginState(#[source] std::io::Error),
    #[error("解析 oauth_creds.json 失败: {0}")]
    ParseLoginState(#[source] serde_json::Error),
    #[error("oauth_creds.json 里没有 access_token")]
    EmptyAccessToken,
    #[error("换取 cli_api_key 网络失败: {0}")]
    FetchKeyNetwork(#[source] reqwest::Error),
    #[error("换取 cli_api_key 返回 {status}: {body}")]
    FetchKeyStatus { status: u16, body: String },
    #[error("换取 cli_api_key 响应异常: {0}")]
    FetchKeyBody(String),
    #[error("转发请求网络失败: {0}")]
    Forward(#[source] reqwest::Error),
}

/// 结果别名。
pub type Result<T> = std::result::Result<T, Error>;
