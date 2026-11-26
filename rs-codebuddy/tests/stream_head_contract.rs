//! A01：WorkBuddy 200 流头分片必须按“协议完成度”判定，不能把网络块边界当帧边界。

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{send, write_auth_files, AppServer, EnvGuard, TestDir};
use rs_codebuddy::cred::StubCredential;
use rs_codebuddy::pool::AccountPool;
use rs_codebuddy::region::Region;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct SplitUpstream {
    base: String,
    hits: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl SplitUpstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let sink = hits.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                sink.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut chunk = [0u8; 8192];
                let header_end = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while request.len() < header_end + content_len {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..n]);
                }

                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                // 第一个 HTTP chunk 只给 `dat`；随后的 `a: ...` 才组成合法 data: 帧。
                socket.write_all(b"3\r\ndat\r\n").await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(40)).await;
                let rest = b"a: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
                socket
                    .write_all(format!("{:x}\r\n", rest.len()).as_bytes())
                    .await
                    .unwrap();
                socket.write_all(rest).await.unwrap();
                socket.write_all(b"\r\n0\r\n\r\n").await.unwrap();
                socket.flush().await.unwrap();
            }
        });
        Self { base, hits, handle }
    }
}

impl Drop for SplitUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 真实 HTTP 入口：首块停在 `dat` 时不得判 EmptyStream 并重放同一账号。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_data_prefix_does_not_resend_the_account() {
    let upstream = SplitUpstream::start().await;
    let auth = TestDir::new("stream-head-auth");
    let data = TestDir::new("stream-head-data");
    let home = TestDir::new("stream-head-home");
    let env = EnvGuard::set(&[]);
    write_auth_files(
        auth.path(),
        &json!([{
            "name": "synthetic.info",
            "body": "{\"auth\":{\"accessToken\":\"synthetic-token\",\"domain\":\"codebuddy.cn\",\"expiresAt\":4102416000000},\"account\":{\"uid\":\"u-stream-head\",\"nickname\":\"synthetic\"}}",
            "mtime_unix_ms": 1_792_000_000_000u64
        }]),
    );
    let auth_s = auth.path().to_string_lossy();
    let data_s = data.path().to_string_lossy();
    let home_s = home.path().to_string_lossy();
    env.set_locked(&[
        ("CODEBUDDY_AUTH_DIR", Some(auth_s.as_ref())),
        ("PROXYDECK_DATA_DIR", Some(data_s.as_ref())),
        ("CODEBUDDY_POOL_DIR", Some(data_s.as_ref())),
        ("USERPROFILE", Some(home_s.as_ref())),
        ("LOCALAPPDATA", Some("")),
        ("XDG_DATA_HOME", Some("")),
    ]);
    let pool = AccountPool::new(Region::Cn).expect("合成账号应入池");
    assert_eq!(pool.size(), 1);
    let cred = Arc::new(
        StubCredential::from_json(&json!({
            "headers": {"Content-Type":"application/json","Authorization":"Bearer synthetic","X-User-Id":"u-stream-head"},
            "headers_ok": true
        }))
        .unwrap(),
    );
    let mut state = rs_codebuddy::AppState::new(
        Region::Cn,
        cred,
        upstream.base.clone(),
        data.path().to_path_buf(),
    );
    state.pool = Some(pool);
    let app = AppServer::start(Arc::new(state)).await;
    let body = serde_json::to_vec(&json!({
        "model":"glm-5.2",
        "messages":[{"role":"user","content":"hello"}],
        "stream":true
    }))
    .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        send(&app.base, "POST", "/v1/chat/completions", Some(&body)),
    )
    .await
    .expect("HTTP 请求不得挂死");
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(response.text().contains("\"content\":\"ok\""), "{}", response.text());
    assert!(response.text().trim_end().ends_with("data: [DONE]"), "{}", response.text());
    assert_eq!(
        upstream.hits.load(Ordering::SeqCst),
        1,
        "合法 SSE 的 dat/a: 分片只能触发一次上游请求"
    );
    env.point_to_empty(data.path());
}

