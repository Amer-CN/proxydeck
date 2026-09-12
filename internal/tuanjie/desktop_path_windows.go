//go:build windows

package tuanjie

import "golang.org/x/sys/windows/registry"

// regReadCoworkInstallLocation 读团结 Cowork 桌面端安装目录（HKCU 卸载表项的
// InstallLocation，如 `D:\Program Files (x86)`）。桌面端内嵌的 CLI 与 npm 全局
// 装的是两条版本轨道，种子卫兵要同时盯住两边。
var regReadCoworkInstallLocation = func() string {
	const uninstall = `SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`
	k, err := registry.OpenKey(registry.CURRENT_USER, uninstall, registry.ENUMERATE_SUB_KEYS|registry.QUERY_VALUE)
	if err != nil {
		return ""
	}
	defer k.Close()
	names, err := k.ReadSubKeyNames(-1)
	if err != nil {
		return ""
	}
	for _, name := range names {
		sub, err := registry.OpenKey(registry.CURRENT_USER, uninstall+`\`+name, registry.QUERY_VALUE)
		if err != nil {
			continue
		}
		display, _, err := sub.GetStringValue("DisplayName")
		if err != nil || display != "Tuanjie Cowork" {
			sub.Close()
			continue
		}
		loc, _, err := sub.GetStringValue("InstallLocation")
		sub.Close()
		if err == nil {
			return loc
		}
	}
	return ""
}
