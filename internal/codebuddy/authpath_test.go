// authpath_test.go —— 登录态路径漂移治理单测（简报 .work/current-task.md）：
// 新版桌面端 nickname 加密对象兼容（双区新文件命中）/ 多候选目录拼装 /
// 目录缺失时如实报"去哪找过"（Find/池/health/NewServer 四处同口径）。
// auth 目录一律 t.TempDir() 造假经 CODEBUDDY_AUTH_DIR 隔离，绝不读写真实目录。
package codebuddy

import (
	"context"
	"encoding/json"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// writeAuthInfoObjNick 写一个 nickname 为加密对象的新版形 .info 文件
//（2026-10-12 本机双区新文件实测形态：nickname={"$wbEncrypted":1,...}）。
func writeAuthInfoObjNick(t *testing.T, dir, name, domain, uid string, mtime time.Time) string {
	t.Helper()
	v := map[string]any{
		"auth": map[string]any{"accessToken": "tok-" + uid, "domain": domain, "expiresAt": 4102416000000},
		"account": map[string]any{"uid": uid,
			"nickname":     map[string]any{"$wbEncrypted": 1, "envelope": "deadbeef"},
			"enterpriseId": "ent"},
	}
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, name)
	if err := os.WriteFile(path, b, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(path, mtime, mtime); err != nil {
		t.Fatal(err)
	}
	return path
}

// writeAuthInfoEncrypted 写一个全加密形态的新版 .info 文件（2026-10-12 本机
// 双区新文件实测形态：nickname/accessToken/refreshToken 均为 {"$wbEncrypted":1,...}）。
func writeAuthInfoEncrypted(t *testing.T, dir, name, domain, uid string, mtime time.Time) string {
	t.Helper()
	enc := map[string]any{"$wbEncrypted": 1, "envelope": "deadbeef"}
	v := map[string]any{
		"auth": map[string]any{"accessToken": enc, "refreshToken": enc,
			"domain": domain, "expiresAt": 4102416000000},
		"account": map[string]any{"uid": uid, "nickname": enc, "enterpriseId": "ent"},
	}
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, name)
	if err := os.WriteFile(path, b, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(path, mtime, mtime); err != nil {
		t.Fatal(err)
	}
	return path
}
// 新版加密 nickname（明文 token 过渡形态）不得整件丢弃：双区各一例——
// Find 按 mtime 命中新文件、Summary 无 error 且 nickname 置空、
// 建池含该号且 Nickname 为空、Pick 可选。
func TestAuthNewSchemaEncryptedNickname(t *testing.T) {
	for _, tc := range []struct {
		region Region
		domain string
		uid    string
	}{
		{RegionCN, "www.codebuddy.cn", "uid-new-cn"},
		{RegionINTL, "www.workbuddy.ai", "uid-new-intl"},
	} {
		t.Run(string(tc.region), func(t *testing.T) {
			dir := t.TempDir()
			t.Setenv("CODEBUDDY_AUTH_DIR", dir)
			base := time.Now()
			// 旧文件（字符串昵称，mtime 老）+ 新文件（加密对象昵称，mtime 新）
			writeAuthInfo(t, dir, "old.info", tc.domain, "uid-old", "老昵称", base.Add(-2*time.Hour))
			newPath := writeAuthInfoObjNick(t, dir, "new.info", tc.domain, tc.uid, base)

			if got := FindAuthFileForRegion(tc.region); got != newPath {
				t.Fatalf("新版文件应按 mtime 命中: got %q want %q", got, newPath)
			}
			sum := NewCredential("", tc.region).Summary()
			if err, ok := sum["error"]; ok {
				t.Fatalf("新版文件 Summary 不应报错: %v", err)
			}
			if nick, _ := sum["nickname"].(string); nick != "" {
				t.Fatalf("加密昵称应置空（不编造）: got %q", nick)
			}
			p := NewAccountPool(tc.region)
			if p == nil {
				t.Fatal("新版文件应能建池")
			}
			var found *AccountView
			for _, v := range p.Snapshot() {
				if v.UID == tc.uid {
					vv := v
					found = &vv
				}
			}
			if found == nil {
				t.Fatalf("池内应含新版号 %s: %+v", tc.uid, p.Snapshot())
			}
			if found.Nickname != "" {
				t.Fatalf("池内加密昵称应为空: got %q", found.Nickname)
			}
			if a := p.Pick(); a == nil {
				t.Fatal("新版号应可被 Pick")
			}
		})
	}
}

// 旧版字符串昵称回归：加密兼容不得破坏原有解析（nickname 原样保留）。
func TestAuthStringNicknameUnchanged(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "甲", time.Now())
	sum := NewCredential("", RegionCN).Summary()
	if err, ok := sum["error"]; ok {
		t.Fatalf("旧版文件不应报错: %v", err)
	}
	if nick, _ := sum["nickname"].(string); nick != "甲" {
		t.Fatalf("字符串昵称应原样保留: got %q", nick)
	}
}

