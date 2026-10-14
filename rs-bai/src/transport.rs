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

/// 转发路径的重试上限（Go 侧 `retryRoundTripper{max: 3}` = 总共最多 3 次尝试）；
/// 矩阵拉取的次数由 `RetryPolicy.max_attempts` 决定，两者不是同一件事。
const MAX_ATTEMPTS: usize = 3;
/// 重试间隔（Go 侧 `time.Sleep(300 * time.Millisecond)`）。
const RETRY_DELAY: Duration = Duration::from_millis(300);
/// 矩阵拉取的总预算（Go `matrixTimeout = 15s`）。
const MATRIX_DEADLINE: Duration = Duration::from_secs(15);

/// 上游客户端。持有所选代理与 base URL。
pub struct UpstreamClient {
    client: Client,
    /// 矩阵拉清单专用：同一个代理选择，**不设总超时**——超时由 `RetryPolicy::deadline`
    /// 罩住整次拉取（含重试），与 Go 的 `context.WithTimeout` 同口径。
    /// 转发路径同样无总超时（见 `with_proxy` 的注释），两者语义不同故不共用一个 client。
    matrix_client: Client,
    base: String,
}

/// 上游拉取的重试/超时口径（对应 Go `retryRoundTripper` + `context.WithTimeout`）。
///
/// Go 侧的两条事实决定了这个结构：
///   * `context.WithTimeout(15s)` 罩住**整次** `client.Do`（含其内部 3 次重试与 300ms 间隔），
///     不是「每次尝试各 15s」；
///   * 单发的唯一门槛是 `server.go:524` 的 `req.Body != nil && req.GetBody == nil`
///     （请求体存在**且**不可重放）。矩阵拉取是 **nil body 的 GET**
///     （`models.go:119`：`http.NewRequestWithContext(ctx, GET, url, nil)`）：
///     `net/http/request.go:906-923` 在 `body == nil` 时让 `req.Body` 保持 nil，
///     并且整段 `if body != nil` 被跳过 → `GetBody` 也是 nil；`NoBody` 只出现在
///     `body != nil && GetBody != nil && ContentLength == 0` 那个分支里，这里走不到。
///     门槛两条同时不成立 ⇒ **照走重试循环**，最多 3 次尝试（300ms 间隔、5xx/传输错误）。
///     第 3 轮把矩阵 GET 读成「一次也不重试」是错的，第 5 轮已更正：`max_attempts` 回到 3，
///     黄金基准同批重采，两个 502 场景的上游命中数从 1 变 3。
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// 最多尝试几次（Go `retryRoundTripper.max`）。
    pub max_attempts: usize,
    /// 重试间隔（Go `time.Sleep(300 * time.Millisecond)`）。
    pub retry_delay: Duration,
    /// 整次拉取的总预算（Go `matrixTimeout = 15s`）。
    pub deadline: Duration,
}

impl RetryPolicy {
    /// 与 Go `fetchUpstreamModels` 一字对齐：15s 总预算 + nil-body GET **同样**最多 3 次尝试
    /// （`models.go:26-27` 的 `&retryRoundTripper{transport: …, max: 3}`）。
    pub fn matrix_default() -> Self {
        Self {
            max_attempts: 3,
            retry_delay: RETRY_DELAY,
            deadline: MATRIX_DEADLINE,
        }
    }

