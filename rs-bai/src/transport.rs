// transport.rs —— 上游客户端：本地代理自探测（对应 Go 的 `detectUpstreamTransport`）
// + 有限次重试（对应 `retryRoundTripper`）+ 出站请求构造与转发。
//
// Go 侧口径（`internal/bai/server.go`）：
//   * 已有 HTTPS_PROXY/https_proxy → 用系统环境变量决定的代理；否则顺次 TCP 试连
//     7897/7890/7892/7896/10809/1080，第一个连通的当代理；全不通则直连。
//   * 重试：传输错误或 5xx（含 Cloudflare 520）最多 3 次，间隔 300ms；4xx 不重试。
//     api.b.ai 对国内直连是间歇性失败的，520/502/503 意味着没到模型层（无计费），重放安全。
//   * 出站 Host 必须改成上游主机名（Go 的 Director 注释：Cloudflare 对不认识的主机名 403）。
//
// 与 Go 的已知差异（试点期如实记录，见 crate README）：Rust 侧没有 uTLS，
// TLS 指纹是 rustls 的默认 ClientHello，与 Go `net/http` 不同——这是圆桌 C 席点名的
// "Rust 无对等物"风险项，本轮不解决（B.AI 无签名/指纹红线，团结 8788 才会撞上）。

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use reqwest::{Client, RequestBuilder};

/// 本地代理候选端口，顺序与 Go 侧 `detectUpstreamTransport` 一字不差。
const LOCAL_PROXY_PORTS: [u16; 6] = [7897, 7890, 7892, 7896, 10809, 1080];

/// 重试上限（Go 侧 `retryRoundTripper{max: 3}` = 总共最多 3 次尝试）。
const MAX_ATTEMPTS: usize = 3;
/// 重试间隔（Go 侧 `time.Sleep(300 * time.Millisecond)`）。
const RETRY_DELAY: Duration = Duration::from_millis(300);

/// 上游客户端。持有所选代理与 base URL。
pub struct UpstreamClient {
    client: Client,
    base: String,
}

/// 代理探测结果（供启动日志如实打印，口径同 Go）。
pub enum ProxyChoice {
    Env(String),
    Local(String),
    Direct,
}

impl UpstreamClient {
    pub async fn new(base: &str) -> Result<Self, String> {
        let (choice, proxy) = detect_proxy().await;
        match choice {
            ProxyChoice::Env(p) => {
                // 只报存在性，**不打印变量值**——对齐 Go 侧 `detectUpstreamTransport`
                // （`internal/bai/server.go:202` 的日志同样不带值）。代理地址可能含凭据，
                // 值只在这一行就地丢弃，不落日志。
                let _ = p;
                println!(
                    "bai-plugin: 检测到 HTTPS_PROXY/https_proxy 环境变量，使用 http.DefaultTransport（按环境变量走代理）"
                )
            }
            ProxyChoice::Local(p) => println!("bai-plugin: 使用本地代理 {p}"),
            ProxyChoice::Direct => println!("bai-plugin: 未发现本地代理，上游直连"),
        }
        Self::with_proxy(base, proxy)
    }

