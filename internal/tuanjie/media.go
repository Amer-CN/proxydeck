// media.go —— 媒体改路由（学群友 Codely Relay 的 _reroute_if_media）。
// 含图片/音频等内容块、且目标模型不识图时，自动把 model 改写为视觉模型
// （默认 codely-vl，可经 /vision-config 配置），避免纯文本模型被上游 400
// 拒绝后客户端无限重试。
// 三类媒体模型统一持久化在 tuanjie-media.json（识图/生图/生视频）：
// vision 与旧 tuanjie-vision.json 双视图——media.json 缺失时从旧文件迁移读取
// （旧文件保留不删），/vision-config 端点内部转到同一机制，行为不变。
package tuanjie

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// defaultVisionModel 是团结的视觉模型（唯一识图入口）。
const defaultVisionModel = "codely-vl"

// exeDirOverride 测试注入用（替换 exe 目录探测），生产为 nil。
var exeDirOverride func() string

// visionModel 当前生效的视觉模型（NewServer 时从 tuanjie-media.json /
// 旧 tuanjie-vision.json 加载，/vision-config、/media-config 端点读写）。
var visionModel struct {
	mu    sync.RWMutex
	model string
}

// mediaCfg 生图/生视频模型（vision 走上面的 visionModel，双视图同步）。
var mediaCfg struct {
	mu    sync.RWMutex
	image string
	video string
}

// ModelKind 按模型名判定类别（Agnes 家规律实测成立；别家不准将来自有
// 手动标签，本轮不做）：名字含 "image"（不区分大小写）→ "image"；
// 含 "video" → "video"；其余 → "chat"。
func ModelKind(model string) string {
	l := strings.ToLower(model)
	if strings.Contains(l, "image") {
		return "image"
	}
	if strings.Contains(l, "video") {
		return "video"
	}
	return "chat"
}

// VisionModel 返回当前视觉模型。
func VisionModel() string {
	visionModel.mu.RLock()
	defer visionModel.mu.RUnlock()
	if visionModel.model == "" {
		return defaultVisionModel
	}
	return visionModel.model
}

// SetVisionModel 更新视觉模型（空串回退默认值）。
func SetVisionModel(m string) {
	if m = strings.TrimSpace(m); m == "" {
		m = defaultVisionModel
	}
	visionModel.mu.Lock()
	visionModel.model = m
	visionModel.mu.Unlock()
}

// ImageModel 返回生图改写目标模型（空 = 不改写，客户端模型名直传）。
func ImageModel() string {
	mediaCfg.mu.RLock()
	defer mediaCfg.mu.RUnlock()
	return mediaCfg.image
}

// SetImageModel 更新生图模型（空串=清空回到默认"不改写"）。
func SetImageModel(m string) {
	mediaCfg.mu.Lock()
	mediaCfg.image = strings.TrimSpace(m)
	mediaCfg.mu.Unlock()
}

// VideoModel 返回生视频改写目标模型（空 = 不改写）。
func VideoModel() string {
	mediaCfg.mu.RLock()
	defer mediaCfg.mu.RUnlock()
	return mediaCfg.video
}

// SetVideoModel 更新生视频模型（空串=清空回到默认"不改写"）。
func SetVideoModel(m string) {
	mediaCfg.mu.Lock()
	mediaCfg.video = strings.TrimSpace(m)
	mediaCfg.mu.Unlock()
}

// builtinVisionCapable 内置原生识图模型清单：这些模型收到图片请求直接放行，
// 媒体改路由只兜底"不识图的文本模型"（学群友 vision_capable_models——
// 劫持原生识图模型等于剥夺它自己的能力，还可能改路由到更差的源）。
var builtinVisionCapable = []string{
	"codely-vl",      // 团结官方视觉模型
	"KIMI-K3",        // Kimi K3 原生多模态
	"GLM-5.3-FLASH",  // GLM-5.3-FLASH 原生多模态（rc.55 新增模型，实测识图正常）
}

// visionCapableExtra 用户扩展的原生识图模型（tuanjie-media.json vision_capable 数组）。
var visionCapableExtra []string

