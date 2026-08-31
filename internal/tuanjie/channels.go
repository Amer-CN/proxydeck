// channels.go —— 注水检测的渠道注册表：四渠道端点/密钥定义 + channels 列表
// 构建（各渠道 /v1/models 拉取，3s 缓存）+ tuanjie-water-channels.json 密钥
// 配置加载（含密钥，.gitignore 已排除，勿入库）。
package tuanjie

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

// waterChannelDef 渠道静态定义：端点 + Bearer 来源。
//   - tuanjie：不走本地端点，走账号池 fetchKeyWithToken 换 key 直探上游（更准）
//   - command：本地 55990，Bearer=api-key.txt 内容（存在才带）
//   - workbuddy：本地 8787，无鉴权头
//   - bai：本地 8891，Bearer=渠道配置 key（tuanjie-water-channels.json）
//   - comate/qoder：zulu 引擎本地端点（8786/8785），无鉴权头
type waterChannelDef struct {
	ID         string
	Name       string
	Port       int
	BaseURL    string
	BearerFrom string // "" | "config" | "api-key.txt"
}

// waterChannels 内置六渠道（顺序即 channels 列表顺序）。
var waterChannels = []waterChannelDef{
	{ID: "tuanjie", Name: "团结", Port: 8788},
	{ID: "command", Name: "Command", Port: 55990, BaseURL: "http://127.0.0.1:55990", BearerFrom: "api-key.txt"},
	{ID: "workbuddy", Name: "WorkBuddy", Port: 8787, BaseURL: "http://127.0.0.1:8787"},
	{ID: "bai", Name: "B.ai", Port: 8891, BaseURL: "http://127.0.0.1:8891", BearerFrom: "config"},
	{ID: "comate", Name: "Comate", Port: 8786, BaseURL: "http://127.0.0.1:8786"},
	{ID: "qoder", Name: "Qoder", Port: 8785, BaseURL: "http://127.0.0.1:8785"},
}

// isStrongChannel 官方链路渠道判定（第 32 轮基准口径诚实化）：tuanjie 走账号池
// 官方上游、comate/qoder 为厂商 zulu 引擎本地端点——凭证链完整、指纹源自厂商，
// 基准口径=「官方链路基准」；其余渠道（command/workbuddy/bai）无官方链路，
// 基准=「首测锚定（弱判）」。
func isStrongChannel(id string) bool {
	return id == "tuanjie" || id == "comate" || id == "qoder"
}

// channelInfo 渠道在 channels 列表里的呈现（GUI 双下拉数据源）。
type channelInfo struct {
	ID      string   `json:"id"`
	Name    string   `json:"name"`
	Port    int      `json:"port"`
	OK      bool     `json:"ok"`
	Strong  bool     `json:"strong"`             // 官方链路渠道（tuanjie/comate/qoder），其余=首测锚定（弱判）
	NeedKey bool     `json:"need_key,omitempty"` // 该渠道需 key 且未配置（前端显示输入框）
	Models  []string `json:"models,omitempty"`
	Note    string   `json:"note,omitempty"`
}

// channelKeyCfg 渠道密钥配置项（tuanjie-water-channels.json）。
type channelKeyCfg struct {
	ID  string `json:"id"`
	Key string `json:"key"`
}

func channelsConfigPath() string {
	return filepath.Join(exeDirForAccounts(), "tuanjie-water-channels.json")
}

// LoadChannelKeys 读渠道密钥配置（缺失 = 空 map）。含密钥，勿进版本库。
func LoadChannelKeys() map[string]string {
	keys := map[string]string{}
	if b, err := os.ReadFile(channelsConfigPath()); err == nil {
		var cfgs []channelKeyCfg
		if json.Unmarshal(b, &cfgs) == nil {
			for _, c := range cfgs {
				if c.ID != "" && c.Key != "" {
					keys[c.ID] = c.Key
				}
			}
		}
	}
	return keys
}

// SaveChannelKey 保存单渠道 key 到 tuanjie-water-channels.json（前端输入用；
// 含密钥，文件已在 .gitignore 排除，勿入库）。
func SaveChannelKey(id, key string) error {
	if id == "" || key == "" {
		return fmt.Errorf("渠道/key 不能为空")
	}
	existing := LoadChannelKeys()
	existing[id] = key
	cfgs := make([]channelKeyCfg, 0, len(existing))
	names := make([]string, 0, len(existing))
	for k := range existing {
		names = append(names, k)
	}
	sort.Strings(names)
	for _, k := range names {
		cfgs = append(cfgs, channelKeyCfg{ID: k, Key: existing[k]})
	}
	b, err := json.MarshalIndent(cfgs, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(channelsConfigPath(), b, 0o600)
}

// readAPIKeyFile 读 exe 同目录 api-key.txt（command 渠道 Bearer；存在才带）。
func readAPIKeyFile() string {
	b, err := os.ReadFile(filepath.Join(exeDirForAccounts(), "api-key.txt"))
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(b))
}

// channelNameOf 渠道 id → 人话名（未命中原样返回；空串回"团结"）。
func channelNameOf(channel string) string {
	for _, c := range waterChannels {
		if c.ID == channel {
			return c.Name
		}
	}
	if channel == "" || channel == "tuanjie" {
		return "团结"
	}
	return channel
}

// channels 3s 缓存（防前端切换渠道反复打各渠道 /v1/models）。
var (
	channelsCacheMu sync.Mutex
	channelsCache   []channelInfo
	channelsCacheAt time.Time
)

