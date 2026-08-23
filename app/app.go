// app.go —— 完整版 app 结构（含插件托管状态字段；作者本地注入，不进公开仓库）。
package main

import (
	"net/http"
	"sync"
	"time"
)

// app 是 GUI 与代理核心之间的控制器：代理直接运行在本进程内。
type app struct {
	host string
	port string

	mu       sync.Mutex
	httpd    *http.Server
	running  bool // 本进程内的代理正在运行
	external bool // 端口被本程序之外的代理实例占用（如开机自启的后台实例）
	started  time.Time
	apiKey   string
	lastErr  string
	plugins  map[string]*pluginState // 开发者模式插件（tuanjie/codebuddy）状态
}
