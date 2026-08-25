// media_config_test.go —— 媒体模型三选择器（ModelKind / RewriteImageModel /
// tuanjie-media.json 读写与旧 tuanjie-vision.json 迁移）的单测。
package tuanjie

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// TestModelKind 名字含 image → image、含 video → video、其余 → chat（不区分大小写）。
func TestModelKind(t *testing.T) {
	cases := map[string]string{
		"agnes-image-2.1-flash": "image",
		"AGNES-IMAGE-2.1":       "image",
		"dall-e-image":          "image",
		"agnes-video-2.5":       "video",
		"Video-X":               "video",
		"codely-vl":             "chat",
		"agnes-2.5-flash":       "chat",
		"GLM-4.5":               "chat",
		"":                      "chat",
	}
	for model, want := range cases {
		if got := ModelKind(model); got != want {
			t.Errorf("ModelKind(%q) = %q, want %q", model, got, want)
		}
	}
}

// TestRewriteImageModel 改写四态：未配置不改写 / chat 类不改写 /
// image 类改写 / 已是目标模型不改写。
func TestRewriteImageModel(t *testing.T) {
	body := func(model string) []byte {
		b, _ := json.Marshal(map[string]any{"model": model, "prompt": "a cat"})
		return b
	}

	// imageModel 空 = 不改写（客户端模型名直传）
	if nb, note := RewriteImageModel(body("whatever-image"), ""); note != "" || string(nb) != string(body("whatever-image")) {
		t.Fatalf("imageModel 为空不应改写: note=%q", note)
	}
	// chat 类模型不改写
	if _, note := RewriteImageModel(body("agnes-2.5-flash"), "agnes-image-2.1-flash"); note != "" {
		t.Fatalf("chat 类模型不应改写: note=%q", note)
	}
	// image 类模型改写为目标模型，其余字段保留
	nb, note := RewriteImageModel(body("dall-e-image-3"), "agnes-image-2.1-flash")
	var m struct {
		Model  string `json:"model"`
		Prompt string `json:"prompt"`
	}
	if err := json.Unmarshal(nb, &m); err != nil {
		t.Fatalf("改写后 body 非法: %v", err)
	}
	if m.Model != "agnes-image-2.1-flash" || m.Prompt != "a cat" {
		t.Fatalf("改写结果不对: model=%q prompt=%q", m.Model, m.Prompt)
	}
	if note != "dall-e-image-3→agnes-image-2.1-flash" {
		t.Fatalf("改写说明不对: %q", note)
	}
	// 已是目标模型不改写
	if _, note := RewriteImageModel(body("agnes-image-2.1-flash"), "agnes-image-2.1-flash"); note != "" {
		t.Fatalf("已是目标模型不应改写: note=%q", note)
	}
	// video 类模型不经此函数（由 handleImagesGenerations 先行 400）
	if _, note := RewriteImageModel(body("agnes-video-2.5"), "agnes-image-2.1-flash"); note != "" {
		t.Fatalf("video 类模型不应走生图改写: note=%q", note)
	}
}

// TestLoadMediaConfigMigratesVision media.json 缺失时从旧 tuanjie-vision.json
// 迁移读取 vision（旧文件保留不删）；media.json 存在时以它为准。
func TestLoadMediaConfigMigratesVision(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	// 场景 1：只有旧 vision.json（值非默认）→ 迁移读取，旧文件仍在
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-vision.json"),
		[]byte(`{"model":"agnes-2.5-flash"}`), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
	LoadMediaConfig()
	if VisionModel() != "agnes-2.5-flash" {
		t.Fatalf("旧 vision.json 应迁移读取, got %q", VisionModel())
	}
	if _, err := os.Stat(filepath.Join(dir, "tuanjie-vision.json")); err != nil {
		t.Fatalf("旧文件应保留不删: %v", err)
	}

	// 场景 2：media.json 存在 → 三值以它为准
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-media.json"),
		[]byte(`{"vision":"codely-vl","image":"agnes-image-2.1-flash","video":"agnes-video-2.5"}`), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
	LoadMediaConfig()
	if VisionModel() != "codely-vl" || ImageModel() != "agnes-image-2.1-flash" || VideoModel() != "agnes-video-2.5" {
		t.Fatalf("media.json 加载不对: vision=%q image=%q video=%q", VisionModel(), ImageModel(), VideoModel())
	}

	// 场景 3：SaveMediaConfig 落盘后重读一致；SaveVisionConfig 同步写两个视图
	SetImageModel("")
	SetVideoModel("v2")
	if err := SaveMediaConfig(); err != nil {
		t.Fatalf("save: %v", err)
	}
	LoadMediaConfig()
	if ImageModel() != "" || VideoModel() != "v2" {
		t.Fatalf("落盘往返不对: image=%q video=%q", ImageModel(), VideoModel())
	}
}

// 回落链构造：vision → 用户配置 → codely-vl 兜尾，去重保序
func TestVisionFallbackChain(t *testing.T) {
	chain := VisionFallbackChain()
	if len(chain) < 1 || chain[0] != VisionModel() {
		t.Fatalf("链首应为当前识图模型: %v", chain)
	}
	if chain[len(chain)-1] != "codely-vl" {
		t.Fatalf("链尾应兜底 codely-vl: %v", chain)
	}
	for i := range chain {
		for j := i + 1; j < len(chain); j++ {
			if strings.EqualFold(chain[i], chain[j]) {
				t.Fatalf("链内有重复: %v", chain)
			}
		}
	}
}

// 回落触发判定：5xx/429/网络错(0/-1)回落；4xx 配置错不回落
func TestShouldFallback(t *testing.T) {
	for _, st := range []int{-1, 429, 500, 502, 503} {
		if !shouldFallback(st) {
			t.Fatalf("状态 %d 应触发回落", st)
		}
	}
	for _, st := range []int{0, 400, 401, 403, 404, 200} {
		if shouldFallback(st) {
			t.Fatalf("状态 %d 不应触发回落", st)
		}
	}
}
