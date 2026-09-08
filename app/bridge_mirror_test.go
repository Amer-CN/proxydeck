//go:build windows

package main

import (
	"net/url"
	"strings"
	"testing"
)

// applyMirror 四分支：空镜像原样 / 非 GitHub 直链原样 / 正常加前缀 / 尾斜杠归一。
func TestApplyMirror(t *testing.T) {
	const dl = "https://github.com/Amer-CN/proxydeck/releases/download/v3.10.3/ProxyDeck.exe"

	// 空镜像 → 原样返回（直连）
	if got := applyMirror(dl, ""); got != dl {
		t.Errorf("空镜像应原样返回，got %s", got)
	}
	// 非 GitHub 直链 → 原样返回（镜像前缀不套到别的域上）
	other := "https://example.com/ProxyDeck.exe"
	if got := applyMirror(other, "https://ghfast.top/"); got != other {
		t.Errorf("非 GitHub 直链应原样返回，got %s", got)
	}
	// 正常加前缀（镜像不带尾斜杠）
	want := "https://ghfast.top/" + dl
	if got := applyMirror(dl, "https://ghfast.top"); got != want {
		t.Errorf("加前缀错误，got %s want %s", got, want)
	}
	// 尾斜杠归一：带 / 与不带 / 产出完全一致
	if got := applyMirror(dl, "https://ghfast.top/"); got != want {
		t.Errorf("尾斜杠应归一，got %s want %s", got, want)
	}
}

// TestUpdateMirrorCandidates 镜像链常量合法性（防将来手滑写重/写坏）：
// 非空；每条是能解析出 host 的 http(s) URL；无尾斜杠（applyMirror 会归一，
// 常量层保持干净）；两两不同（重复 = 白试一遍）。
func TestUpdateMirrorCandidates(t *testing.T) {
	if len(updateMirrorCandidates) == 0 {
		t.Fatal("镜像候选链为空：下载将只剩直连，链路退化")
	}
	seen := make(map[string]bool, len(updateMirrorCandidates))
	for _, m := range updateMirrorCandidates {
		if m == "" {
			t.Errorf("候选含空串")
			continue
		}
		if !strings.HasPrefix(m, "http://") && !strings.HasPrefix(m, "https://") {
			t.Errorf("候选 %q 必须是 http(s) URL", m)
		}
		if u, err := url.Parse(m); err != nil || u.Host == "" {
			t.Errorf("候选 %q 不是合法 URL（err=%v）", m, err)
		}
		if strings.HasSuffix(m, "/") {
			t.Errorf("候选 %q 不应带尾斜杠", m)
		}
		if seen[m] {
			t.Errorf("候选 %q 重复", m)
		}
		seen[m] = true
	}
}