#[tokio::test]
async fn split_data_prefix_is_incomplete_and_all_bytes_survive() {
    use axum::body::Bytes;
    use rs_codebuddy::chat::ChunkReader;
    use rs_codebuddy::failover::{classify_stream_head, HeadCompleteness, StreamVerdict};

    let parts: [&[u8]; 3] = [
        b"dat",
        b"a: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        b"data: [DONE]\n\n",
    ];
    let chunks: Vec<Result<Bytes, std::io::Error>> = parts
        .iter()
        .map(|part| Ok(Bytes::from_static(part)))
        .collect();
    let mut reader = ChunkReader::from_stream(Box::pin(futures_util::stream::iter(chunks)));
    let probe = reader.peek_within(8 * 1024, Duration::from_millis(100)).await;
    assert_eq!(probe.bytes, b"dat");
    assert_eq!(probe.completeness, HeadCompleteness::Incomplete);
    assert_eq!(
        classify_stream_head(
            &String::from_utf8_lossy(&probe.bytes),
            probe.completeness
        ),
        StreamVerdict::Ok
    );

    let mut got = Vec::new();
    while let Some(chunk) = reader.next_chunk().await {
        got.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(got, parts.concat(), "peek 后一个字节都不能丢");
}

#[tokio::test]
async fn eof_and_budget_have_distinct_completeness() {
    use axum::body::Bytes;
    use rs_codebuddy::chat::ChunkReader;
    use rs_codebuddy::failover::HeadCompleteness;

    let mut eof = ChunkReader::from_stream(Box::pin(futures_util::stream::empty::<
        Result<Bytes, std::io::Error>,
    >()));
    let probe = eof.peek_within(64, Duration::from_millis(20)).await;
    assert!(probe.bytes.is_empty());
    assert_eq!(probe.completeness, HeadCompleteness::Complete);

    let mut stalled = ChunkReader::from_stream(Box::pin(futures_util::stream::pending::<
        Result<Bytes, std::io::Error>,
    >()));
    let probe = tokio::time::timeout(
        Duration::from_millis(200),
        stalled.peek_within(64, Duration::from_millis(20)),
    )
    .await
    .expect("预算耗尽必须返回");
    assert!(probe.bytes.is_empty());
    assert_eq!(probe.completeness, HeadCompleteness::Incomplete);
}

#[tokio::test]
async fn full_byte_window_and_mid_utf8_do_not_claim_frame_complete() {
    use axum::body::Bytes;
    use rs_codebuddy::chat::ChunkReader;
    use rs_codebuddy::failover::{classify_stream_head, HeadCompleteness, StreamVerdict};

    for bytes in [
        Bytes::from_static(b"{\"code\":"),
        Bytes::from_static(&[b'd', b'a', b't', 0xe4]),
    ] {
        let mut reader = ChunkReader::from_stream(Box::pin(futures_util::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(bytes.clone()),
        ])));
        let probe = reader.peek_within(bytes.len(), Duration::from_millis(100)).await;
        assert_eq!(probe.bytes, bytes.to_vec());
        assert_eq!(probe.completeness, HeadCompleteness::Incomplete);
        assert_eq!(
            classify_stream_head(
                &String::from_utf8_lossy(&probe.bytes),
                probe.completeness
            ),
            StreamVerdict::Ok
        );
        assert_eq!(reader.next_chunk().await.unwrap().unwrap(), bytes);
    }
}

#[test]
fn complete_error_and_done_frames_keep_existing_classification() {
    use rs_codebuddy::failover::{classify_stream_head, HeadCompleteness, StreamVerdict};

    assert_eq!(
        classify_stream_head(
            "data: {\"code\":11128,\"msg\":\"blocked\"}\n\n",
            HeadCompleteness::Complete
        ),
        StreamVerdict::ErrCode(11128)
    );
    assert_eq!(
        classify_stream_head("data: [DONE]\n\n", HeadCompleteness::Complete),
        StreamVerdict::EmptyStream
    );
    assert_eq!(
        classify_stream_head("<html>gateway</html>", HeadCompleteness::Complete),
        StreamVerdict::EmptyStream
    );
}
