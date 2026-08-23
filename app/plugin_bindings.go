// plugin_bindings.go —— 插件相关桥接绑定：列表 / 启动 / 停止 / 日志。
package main

import (
	"encoding/json"
	"os"
	"strings"

	webview "github.com/webview/webview_go"
)

// bindPluginBindings 注册插件相关绑定：列表 / 启动 / 停止 / 日志。
func (a *app) bindPluginBindings(w webview.WebView) {
	// 插件列表（含状态）。
	_ = w.Bind("ccPluginList", func() string {
		b, _ := json.Marshal(a.pluginList())
		return string(b)
	})
	// 读插件日志尾部（插件视图的实时日志面板用）。
	_ = w.Bind("ccPluginLog", func(id string, lines int) string {
		if lines <= 0 || lines > 500 {
			lines = 200
		}
		var d *pluginDef
		for i := range pluginDefs {
			if pluginDefs[i].ID == id {
				d = &pluginDefs[i]
				break
			}
		}
		if d == nil {
			return `{"ok":false,"msg":"未知插件"}`
		}
		data, err := os.ReadFile(a.pluginLog(*d))
		if err != nil {
			return `{"ok":false,"msg":"暂无日志"}`
		}
		all := strings.Split(strings.TrimRight(string(data), "\n"), "\n")
		if len(all) > lines {
			all = all[len(all)-lines:]
		}
		b, _ := json.Marshal(map[string]any{"ok": true, "lines": all})
		return string(b)
	})
	// 启动插件（tuanjie / codebuddy / notion / lingxi）。
	_ = w.Bind("ccPluginStart", func(id string) string {
		if err := a.pluginStart(strings.TrimSpace(id)); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已启动")
	})
	// 停止插件。
	_ = w.Bind("ccPluginStop", func(id string) string {
		if err := a.pluginStop(strings.TrimSpace(id)); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已停止")
	})
}
