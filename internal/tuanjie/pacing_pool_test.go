// pacing_pool_test.go —— 池路径 429 兜底第一层「即时换号」的退避与次数上限：
// 两账号对同模型持续回 429 时，换号重发前带退避、每请求即时换号 ≤3 次、
// 超限后进入第二层 Retry-After 等待路径。假上游走 httptest，退避时长注入
// 短值（litellmAPIBase/cliAPIKeyURL 为包级常量，经传输层改写重定向，
// 不打真实网络）；第二层等待靠取消请求 ctx 中断，不依赖真实秒级睡眠。
package tuanjie

import (
	"bytes"
	"context"
	"crypto/tls"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

// syncWriter 线程安全日志缓冲（handleChat 跑在 goroutine，主 goroutine 轮询日志）。
type syncWriter struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (w *syncWriter) Write(p []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.buf.Write(p)
}

func (w *syncWriter) String() string {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.buf.String()
}

func TestPacingPoolSwitchBackoffAndLimit(t *testing.T) {
	const injectBackoff = 120 * time.Millisecond

	var mu sync.Mutex
	var chatAt []time.Time // 上游收到 chat 请求的到达时刻（退避间隔断言用）

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// 假上游（TLS 自签）：同时服务换 key 与 chat 转发两个路径。
	// litellmAPIBase/cliAPIKeyURL 是包级 https 常量，无法改指——覆写
	// smartProxyTransport.DialTLSContext 把 TLS 拨号重定向到本假上游
	// （换 key 与池转发都经该包级 transport，不打真实网络）。
	up := httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/api/api-token/cli-api-key": // fetchKeyWithToken 换 key
			_, _ = w.Write([]byte(`{"cli_api_key":"test-key"}`))
		case "/v1/chat/completions": // ForwardDirect 转发
			mu.Lock()
			chatAt = append(chatAt, time.Now())
			mu.Unlock()
			w.Header().Set("Retry-After", "3600")
			w.WriteHeader(http.StatusTooManyRequests)
			_, _ = w.Write([]byte(`{"error":{"message":"rate limited"}}`))
		default:
			http.NotFound(w, r)
		}
	}))
	defer up.Close()
	upAddr := up.Listener.Addr().String()

	oldTLSDial := smartProxyTransport.DialTLSContext
	smartProxyTransport.DialTLSContext = func(ctx context.Context, network, addr string) (net.Conn, error) {
		var d net.Dialer
		raw, err := d.DialContext(ctx, network, upAddr)
		if err != nil {
			return nil, err
		}
		c := tls.Client(raw, &tls.Config{InsecureSkipVerify: true}) // 测试假上游自签证书
		if err := c.HandshakeContext(ctx); err != nil {
			raw.Close()
			return nil, err
		}
		return c, nil
	}
	defer func() { smartProxyTransport.DialTLSContext = oldTLSDial }()

	oldBackoff := pacingSwitchBackoff
	pacingSwitchBackoff = injectBackoff
	defer func() { pacingSwitchBackoff = oldBackoff }()

	s := &Server{
		client: &Client{httpClient: &http.Client{Transport: smartProxyTransport}},
		pool: newTestPool(
			&Account{UserID: "a1", Enabled: true, AccessToken: "ta"},
			&Account{UserID: "a2", Enabled: true, AccessToken: "tb"},
		),
		registry:  NewRegistry(),
		activity:  NewActivityLog(),
		providers: NewProviderStore(),
		pacer:     &Pacer{},
		stats:     map[string]*modelStat{},
	}
	s.pacer.enabled.Store(true) // 直接置位，避免 SetEnabled 落盘

	var sb syncWriter
	oldOut, oldFlags := log.Writer(), log.Flags()
	log.SetFlags(0)
	log.SetOutput(&sb)
	defer func() { log.SetOutput(oldOut); log.SetFlags(oldFlags) }()

	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions",
		strings.NewReader(`{"model":"KIMI-K3","messages":[{"role":"user","content":"hi"}]}`)).WithContext(ctx)
	rec := httptest.NewRecorder()

	done := make(chan struct{})
	go func() {
		defer close(done)
		s.handleChat(rec, req)
	}()

	// 等第二层「等待」日志出现后取消请求 ctx：第二层等待挂在 r.Context()
	// 的 select 上，取消即退出循环（等价客户端断连），避开真实长睡眠。
	deadline := time.Now().Add(10 * time.Second)
	for {
		if strings.Contains(sb.String(), "status=429 等待") {
			break
		}
		if time.Now().After(deadline) {
			cancel()
			<-done
			t.Fatalf("未进入第二层等待路径；上游收到 %v 次请求，日志：\n%s",
				func() int { mu.Lock(); defer mu.Unlock(); return len(chatAt) }(), sb.String())
		}
		time.Sleep(2 * time.Millisecond)
	}
	cancel()
	<-done
	out := sb.String()

	// 断言 1：即时换号恰好 3 次，日志带「第 n/3 次」序号
	if got := strings.Count(out, "即时换号"); got != 3 {
		t.Fatalf("即时换号日志 %d 行, want 3；日志：\n%s", got, out)
	}
	for n := 1; n <= 3; n++ {
		if !strings.Contains(out, fmt.Sprintf("第 %d/3 次", n)) {
			t.Fatalf("日志缺少「第 %d/3 次」；日志：\n%s", n, out)
		}
	}
	if strings.Contains(out, "第 4/3 次") {
		t.Fatalf("即时换号超出 3 次上限；日志：\n%s", out)
	}
	// 断言 2：进入第二层 Retry-After 等待路径
	if !strings.Contains(out, "status=429 等待") {
		t.Fatalf("未记录第二层等待日志；日志：\n%s", out)
	}
	// 断言 3：上游恰好收到 4 次 chat 请求（初始 1 + 换号 3）
	mu.Lock()
	n := len(chatAt)
	times := append([]time.Time(nil), chatAt...)
	mu.Unlock()
	if n != 4 {
		t.Fatalf("上游收到 %d 次 chat 请求, want 4（初始 1 + 即时换号 3）", n)
	}
	// 断言 4：换号重发带退避——相邻两次上游到达间隔 ≥ 注入退避（留时钟粒度容差）
	for i := 1; i < n; i++ {
		if gap := times[i].Sub(times[i-1]); gap < injectBackoff-15*time.Millisecond {
			t.Fatalf("第 %d 次重发距上次仅 %v，退避未生效（注入 %v）", i, gap, injectBackoff)
		}
	}
	// 断言 5：循环退出后透传最后一次原始 429
	if rec.Code != http.StatusTooManyRequests {
		t.Fatalf("响应码 = %d, want 429（透传原始错误）", rec.Code)
	}
}
