//go:build windows

package main

import (
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

// normalizeMirrorInput（ccSetUpdateMirror 的校验纯函数）：空串=清除；合法 http(s) 通过；
// 非 http(s) / 无 host 拒绝。
func TestNormalizeMirrorInput(t *testing.T) {
	// 空串（含纯空白）→ ("", nil)：清除回直连
	for _, s := range []string{"", "   ", "\t"} {
		m, err := normalizeMirrorInput(s)
		if err != nil || m != "" {
			t.Errorf("%q 应归一为空串（清除），got (%q, %v)", s, m, err)
		}
	}
	// 合法 http(s) URL → trim 后原样返回
	for _, s := range []string{
		"https://ghfast.top/",
		"http://127.0.0.1:8080/",
		"  https://gh-proxy.com/  ",
	} {
		m, err := normalizeMirrorInput(s)
		if err != nil {
			t.Errorf("%q 应合法，got err %v", s, err)
		}
		if want := strings.TrimSpace(s); m != want {
			t.Errorf("%q 归一错误，got %q want %q", s, m, want)
		}
	}
	// 拒绝：裸域名 / 非 http(s) scheme / 无 host
	for _, s := range []string{
		"ghfast.top",        // 裸域名，无 scheme
		"ftp://ghfast.top/", // 非 http(s) scheme
		"https://",          // 无 host
		"://broken",
	} {
		if _, err := normalizeMirrorInput(s); err == nil {
			t.Errorf("%q 应被拒绝", s)
		}
	}
}
