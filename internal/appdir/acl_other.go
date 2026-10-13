//go:build !windows

package appdir

import "fmt"

// 非 Windows 平台（go vet / 交叉检查）没有 icacls。
func aclUserOnly(_ string) error { return fmt.Errorf("本平台无 icacls") }
