package tuanjie

import (
	"encoding/json"
	"testing"
)

// 静态候选池：标准 OpenAI 列表形、含已知 8 模型、确定性输出（排序＋键序）。
func TestStaticModelsShape(t *testing.T) {
	s := &Server{}
	b1 := s.staticModels()
	b2 := s.staticModels()
	var m struct {
		Object string `json:"object"`
		Data   []struct {
			ID      string `json:"id"`
			Object  string `json:"object"`
			OwnedBy string `json:"owned_by"`
		} `json:"data"`
	}
	if err := json.Unmarshal(b1, &m); err != nil {
		t.Fatalf("静态体不是合法 JSON: %v", err)
	}
	if m.Object != "list" {
		t.Errorf("顶层 object = %q, want list", m.Object)
	}
	want := []string{"DeepSeek-V4.1-Flash", "GLM-5.3-FLASH", "KIMI-K3",
		"codely-air", "codely-basic", "codely-core", "codely-flash", "codely-vl"}
	if len(m.Data) != len(want) {
		t.Fatalf("模型数 = %d, want %d", len(m.Data), len(want))
	}
	for i, w := range want {
		if m.Data[i].ID != w {
			t.Errorf("data[%d].id = %q, want %q（应排序）", i, m.Data[i].ID, w)
		}
		if m.Data[i].Object != "model" || m.Data[i].OwnedBy != "tuanjie" {
			t.Errorf("data[%d] 字段异常：%+v", i, m.Data[i])
		}
	}
	if string(b1) != string(b2) {
		// created 取秒级时间戳：两次调用若恰跨秒则字节不同，逐字段比（确定性指排序与集合）。
		var m2 struct {
			Object string `json:"object"`
			Data   []struct {
				ID      string `json:"id"`
				Object  string `json:"object"`
				OwnedBy string `json:"owned_by"`
			} `json:"data"`
		}
		if err := json.Unmarshal(b2, &m2); err != nil {
			t.Fatalf("第二次输出非法 JSON: %v", err)
		}
		if m2.Object != m.Object || len(m2.Data) != len(m.Data) {
			t.Fatalf("两次输出结构不一致")
		}
		for i := range m.Data {
			if m2.Data[i] != m.Data[i] {
				t.Fatalf("data[%d] 不一致（除 created 外应逐字节同序）", i)
			}
		}
	}
}

// 账号认领并集＋去重：用户配置的 claim 必须出现，已知 8 个不丢。
func TestStaticModelsUnionClaims(t *testing.T) {
	p := newTestPool(
		&Account{UserID: "u1", Enabled: true, Models: []string{"my-custom", "codely-core"}},
		&Account{UserID: "u2", Enabled: true, Models: nil},
	)
	s := &Server{pool: p}
	var m struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if err := json.Unmarshal(s.staticModels(), &m); err != nil {
		t.Fatal(err)
	}
	ids := map[string]bool{}
	for _, d := range m.Data {
		if ids[d.ID] {
			t.Errorf("重复 id：%q", d.ID)
		}
		ids[d.ID] = true
	}
	for _, w := range []string{"my-custom", "codely-core", "KIMI-K3", "DeepSeek-V4.1-Flash"} {
		if !ids[w] {
			t.Errorf("应含 %q，实际 %d 个：%v", w, len(m.Data), ids)
		}
	}
	if len(m.Data) != 9 {
		t.Errorf("总数 = %d, want 9（8 已知＋1 自定义，去重后）", len(m.Data))
	}
}

// mergeProviderModels 兼容：静态体穿过合并层不丢 id、无 provider 时原样。
func TestStaticModelsMergeCompat(t *testing.T) {
	s := &Server{providers: &ProviderStore{}}
	b := s.staticModels()
	out := s.mergeProviderModels(b)
	if string(out) != string(b) {
		t.Errorf("无 provider 时合并层应原样返回")
	}
	var m struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if err := json.Unmarshal(out, &m); err != nil || len(m.Data) != 8 {
		t.Errorf("合并后应仍为 8 个，err=%v", err)
	}
}
