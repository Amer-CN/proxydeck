//go:build windows

package tuanjie

import (
	"strings"

	"golang.org/x/sys/windows/registry"
)

// regReadProxy 在 Windows 上读注册表 HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings。
// ProxyEnable=1 时返回 "http://<ProxyServer>"；否则空串。
var regReadProxy = func() string {
	k, err := registry.OpenKey(registry.CURRENT_USER,
		`Software\Microsoft\Windows\CurrentVersion\Internet Settings`,
		registry.QUERY_VALUE)
	if err != nil {
		return ""
	}
	defer k.Close()
	enabled, _, err := k.GetIntegerValue("ProxyEnable")
	if err != nil || enabled == 0 {
		return ""
	}
	server, _, err := k.GetStringValue("ProxyServer")
	if err != nil || server == "" {
		return ""
	}
	// ProxyServer 可能是 "127.0.0.1:7897" 或 "http=127.0.0.1:7897;https=..."
	// 取第一个有效地址
	for _, part := range strings.Split(server, ";") {
		// 去掉 "http=" / "https=" 前缀
		addr := strings.TrimSpace(part)
		if eq := strings.Index(addr, "="); eq >= 0 {
			addr = addr[eq+1:]
		}
		if addr == "" {
			continue
		}
		if !strings.HasPrefix(addr, "http://") && !strings.HasPrefix(addr, "https://") {
			addr = "http://" + addr
		}
		return addr
	}
	return ""
}
