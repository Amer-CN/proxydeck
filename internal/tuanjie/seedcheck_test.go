package tuanjie

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
)

// TestSeedCheck 种子卫兵表驱动单测：临时目录假 bundle 文件，不打网络。
// 覆盖：①特征齐全→ok；②种子消失→告警；③头名消失→告警；④bundle 不存在→不告警 cli_not_found。
func TestSeedCheck(t *testing.T) {
	seed := codelySigningSeedHex
	header := seedFeatureHeaderName
	otherHex := strings.Repeat("ab", 32) // 等长异值 hex，模拟官方轮换种子

	fakeOK := "var cfg={signingSeedHex:\"" + seed + "\"};req.headers[\"" + header + "\"]=sig;"
	fakeSeedGone := strings.ReplaceAll(fakeOK, seed, otherHex)
	fakeHeaderGone := strings.ReplaceAll(fakeOK, header, "X-New-Signature")

	dir := t.TempDir()
	writeBundle := func(name, content string) string {
		t.Helper()
		p := filepath.Join(dir, name)
		if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
			t.Fatalf("写假 bundle 失败: %v", err)
		}
		return p
	}

	cases := []struct {
		name        string
		path        string // 要扫的 bundle 路径（不存在即模拟未装 CLI）
		wantScanned bool
		wantSeed    bool
		wantHeader  bool
		wantActive  bool
		wantSignal  string
	}{
		{name: "特征齐全→ok", path: writeBundle("ok.js", fakeOK), wantScanned: true, wantSeed: true, wantHeader: true, wantActive: false, wantSignal: "ok"},
		{name: "种子消失→告警", path: writeBundle("seed_gone.js", fakeSeedGone), wantScanned: true, wantSeed: false, wantHeader: true, wantActive: true},
		{name: "头名消失→告警", path: writeBundle("header_gone.js", fakeHeaderGone), wantScanned: true, wantSeed: true, wantHeader: false, wantActive: true},
		{name: "bundle不存在→cli_not_found", path: filepath.Join(dir, "missing.js"), wantScanned: false, wantSeed: false, wantHeader: false, wantActive: false, wantSignal: "cli_not_found"},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			scanned, seedFound, headerFound := scanSeedBundle(tc.path)
			if scanned != tc.wantScanned {
				t.Fatalf("scanned = %v，要 %v", scanned, tc.wantScanned)
			}
			if seedFound != tc.wantSeed || headerFound != tc.wantHeader {
				t.Fatalf("特征命中 = (%v,%v)，要 (%v,%v)", seedFound, headerFound, tc.wantSeed, tc.wantHeader)
			}
			active, signal := evalSeedScan(scanned, seedFound, headerFound)
			if active != tc.wantActive {
				t.Fatalf("active = %v，要 %v（signal=%q）", active, tc.wantActive, signal)
			}
			if tc.wantSignal != "" && signal != tc.wantSignal {
				t.Fatalf("signal = %q，要 %q", signal, tc.wantSignal)
			}
		})
	}
}

// buildSeedTgz 在内存里构造一个最小 npm 包 tgz：package/bundle/gemini.js
// （内容为 gemini）外加一个无关文件（练「其余文件跳过不解」路径）。
func buildSeedTgz(t *testing.T, gemini string) []byte {
	t.Helper()
	var buf bytes.Buffer
	gw := gzip.NewWriter(&buf)
	tw := tar.NewWriter(gw)
	add := func(name, content string) {
		t.Helper()
		if err := tw.WriteHeader(&tar.Header{Name: name, Mode: 0o644, Size: int64(len(content))}); err != nil {
			t.Fatalf("写 tar 头失败: %v", err)
		}
		if _, err := tw.Write([]byte(content)); err != nil {
			t.Fatalf("写 tar 体失败: %v", err)
		}
	}
	add("package/README.md", "not the bundle")
	add("package/bundle/gemini.js", gemini)
	if err := tw.Close(); err != nil {
		t.Fatalf("收 tar 失败: %v", err)
	}
	if err := gw.Close(); err != nil {
		t.Fatalf("收 gzip 失败: %v", err)
	}
	return buf.Bytes()
}

