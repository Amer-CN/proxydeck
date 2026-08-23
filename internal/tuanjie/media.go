// media.go —— 媒体改路由（学群友 Codely Relay 的 _reroute_if_media）。
// 含图片/音频等内容块、且目标模型不识图时，自动把 model 改写为视觉模型
// （默认 codely-vl），避免纯文本模型被上游 400 拒绝后客户端无限重试。
package tuanjie

import (
	"encoding/json"
	"strings"
)

// visionModelName 是团结的视觉模型（唯一识图入口）。
const visionModelName = "codely-vl"

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

// RerouteIfMedia 含媒体内容且目标模型不识图时，就地改写 body 的 model 字段。
// 返回 (新body, 改写说明)；未改写返回 (原body, "")。
func RerouteIfMedia(body []byte) ([]byte, string) {
	var req struct {
		Model string `json:"model"`
	}
	if json.Unmarshal(body, &req) != nil || req.Model == "" {
		return body, ""
	}
	model := req.Model
	if strings.EqualFold(model, visionModelName) {
		return body, "" // 已是视觉模型
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
	m["model"] = visionModelName
	nb, err := json.Marshal(m)
	if err != nil {
		return body, ""
	}
	return nb, model + "→" + visionModelName + "（检测到 " + media + "）"
}