// VisionCapable 报告 model 是否原生识图（内置 ∪ 用户扩展，大小写不敏感）。
func VisionCapable(model string) bool {
	if model == "" {
		return false
	}
	if strings.EqualFold(model, VisionModel()) {
		return true // 识图模型自己永远算识图
	}
	for _, m := range builtinVisionCapable {
		if strings.EqualFold(model, m) {
			return true
		}
	}
	visionModel.mu.RLock()
	extra := append([]string(nil), visionCapableExtra...)
	visionModel.mu.RUnlock()
	for _, m := range extra {
		if strings.EqualFold(model, m) {
			return true
		}
	}
	return false
}

// visionFallbackExtra 用户配置的识图回落链（tuanjie-media.json vision_fallback 数组）。
// 识图兜底源失败（网络错/5xx/429）时按序回落：vision → vision_fallback… → codely-vl 兜尾。
var visionFallbackExtra []string

// VisionFallbackChain 返回识图回落链：当前识图模型 → 用户配置回落序列 →
// codely-vl 兜尾（永远兜得住的团结官方视觉模型），去重保序。
func VisionFallbackChain() []string {
	chain := []string{VisionModel()}
	visionModel.mu.RLock()
	extra := append([]string(nil), visionFallbackExtra...)
	visionModel.mu.RUnlock()
	chain = append(chain, extra...)
	chain = append(chain, defaultVisionModel)
	out := chain[:0]
	for _, m := range chain {
		dup := false
		for _, e := range out {
			if strings.EqualFold(e, m) {
				dup = true
				break
			}
		}
		if !dup && m != "" {
			out = append(out, m)
		}
	}
	return out
}

// shouldFallback 报告外部源失败状态是否值得回落重试：
// 网络错（-1，forwardExternal 网络失败约定值）/ 5xx / 429 是瞬时故障
// （换源有意义）；4xx 客户端错误是配置错，换源无意义，如实透传。
func shouldFallback(status int) bool {
	return status == -1 || status >= 500 || status == 429
}

// visionConfigPath 旧视觉模型配置文件（exe 同目录 tuanjie-vision.json，
// 只读迁移，保留不删）。
func visionConfigPath() string {
	return filepath.Join(exeDirForAccounts(), "tuanjie-vision.json")
}

// mediaConfigPath 媒体模型统一配置文件（exe 同目录 tuanjie-media.json）。
func mediaConfigPath() string {
	return filepath.Join(exeDirForAccounts(), "tuanjie-media.json")
}

// LoadMediaConfig 从 tuanjie-media.json 读 {"vision","image","video"}；
// media.json 缺失但旧 tuanjie-vision.json 有值时迁移读取 vision
// （旧文件保留）。缺失/非法 = 全部默认值。
func LoadMediaConfig() {
	// 旧 vision 机制先行加载（含旧文件回退），保住 visionModel 值。
	LoadVisionConfig()
	raw, err := os.ReadFile(mediaConfigPath())
	if err != nil {
		return
	}
	var cfg struct {
		Vision string   `json:"vision"`
		Image  string   `json:"image"`
		Video  string   `json:"video"`
		VisionCapable []string `json:"vision_capable"` // 用户扩展的原生识图模型
		VisionFallback []string `json:"vision_fallback"` // 识图回落链（失败时按序切换）
	}
	if json.Unmarshal(raw, &cfg) != nil {
		return
	}
	SetVisionModel(cfg.Vision) // 空串回退默认，与旧 LoadVisionConfig 的"非法=默认"一致
	SetImageModel(cfg.Image)
	SetVideoModel(cfg.Video)
	visionModel.mu.Lock()
	visionCapableExtra = cfg.VisionCapable
	visionFallbackExtra = cfg.VisionFallback
	visionModel.mu.Unlock()
}

