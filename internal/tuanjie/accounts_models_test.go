// accounts_models_test.go —— 账号池模型路由：Pick() 按 Models 列表精确匹配选号，
// 空白名单 = 兜底接所有未被认领的模型。
package tuanjie

import (
	"os"
	"path/filepath"
	"testing"
)

// newTestPool 构造一个不依赖文件系统的内存池（绕过 reload/EnsureLocalAccount）。
func newTestPool(accs ...*Account) *AccountPool {
	return &AccountPool{
		accounts: accs,
		loads:    map[string]int{},
	}
}

func TestPick_ModelClaimed(t *testing.T) {
	a := &Account{UserID: "paid", Enabled: true, HasGLM53: true, Models: []string{"gpt-4o"}}
	b := &Account{UserID: "free", Enabled: true, HasGLM53: true, Models: nil} // 兜底
	p := newTestPool(a, b)

	for i := 0; i < 10; i++ {
		got := p.Pick("gpt-4o")
		if got == nil || got.UserID != "paid" {
			t.Fatalf("iter %d: Pick(gpt-4o) = %v, want paid", i, got)
		}
	}
}

func TestPick_Fallback(t *testing.T) {
	a := &Account{UserID: "paid", Enabled: true, HasGLM53: true, Models: []string{"gpt-4o"}}
	b := &Account{UserID: "free", Enabled: true, HasGLM53: true, Models: nil}
	p := newTestPool(a, b)

	for i := 0; i < 10; i++ {
		got := p.Pick("qwen-max")
		if got == nil || got.UserID != "free" {
			t.Fatalf("iter %d: Pick(qwen-max) = %v, want free (fallback)", i, got)
		}
	}
}

func TestPick_NoFallbackNoClaimer(t *testing.T) {
	// 所有账号都有非空 Models 且都不包含请求模型 → 向后兼容：仍能返回某账号
	a := &Account{UserID: "a1", Enabled: true, HasGLM53: true, Models: []string{"gpt-4o"}}
	b := &Account{UserID: "a2", Enabled: true, HasGLM53: true, Models: []string{"claude-3"}}
	p := newTestPool(a, b)

	got := p.Pick("qwen-max")
	if got == nil {
		t.Fatal("Pick(qwen-max) = nil, want non-nil (backward compat)")
	}
}

func TestPick_MultipleClaimers(t *testing.T) {
	a := &Account{UserID: "a1", Enabled: true, HasGLM53: true, Models: []string{"gpt-4o"}}
	b := &Account{UserID: "a2", Enabled: true, HasGLM53: true, Models: []string{"gpt-4o"}}
	p := newTestPool(a, b)

	counts := map[string]int{}
	for i := 0; i < 20; i++ {
		got := p.Pick("gpt-4o")
		if got == nil {
			t.Fatalf("iter %d: nil", i)
		}
		counts[got.UserID]++
	}
	if counts["a1"] == 0 || counts["a2"] == 0 {
		t.Fatalf("expected both claimers to be picked, got %v", counts)
	}
}

func TestSetModels_Persist(t *testing.T) {
	// 直接用 pool 内存态验证 SetModels 写入正确性（saveLocked/reload 依赖 exeDir，单元测试只验内存态）
	p := newTestPool(&Account{UserID: "u1", Enabled: true, AccessToken: "tok", HasGLM53: true})

	if !p.SetModels("u1", []string{"gpt-4o", "claude-3"}) {
		t.Fatal("SetModels returned false")
	}
	got := p.Get("u1")
	if len(got.Models) != 2 || got.Models[0] != "gpt-4o" || got.Models[1] != "claude-3" {
		t.Fatalf("after SetModels: Models = %v", got.Models)
	}

	// 清空 → nil
	if !p.SetModels("u1", nil) {
		t.Fatal("SetModels(nil) returned false")
	}
	got = p.Get("u1")
	if got.Models != nil {
		t.Fatalf("after SetModels(nil): Models = %v, want nil", got.Models)
	}
}

func TestSetModels_NotFound(t *testing.T) {
	p := newTestPool(&Account{UserID: "u1", Enabled: true})
	if p.SetModels("nonexistent", []string{"x"}) {
		t.Fatal("SetModels on nonexistent user should return false")
	}
}

// TestPick_DisabledExcluded 确认 disabled 账号即使认领了模型也不参与选择
func TestPick_DisabledExcluded(t *testing.T) {
	a := &Account{UserID: "paid", Enabled: false, HasGLM53: true, Models: []string{"gpt-4o"}}
	b := &Account{UserID: "free", Enabled: true, HasGLM53: true, Models: nil}
	p := newTestPool(a, b)

	got := p.Pick("gpt-4o")
	if got == nil || got.UserID != "free" {
		t.Fatalf("disabled claimer should be skipped; got %v", got)
	}
}

// 确保测试不污染真实配置文件
func init() {
	// t.TempDir 在各 test 内使用；此处无需全局 setup
	_ = filepath.Join
	_ = os.TempDir
}