// newFakeRegistry 起 httptest 假 registry：packument 返回 *latestP 指向的
// dist-tags.latest，单版本文档返回指向本服务的 dist.tarball，tarball 路由计数
// 命中次数。测试全程不碰真实网络、不碰真实 npm。
func newFakeRegistry(t *testing.T, latestP *string, tgz []byte) (*httptest.Server, *atomic.Int64) {
	t.Helper()
	var hits atomic.Int64
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		latest := *latestP
		switch r.URL.Path {
		case "/@unity-china/codely-cli":
			_, _ = w.Write([]byte(`{"name":"@unity-china/codely-cli","dist-tags":{"latest":"` + latest + `"}}`))
		case "/@unity-china/codely-cli/" + latest:
			_, _ = w.Write([]byte(`{"dist":{"tarball":"http://` + r.Host + `/tarball/` + latest + `.tgz"}}`))
		case "/tarball/" + latest + ".tgz":
			hits.Add(1)
			_, _ = w.Write(tgz)
		default:
			http.NotFound(w, r)
		}
	}))
	t.Cleanup(ts.Close)
	return ts, &hits
}

// assertSeedOnlineState 校验在线层状态字段（持锁快照）。
func assertSeedOnlineState(t *testing.T, s *Server, wantLatest, wantOnline string, wantActive bool) {
	t.Helper()
	s.seedAlert.mu.Lock()
	defer s.seedAlert.mu.Unlock()
	if s.seedAlert.latest != wantLatest {
		t.Fatalf("seed_latest = %q，要 %q", s.seedAlert.latest, wantLatest)
	}
	if s.seedAlert.online != wantOnline {
		t.Fatalf("seed_online = %q，要 %q", s.seedAlert.online, wantOnline)
	}
	if s.seedAlert.onlineActive != wantActive {
		t.Fatalf("在线层告警 = %v，要 %v（signal=%q）", s.seedAlert.onlineActive, wantActive, s.seedAlert.onlineSignal)
	}
}

