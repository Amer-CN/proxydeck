//go:build !windows

package tuanjie

// regReadProxy 在非 Windows 平台上返回 nil（无注册表）。
var regReadProxy func() string = nil
