package tuanjie

import (
	"bufio"
	"context"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"time"

	utls "github.com/refraction-networking/utls"
)

// utls_transport.go：把 Go 标准库 TLS 握手替换为 utls 指纹
// （HelloChrome_Auto，Chrome/BoringSSL 系，对齐官方 CLI 的 Node.js 指纹）。
//
// 挂点选择（2026-08-26 实验实证 /tmp/utls-check2）：
//   - 场景A（Transport.Proxy=http 代理 + https 目标）：标准库只调
//     DialContext(代理地址) 后自己 CONNECT + 标准 TLS——自定义 DialTLSContext
//     完全不会被调（DialTLSContext=0），utls 被绕过；
//   - 场景B（无代理 + https 目标）：DialTLSContext(目标地址) 被调；
//   - 场景C（不设 Transport.Proxy + https 目标）：DialTLSContext(目标地址) 被调。
// 因此正确做法：【不设 Transport.Proxy】，完全自己管代理——
//   - DialTLSContext(ctx, "tcp", "host:443")：问 proxyFn；有 http(s) 代理就
//     自己 TCP 连代理 + 发 CONNECT 建隧道 + 隧道内 utls 握手；无代理直接
//     TCP + utls。DialContext 返回已握手连接且 Transport.hasCustomTLSDialer
//     为真时标准库不会再 addTLS，无二次握手问题。
//   - DialContext(ctx, "tcp", addr)（明文 http 目标）：Transport 不设 Proxy
//     不会自动走代理，这里自己兜——有代理就连代理地址（HTTP 代理对明文
//     目标就是普通转发，请求行由上层按 origin-form 发出，代理可识别），
//     无代理直连。本地/内网地址 smartProxy 本身就返回 nil，不受影响。
//   - socks5 代理：标准库 http.Transport 不支持（老 smartProxyTransport 同样
//     不支持，ProxyFromEnvironment 的 socks5 会走 TCP 拨号失败）——保持
//     原行为：socks5 场景回落标准 Transport（无 utls），不新增依赖。
//
// 代理决策复用 smartProxy（环境变量 + Windows 注册表），逻辑零改动。

// utlsHelloID 对齐官方 CLI 的 TLS 指纹。官方 CLI 是 Node.js（undici/fetch），
// ClientHello 走 BoringSSL 系；utls 的 HelloChrome_Auto（Chrome 系 BoringSSL
// 扩展序）与 Node.js 18+ 高度同源，是可用的最佳近似。
var utlsHelloID = utls.HelloChrome_Auto

// newUtlsTransport 构造 utls 指纹传输层。proxyFn 为代理决策函数（smartProxy）。
func newUtlsTransport(proxyFn func(*http.Request) (*url.URL, error)) *http.Transport {
	plain := &net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second}
	return &http.Transport{
		// 注意：不设 Proxy——代理在 Dial 路径里自管（见文件头注释）。
		DialContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
			// 明文目标：有代理连代理（明文经 HTTP 代理即普通正向代理转发），
			// 无代理直连。
			if u := proxyURLForAddr(proxyFn, addr); u != nil {
				return plain.DialContext(ctx, network, canonicalProxyAddr(u))
			}
			return plain.DialContext(ctx, network, addr)
		},
		DialTLSContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
			if u := proxyURLForAddr(proxyFn, addr); u != nil && !isSocksProxy(u) {
				// HTTP(S) 代理：TCP 连代理 → CONNECT 隧道 → 隧道内 utls 握手
				return dialTLSViaProxy(ctx, network, addr, u)
			}
			// 无代理（或 socks——见文件头，socks 回落标准路径由调用方兜底；
			// 这里对 socks 目标直接按无代理处理会连不上，所以 socks 场景
			// 整体不该走本 Transport，newUtlsTransportSocks 兜底）。
			return dialTLSDirect(ctx, network, addr)
		},
		// 自定义 DialTLSContext 存在时标准库不再 addTLS（hasCustomTLSDialer）。
		// ALPN 固定 http/1.1（见 utlsHandshake），不启用 h2。
	}
}

// newUtlsTransportKeepStdlib 兜底：socks5 环境下维持老行为（标准 Transport +
// Proxy，标准 TLS，无 utls）。smartProxy 返回 socks5 时 client.go 用这个。
func newUtlsTransportKeepStdlib(proxyFn func(*http.Request) (*url.URL, error)) *http.Transport {
	return &http.Transport{Proxy: proxyFn}
}

// proxyURLForAddr 问 proxyFn 某目标地址用不用代理（构造伪请求复用 smartProxy）。
func proxyURLForAddr(proxyFn func(*http.Request) (*url.URL, error), addr string) *url.URL {
	if proxyFn == nil {
		return nil
	}
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		host = addr
	}
	_ = host
	req := &http.Request{URL: &url.URL{Scheme: "https", Host: addr}}
	u, err := proxyFn(req)
	if err != nil {
		return nil
	}
	return u
}

// isSocksProxy 判断代理是否 socks5 系。
func isSocksProxy(u *url.URL) bool {
	return u.Scheme == "socks5" || u.Scheme == "socks5h" || u.Scheme == "socks4"
}