// 多候选目录拼装（纯函数，喂假根）：顺序 = 本地默认 > macOS > Linux；
// 去重（本地与 xdg 指向同一目录时只留一个）；空根不产出空串条目。
// 漫游位已删：2026-10-12 本机实测 %APPDATA%\CodeBuddyExtension\... 根本不存在
//（.work/roaming_verdict.txt），无命中实证，不保留。
func TestAuthCandidateDirs(t *testing.T) {
	got := candidateAuthDirs(`C:\L`, `/home/u`, `/data`)
	want := []string{
		filepath.Join(`C:\L`, "CodeBuddyExtension", "Data", "Public", "auth"),
		filepath.Join("/home/u", "Library", "Application Support", "CodeBuddyExtension", "Data", "Public", "auth"),
		filepath.Join("/data", "CodeBuddyExtension", "Data", "Public", "auth"),
	}
	if len(got) != len(want) {
		t.Fatalf("候选数不符: got %q", got)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("第 %d 位不符: got %q want %q\n全量 %q", i, got[i], want[i], got)
		}
	}
	// 本地==xdg 指向同一目录：去重只留一个
	if dup := candidateAuthDirs(`C:\L`, "", `C:\L`); len(dup) != 1 {
		t.Fatalf("相同目录应去重为 1 条: got %q", dup)
	}
	// 空根：不产出空串条目
	if none := candidateAuthDirs("", "", ""); len(none) != 0 {
		t.Fatalf("全空根应返回空: got %q", none)
	}
}

// CODEBUDDY_AUTH_DIR 逃生口回归：命中即唯一（多候选不掺入）。
func TestAuthEnvOverrideWins(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	if got := findAuthDirs(); len(got) != 1 || got[0] != dir {
		t.Fatalf("环境变量应唯一胜出: got %q want [%q]", got, dir)
	}
}

// 目录缺失如实报错：空目录与不存在目录两例——Find 空、建池 nil、
// Summary 与 NewServer 错误里都带"去哪找过"的目录、health 同构透出。
func TestAuthMissingReportsSearchedDirs(t *testing.T) {
	for _, tc := range []struct {
		name string
		dir  func(base string) string
	}{
		{"empty-dir", func(base string) string { return base }},
		{"nonexistent-dir", func(base string) string { return filepath.Join(base, "根本不存在") }},
	} {
		t.Run(tc.name, func(t *testing.T) {
			base := t.TempDir()
			dir := tc.dir(base)
			t.Setenv("CODEBUDDY_AUTH_DIR", dir)
			t.Setenv("CODEBUDDY_POOL_DIR", t.TempDir())

			if got := FindAuthFileForRegion(RegionCN); got != "" {
				t.Fatalf("零命中应返回空: got %q", got)
			}
			if p := NewAccountPool(RegionCN); p != nil {
				t.Fatalf("零命中建池应返回 nil, got %d 个号", p.Size())
			}
			sum := NewCredential("", RegionCN).Summary()
			errMsg, ok := sum["error"].(string)
			if !ok || errMsg == "" {
				t.Fatalf("零命中 Summary 应带 error: %+v", sum)
			}
			if !strings.Contains(errMsg, dir) {
				t.Fatalf("error 应列出搜过的目录 %q: %q", dir, errMsg)
			}
			if _, err := NewServerForRegion(RegionCN, false); err == nil || !strings.Contains(err.Error(), dir) {
				t.Fatalf("NewServer 错误应列出搜过的目录 %q: %v", dir, err)
			}
			// health 同构透出：status=error + message 带目录 + auth_search_dirs 带目录
			s := &Server{cred: NewCredential("", RegionCN), region: RegionCN, stats: map[string]*modelStat{}}
			rr := httptest.NewRecorder()
			s.handleHealth(rr, httptest.NewRequest("GET", "/health", nil))
			var body map[string]any
			if err := json.Unmarshal(rr.Body.Bytes(), &body); err != nil {
				t.Fatal(err)
			}
			if body["status"] != "error" {
				t.Fatalf("零命中 health 应为 error: %s", rr.Body.String())
			}
			if msg, _ := body["message"].(string); !strings.Contains(msg, dir) {
				t.Fatalf("health message 应列出搜过的目录 %q: %q", dir, msg)
			}
			dirs, _ := body["auth_search_dirs"].([]any)
			if len(dirs) != 1 || dirs[0] != dir {
				t.Fatalf("health auth_search_dirs 应为 [%q]: %v", dir, dirs)
			}
			// 健康原文进测试日志：reviewer 无 shell，凭此行核对三字段。
			t.Logf("missing-dir health body: %s", rr.Body.String())
		})
	}
}