    /// 显式指定代理构造（`None` = 直连，且**关掉 reqwest 对 HTTP_PROXY/NO_PROXY 的自动读取**）。
    /// 供测试与捕获对照固定出站路径——生产路径走 `new()` 的自探测。
    pub fn with_proxy(base: &str, proxy: Option<String>) -> Result<Self, String> {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90));
        // 不设总超时（对齐 Go：`internal/bai/server.go` 的 `http.Server` 没设
        // ReadTimeout/WriteTimeout/IdleTimeout，转发用的 `http.DefaultTransport`
        // 也只给 `DialContext` 30s 连接超时，**没有含响应体的总超时**）。
        // reqwest 未调用 `.timeout()` 即为无总超时；原先的 600s 总超时会在长流式
        // 响应上把连接掐断，比 Go 少收。
        match &proxy {
            Some(p) => {
                builder = builder
                    .proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理地址非法 {}: {}", p, e))?);
            }
            None => {
                builder = builder.no_proxy();
            }
        }
        let client = builder
            .build()
            .map_err(|e| format!("HTTP 客户端构造失败: {}", e))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// 出站 URL（上游 base + 路径与查询串）。捕获记录与转发共用这一处构造，避免两处漂移。
    pub fn upstream_url(&self, uri: &Uri) -> String {
        match uri.query() {
            Some(q) => format!("{}{}?{}", self.base, uri.path(), q),
            None => format!("{}{}", self.base, uri.path()),
        }
    }

    /// 转发一个请求到上游，返回下游响应。
    /// `body` 已由调用方做完改写（/v1/chat/completions 走 adapt_quirks_body），
    /// 出站捕获也由调用方在做完改写后记录（见 lib.rs 两个 handler）。
    pub async fn forward(
        &self,
        method: Method,
        uri: &Uri,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Response {
        let url = self.upstream_url(uri);

        let mut last_err: Option<String> = None;
        for attempt in 0..MAX_ATTEMPTS {
            let last = attempt == MAX_ATTEMPTS - 1;
            let req = match self.build_request(&method, &url, &headers, body.clone()) {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            };
            match req.send().await {
                Ok(resp) => {
                    if resp.status().is_server_error() && !last {
                        // 5xx：丢掉响应体后重试（520/502/503 没到模型层，重放安全）
                        drop(resp);
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    return self.relay(resp).await;
                }
                Err(e) => {
                    last_err = Some(format!("{e}"));
                    if last {
                        break;
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
        // 三次都失败：对应 Go 侧 ReverseProxy 的 502 行为
        eprintln!(
            "bai-plugin: 上游转发失败（已尝试 {} 次）: {}",
            MAX_ATTEMPTS,
            last_err.unwrap_or_default()
        );
        StatusCode::BAD_GATEWAY.into_response()
    }

    fn build_request(
        &self,
        method: &Method,
        url: &str,
        headers: &HeaderMap,
        body: Vec<u8>,
    ) -> Result<RequestBuilder, String> {
        let mut req = self.client.request(method.clone(), url);
        // 透传客户端头，剔除 hop-by-hop 与由传输层重算的项（Content-Length 由 reqwest 按实际体重算，
        // Host 由 URL 决定——Go 侧 Director 也是把 req.Host 改成上游主机名）。
        for (name, value) in headers.iter() {
            let n = name.as_str().to_ascii_lowercase();
            if matches!(
                n.as_str(),
                "host"
                    | "content-length"
                    | "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "proxy-authorization"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
            ) {
                continue;
            }
            req = req.header(name.clone(), value.clone());
        }
        // 出站捕获由调用方在本方法之前记录（那里的 body 才是最终出站体）
        if !body.is_empty() {
            req = req.body(body);
        }
        Ok(req)
    }

    /// 把上游响应中继给下游：状态码与原响应头照搬（剔 hop-by-hop），体流式转发。
    /// 对应 Go 侧 `FlushInterval = 50ms`——用流式 body 而非整段缓冲，SSE 逐帧到达即下推。
    async fn relay(&self, resp: reqwest::Response) -> Response {
        let status = resp.status();
        let upstream_headers = resp.headers().clone();
        let mut out = Response::builder().status(status);
        {
            let h = out.headers_mut().expect("builder 刚构造，headers 必然可用");
            for (name, value) in upstream_headers.iter() {
                let n = name.as_str().to_ascii_lowercase();
                if matches!(
                    n.as_str(),
                    "connection"
                        | "keep-alive"
                        | "proxy-connection"
                        | "te"
                        | "trailer"
                        | "transfer-encoding"
                        | "upgrade"
                        | "content-length"
                ) {
                    continue;
                }
                h.insert(name.clone(), value.clone());
            }
        }
        let stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
        let body = Body::from_stream(stream);
        out.body(body).unwrap_or_else(|e| {
            eprintln!("bai-plugin: 响应构造失败: {e}");
            let mut fallback = StatusCode::BAD_GATEWAY.into_response();
            fallback.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            fallback
        })
    }
}

/// 代理自探测：口径与 Go 侧 `detectUpstreamTransport` 一致。
async fn detect_proxy() -> (ProxyChoice, Option<String>) {
    for key in ["HTTPS_PROXY", "https_proxy"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return (ProxyChoice::Env(v.clone()), Some(v));
            }
        }
    }
    for port in LOCAL_PROXY_PORTS {
        let addr = format!("127.0.0.1:{port}");
        if tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
        {
            let url = format!("http://{addr}");
            return (ProxyChoice::Local(url.clone()), Some(url));
        }
    }
    (ProxyChoice::Direct, None)
}
