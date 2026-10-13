//go:build !windows

// atrest_other.go —— 非 Windows：无注册表，只有 WORKBUDDY_ELECTRON_PATH
// 逃生口；没有默认路径可拼，找不到即如实报错。
package codebuddy

import (
	"os"
	"path/filepath"
)

func electronCandidates(region Region) []electronCand {
	p := os.Getenv("WORKBUDDY_ELECTRON_PATH")
	if p == "" {
		return nil
	}
	if isFile(p) {
		return []electronCand{{path: p, label: string(region)}}
	}
	for _, name := range []string{"WorkBuddy.exe", "WorkBuddyAI.exe"} {
		if fp := filepath.Join(p, name); isFile(fp) {
			return []electronCand{{path: fp, label: string(region)}}
		}
	}
	return nil
}

func isFile(p string) bool {
	info, err := os.Stat(p)
	return err == nil && !info.IsDir()
}
