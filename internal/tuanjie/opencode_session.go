package tuanjie

import (
	"net/http"
	"strings"
	"sync"

	"github.com/google/uuid"
)

// opencodeSessionID 进程级稳定的 opencode 会话标识（UUID v4，懒生成一次）。
// 依据 opencode.ai 官方文档（https://opencode.ai/docs/go/ "Where can I use it"
// 一节）：客户端须为每段会话发送稳定的 x-opencode-session 头，供服务端做
// 路由优化、prompt 缓存与防滥用监测——这是文档化的 API 契约（缺头会被
// Console 按出口分片以 400 MissingSessionID 拒绝），不是绕过手段。
var (
	opencodeSessionOnce sync.Once
	opencodeSessionID   string
)

func opencodeSession() string {
	opencodeSessionOnce.Do(func() { opencodeSessionID = uuid.NewString() })
	return opencodeSessionID
}

// applyOpenCodeSession 当 baseURL 指向 opencode.ai 时，为出站请求附加
// x-opencode-session 会话头（官方文档要求的会话标识，依据见上）；
// 其他 provider 原样不动。在构造出站 *http.Request 之后、client.Do 之前调用。
func applyOpenCodeSession(req *http.Request, baseURL string) {
	if req == nil || !strings.Contains(baseURL, "opencode.ai") {
		return
	}
	req.Header.Set("x-opencode-session", opencodeSession())
}