    /// 测试用：可指定总预算与尝试次数（验证「总预算罩住全部尝试」时不必真等 15 秒）。
    pub fn for_test(max_attempts: usize, deadline: Duration) -> Self {
        Self {
            max_attempts,
            retry_delay: RETRY_DELAY,
            deadline,
        }
    }
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
        let client = proxy_builder(&proxy)?
            .connect_timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| format!("HTTP 客户端构造失败: {}", e))?;
        // 不设总超时（对齐 Go：`internal/bai/server.go` 的 `http.Server` 没设
        // ReadTimeout/WriteTimeout/IdleTimeout，转发用的 `http.DefaultTransport`
        // 也只给 `DialContext` 30s 连接超时，**没有含响应体的总超时**）。
        // reqwest 未调用 `.timeout()` 即为无总超时；原先的 600s 总超时会在长流式
        // 响应上把连接掐断，比 Go 少收。
        let matrix_client = proxy_builder(&proxy)?
            // 与转发 client 同值（Go `http.DefaultTransport` 的 `net.Dialer{Timeout: 30s}`）。
            // 真正生效的上限是 `RetryPolicy.deadline`（15s）——它罩住连接、等响应头与读体三段，
            // 见 `fetch_matrix_once`。
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP 客户端构造失败: {}", e))?;
        Ok(Self {
            client,
            matrix_client,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// 矩阵拉清单用的 client（**无总超时**；超时预算由 `RetryPolicy.deadline` 提供，
    /// 对应 Go `fetchUpstreamModels` 里的 `context.WithTimeout(15s)` + 新建 `http.Client`）。
    pub fn matrix_client(&self) -> Client {
        self.matrix_client.clone()
    }

    /// 上游 base URL（矩阵端点拼 `/v1/models` 用）。
    pub fn base_url(&self) -> &str {
        &self.base
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

/// 按同一个代理选择构造 reqwest builder（转发用与矩阵拉清单共用，避免两处漂移）。
fn proxy_builder(proxy: &Option<String>) -> Result<reqwest::ClientBuilder, String> {
    let b = Client::builder();
    match proxy {
        Some(p) => b.proxy(reqwest::Proxy::all(p).map_err(|e| format!("代理地址非法 {}: {}", p, e))?),
        None => b.no_proxy(),
    }
    .pipe(Ok)
}

/// 拉上游 `/v1/models` 清单（对应 Go `fetchUpstreamModels`）：
/// 带 `Bearer <key>`、`data` 必须存在且是数组；返回 `(data 原文, 条数)`。
///
/// 超时/重试口径全部来自 `policy`（见 `RetryPolicy` 的说明）：
///   * `policy.deadline` 是整个循环的**总预算**（Go 的 context 罩住全部重试），
///     连接、等响应头、读体三段都算在里面：每一轮开始前查 deadline，`req.send()` 与读体
///     各自套在**剩余**预算里；超了如实报错，不再开新尝试。
///   * `policy.max_attempts = 3`（矩阵 GET 的 Go 口径，见 `RetryPolicy`）。
///   * **响应体读取失败不重试**：Go 的重试只发生在 `client.Do`（拿到响应头之前），
///     `io.ReadAll` 出错时直接回错误、不重发。
pub async fn fetch_matrix_upstream(
    client: &Client,
    url: &str,
    key: &str,
    policy: &RetryPolicy,
) -> Result<(Vec<u8>, usize), String> {
    let start = tokio::time::Instant::now();
    let mut last_err = String::new();
    let attempts = policy.max_attempts.max(1);
    for attempt in 0..attempts {
        let last = attempt == attempts - 1;
        if start.elapsed() >= policy.deadline {
            // Go: context deadline exceeded —— 预算已耗尽，剩余尝试不再发起。
            return Err("context deadline exceeded".to_string());
        }
        let remaining = policy.deadline.saturating_sub(start.elapsed());
        match fetch_matrix_once(client, url, key, remaining).await {
            Ok(v) => return Ok(v),
            Err(MatrixFetchErr::Retryable(e)) => {
                last_err = e;
                if last {
                    break;
                }
                tokio::time::sleep(policy.retry_delay).await;
            }
            Err(MatrixFetchErr::Fatal(e)) => return Err(e),
        }
    }
    Err(last_err)
}

/// 矩阵拉清单的错误分类：Go 的 `retryRoundTripper` 只在**传输错误 / 5xx** 上重试
/// （矩阵 GET 的 nil body 不在 `server.go:524` 的单发门槛里，所以它**会**重试），
/// 4xx 与「清单无 data 数组」是确定性失败，重试没有意义。
enum MatrixFetchErr {
    Retryable(String),
    Fatal(String),
}

async fn fetch_matrix_once(
    client: &Client,
    url: &str,
    key: &str,
    budget: Duration,
) -> Result<(Vec<u8>, usize), MatrixFetchErr> {
    let req = client
        .get(url)
        .header(header::AUTHORIZATION, format!("Bearer {key}"));
    // 连接 + 等响应头也套在**剩余总预算**里：Go 的 `context.WithTimeout` 在 `client.Do`
    // 阶段同样会中止请求，而 reqwest 的 client 只有 30s 连接超时、没有总超时，
    // 不套这一层就会在「上游收下连接却永不应答」时无限等下去（第 5 轮 R3 修的就是这个）。
    let resp = match tokio::time::timeout(budget, req.send()).await {
        Ok(Ok(r)) => r,
        // 到点即整次失败：与读体阶段同口径（下一轮开头也会因预算耗尽而退出，不再重发）
        Err(_) => return Err(MatrixFetchErr::Fatal("context deadline exceeded".into())),
        Ok(Err(e)) => return Err(MatrixFetchErr::Retryable(format!("{e}"))),
    };
    let status = resp.status();
    if status.is_server_error() {
        return Err(MatrixFetchErr::Retryable(format!(
            "上游 HTTP {}",
            status.as_u16()
        )));
    }
    // 读体也在总预算内（Go 的 context 在读体阶段同样会超时，且**读了就不重试**）。
    let body = match tokio::time::timeout(budget, read_limited(resp, crate::matrix::MATRIX_MAX_BYTES)).await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Err(MatrixFetchErr::Fatal(e)),
        Err(_) => return Err(MatrixFetchErr::Fatal("context deadline exceeded".into())),
    };
    if status != StatusCode::OK {
        return Err(MatrixFetchErr::Fatal(format!(
            "上游 HTTP {}",
            status.as_u16()
        )));
    }
    // 把 `data` 的**原始 JSON 文本**抠出来（对应 Go 的 `json.RawMessage`）：
    // 解析成 Value 再序列化会重排对象键序，那样响应体就不是上游原文了。
    // `Box<RawValue>.get()` 给的正是该值在原文档里的那段字节（serde_json 的文档保证）。
    #[derive(serde::Deserialize)]
    struct Envelope {
        data: Box<serde_json::value::RawValue>,
    }
    let env: Envelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(_) => return Err(MatrixFetchErr::Fatal("上游清单无 data 数组".into())),
    };
    // Go 的判据是 `len(envelope.Data) == 0`（RawMessage 的**字节长度**）：
    // 因此 `{"data": null}` 与 `{"data": []}` 都是「存在」，都算成功、不报错；
    // 只有「字段缺失」和「显式空串」才是 0 长度。这里按字节比较，不用字符串转换。
    let data = env.data.get().as_bytes();
    if data.is_empty() {
        return Err(MatrixFetchErr::Fatal("上游清单无 data 数组".into()));
    }
    // 与 Go 一样把 data 解成数组取条数。Go 的 `json.Unmarshal("null", &[]T{})` 结果是
    // nil 切片、**不报错**（count=0）；serde_json 反过来会拒（null 不是 array），
    // 这里显式按 Go 的口径处理 `null`，其余形态照常解。
    if data == b"null" {
        return Ok((b"null".to_vec(), 0));
    }
    let items: Vec<serde_json::Value> = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return Err(MatrixFetchErr::Fatal("上游 data 不是数组".into())),
    };
    Ok((data.to_vec(), items.len()))
}

