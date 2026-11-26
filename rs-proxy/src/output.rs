//! 三协议的增量交付骨架：上游 SSE 事件驱动下游 chunk，而不是先攒完再写。
//!
//! * [`LiveSse`] 是 `Body::from_stream` 可直接消费的字节流；
//! * [`ChannelSink`] 是协议翻译器向该流写入的出口（接收端被丢弃 = 客户端取消，
//!   发送失败只标记关闭，不 panic）；
//! * [`UsageRecorder`] 在流结束/取消/解码错误时同步入账一次（不再另起任务）。

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::stats::UsageStats;

/// 下游 SSE 字节流。
pub struct LiveSse {
    rx: UnboundedReceiver<Vec<u8>>,
}

impl futures_util::Stream for LiveSse {
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(chunk))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 协议翻译器的下游出口：三协议都直接写通道（结构由 `ChannelSink` 持有）。
pub trait SseSink {
    fn push_chunk(&mut self, chunk: Vec<u8>);
}

/// 建一对（下游流，翻译器出口）。
pub fn channel() -> (LiveSse, ChannelSink) {
    let (tx, rx) = unbounded_channel();
    (LiveSse { rx }, ChannelSink { tx, closed: false })
}

/// 协议翻译器的 chunk 出口。
pub struct ChannelSink {
    tx: UnboundedSender<Vec<u8>>,
    closed: bool,
}

impl ChannelSink {
    /// 写一块下游字节；接收端已断开（客户端取消）后静默丢弃并标记关闭。
    pub fn push(&mut self, chunk: Vec<u8>) {
        if self.closed {
            return;
        }
        if self.tx.send(chunk).is_err() {
            self.closed = true;
        }
    }

    /// 下游是否已取消。翻译循环据此停止继续读上游。
    pub fn is_closed(&self) -> bool {
        self.closed || self.tx.is_closed()
    }
}

impl SseSink for ChannelSink {
    fn push_chunk(&mut self, chunk: Vec<u8>) {
        self.push(chunk);
    }
}

/// 流式 usage 记账：在流对象被丢弃（正常结束、客户端取消、解码错误）时入账一次。
///
/// 只做同步锁内累加，不 spawn 任务、不重试网络。
pub struct UsageRecorder {
    stats: Arc<Mutex<UsageStats>>,
    model: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    recorded: bool,
}

impl UsageRecorder {
    pub fn new(stats: Arc<Mutex<UsageStats>>, model: &str) -> Self {
        Self {
            stats,
            model: model.to_string(),
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            recorded: false,
        }
    }

    /// 每个上游事件后同步累计（与缓冲路径的最终 `record` 同口径）。
    pub fn add(&mut self, input: i64, output: i64, cache_read: i64, cache_write: i64) {
        self.input += input;
        self.output += output;
        self.cache_read += cache_read;
        self.cache_write += cache_write;
    }

    pub fn record(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        if self.input > 0 || self.output > 0 || self.cache_read > 0 || self.cache_write > 0 {
            if let Ok(mut stats) = self.stats.lock() {
                stats.record(
                    &self.model,
                    self.input,
                    self.output,
                    self.cache_read,
                    self.cache_write,
                );
            }
        }
    }
}

impl Drop for UsageRecorder {
    fn drop(&mut self) {
        self.record();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_recorder_records_once_and_handles_cancel() {
        let stats = Arc::new(Mutex::new(UsageStats::new("")));
        {
            let mut rec = UsageRecorder::new(stats.clone(), "glm-5.3");
            rec.add(7, 3, 0, 0);
        }
        let first = stats.lock().unwrap().snapshot().models.get("glm-5.3").cloned();
        assert_eq!(first.map(|m| m.runs), Some(1), "流结束时入账一次");
        // 丢弃后再次创建不会重复入账（新 recorder 各记各的）
        let mut rec = UsageRecorder::new(stats.clone(), "glm-5.3");
        rec.record();
        let calls = stats
            .lock()
            .unwrap()
            .snapshot()
            .models
            .get("glm-5.3")
            .map(|m| m.runs)
            .unwrap_or(0);
        assert_eq!(calls, 1, "无 usage 的流不额外记调用");
    }
}
