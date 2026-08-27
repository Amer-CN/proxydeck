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

// providerFetchTimeout 单个 provider 计费/模型查询超时。8s 是"管理端同步调用无所谓"
// 时代的口径，但子页面打开就是 8s×N 的白屏，压到 2.5s：拉不到就下次后台补。
const providerFetchTimeout = 2500 * time.Millisecond

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
	OK           bool           `json:"ok"`
	Error        string         `json:"error,omitempty"`
	SubStatus    string         `json:"subscription_status,omitempty"`
	Sub          map[string]any `json:"subscription,omitempty"`
	UsageStatus  string         `json:"usage_status,omitempty"`
	Usage        map[string]any `json:"usage,omitempty"`
}

// ProviderStore 外部 provider 配置 + 计费信息缓存。
type ProviderStore struct {
	mu         sync.Mutex
	path       string
	list       []ExternalProvider
	cache      map[string]cachedInfo
	refreshing map[string]bool
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
	ps := &ProviderStore{path: providersFilePath(), cache: map[string]cachedInfo{}, refreshing: map[string]bool{}}
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

// Infos 返回全部 provider 的展示信息（stale-while-revalidate）：120s 内直接用
// 缓存，过期也先把旧值立刻返回、刷新丢给后台单飞并行——管理端子页面（媒体转路由
// /外部账号）点开不能被远端计费查询同步阻塞。只有完全没有缓存可兜底的头一次才
// 同步等（并行拉取 + providerFetchTimeout 超时）。
func (ps *ProviderStore) Infos() []ProviderInfo {
	ps.mu.Lock()
	list := make([]ExternalProvider, len(ps.list))
	copy(list, ps.list)
	out := make([]ProviderInfo, len(list))
	var cold, warm []providerJob
	for i, p := range list {
		c, ok := ps.cache[p.Name]
		if ok {
			out[i] = c.info
		} else {
			out[i] = baseInfo(p)
		}
		if ok && time.Since(c.fetched) < providerTTL {
			continue
		}
		if ps.refreshing[p.Name] {
			continue // 另一路 Infos 已在拉，本次先把占位/旧值给出去
		}
		ps.refreshing[p.Name] = true
		if ok {
			warm = append(warm, providerJob{p: p, idx: i})
		} else {
			cold = append(cold, providerJob{p: p, idx: i})
		}
	}
	ps.mu.Unlock()

	if len(cold) > 0 {
		ps.fetchInto(cold, out)
	}
	if len(warm) > 0 {
		go ps.fetchInto(warm, nil)
	}
	return out
}

// providerJob 一次计费查询任务，idx 指向 Infos 结果切片中的落位。
type providerJob struct {
	p   ExternalProvider
	idx int
}

// fetchInto 并行拉取 jobs 并写缓存（同时复位 refreshing）；out 非 nil 时按 idx 回填。
// out 会被调用方持有，因此后台刷新路径必须传 nil——否则会写进已交给调用方的切片。
func (ps *ProviderStore) fetchInto(jobs []providerJob, out []ProviderInfo) {
	var wg sync.WaitGroup
	for _, j := range jobs {
		wg.Add(1)
		go func(j providerJob) {
			defer wg.Done()
			info := fetchProviderInfo(j.p)
			ps.mu.Lock()
			ps.cache[j.p.Name] = cachedInfo{info: info, fetched: time.Now()}
			delete(ps.refreshing, j.p.Name)
			ps.mu.Unlock()
			if out != nil {
				out[j.idx] = info
			}
		}(j)
	}
	wg.Wait()
}

// Invalidate 清空缓存（添加/删除后调用，让下次 Infos 全量刷新）。
func (ps *ProviderStore) Invalidate() {
	ps.mu.Lock()
	defer ps.mu.Unlock()
	ps.cache = map[string]cachedInfo{}
}

// baseInfo 仅由本地配置拼出的展示信息（不发网络），供冷启动占位与拉取初值共用。
func baseInfo(p ExternalProvider) ProviderInfo {
	return ProviderInfo{Name: p.Name, BaseURL: p.BaseURL,
		ConfigModels: append([]string{}, p.Models...), Models: []string{}}
}

// fetchProviderInfo 查单个 provider：模型列表 + 订阅 + 用量。
func fetchProviderInfo(p ExternalProvider) ProviderInfo {
	info := baseInfo(p)
	cl := &http.Client{Timeout: providerFetchTimeout, Transport: smartProxyTransport}
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
