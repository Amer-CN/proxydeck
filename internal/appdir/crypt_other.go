//go:build !windows

package appdir

import (
	"bytes"
	"fmt"
)

// 非 Windows 平台（go vet / 交叉检查）没有 DPAPI：如实报错，
// 调用方一律走「读兼容、写失败不改动原文件」的降级路径，不静默丢数据。
func protect(_ []byte) ([]byte, error) {
	return nil, fmt.Errorf("本平台无 DPAPI（CryptProtectData）")
}
func unprotect(_ []byte) ([]byte, error) {
	return nil, fmt.Errorf("本平台无 DPAPI（CryptUnprotectData）")
}

func protectFileBytes(_ []byte) ([]byte, error) {
	return nil, fmt.Errorf("本平台无 DPAPI（CryptProtectData）")
}

func unprotectFileBytes(_ []byte) ([]byte, error) {
	return nil, fmt.Errorf("本平台无 DPAPI（CryptUnprotectData）")
}

func isProtected(b []byte) bool { return bytes.HasPrefix(b, cryptPrefix) }
