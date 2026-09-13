package proxy

import (
	"net/http"
	"time"
)

// ProbeKey 探活一个 CommandCode API key：GET baseURL+"/alpha/whoami"。
//
// 用途：GUI 主进程自己持有 api-key.txt 里的 key（headless 子进程的 401 主进程
// 拿不到），存储 key 失效时前端毫无感知。whoami 是 usage.go 在用的轻量免费端点，
// 零生成计费，适合做探活。
// 返回 status："ok"（200）/ "unauthorized"（401）/ "error"（其他状态码或网络错误）；
// httpCode 为实际状态码（网络错误时为 0）；err 仅网络层错误（非 nil 时 status 恒 "error"）。
func ProbeKey(baseURL, apiKey string) (status string, httpCode int, err error) {
	req, err := http.NewRequest(http.MethodGet, baseURL+"/alpha/whoami", nil)
	if err != nil {
		return "error", 0, err
	}
	req.Header.Set("Authorization", "Bearer "+apiKey)
	client := &http.Client{Timeout: 10 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return "error", 0, err
	}
	defer resp.Body.Close()
	switch resp.StatusCode {
	case http.StatusOK:
		return "ok", resp.StatusCode, nil
	case http.StatusUnauthorized:
		return "unauthorized", resp.StatusCode, nil
	default:
		return "error", resp.StatusCode, nil
	}
}