// 有登录态时 health 同构透出 auth_search_dirs（成功路径不断言凭据值，只认结构）。
func TestAuthHealthListsSearchDirsOnSuccess(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	t.Setenv("CODEBUDDY_POOL_DIR", t.TempDir())
	writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "甲", time.Now())

	s, err := NewServerForRegion(RegionCN, false)
	if err != nil {
		t.Fatalf("有登录文件应建服务成功: %v", err)
	}
	rr := httptest.NewRecorder()
	s.handleHealth(rr, httptest.NewRequest("GET", "/health", nil))
	var body map[string]any
	if err := json.Unmarshal(rr.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if body["status"] != "ok" {
		t.Fatalf("有登录态 health 应为 ok: %s", rr.Body.String())
	}
	dirs, _ := body["auth_search_dirs"].([]any)
	if len(dirs) != 1 || dirs[0] != dir {
		t.Fatalf("health auth_search_dirs 应为 [%q]: %v", dir, dirs)
	}
	if _, ok := body["credential"]; !ok {
		t.Fatalf("有登录态 health 应带 credential: %s", rr.Body.String())
	}
}

// 全加密新文件 + 旧明文文件并存：Find 回退到最后可用（旧文件），池只收旧号；
// 显式拿加密文件取头被兜底拦（错误带文件名、不带全路径、不带 token）。
func TestAuthEncryptedTokenPreferUsable(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	t.Setenv("CODEBUDDY_POOL_DIR", t.TempDir())
	base := time.Now()
	oldPath := writeAuthInfo(t, dir, "old.info", "www.codebuddy.cn", "uid-old", "老昵称", base.Add(-2*time.Hour))
	encPath := writeAuthInfoEncrypted(t, dir, "new.info", "www.codebuddy.cn", "uid-new", base)

	// 解析本身不炸（uid 可读），但 token 不可用
	if domain, uid, _, ok := parseAuthInfo(encPath); !ok || uid != "uid-new" || domain != "www.codebuddy.cn" {
		t.Fatalf("加密文件应可解析出 domain/uid: ok=%v uid=%q domain=%q", ok, uid, domain)
	}
	if authTokenUsable(encPath) || !authTokenUsable(oldPath) {
		t.Fatal("加密文件不可用、旧明文文件可用")
	}
	if got := FindAuthFileForRegion(RegionCN); got != oldPath {
		t.Fatalf("应回退到最后可用文件: got %q want %q", got, oldPath)
	}
	p := NewAccountPool(RegionCN)
	if p == nil || p.Size() != 1 {
		t.Fatalf("池应只收旧号: got %v", p)
	}
	if a := p.Pick(); a == nil || a.UID != "uid-old" {
		t.Fatalf("Pick 应出旧号: got %v", a)
	}
	// 兜底：显式加密路径取头必须失败（防"Bearer 空串"出站）
	c := NewCredential(encPath, RegionCN)
	if _, err := c.Headers(context.Background()); err == nil || !strings.Contains(err.Error(), "加密") {
		t.Fatalf("加密文件取头应报加密错误: %v", err)
	}
	if _, err := c.Headers(context.Background()); err != nil && strings.Contains(err.Error(), encPath) {
		t.Fatalf("错误不得带全路径: %v", err)
	}
}

// 目录里只剩全加密文件：Find 空、建池 nil、Summary/NewServer/health 如实报
// "不是没文件，是解密不了"（带"加密"字样 + 搜过的目录）。
func TestAuthAllEncryptedHonestError(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	t.Setenv("CODEBUDDY_POOL_DIR", t.TempDir())
	writeAuthInfoEncrypted(t, dir, "new.info", "www.codebuddy.cn", "uid-new", time.Now())

	if got := FindAuthFileForRegion(RegionCN); got != "" {
		t.Fatalf("全加密应返回空: got %q", got)
	}
	if p := NewAccountPool(RegionCN); p != nil {
		t.Fatalf("全加密建池应返回 nil, got %d 个号", p.Size())
	}
	sum := NewCredential("", RegionCN).Summary()
	errMsg, ok := sum["error"].(string)
	if !ok || !strings.Contains(errMsg, "加密") || !strings.Contains(errMsg, dir) {
		t.Fatalf("Summary 应报加密+目录: %+v", sum)
	}
	if _, err := NewServerForRegion(RegionCN, false); err == nil ||
		!strings.Contains(err.Error(), "加密") || !strings.Contains(err.Error(), dir) {
		t.Fatalf("NewServer 错误应报加密+目录: %v", err)
	}
	s := &Server{cred: NewCredential("", RegionCN), region: RegionCN, stats: map[string]*modelStat{}}
	rr := httptest.NewRecorder()
	s.handleHealth(rr, httptest.NewRequest("GET", "/health", nil))
	var body map[string]any
	if err := json.Unmarshal(rr.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if body["status"] != "error" {
		t.Fatalf("全加密 health 应为 error: %s", rr.Body.String())
	}
	if msg, _ := body["message"].(string); !strings.Contains(msg, "加密") {
		t.Fatalf("health message 应报加密: %q", msg)
	}
}
