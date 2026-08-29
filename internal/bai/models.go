// models.go —— B.AI 甲板的模型矩阵数据源（GUI 专用旁路端点）。
//
// 为什么要有这个端点：/v1/models 是逐字节透传，客户端必须自带 key；而 GUI 没有 key，
// 直接透传只会拿到上游 401（矩阵永远空），且这个无 key 请求被 3 秒轮询驱动，会持续刷
// api.b.ai。本端点在服务端用渠道配置的 key 拉一次清单并缓存，上游只在三种时机被访问：
// 冷启动首次、拉杆点火（?refresh=1）、手动 ↻（?refresh=1）。
package bai

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"sync"
	"time"
)

// 测试缝：上游地址、渠道配置路径、传输层（生产用带重试的代理自探测 Transport）。
var (
	upstreamModelsURL  = upstreamBase + "/v1/models"
	channelsConfigPath = defaultChannelsConfigPath
	matrixTransport    = func() http.RoundTripper {
		return &retryRoundTripper{transport: detectUpstreamTransport(), max: 3}
	}
)

const (
	baiChannelID  = "bai"       // 渠道配置里 B.AI 的 id（与注水检测共用同一文件）
	matrixErrTTL  = time.Minute // 失败负缓存：窗口内轮询不重试，防打穿上游
	matrixTimeout = 15 * time.Second
)

// matrixResp /model/matrix 的响应。data 为上游 data 数组原样透传，不重塑字段。
type matrixResp struct {
	OK        bool            `json:"ok"`
	Source    string          `json:"source"` // live = 本次真打了上游；cache = 吃缓存
	FetchedAt int64           `json:"fetched_at"`
	Count     int             `json:"count"`
	Data      json.RawMessage `json:"data"`
	Err       string          `json:"err,omitempty"` // need_key | upstream_error
}

// matrixState 包级无 TTL 缓存：刷新时机由拉杆决定，不由时钟决定，
// 所以不加过期——过期会在长跑进程里偷偷打官网。
type matrixState struct {
	mu        sync.Mutex
	loaded    bool
	data      json.RawMessage
	count     int
	fetchedAt time.Time
	errKind   string
	errAt     time.Time
}

// handleModelMatrix GET /model/matrix[?refresh=1]。
// 全程持锁 = single-flight：并发请求排队，第一个拉完后面的直接吃缓存。
func (s *Server) handleModelMatrix(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	force := r.URL.Query().Get("refresh") == "1"

	st := &s.matrix
	st.mu.Lock()
	defer st.mu.Unlock()

	fetchedNow := false
	if force || !st.loaded {
		if force || time.Since(st.errAt) >= matrixErrTTL {
			fetchedNow = st.refreshLocked()
		}
	}

	resp := matrixResp{Count: st.count, Data: st.data, FetchedAt: st.fetchedAt.Unix()}
	switch {
	case st.loaded:
		resp.OK, resp.Source = true, "cache"
		if fetchedNow {
			resp.Source = "live"
		}
	case st.errKind != "":
		resp.OK, resp.Source, resp.Err = false, "cache", st.errKind
	default:
		resp.OK, resp.Source = true, "cache"
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(resp)
}

// refreshLocked 拉一次上游清单。返回是否拿到了新数据。失败只记状态、不清旧值。
// 调用方须持锁。
func (st *matrixState) refreshLocked() bool {
	key := baiChannelKey()
	if key == "" {
		st.errKind, st.errAt = "need_key", time.Now()
		return false
	}
	data, count, err := fetchUpstreamModels(key)
	if err != nil {
		st.errKind, st.errAt = "upstream_error", time.Now()
		log.Printf("bai-plugin: 模型矩阵刷新失败（%v），沿用旧清单 %d 个", err, st.count)
		return false
	}
	st.data, st.count, st.fetchedAt = data, count, time.Now()
	st.errKind, st.loaded = "", true
	log.Printf("bai-plugin: 模型矩阵已刷新，%d 个模型", count)
	return true
}

// fetchUpstreamModels 带 Bearer 拉上游 /v1/models，返回 data 数组原文与条数。
func fetchUpstreamModels(key string) (json.RawMessage, int, error) {
	ctx, cancel := context.WithTimeout(context.Background(), matrixTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, upstreamModelsURL, nil)
	if err != nil {
		return nil, 0, err
	}
	req.Header.Set("Authorization", "Bearer "+key)
	client := &http.Client{Transport: matrixTransport()}
	resp, err := client.Do(req)
	if err != nil {
		return nil, 0, err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		return nil, 0, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, 0, fmt.Errorf("上游 HTTP %d", resp.StatusCode)
	}
	var envelope struct {
		Data json.RawMessage `json:"data"`
	}
	if err := json.Unmarshal(body, &envelope); err != nil || len(envelope.Data) == 0 {
		return nil, 0, fmt.Errorf("上游清单无 data 数组")
	}
	var items []json.RawMessage
	if err := json.Unmarshal(envelope.Data, &items); err != nil {
		return nil, 0, fmt.Errorf("上游 data 不是数组")
	}
	return envelope.Data, len(items), nil
}

// channelKeyCfg 渠道密钥条目（tuanjie-water-channels.json，含密钥，勿入库）。
type channelKeyCfg struct {
	ID  string `json:"id"`
	Key string `json:"key"`
}

// baiChannelKey 从注水检测共用的渠道配置里取 B.AI 的 Bearer；缺失/坏文件回空串。
func baiChannelKey() string {
	b, err := os.ReadFile(channelsConfigPath())
	if err != nil {
		return ""
	}
	var cfgs []channelKeyCfg
	if json.Unmarshal(b, &cfgs) != nil {
		return ""
	}
	for _, c := range cfgs {
		if c.ID == baiChannelID {
			return c.Key
		}
	}
	return ""
}

// defaultChannelsConfigPath 渠道配置在 exe 同目录（回退工作目录），与注水检测同一路径。
func defaultChannelsConfigPath() string {
	if exe, err := os.Executable(); err == nil {
		return filepath.Join(filepath.Dir(exe), "tuanjie-water-channels.json")
	}
	return filepath.Join(".", "tuanjie-water-channels.json")
}
