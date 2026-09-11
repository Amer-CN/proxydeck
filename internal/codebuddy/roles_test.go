// roles_test.go —— developer→system 角色降级单测。
// 2026-09-11 实测：腾讯后端见 developer 角色一律 400/11128（与内容无关），
// 故出站前统一降级为 system。不碰网络。
package codebuddy

import "testing"

func TestNormalizeRolesDeveloperToSystem(t *testing.T) {
	msgs := []any{
		map[string]any{"role": "developer", "content": "You are a helpful assistant."},
		map[string]any{"role": "user", "content": "hi"},
		map[string]any{"role": "developer", "content": "Be concise."},
	}
	out := normalizeRoles(msgs)
	if len(out) != 3 {
		t.Fatalf("消息条数不应变化, got %d", len(out))
	}
	for i, raw := range out {
		m := raw.(map[string]any)
		want := "system"
		if i == 1 {
			want = "user"
		}
		if got, _ := m["role"].(string); got != want {
			t.Fatalf("第 %d 条 role 应为 %q, got %q", i, want, got)
		}
	}
	// 内容不动（只改角色名）
	if got, _ := out[0].(map[string]any)["content"].(string); got != "You are a helpful assistant." {
		t.Fatalf("content 不应被改动, got %q", got)
	}
	// 降级后可满足 INTL「首条必须是 system」判定
	if !msgsHaveSystem(out) {
		t.Fatalf("降级后应能识别出 system 角色")
	}
}

func TestNormalizeRolesNoDeveloperUnchanged(t *testing.T) {
	msgs := []any{
		map[string]any{"role": "system", "content": "s"},
		map[string]any{"role": "user", "content": "u"},
	}
	out := normalizeRoles(msgs)
	for i, raw := range out {
		m := raw.(map[string]any)
		if got, _ := m["role"].(string); got != msgs[i].(map[string]any)["role"].(string) {
			t.Fatalf("无 developer 时不应改动, 第 %d 条 got %q", i, got)
		}
	}
}

func TestNormalizeRolesToleratesNonMapEntries(t *testing.T) {
	msgs := []any{"not-a-map", map[string]any{"role": "developer", "content": "x"}}
	out := normalizeRoles(msgs)
	if len(out) != 2 {
		t.Fatalf("消息条数不应变化, got %d", len(out))
	}
	if got, _ := out[1].(map[string]any)["role"].(string); got != "system" {
		t.Fatalf("map 条目应降级为 system, got %q", got)
	}
}
