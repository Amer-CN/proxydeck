package proxy

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/Amer-CN/proxydeck/internal/api"
)

// TestHandleModels_FullCatalog 断言 /v1/models 返回官方完整模型目录（61 个）。
// 2026-08-22 同步自 https://commandcode.ai/docs/plans/go（官方 ID 已改为短横线格式）。
func TestHandleModels_FullCatalog(t *testing.T) {
	p := &Proxy{}
	req := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	rec := httptest.NewRecorder()
	p.HandleModels(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rec.Code)
	}

	var list api.OpenAIModelList
	if err := json.Unmarshal(rec.Body.Bytes(), &list); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	if len(list.Data) != 61 {
		t.Errorf("model count = %d, want 61", len(list.Data))
	}

	// 关键模型抽查：2026-08-22 新增的模型必须都在
	want := []string{
		// 新增
		"ox-alpha",
		"glm-5.3",
		"deepseek-v4-flash-vision-exp",
		"gemini-3.7-flash",
		"qwen-3.8-27b",
		// 常青关键模型
		"claude-opus-5",
		"claude-sonnet-5",
		"claude-fable-5",
		"claude-haiku-4-5",
		"gemini-3.5-flash",
		"gemini-3.6-flash",
		"ling-3.0-flash-free",
		"muse-spark-1.1",
		"muse-spark-1.2",
		"muse-spark-1.2-contributor",
		"kimi-k3",
		"kimi-k2.7-code",
		"kimi-k2.7-code-highspeed",
		"gpt-5.3-codex",
		"gpt-5.4",
		"gpt-5.5",
		"gpt-5.6-luna",
		"gpt-5.6-sol",
		"gpt-5.6-terra",
		"laguna-s-2.1-free",
		"fugu-ultra",
		"tencent/hy3-paid",
		"inkling",
		"inkling-small",
		"grok-4.5",
		"glm-5.2",
		"glm-5.2-fast",
		"deepseek-v4-flash",
		"deepseek-v4-pro",
		"minimax-m3",
	}
	got := make(map[string]bool, len(list.Data))
	for _, m := range list.Data {
		got[m.ID] = true
	}
	for _, id := range want {
		if !got[id] {
			t.Errorf("missing model %q", id)
		}
	}

	// 已下架的模型不应出现
	for _, id := range []string{"Qwen/Qwen3.6-Max-Preview"} {
		if got[id] {
			t.Errorf("delisted model %q should not appear", id)
		}
	}

	// 不允许重复 ID
	seen := make(map[string]bool, len(list.Data))
	for _, m := range list.Data {
		if seen[m.ID] {
			t.Errorf("duplicate model id %q", m.ID)
		}
		seen[m.ID] = true
	}
}

// TestHandleModels_GoPlan 断言 ?plan=go 只返回 Go 套餐内的 36 个模型。
func TestHandleModels_GoPlan(t *testing.T) {
	p := &Proxy{}
	req := httptest.NewRequest(http.MethodGet, "/v1/models?plan=go", nil)
	rec := httptest.NewRecorder()
	p.HandleModels(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", rec.Code)
	}

	var list api.OpenAIModelList
	if err := json.Unmarshal(rec.Body.Bytes(), &list); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	if len(list.Data) != 36 {
		t.Errorf("go plan model count = %d, want 36", len(list.Data))
	}

	// Go 套餐必须包含的模型
	want := []string{
		"deepseek-v4-flash",
		"deepseek-v4-pro",
		"qwen-3.8-max",
		"qwen-3.7-max",
		"qwen-3.7-plus",
		"qwen-3.7-flash",
		"kimi-k3",
		"kimi-k2.7-code",
		"glm-5.2",
		"glm-5.2-fast",
		"minimax-m3",
		"gpt-5.6-luna",
		"laguna-s-2.1-free",
		"tencent/hy3-paid",
		"grok-4.5",
		"mimo-v2.5-pro",
		"step-3.7-flash",
		"nemotron-3-ultra",
		"muse-spark-1.2-contributor",
		"ox-alpha",
		"glm-5.3",
		"deepseek-v4-flash-vision-exp",
	}
	got := make(map[string]bool, len(list.Data))
	for _, m := range list.Data {
		got[m.ID] = true
	}
	for _, id := range want {
		if !got[id] {
			t.Errorf("go plan missing model %q", id)
		}
	}

	// Go 套餐绝不应包含的模型（需 Pro/Max）
	notWant := []string{
		"claude-opus-5",
		"claude-sonnet-5",
		"claude-fable-5",
		"gemini-3.6-flash",
		"gpt-5.6-sol",
		"gpt-5.6-terra",
		"gpt-5.5",
		"fugu-ultra",
		"muse-spark-1.2",
	}
	for _, id := range notWant {
		if got[id] {
			t.Errorf("go plan should NOT include %q", id)
		}
	}
}
