// providers.go —— 外部账号（external provider）：管理 + 信息展示。
// 学群友 /api/providers/info：订阅/用量取自 provider 的 OpenAI 计费风格接口
// （{base_url}/dashboard/billing/subscription|usage），不是所有 provider 都实现，
// 不可用时如实返回状态码，前端降级展示。响应绝不包含 api_key。
// 边界：只做管理与信息展示，不接入代理流量转发。
package tuanjie

import (
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

// providerTTL 计费查询缓存时长（学群友 120s）。
const providerTTL = 120 * time.Second

// ExternalProvider 用户配置的外部 provider（tuanjie-providers.json）。
// Models 是参与转发的模型列表（缺省空 = 仅展示不转发，向后兼容旧配置）。
type ExternalProvider struct {
	Name    string   `json:"name"`
	BaseURL string   `json:"base_url"`
	APIKey  string   `json:"api_key"`
	Models  []string `json:"models,omitempty"`
}

// ProviderInfo 单个 provider 的展示信息（不含 api_key）。
// ConfigModels 是用户配置的 models（参与转发），与 Models（计费接口 /models
// 拉到的展示列表）区分：前端徽章优先用 ConfigModels。
type ProviderInfo struct {
	Name         string         `json:"name"`
	BaseURL      string         `json:"base_url"`
	ConfigModels []string       `json:"config_models"`
	Models       []string       `json:"models"`
	OK          bool           `json:"ok"`
	Error       string         `json:"error,omitempty"`
	SubStatus   string         `json:"subscription_status,omitempty"`
	Sub         map[string]any `json:"subscription,omitempty"`
	UsageStatus string         `json:"usage_status,omitempty"`
	Usage       map[string]any `json:"usage,omitempty"`
}

// ProviderStore 外部 provider 配置 + 计费信息缓存。
type ProviderStore struct {
	mu    sync.Mutex
	path  string
	list  []ExternalProvider
	cache map[string]cachedInfo
}

type cachedInfo struct {
	info    ProviderInfo
	fetched time.Time
}

func providersFilePath() string {
	return filepath.Join(exeDirForAccounts(), "tuanjie-providers.json")
}

// NewProviderStore 从 tuanjie-providers.json 加载（文件缺失 = 空）。
func NewProviderStore() *ProviderStore {
	ps := &ProviderStore{path: providersFilePath(), cache: map[string]cachedInfo{}}
	raw, err := os.ReadFile(ps.path)
	if err == nil {
		_ = json.Unmarshal(raw, &ps.list)
	}
	return ps
}

func (ps *ProviderStore) saveLocked() {
	if raw, err := json.MarshalIndent(ps.list, "", "  "); err == nil {
		_ = os.WriteFile(ps.path, raw, 0o600)
	}
}

// List 返回全部配置（含 api_key，仅供内部/管理端校验；对外接口用 Infos）。
func (ps *ProviderStore) List() []ExternalProvider {
	ps.mu.Lock()
	defer ps.mu.Unlock()
	out := make([]ExternalProvider, len(ps.list))
	copy(out, ps.list)
	return out
}

// Match model 命中某 provider 的 models 时返回该 provider 的副本，否则 nil
// （学群友 providers.match；空 models 的 provider 永不命中）。
func (ps *ProviderStore) Match(model string) *ExternalProvider {
	if model == "" {
		return nil
	}
	ps.mu.Lock()
	defer ps.mu.Unlock()
	for _, p := range ps.list {
		for _, m := range p.Models {
			if m == model {
				cp := p
				return &cp
			}
		}
	}
	return nil
}

// AllModels 所有外部模型条目（供 /v1/models 合并展示，owned_by=provider 名）。
func (ps *ProviderStore) AllModels() []struct {
	ID      string `json:"id"`
	Object  string `json:"object"`
	OwnedBy string `json:"owned_by"`
} {
	ps.mu.Lock()
	defer ps.mu.Unlock()
	out := []struct {
		ID      string `json:"id"`
		Object  string `json:"object"`
		OwnedBy string `json:"owned_by"`
	}{}
	for _, p := range ps.list {
		for _, m := range p.Models {
			out = append(out, struct {
				ID      string `json:"id"`
				Object  string `json:"object"`
				OwnedBy string `json:"owned_by"`
			}{ID: m, Object: "model", OwnedBy: p.Name})
		}
	}
	return out
}

// Add 添加（重名拒绝）。命名/URL 规范化、models 逐条去空白后落盘。
func (ps *ProviderStore) Add(p ExternalProvider) bool {
	p.Name = strings.TrimSpace(p.Name)
	p.BaseURL = strings.TrimRight(strings.TrimSpace(p.BaseURL), "/")
	p.APIKey = strings.TrimSpace(p.APIKey)
	models := p.Models[:0:0]
	for _, m := range p.Models {
		if m = strings.TrimSpace(m); m != "" {
			models = append(models, m)
		}
	}
	p.Models = models
	if p.Name == "" || p.BaseURL == "" || p.APIKey == "" {
		return false
	}
	ps.mu.Lock()
	defer ps.mu.Unlock()
	for _, e := range ps.list {
		if e.Name == p.Name {
			return false
		}
	}
	ps.list = append(ps.list, p)
	ps.saveLocked()
	return true
}

// AddModel 给指定 provider 追加一个模型（trim 后落盘；重复或 provider 不存在返回 false）。
func (ps *ProviderStore) AddModel(providerName, model string) bool {
	model = strings.TrimSpace(model)
	if model == "" {
		return false
	}
	ps.mu.Lock()
	defer ps.mu.Unlock()
	for i, e := range ps.list {
		if e.Name == providerName {
			for _, m := range e.Models {
				if m == model {
					return false
				}
			}
			ps.list[i].Models = append(ps.list[i].Models, model)
			ps.saveLocked()
			return true
		}
	}
	return false
}

// RemoveModel 从指定 provider 删除一个模型（不存在返回 false）。
func (ps *ProviderStore) RemoveModel(providerName, model string) bool {
	ps.mu.Lock()
	defer ps.mu.Unlock()
	for i, e := range ps.list {
		if e.Name == providerName {
			for j, m := range e.Models {
				if m == model {
					ps.list[i].Models = append(e.Models[:j], e.Models[j+1:]...)
					ps.saveLocked()
					return true
				}
			}
			return false
		}
	}
	return false
}

// Remove 按名称删除。
func (ps *ProviderStore) Remove(name string) bool {
	ps.mu.Lock()
	defer ps.mu.Unlock()
	for i, e := range ps.list {
		if e.Name == name {
			ps.list = append(ps.list[:i], ps.list[i+1:]...)
			delete(ps.cache, name)
			ps.saveLocked()
			return true
		}
	}
	return false
}

// Infos 返回全部 provider 的展示信息：缓存 120s 内直接用，过期同步刷新
// （管理端低频调用，同步实现最简单；单个查询超时 8s，学群友口径）。
func (ps *ProviderStore) Infos() []ProviderInfo {
	ps.mu.Lock()
	list := make([]ExternalProvider, len(ps.list))
	copy(list, ps.list)
	out := make([]ProviderInfo, 0, len(list))
	stale := map[string]ExternalProvider{}
	for _, p := range list {
		if c, ok := ps.cache[p.Name]; ok && time.Since(c.fetched) < providerTTL {
			out = append(out, c.info)
		} else {
			stale[p.Name] = p
		}
	}
	ps.mu.Unlock()

	for _, p := range stale {
		info := fetchProviderInfo(p)
		ps.mu.Lock()
		ps.cache[p.Name] = cachedInfo{info: info, fetched: time.Now()}
		ps.mu.Unlock()
		out = append(out, info)
	}
	return out
}

// Invalidate 清空缓存（添加/删除后调用，让下次 Infos 全量刷新）。
func (ps *ProviderStore) Invalidate() {
	ps.mu.Lock()
	defer ps.mu.Unlock()
	ps.cache = map[string]cachedInfo{}
}

// fetchProviderInfo 查单个 provider：模型列表 + 订阅 + 用量。
func fetchProviderInfo(p ExternalProvider) ProviderInfo {
	info := ProviderInfo{Name: p.Name, BaseURL: p.BaseURL, ConfigModels: append([]string{}, p.Models...), Models: []string{}}
	cl := &http.Client{Timeout: 8 * time.Second, Transport: smartProxyTransport}
	hdr := map[string]string{"Authorization": "Bearer " + p.APIKey, "Accept": "application/json"}

	// 模型列表（失败不致命：留空列表）
	if r, err := getJSON(cl, p.BaseURL+"/models", hdr); err == nil {
		if ms, ok := r["data"].([]any); ok {
			for _, m := range ms {
				if mm, ok := m.(map[string]any); ok {
					if id, _ := mm["id"].(string); id != "" {
						info.Models = append(info.Models, id)
					}
				}
			}
		}
	}

	// 订阅 + 用量（任一失败如实记录状态码；网络级异常 = 不可达，学群友 ok=false 口径）
	sub, subErr := getJSON(cl, p.BaseURL+"/dashboard/billing/subscription", hdr)
	info.SubStatus = statusOf(sub, subErr)
	if subErr == nil {
		info.Sub = sub
	} else if _, isStatus := subErr.(*statusError); !isStatus {
		info.OK = false
		info.Error = truncateErr(subErr)
	}
	usage, usageErr := getJSON(cl, p.BaseURL+"/dashboard/billing/usage", hdr)
	info.UsageStatus = statusOf(usage, usageErr)
	if usageErr == nil {
		info.Usage = usage
	} else if _, isStatus := usageErr.(*statusError); !isStatus {
		info.OK = false
		info.Error = truncateErr(usageErr)
	}
	if info.Error == "" {
		info.OK = true // 两个计费端点都连通（HTTP 状态码不论）即视为可达
	}
	return info
}

// truncateErr 错误信息截断到 120 字符（学群友口径）。
func truncateErr(err error) string {
	if len(err.Error()) > 120 {
		return err.Error()[:120]
	}
	return err.Error()
}

func getJSON(cl *http.Client, url string, headers map[string]string) (map[string]any, error) {
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	resp, err := cl.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, &statusError{code: resp.StatusCode}
	}
	var m map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&m); err != nil {
		return nil, err
	}
	return m, nil
}

type statusError struct{ code int }

func (e *statusError) Error() string { return "HTTP " + strconv.Itoa(e.code) }

// statusOf 把查询结果转成前端展示的状态字符串（学群友口径：非 200 如实报码）。
func statusOf(_ map[string]any, err error) string {
	if err == nil {
		return "200"
	}
	if se, ok := err.(*statusError); ok {
		return "HTTP " + strconv.Itoa(se.code)
	}
	if len(err.Error()) > 120 {
		return err.Error()[:120]
	}
	return err.Error()
}
