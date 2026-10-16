//! rs-tuanjie —— 团结（Codely）插件 Rust 重写，**P8a 小轮：凭证链与请求整形**。
//!
//! 逐字节复刻 `internal/tuanjie/client.go` + `reqshape.go`（Go 侧只读不改）。八项红线
//! 的落点见 `README.md` 的「八项红线对照表」；每一项都有 Go 行号 + Rust 位置 + 黄金门禁。
//!
//! 本小轮**不含**服务与转发编排（`server.go`/`handlers_extra.go` 等，属 P8b）；
//! 出站 TLS 指纹的取舍见 `README.md`「uTLS 路线取证结论」（P8a 用 rustls，差异已登记）。

pub mod appdir;
pub mod client;
pub mod credentials;
pub mod gojson;
pub mod headers;
pub mod reqshape;
pub mod session;
pub mod sign;
pub mod transport;
pub mod ua;

use std::time::Duration;

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