// handleChannels GET /water-probe?channels=1：六渠道列表（含各自 models）。
func (s *Server) handleChannels(w http.ResponseWriter, r *http.Request) {
	channelsCacheMu.Lock()
	fresh := time.Since(channelsCacheAt) < 3*time.Second && channelsCache != nil
	cached := channelsCache
	channelsCacheMu.Unlock()
	if fresh {
		writeJSON(w, map[string]any{"ok": true, "channels": cached})
		return
	}
	list := s.buildChannels(r.Context())
	channelsCacheMu.Lock()
	channelsCache = list
	channelsCacheAt = time.Now()
	channelsCacheMu.Unlock()
	writeJSON(w, map[string]any{"ok": true, "channels": list})
}

// buildChannels 逐渠道拉 /v1/models 组装列表（不通渠道 ok:false + note）。
func (s *Server) buildChannels(ctx context.Context) []channelInfo {
	keys := LoadChannelKeys()
	out := make([]channelInfo, 0, len(waterChannels))
	for _, c := range waterChannels {
		ci := channelInfo{ID: c.ID, Name: c.Name, Port: c.Port, OK: true, Strong: isStrongChannel(c.ID), NeedKey: c.BearerFrom == "config" && keys[c.ID] == ""}
		models, note := s.fetchChannelModels(ctx, c, keys)
		if note != "" {
			ci.OK = false
			ci.Note = note
		}
		ci.Models = models
		out = append(out, ci)
	}
	return out
}

// fetchChannelModels 按渠道拿模型列表：团结走账号池上游；本地渠道直接打端点。
func (s *Server) fetchChannelModels(ctx context.Context, c waterChannelDef, keys map[string]string) ([]string, string) {
	if c.ID == "tuanjie" {
		return s.fetchTuanjieModels(ctx)
	}
	return fetchLocalModels(ctx, c, keys)
}

// fetchTuanjieModels 团结渠道模型：账号池上游 /v1/models + 外部 provider 合并。
func (s *Server) fetchTuanjieModels(ctx context.Context) ([]string, string) {
	resp, err := s.client.Forward(ctx, http.MethodGet, "/v1/models", nil, "")
	if err != nil {
		return nil, "团结上游不可达（" + err.Error() + "）"
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Sprintf("团结上游 %d", resp.StatusCode)
	}
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 2<<20))
	body = s.mergeProviderModels(body)
	var ml struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if json.Unmarshal(body, &ml) != nil {
		return nil, "团结模型列表解析失败"
	}
	models := []string{}
	for _, m := range ml.Data {
		if m.ID != "" {
			models = append(models, m.ID)
		}
	}
	if len(models) == 0 {
		return nil, "团结模型列表为空（积分不足也能列出模型，请检查上游）"
	}
	return models, ""
}

// fetchLocalModels 本地渠道模型：GET {base}/v1/models（Bearer 视渠道定义）。
// 注意渠道定义 BaseURL 不带 /v1（探针层也按此约定），这里必须补全——
// 漏 /v1 会 404（command/workbuddy 模型列表曾因此显示不出的根因）。
func fetchLocalModels(ctx context.Context, c waterChannelDef, keys map[string]string) ([]string, string) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.BaseURL+"/v1/models", nil)
	if err != nil {
		return nil, "构造请求失败: " + err.Error()
	}
	req.Header.Set("Accept", "application/json")
	switch c.BearerFrom {
	case "config":
		key := keys[c.ID]
		if key == "" {
			return nil, "B.ai 渠道未配置 key，请在 tuanjie-water-channels.json 配置"
		}
		req.Header.Set("Authorization", "Bearer "+key)
	case "api-key.txt":
		if k := readAPIKeyFile(); k != "" {
			req.Header.Set("Authorization", "Bearer "+k)
		}
	}
	client := &http.Client{Timeout: 8 * time.Second, Transport: noProxyTransport}
	resp, err := client.Do(req)
	if err != nil {
		return nil, "未点火或不可达（" + err.Error() + "）"
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Sprintf("上游 %d", resp.StatusCode)
	}
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 2<<20))
	var ml struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if json.Unmarshal(body, &ml) != nil {
		return nil, "模型列表解析失败"
	}
	models := []string{}
	for _, m := range ml.Data {
		if m.ID != "" {
			models = append(models, m.ID)
		}
	}
	return models, ""
}

// channelTarget 按渠道构造探针 target（tuanjie 由调用方走账号池，返回 nil）。
// 本地渠道从注册表 + 密钥配置构造；bai 未配置 key 返回明确引导错误。
func (s *Server) channelTarget(channel string) (*probeTarget, error) {
	if channel == "" || channel == "tuanjie" {
		return nil, nil
	}
	var def *waterChannelDef
	for i := range waterChannels {
		if waterChannels[i].ID == channel {
			def = &waterChannels[i]
			break
		}
	}
	if def == nil {
		return nil, fmt.Errorf("未知渠道: %s", channel)
	}
	headers := map[string]string{}
	switch def.BearerFrom {
	case "config":
		key := LoadChannelKeys()[channel]
		if key == "" {
			return nil, fmt.Errorf("B.ai 渠道未配置 key，请在 tuanjie-water-channels.json 配置")
		}
		headers["Authorization"] = "Bearer " + key
	case "api-key.txt":
		if k := readAPIKeyFile(); k != "" {
			headers["Authorization"] = "Bearer " + k
		}
	}
	return &probeTarget{BaseURL: def.BaseURL, Headers: headers, Channel: channel}, nil
}
