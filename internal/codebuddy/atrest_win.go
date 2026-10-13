//go:build windows

// atrest_win.go —— 桌面端 exe 发现（Windows）：注册表卸载表推导真实路径。
//
// InstallLocation 为空（2026-10-14 本机 CN/INTL 双条目实测），故从
// DisplayIcon（"D:\…\WorkBuddy.exe,0"）或 UninstallString（ quoted 目录 +
// exe 名）反推；HKLM/HKCU × 原生/Wow6432Node 四表全扫、去重；region 只定
// 尝试顺序。默认安装位置一律不拼——候选只来自注册表与 WORKBUDDY_ELECTRON_PATH。
package codebuddy

import (
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/windows/registry"
)

// electronCandidates 返回候选桌面端 exe（region 命中的排前）。
func electronCandidates(region Region) []electronCand {
	if p := os.Getenv("WORKBUDDY_ELECTRON_PATH"); p != "" {
		if info, err := os.Stat(p); err == nil && !info.IsDir() {
			return []electronCand{{path: p, label: string(region)}}
		}
		// 指向目录时按两 exe 名试（CN/INTL 各一）。
		for _, name := range []string{"WorkBuddy.exe", "WorkBuddyAI.exe"} {
			if fp := filepath.Join(p, name); isFile(fp) {
				return []electronCand{{path: fp, label: string(region)}}
			}
		}
	}
	var cands []electronCand
	seen := map[string]bool{}
	add := func(path, label string) {
		if path == "" || seen[path] || !isFile(path) {
			return
		}
		seen[path] = true
		cands = append(cands, electronCand{path: path, label: label})
	}
	roots := []registry.Key{registry.LOCAL_MACHINE, registry.CURRENT_USER}
	subs := []string{
		`SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`,
		`SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall`,
	}
	for _, root := range roots {
		for _, sub := range subs {
			k, err := registry.OpenKey(root, sub, registry.ENUMERATE_SUB_KEYS)
			if err != nil {
				continue
			}
			names, err := k.ReadSubKeyNames(-1)
			k.Close()
			if err != nil {
				continue
			}
			for _, n := range names {
				sk, err := registry.OpenKey(root, sub+`\`+n, registry.QUERY_VALUE)
				if err != nil {
					continue
				}
				disp, _, _ := sk.GetStringValue("DisplayName")
				icon, _, _ := sk.GetStringValue("DisplayIcon")
				uninst, _, _ := sk.GetStringValue("UninstallString")
				sk.Close()
				if !strings.Contains(strings.ToLower(disp), "workbuddy") {
					continue
				}
				label := "CN"
				if strings.Contains(disp, " AI") || strings.Contains(icon, "WorkBuddyAI.exe") {
					label = "INTL"
				}
				// 首选 DisplayIcon（直指真实 exe），回退 UninstallString 目录。
				if exe := exeFromIcon(icon); exe != "" {
					add(exe, label)
					continue
				}
				if dir := dirFromUninstall(uninst); dir != "" {
					for _, name := range []string{"WorkBuddyAI.exe", "WorkBuddy.exe"} {
						add(filepath.Join(dir, name), label)
					}
				}
			}
		}
	}
	// region 命中的排前（同区 exe 的 secret 优先试，省一次子进程）。
	var hit, rest []electronCand
	want := string(region)
	for _, c := range cands {
		if c.label == want {
			hit = append(hit, c)
		} else {
			rest = append(rest, c)
		}
	}
	return append(hit, rest...)
}

// exeFromIcon 从 DisplayIcon（"D:\…\x.exe,0"）提 exe 路径。
func exeFromIcon(icon string) string {
	if icon == "" {
		return ""
	}
	p := strings.Trim(strings.TrimSpace(strings.Split(icon, ",")[0]), `"`)
	if strings.HasSuffix(strings.ToLower(p), ".exe") && isFile(p) {
		return p
	}
	return ""
}

// dirFromUninstall 从 UninstallString 提安装目录。
func dirFromUninstall(uninst string) string {
	if uninst == "" {
		return ""
	}
	s := strings.TrimSpace(uninst)
	rest := s
	if strings.HasPrefix(s, `"`) {
		if i := strings.Index(s[1:], `"`); i >= 0 {
			rest = s[1 : 1+i]
		}
	} else if i := strings.IndexAny(s, " \t/"); i >= 0 {
		rest = s[:i]
	}
	d := filepath.Dir(strings.Trim(rest, `"`))
	if d == "" || d == "." {
		return ""
	}
	return d
}

func isFile(p string) bool {
	info, err := os.Stat(p)
	return err == nil && !info.IsDir()
}