/// 读满上限即报错（对应 Go `io.LimitReader(resp.Body, 4<<20)`：
/// 4MB 之内原样接受，超出即「读不动」）。
async fn read_limited(resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读上游响应失败: {e}"))?;
    if bytes.len() > limit {
        return Err(format!("上游响应超过 {limit} 字节上限"));
    }
    Ok(bytes.to_vec())
}

/// 小工具：把值塞进 `Result`（避免多一层临时变量）。
trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写一个「回了响应头就开始吊着、正文永不到达」的上游（**不引新 dev-dependency**）。
    async fn hanging_upstream() -> String {
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = ln.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n")
                        .await;
                    // 正文永不发送；连接就吊在这里，直到对方断开
                    std::future::pending::<()>().await;
                });
            }
        });
        format!("http://{addr}/v1/models")
    }

    /// `policy.deadline` 是**整次拉取的总预算**（Go 的 context 罩住全部尝试），
    /// 不是「每次尝试各一份」。这里用 2 秒预算 + 3 次尝试（每次都要 300ms 间隔）验证：
    /// 若预算按次算，最坏会跑 3×2s + 2×300ms ≈ 6.6s；按总预算算则 ≤ 2s 出头。
    #[tokio::test]
    async fn matrix_deadline_is_a_total_budget_not_per_attempt() {
        let url = hanging_upstream().await;
        let client = Client::builder().no_proxy().build().unwrap();
        let policy = RetryPolicy::for_test(3, Duration::from_secs(2));

        let start = std::time::Instant::now();
        let err = fetch_matrix_upstream(&client, &url, "sk-test", &policy)
            .await
            .expect_err("上游吊住正文时必须报错，不得永远等下去");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(4000),
            "总预算 2 秒，实际耗时 {elapsed:?}——看形态像是「每次尝试各给一份预算」了"
        );
        assert!(
            !err.is_empty(),
            "错误信息不能是空的（要能在日志里说清失败原因）"
        );
        println!("总预算验证：2s 预算 / 3 次尝试，实际 {elapsed:?}，错误 = {err}");
    }

    /// `policy.deadline` 还要罩住**连接与等响应头**阶段，不只是读体（第 5 轮 R3）：
    /// 上游 accept 了 TCP 却一个字节都不回，若 `req.send()` 不被剩余预算包住，
    /// 这里的 client 没有总超时（只有 30s 连接超时，而连接已经建成）→ 会永远挂住。
    #[tokio::test]
    async fn matrix_deadline_covers_waiting_for_response_headers() {
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        tokio::spawn(async move {
            // 收下连接就攥住不放，**不回任何字节**（连状态行都不发）
            let mut held = Vec::new();
            loop {
                let Ok((sock, _)) = ln.accept().await else {
                    return;
                };
                held.push(sock);
            }
        });

        let client = Client::builder().no_proxy().build().unwrap();
        let policy = RetryPolicy::for_test(3, Duration::from_secs(2));
        let start = std::time::Instant::now();
        let err = fetch_matrix_upstream(
            &client,
            &format!("http://{addr}/v1/models"),
            "sk-test",
            &policy,
        )
        .await
        .expect_err("上游永不应答时必须报错，不得永远等下去");
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(4000),
            "总预算 2 秒，实际耗时 {elapsed:?}——send 阶段没有被剩余预算罩住"
        );
        assert!(!err.is_empty(), "错误信息不能是空的");
        println!("连接/等响应头阶段的总预算验证：实际 {elapsed:?}，错误 = {err}");
    }

    /// `data: null` 是 Go 眼中的成功（`len(RawMessage)==4`、数组解成 nil、count=0），
    /// 对齐后 Rust 侧也必须成功——这是第 3 轮审查点名的**中**级合同分歧。
    #[tokio::test]
    async fn matrix_data_null_is_success_like_go() {
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ln.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = ln.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let body = br#"{"object":"list","data":null}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    // 不能立刻 drop：写完就关会把 FIN/RST 抢在客户端读完之前发出
                    // （reqwest 报 "error sending request"，第一版就栽在这里）。
                    tokio::time::sleep(Duration::from_millis(200)).await;
                });
            }
        });

        let client = Client::builder().no_proxy().build().unwrap();
        let policy = RetryPolicy::matrix_default();
        let (data, count) =
            fetch_matrix_upstream(&client, &format!("http://{addr}/v1/models"), "sk-test", &policy)
                .await
                .expect("data:null 必须按成功处理（Go 的口径）");
        assert_eq!(data, b"null", "data 必须是上游原文 null（4 字节）");
        assert_eq!(count, 0, "null 解成空数组，条数为 0");
    }
}