// dialTLSDirect 直连 + utls 握手。
func dialTLSDirect(ctx context.Context, network, addr string) (net.Conn, error) {
	d := &net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second}
	raw, err := d.DialContext(ctx, network, addr)
	if err != nil {
		return nil, err
	}
	return utlsHandshake(ctx, raw, addr)
}

// dialTLSViaProxy HTTP 代理 CONNECT 隧道 + 隧道内 utls 握手。
func dialTLSViaProxy(ctx context.Context, network, addr string, proxyURL *url.URL) (net.Conn, error) {
	d := &net.Dialer{Timeout: 30 * time.Second, KeepAlive: 30 * time.Second}
	proxyAddr := canonicalProxyAddr(proxyURL)
	// 代理本身是 https 时（罕见）：代理层握手用标准 tls（对代理的 TLS，
	// 不需要伪装指纹——指纹只对最终目标的 ClientHello 有意义）。
	var raw net.Conn
	raw, err := d.DialContext(ctx, network, proxyAddr)
	if err != nil {
		return nil, fmt.Errorf("utls 连代理 %s 失败: %w", proxyAddr, err)
	}
	if proxyURL.Scheme == "https" {
		utlsRaw, herr := stdTLSClient(ctx, raw, proxyURL.Hostname(), proxyAddr)
		if herr != nil {
			raw.Close()
			return nil, fmt.Errorf("utls 代理 %s TLS 失败: %w", proxyAddr, herr)
		}
		raw = utlsRaw
	}
	if err := httpConnectThrough(raw, proxyURL, addr); err != nil {
		raw.Close()
		return nil, err
	}
	return utlsHandshake(ctx, raw, addr)
}

// stdTLSClient 对代理服务器做标准 TLS 握手（代理是 https:// 时）。
func stdTLSClient(ctx context.Context, raw net.Conn, host, addr string) (net.Conn, error) {
	return stdTLSDial(ctx, raw, host)
}

// utlsHandshake 在裸连接上做 utls 指纹握手。
// ALPN 强制仅 http/1.1：HelloChrome_Auto 的 ClientHello 自带 h2+http/1.1
// 双 ALPN（Config.NextProtos 不生效，spec 覆盖它）；协商出 h2 时标准库
// http.Transport 会往这条 h2-only 连接发 HTTP/1 请求 → 上游直接断连（实测
// 2026-08-26：EOF）。原位整条替换 spec 的 ALPN 扩展为仅 http/1.1——扩展
// 数量与顺序保持 Chrome spec 原样（JA3/JA4 指纹对齐），对齐官方 CLI 也无碍
// （官方走 undici 自己的 h2 栈，我们走标准库 H1）。
func utlsHandshake(ctx context.Context, raw net.Conn, addr string) (net.Conn, error) {
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		host = addr
	}
	tlsConn := utls.UClient(raw, &utls.Config{ServerName: host}, utlsHelloID)
	if err := tlsConn.BuildHandshakeState(); err != nil {
		raw.Close()
		return nil, fmt.Errorf("utls 构造握手状态失败 %s: %w", addr, err)
	}
	// ALPN 原位整条替换为仅 http/1.1（不 append——保持 Chrome spec 的扩展
	// 数量与顺序不变，JA3 按扩展序哈希，挪位会偏离真 Chrome 布局；
	// 整条替换而非改 AlpnProtocols 字段——utls 在 BuildHandshakeState 时
	// 已烘焙扩展数据，字段改动可能不生效导致 h2 仍被通告）
	for i, ext := range tlsConn.Extensions {
		if _, ok := ext.(*utls.ALPNExtension); ok {
			tlsConn.Extensions[i] = &utls.ALPNExtension{AlpnProtocols: []string{"http/1.1"}}
			break
		}
	}
	if err := tlsConn.HandshakeContext(ctx); err != nil {
		raw.Close()
		return nil, fmt.Errorf("utls 握手失败 %s: %w", addr, err)
	}
	return tlsConn, nil
}

// canonicalProxyAddr 规范化代理地址为 host:port（补默认端口）。
func canonicalProxyAddr(u *url.URL) string {
	port := u.Port()
	if port == "" {
		if u.Scheme == "https" {
			port = "443"
		} else {
			port = "80"
		}
	}
	return net.JoinHostPort(u.Hostname(), port)
}

// httpConnectThrough 在已连上代理的连接上发 HTTP CONNECT 建隧道。
func httpConnectThrough(conn net.Conn, proxyURL *url.URL, target string) error {
	req := &http.Request{
		Method: http.MethodConnect,
		URL:    &url.URL{Opaque: target},
		Host:   target,
		Header: make(http.Header),
	}
	if u := proxyURL.User; u != nil {
		pass, _ := u.Password()
		req.SetBasicAuth(u.Username(), pass)
	}
	if err := req.Write(conn); err != nil {
		return fmt.Errorf("发送 CONNECT 失败: %w", err)
	}
	br := bufio.NewReader(conn)
	resp, err := http.ReadResponse(br, req)
	if err != nil {
		return fmt.Errorf("读 CONNECT 响应失败: %w", err)
	}
	_ = resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("CONNECT %s 经代理 %s 返回 %d", target, proxyURL.Host, resp.StatusCode)
	}
	if br.Buffered() > 0 {
		// CONNECT 响应后不应有前置数据；丢弃缓冲防粘包（极罕见）。
		_, _ = br.Discard(br.Buffered())
	}
	return nil
}