// TestSeedCheckRegistry 在线层用例：①latest 变了且新包含两特征→ok 且版本更新；
// ②latest 未变→不下载（tarball 计数 0）；③latest 变了且新包缺种子→告警；
// ④registry 不可达→registry_error 不告警；收尾断言临时目录删净。
func TestSeedCheckRegistry(t *testing.T) {
	okBundle := `var cfg={signingSeedHex:"` + codelySigningSeedHex + `"};req.headers["` + seedFeatureHeaderName + `"]=sig;`
	seedGone := strings.ReplaceAll(okBundle, codelySigningSeedHex, strings.Repeat("cd", 32))
	packumentPath := "/@unity-china/codely-cli"

	t.Run("latest变了且新包含两特征→ok且版本更新", func(t *testing.T) {
		latest := "rc.58"
		ts, hits := newFakeRegistry(t, &latest, buildSeedTgz(t, okBundle))
		s := &Server{}
		base := ts.URL + packumentPath

		active, _ := s.checkSeedRegistry(context.Background(), base) // 首验（内存空→需首验）
		if active {
			t.Fatalf("首验应 ok，却告警：signal=%q", s.seedAlert.onlineSignal)
		}
		assertSeedOnlineState(t, s, "rc.58", "ok", false)
		if s.seedAlert.checkedVersion != "rc.58" {
			t.Fatalf("已核版本 = %q，要 rc.58", s.seedAlert.checkedVersion)
		}

		latest = "rc.59" // 官方发新版（仍包含两特征）
		active, signal := s.checkSeedRegistry(context.Background(), base)
		if active {
			t.Fatalf("新包特征齐全应 ok，却告警：signal=%q", signal)
		}
		assertSeedOnlineState(t, s, "rc.59", "ok", false)
		if s.seedAlert.checkedVersion != "rc.59" {
			t.Fatalf("已核版本 = %q，要 rc.59", s.seedAlert.checkedVersion)
		}
		if n := hits.Load(); n != 2 {
			t.Fatalf("tarball 下载数 = %d，要 2（两个版本各一次）", n)
		}
	})

	t.Run("latest未变→不下载", func(t *testing.T) {
		latest := "rc.59"
		ts, hits := newFakeRegistry(t, &latest, buildSeedTgz(t, okBundle))
		s := &Server{}
		base := ts.URL + packumentPath
		s.checkSeedRegistry(context.Background(), base) // 首验建立基线
		before := hits.Load()
		active, _ := s.checkSeedRegistry(context.Background(), base)
		if active {
			t.Fatalf("不应告警，signal=%q", s.seedAlert.onlineSignal)
		}
		if n := hits.Load() - before; n != 0 {
			t.Fatalf("版本未变却又请求了 %d 次 tarball，要 0", n)
		}
		assertSeedOnlineState(t, s, "rc.59", "ok", false)
	})

	t.Run("latest变了且新包缺种子→告警", func(t *testing.T) {
		latest := "rc.59"
		ts, hits := newFakeRegistry(t, &latest, buildSeedTgz(t, seedGone))
		s := &Server{}
		base := ts.URL + packumentPath

		active, signal := s.checkSeedRegistry(context.Background(), base)
		if !active {
			t.Fatalf("新包缺种子应置告警，signal=%q", signal)
		}
		if !strings.Contains(signal, "rc.59") || !strings.Contains(signal, "签名种子") {
			t.Fatalf("signal=%q，要带版本号 rc.59 与缺失特征说明", signal)
		}
		assertSeedOnlineState(t, s, "rc.59", "seed_missing", true)

		// 同版本告警期间不重复下载（禁每轮无条件拉 16MB tgz）
		before := hits.Load()
		active2, _ := s.checkSeedRegistry(context.Background(), base)
		if !active2 || hits.Load() != before {
			t.Fatalf("同版本第二轮应沿用告警且不再下载（active=%v，新增下载=%d）", active2, hits.Load()-before)
		}
	})

	t.Run("registry不可达→registry_error不告警", func(t *testing.T) {
		ts, hits := newFakeRegistry(t, new(string), buildSeedTgz(t, okBundle))
		ts.Close() // 立即关闭 → 连接拒绝
		s := &Server{}
		active, signal := s.checkSeedRegistry(context.Background(), ts.URL+packumentPath)
		if active {
			t.Fatalf("registry 失败不算告警，却 active=true signal=%q", signal)
		}
		if n := hits.Load(); n != 0 {
			t.Fatalf("不应有 tarball 请求，实得 %d", n)
		}
		assertSeedOnlineState(t, s, "", "registry_error", false)
	})

	t.Run("registry_error后恢复同版本→online回到ok", func(t *testing.T) {
		latest := "rc.59"
		ts, hits := newFakeRegistry(t, &latest, buildSeedTgz(t, okBundle))
		s := &Server{}
		base := ts.URL + packumentPath
		s.checkSeedRegistry(context.Background(), base) // 首验建立基线（ok）

		dead := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
		deadURL := dead.URL
		dead.Close() // 立即关闭 → 该轮 registry 不可达
		active, _ := s.checkSeedRegistry(context.Background(), deadURL+packumentPath)
		if active {
			t.Fatalf("registry 失败不算告警，却 active=true")
		}
		assertSeedOnlineState(t, s, "", "registry_error", false)

		// 回归：registry 恢复且版本未变（早退分支）→ online 应回到 ok，
		// 不许永久卡在 registry_error；且不重复下载
		before := hits.Load()
		active, _ = s.checkSeedRegistry(context.Background(), base)
		if active {
			t.Fatalf("版本未变不应告警，signal=%q", s.seedAlert.onlineSignal)
		}
		assertSeedOnlineState(t, s, "rc.59", "ok", false)
		if n := hits.Load() - before; n != 0 {
			t.Fatalf("恢复轮不应重新下载 tarball，实增 %d", n)
		}
	})

	// 全部用例走完，临时目录应删净
	left, err := filepath.Glob(filepath.Join(os.TempDir(), "seedguard-*"))
	if err != nil {
		t.Fatalf("glob 临时目录失败: %v", err)
	}
	if len(left) > 0 {
		t.Fatalf("临时目录未删净: %v", left)
	}
}

// TestSeedCheckCombine 两层合成（纯函数）：任一层特征消失即告警；在线层告警
// 优先；registry_error（在线层无新结论）不改变本机层结论、不算告警。
func TestSeedCheckCombine(t *testing.T) {
	cases := []struct {
		name    string
		localA  bool
		localS  string
		onlineA bool
		onlineS string
		wantA   bool
		wantS   string
	}{
		{name: "两层正常→本机ok", localA: false, localS: "ok", onlineA: false, wantA: false, wantS: "ok"},
		{name: "本机告警在线正常→本机信号", localA: true, localS: "签名种子未命中（官方可能已轮换种子）", onlineA: false, wantA: true, wantS: "签名种子未命中（官方可能已轮换种子）"},
		{name: "在线告警优先", localA: false, localS: "ok", onlineA: true, onlineS: "rc.59 签名种子未命中（官方可能已轮换种子）", wantA: true, wantS: "rc.59 签名种子未命中（官方可能已轮换种子）"},
		{name: "registry_error不改变结论", localA: false, localS: "cli_not_found", onlineA: false, wantA: false, wantS: "cli_not_found"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			active, signal := combineSeedAlerts(tc.localA, tc.localS, tc.onlineA, tc.onlineS)
			if active != tc.wantA || signal != tc.wantS {
				t.Fatalf("combine = (%v,%q)，要 (%v,%q)", active, signal, tc.wantA, tc.wantS)
			}
		})
	}
}
