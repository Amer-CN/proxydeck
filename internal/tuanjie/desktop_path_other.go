//go:build !windows

package tuanjie

// regReadCoworkInstallLocation 在非 Windows 平台上返回空串（无注册表）。
var regReadCoworkInstallLocation func() string = nil
