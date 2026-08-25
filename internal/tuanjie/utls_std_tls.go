package tuanjie

import (
	"context"
	"crypto/tls"
	"net"
)

// stdTLSDial 标准库 TLS 握手（代理本身是 https:// 时对代理层的握手，
// 指纹伪装只针对最终目标，代理层用标准握手即可）。
func stdTLSDial(ctx context.Context, raw net.Conn, host string) (net.Conn, error) {
	c := tls.Client(raw, &tls.Config{ServerName: host})
	if err := c.HandshakeContext(ctx); err != nil {
		return nil, err
	}
	return c, nil
}
