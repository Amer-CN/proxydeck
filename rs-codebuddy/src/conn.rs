//! conn.rs —— 连接级「客户端已断开」信号（差异 13：499 的取消源）。
//!
//! Go 的 `r.Context()` 是**请求级 context**：客户端断开连接时（HTTP/1.1 上 Go 的
//! `connReader` 后台读检测到 EOF/错误）立刻 Done，所以 `select { <-time.After(backoff) ;
//! <-r.Context().Done() }` 能在 11133 退避等待**期间**中断并回 499。
//!
//! axum 侧要拿到同一信号，用**文档化**的扩展点拼出来（不引 `hyper_util`、不自写 accept 循环）：
//! 1. 自定义 `Listener`：把每个 accept 到的 `TcpStream` 包进 `WatchedIo`，同一连接共享一个
//!    `watch::Sender<bool>`；`poll_read` 返回 `Ok(0)`（EOF）或 `Err` 时把它置 true；
//! 2. `impl Connected<IncomingStream<'_, WatchedListener>> for ConnClosed`：靠
//!    `Router::into_make_service_with_connect_info` 把**同一个连接的**信号作为请求扩展塞给
//!    handler（`ConnectInfo` 的既有机制，逐连接、逐请求）。
//!
//! 于是 handler 拿到的是「本连接已断开」的未来，语义与 Go 的 `r.Context().Done()` 对齐。

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::serve::{IncomingStream, Listener};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// 连接级断开信号（handler 侧持有；`closed()` 完成即代表客户端已断开）。
#[derive(Clone)]
pub struct ConnClosed {
    rx: watch::Receiver<bool>,
}

impl ConnClosed {
    /// 等到客户端断开（已断开时立即返回）。
    pub async fn closed(&self) {
        let mut rx = self.rx.clone();
        let _ = rx.wait_for(|v| *v).await;
    }

    /// 当前是否已断开（`wait_for` 之前的一次快速判定，测试用）。
    pub fn is_closed(&self) -> bool {
        *self.rx.borrow()
    }
}

/// 连接级信号的发送端（`WatchedIo` 持有）。
struct ClosedTx {
    tx: watch::Sender<bool>,
    fired: bool,
}

impl ClosedTx {
    fn fire(&mut self) {
        if !self.fired {
            self.fired = true;
            let _ = self.tx.send(true);
        }
    }
}

/// 包住 `TcpStream`，在读到 EOF/错误时点亮断开信号。
pub struct WatchedIo {
    inner: TcpStream,
    closed: Arc<watch::Sender<bool>>,
    fired: bool,
}

impl WatchedIo {
    fn guard(&self) -> ConnClosed {
        ConnClosed {
            rx: self.closed.subscribe(),
        }
    }
}

impl AsyncRead for WatchedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let r = Pin::new(&mut me.inner).poll_read(cx, buf);
        let mut tx = ClosedTx {
            tx: (*me.closed).clone(),
            fired: me.fired,
        };
        match &r {
            Poll::Ready(Ok(())) if buf.filled().is_empty() => tx.fire(),
            Poll::Ready(Err(_)) => tx.fire(),
            _ => {}
        }
        me.fired = tx.fired;
        r
    }
}

impl AsyncWrite for WatchedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// 自定义监听器：accept 时装配 `WatchedIo`（与 axum 对 `TcpListener` 的实现同款：错误重试）。
pub struct WatchedListener {
    inner: TcpListener,
}

impl WatchedListener {
    pub fn new(inner: TcpListener) -> WatchedListener {
        WatchedListener { inner }
    }
}

impl Listener for WatchedListener {
    type Io = WatchedIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((s, addr)) => {
                    let (tx, _rx) = watch::channel(false);
                    return (
                        WatchedIo {
                            inner: s,
                            closed: Arc::new(tx),
                            fired: false,
                        },
                        addr,
                    );
                }
                // 与 axum 的 `handle_accept_error` 同义：EMFILE/ECONNABORTED 等退避重试。
                Err(e) => {
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted
                    ) {
                        continue;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl axum::extract::connect_info::Connected<IncomingStream<'_, WatchedListener>> for ConnClosed {
    fn connect_info(stream: IncomingStream<'_, WatchedListener>) -> Self {
        stream.io().guard()
    }
}