// SaveMediaConfig 把三类媒体模型 + 用户扩展识图清单 + 回落链落盘到 tuanjie-media.json。
func SaveMediaConfig() error {
	visionModel.mu.RLock()
	extra := append([]string(nil), visionCapableExtra...)
	fb := append([]string(nil), visionFallbackExtra...)
	visionModel.mu.RUnlock()
	b, err := json.MarshalIndent(map[string]any{
		"vision":          VisionModel(),
		"image":           ImageModel(),
		"video":           VideoModel(),
		"vision_capable":  extra,
		"vision_fallback": fb,
	}, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(mediaConfigPath(), b, 0o644)
}

// RewriteImageModel 生图统一改写（纯函数，可单测）：imageModel 已配置且
// 请求模型为 image 类 → 把 body 的 model 就地改写为 imageModel，返回
// (新body, 改写说明)；其余情形原样返回 (原body, "")。
func RewriteImageModel(body []byte, imageModel string) ([]byte, string) {
	if imageModel == "" {
		return body, ""
	}
	var req struct {
		Model string `json:"model"`
	}
	if json.Unmarshal(body, &req) != nil || req.Model == "" {
		return body, ""
	}
	model := req.Model
	if ModelKind(model) != "image" {
		return body, ""
	}
	if model == imageModel {
		return body, ""
	}
	var m map[string]any
	if json.Unmarshal(body, &m) != nil {
		return body, ""
	}
	m["model"] = imageModel
	nb, err := json.Marshal(m)
	if err != nil {
		return body, ""
	}
	return nb, model + "→" + imageModel
}

// DetectMediaType 扫描 messages，返回第一个非文本内容块类型
// （image_url / input_audio / file …），纯文本返回空串。
func DetectMediaType(body []byte) string {
	var req struct {
		Messages []struct {
			Content json.RawMessage `json:"content"`
		} `json:"messages"`
	}
	if json.Unmarshal(body, &req) != nil {
		return ""
	}
	for _, m := range req.Messages {
		var parts []struct {
			Type string `json:"type"`
		}
		if json.Unmarshal(m.Content, &parts) != nil {
			continue // content 是字符串（纯文本），跳过
		}
		for _, p := range parts {
			if p.Type != "" && p.Type != "text" {
				return p.Type
			}
		}
	}
	return ""
}

// LoadVisionConfig 从旧 tuanjie-vision.json 读 {"model":"..."}
// （缺失/非法 = 默认值；只读迁移，不删旧文件）。
func LoadVisionConfig() {
	raw, err := os.ReadFile(visionConfigPath())
	if err != nil {
		return
	}
	var cfg struct {
		Model string `json:"model"`
	}
	if json.Unmarshal(raw, &cfg) != nil || cfg.Model == "" {
		return
	}
	SetVisionModel(cfg.Model)
}

// SaveVisionConfig 旧视觉模型持久化（/vision-config 兼容层）：
// 落盘旧文件 + 同步写新 media.json（一个机制两个视图）。
func SaveVisionConfig() error {
	b, err := json.MarshalIndent(map[string]string{"model": VisionModel()}, "", "  ")
	if err != nil {
		return err
	}
	if err := os.WriteFile(visionConfigPath(), b, 0o644); err != nil {
		return err
	}
	return SaveMediaConfig()
}

// RerouteIfMedia 含媒体内容且目标模型不识图时，就地改写 body 的 model 字段。
// 返回 (新body, 改写说明)；未改写返回 (原body, "")。
func RerouteIfMedia(body []byte) ([]byte, string) {
	vision := VisionModel()
	var req struct {
		Model string `json:"model"`
	}
	if json.Unmarshal(body, &req) != nil || req.Model == "" {
		return body, ""
	}
	model := req.Model
	if VisionCapable(model) {
		return body, "" // 原生识图模型（内置清单 ∪ 用户扩展 ∪ 识图模型本身）：直接放行
	}
	media := DetectMediaType(body)
	if media == "" {
		return body, ""
	}
	// 就地改写 model（保持其余字段字节不动）
	var m map[string]any
	if json.Unmarshal(body, &m) != nil {
		return body, ""
	}
	m["model"] = vision
	nb, err := json.Marshal(m)
	if err != nil {
		return body, ""
	}
	return nb, model + "→" + vision + "（检测到 " + media + "）"
}

// sanitizeForExternal 修复客户端历史消息里缺失的 tool_call_id 等字段（就地修改，
// 学群友 _sanitize_for_external）。外部 provider（如 Agnes）对 OpenAI schema 校验
// 比团结严格，tool 消息缺 tool_call_id 会直接 400（json_parse_error）。
// 策略：assistant.tool_calls 缺 id 则补齐并记录；role=tool 消息缺 tool_call_id
// 时按序回填；无对应调用则降级为 user 文本消息。
// 返回修复后的 body（无 messages 或解析失败时原样返回）。
func sanitizeForExternal(body []byte) []byte {
	var m map[string]any
	if json.Unmarshal(body, &m) != nil {
		return body
	}
	msgs, ok := m["messages"].([]any)
	if !ok {
		return body
	}
	pendingIDs := []string{}
	for _, raw := range msgs {
		msg, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		role, _ := msg["role"].(string)
		if role == "assistant" {
			tcs, ok := msg["tool_calls"].([]any)
			if !ok {
				continue
			}
			for _, rawTC := range tcs {
				tc, ok := rawTC.(map[string]any)
				if !ok {
					continue
				}
				cid, _ := tc["id"].(string)
				if cid == "" {
					cid = "call_" + newRID()
					tc["id"] = cid
				}
				pendingIDs = append(pendingIDs, cid)
			}
		} else if role == "tool" {
			tcid, _ := msg["tool_call_id"].(string)
			if tcid != "" {
				continue
			}
			if len(pendingIDs) > 0 {
				msg["tool_call_id"] = pendingIDs[0]
				pendingIDs = pendingIDs[1:]
				continue
			}
			// 无对应调用：降级为 user 文本消息
			var text string
			switch c := msg["content"].(type) {
			case []any:
				parts := []string{}
				for _, p := range c {
					if pm, ok := p.(map[string]any); ok {
						if t, _ := pm["text"].(string); t != "" {
							parts = append(parts, t)
						}
					}
				}
				text = strings.Join(parts, " ")
			case string:
				text = c
			}
			msg["role"] = "user"
			msg["content"] = "[tool result] " + text
			delete(msg, "tool_call_id")
		}
	}
	if nb, err := json.Marshal(m); err == nil {
		return nb
	}
	return body
}

// fetchImgClient 下载远程图片的 HTTP 客户端（超时 15s，限制 10MB）。
var fetchImgClient = &http.Client{Timeout: 15 * time.Second, Transport: smartProxyTransport}

// FetchRemoteImages 扫描 messages 中 image_url 类型的远程图片（http/https），
// 下载后转为 base64 data URI 就地替换。上游（Agnes / 团结）对远程 URL 图片
// 经常挂死或下载超时，base64 data URI 则稳定可用。
// 返回 (新body, 替换条数)；无远程图片或解析失败时原样返回。
func FetchRemoteImages(body []byte) ([]byte, int) {
	var m map[string]any
	if json.Unmarshal(body, &m) != nil {
		return body, 0
	}
	msgs, ok := m["messages"].([]any)
	if !ok {
		return body, 0
	}
	count := 0
	for _, raw := range msgs {
		msg, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		parts, ok := msg["content"].([]any)
		if !ok {
			continue
		}
		for _, p := range parts {
			pm, ok := p.(map[string]any)
			if !ok {
				continue
			}
			if t, _ := pm["type"].(string); t != "image_url" {
				continue
			}
			iu, ok := pm["image_url"].(map[string]any)
			if !ok {
				continue
			}
			url, _ := iu["url"].(string)
			if !strings.HasPrefix(url, "http://") && !strings.HasPrefix(url, "https://") {
				continue // base64 data URI 或其他：跳过
			}
			dataURI, err := downloadAsDataURI(url)
			if err != nil {
				log.Printf("[tuanjie] fetch-remote-img %s err=%v", url, err)
				continue // 下载失败保留原 URL，让上游自行处理
			}
			iu["url"] = dataURI
			count++
		}
	}
	if count == 0 {
		return body, 0
	}
	nb, err := json.Marshal(m)
	if err != nil {
		return body, count
	}
	return nb, count
}

// downloadAsDataURI 下载图片并转为 data URI（含 MIME 推断）。
func downloadAsDataURI(url string) (string, error) {
	resp, err := fetchImgClient.Get(url)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, 10<<20)) // 最大 10MB
	if err != nil {
		return "", err
	}
	mime := resp.Header.Get("Content-Type")
	if mime == "" || !strings.HasPrefix(mime, "image/") {
		mime = sniffImageMIME(data)
	}
	return "data:" + mime + ";base64," + base64.StdEncoding.EncodeToString(data), nil
}

// sniffImageMIME 按魔术字节推断图片 MIME 类型。
func sniffImageMIME(data []byte) string {
	if len(data) >= 4 {
		switch {
		case data[0] == 0x89 && data[1] == 'P' && data[2] == 'N' && data[3] == 'G':
			return "image/png"
		case data[0] == 0xFF && data[1] == 0xD8:
			return "image/jpeg"
		case data[0] == 'G' && data[1] == 'I' && data[2] == 'F':
			return "image/gif"
		case data[0] == 'R' && data[1] == 'I' && data[2] == 'F' && data[3] == 'F':
			return "image/webp"
		}
	}
	return "application/octet-stream"
}
