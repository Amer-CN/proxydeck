// plugin_bindings.go —— 插件相关桥接绑定：列表 / 启动 / 停止 / 日志。
package main

import (
	"bytes"
	"encoding/json"
	"io"
	"os"
	"strings"

	webview "github.com/webview/webview_go"
)

// readLogTail 读日志尾部至多 max 字节（文件更小则全读）。
func readLogTail(path string, max int64) ([]byte, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	st, err := f.Stat()
	if err != nil {
		return nil, err
	}
	if st.Size() > max {
		if _, err := f.Seek(st.Size()-max, io.SeekStart); err != nil {
			return nil, err
		}
		b, err := io.ReadAll(f)
		if err != nil {
			return nil, err
		}
		// 截断读的起点常落在行中间：丢掉首个不完整行，保证返回的都是整行
		if i := bytes.IndexByte(b, '\n'); i >= 0 {
			b = b[i+1:]
		}
		return b, nil
	}
	return io.ReadAll(f)
}

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
		// 只读尾部：日志每 3 秒拉一次且只用尾 lines 行，整份读入时日志越大越卡。
		// 文件小于 64KB 全读（行为与整读一致），否则 Seek 到尾部读最后 64KB；
		// 尾部起点可能落在行中间，多出的半行由下面的尾切自然丢弃。
		data, err := readLogTail(a.pluginLog(*d), 64<<10)
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
	// 启动插件（tuanjie / codebuddy / bai）。
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
