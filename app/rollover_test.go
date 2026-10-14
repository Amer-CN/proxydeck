//go:build windows

package main

import (
	"errors"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
)

// 判定核心：只有"已知插件端口＋镜像命中 *.old*"才换血，其余一律不动。
func TestIsStaleImage(t *testing.T) {
	cases := []struct {
		name string
		exe  string
		want bool
	}{
		// 真雷：用户现场原样（群友那台的腾位残留名）
		{"unique-old", `F:\AIXM\proxydeck\ProxyDeck.exe.old-1791936628190504400`, true},
		{"bare-old", `D:\桌面\ProxyDeck.exe.old`, true},
		{"upper", `C:\x\PROXYDECK.EXE.OLD`, true},
		// 不能碰的
		{"current", `F:\AIXM\proxydeck\ProxyDeck.exe`, false},
		{"renamed-copy", `D:\桌面\ProxyDeck_2.exe`, false},
		{"downloading", `F:\AIXM\proxydeck\ProxyDeck.exe.new`, false},
		{"foreign", `C:\x\foo.old.exe`, false},
		{"empty", ``, false},
	}
	for _, c := range cases {
		if got := isStaleImage(c.exe); got != c.want {
			t.Errorf("%s: isStaleImage(%q) = %v, want %v", c.name, c.exe, got, c.want)
		}
	}
}

const fakeNetstat = `Active Connections

  Proto  Local Address          Foreign Address        State           PID
  TCP    127.0.0.1:8788         0.0.0.0:0              LISTENING       1111
  TCP    127.0.0.1:8788         0.0.0.0:0              LISTENING       1111
  TCP    0.0.0.0:8787           0.0.0.0:0              LISTENING       2222
  TCP    [::]:8789              0.0.0.0:0              LISTENING       3333
  TCP    127.0.0.1:55990        0.0.0.0:0              LISTENING       4444
  TCP    127.0.0.1:8788         0.0.0.0:0              TIME_WAIT       5555
  TCP    127.0.0.1:9999         0.0.0.0:0              LISTENING       abc
  UDP    127.0.0.1:8788         *:*                                    6666
`

func TestParseNetstatListeners(t *testing.T) {
	got := parseNetstatListeners(fakeNetstat)
	want := map[string][]int{"8788": {1111}, "8787": {2222}, "8789": {3333}, "55990": {4444}}
	if len(got) != len(want) {
		t.Fatalf("端口数 = %d, want %d (%v)", len(got), len(want), got)
	}
	for port, pids := range want {
		if len(got[port]) != len(pids) || got[port][0] != pids[0] {
			t.Errorf("port %s = %v, want %v", port, got[port], pids)
		}
	}
}

func TestClassifyHolders(t *testing.T) {
	defs := []pluginDef{
		{ID: "tuanjie", Name: "团结", Port: 8788},
		{ID: "codebuddy", Name: "WB", Port: 8787},
	}
	snap := map[string][]int{
		"8788":  {1111},       // 跑在旧包 → 换
		"8787":  {2222, 3333}, // 2222 是现包 → 跳过；3333 旧包 → 换
		"55990": {4444},       // 非插件端口 → 看都不看（即使旧包）
		"18788": {5555},       // 测试端口 → 同上
	}
	images := map[int]string{
		1111: `F:\AIXM\proxydeck\ProxyDeck.exe.old-1`,
		2222: `F:\AIXM\proxydeck\ProxyDeck.exe`, // == currentExe
		3333: `F:\AIXM\proxydeck\ProxyDeck.exe.old-2`,
		4444: `F:\AIXM\proxydeck\ProxyDeck.exe.old-3`,
		5555: `F:\AIXM\proxydeck\ProxyDeck.exe.old-4`,
	}
	imageOf := func(pid int) (string, bool) {
		exe, ok := images[pid]
		return exe, ok
	}
	current := `F:\AIXM\proxydeck\ProxyDeck.exe`
	got := classifyHolders(defs, snap, imageOf, current, 9999)
	if len(got) != 2 {
		t.Fatalf("换血名单 = %d 条, want 2 (%v)", len(got), got)
	}
	if got[0].def.ID != "tuanjie" || got[0].pid != 1111 {
		t.Errorf("第一条应为 tuanjie/1111，实际 %+v", got[0])
	}
	if got[1].def.ID != "codebuddy" || got[1].pid != 3333 {
		t.Errorf("第二条应为 codebuddy/3333（跳过现包 2222），实际 %+v", got[1])
	}

	// 未知镜像（读不出）→ 不动
	unknown := func(pid int) (string, bool) { return "", false }
	if got := classifyHolders(defs, snap, unknown, current, 9999); len(got) != 0 {
		t.Errorf("读不出镜像必须不动，实际 %+v", got)
	}
	// 自己 → 不动（哪怕监听着插件端口）
	selfSnap := map[string][]int{"8788": {9999}}
	selfImg := func(pid int) (string, bool) { return `F:\other\ProxyDeck.exe.old-9`, true }
	if got := classifyHolders(defs, selfSnap, selfImg, current, 9999); len(got) != 0 {
		t.Errorf("selfPid 必须排除，实际 %+v", got)
	}
}

func TestWaitBackendHealthy(t *testing.T) {
	n := 0
	ok := waitBackendHealthy(func() bool { n++; return n >= 3 }, 2*time.Second, 5*time.Millisecond)
	if !ok || n < 3 {
		t.Errorf("应轮询到绿，实际 ok=%v calls=%d", ok, n)
	}
	if waitBackendHealthy(func() bool { return false }, 50*time.Millisecond, 10*time.Millisecond) {
		t.Errorf("超时必须返回 false")
	}
}

// 换血顺序：stop→start→health 全绿才算成功；调用序列可审计。
func TestRollOneOrder(t *testing.T) {
	var calls []string
	tgt := staleTarget{def: pluginDef{ID: "tj", Name: "团结", Port: 0}, pid: 1111} // Port 0 永不被占：rollOne 自带杀后端口复验，真端口会受环境影响
	n := 0
	msg := rollOne(tgt,
		func(id string) error { calls = append(calls, "stop:"+id); return nil },
		func(id string) error { calls = append(calls, "start:"+id); return nil },
		func(d pluginDef) bool {
			calls = append(calls, "healthy:"+d.ID)
			n++
			return n >= 2
		},
		2*time.Second,
	)
	if msg != "" {
		t.Fatalf("应成功，实际 %q", msg)
	}
	seq := strings.Join(calls, " ")
	if !strings.HasPrefix(seq, "stop:tj start:tj healthy:tj") {
		t.Errorf("调用顺序应为 stop→start→healthy，实际 %q", seq)
	}
}

func TestRollOneStopFailsSkipsRest(t *testing.T) {
	var calls []string
	tgt := staleTarget{def: pluginDef{ID: "tj", Name: "团结", Port: 0}, pid: 1111} // Port 0 永不被占：rollOne 自带杀后端口复验，真端口会受环境影响
	msg := rollOne(tgt,
		func(id string) error { calls = append(calls, "stop"); return errors.New("杀不掉") },
		func(id string) error { calls = append(calls, "start"); return nil },
		func(d pluginDef) bool { calls = append(calls, "healthy"); return true },
		time.Second,
	)
	if msg == "" || len(calls) != 1 || calls[0] != "stop" {
		t.Errorf("stop 失败后 start/health 一律不得调用，实际 msg=%q calls=%v", msg, calls)
	}
}

func TestRollOneStartFailsSkipsHealth(t *testing.T) {
	var calls []string
	tgt := staleTarget{def: pluginDef{ID: "tj", Name: "团结", Port: 0}, pid: 1111} // Port 0 永不被占：rollOne 自带杀后端口复验，真端口会受环境影响
	msg := rollOne(tgt,
		func(id string) error { calls = append(calls, "stop"); return nil },
		func(id string) error { calls = append(calls, "start"); return errors.New("拉不起") },
		func(d pluginDef) bool { calls = append(calls, "healthy"); return true },
		time.Second,
	)
	if msg == "" || len(calls) != 2 {
		t.Errorf("start 失败后 health 不得轮询，实际 msg=%q calls=%v", msg, calls)
	}
}

func TestRollOneUnhealthyReports(t *testing.T) {
	calls := 0
	tgt := staleTarget{def: pluginDef{ID: "tj", Name: "团结", Port: 0}, pid: 1111} // Port 0 永不被占：rollOne 自带杀后端口复验，真端口会受环境影响
	msg := rollOne(tgt,
		func(id string) error { return nil },
		func(id string) error { return nil },
		func(d pluginDef) bool { calls++; return false },
		100*time.Millisecond,
	)
	if msg == "" || calls == 0 {
		t.Errorf("health 超时必须返回失败文案且确实轮询过，实际 msg=%q calls=%d", msg, calls)
	}
}

// 读自己进程镜像：hermetic，不碰任何端口与外部进程。
func TestImagePathOfPIDSelf(t *testing.T) {
	exe, err := imagePathOfPID(os.Getpid())
	if err != nil {
		t.Fatalf("读自己镜像失败: %v", err)
	}
	if exe == "" || !strings.HasSuffix(strings.ToLower(exe), ".exe") {
		t.Errorf("自己镜像路径异常: %q", exe)
	}
}

// 换血标记往返：写→读回版本→文件消失；缺席/损坏返回 false（损坏文件照删，不重复触发）。
func TestUpdateMarkerRoundTrip(t *testing.T) {
	p := filepath.Join(t.TempDir(), ".update-rollover.json")
	if _, ok := consumeUpdateMarkerFrom(p); ok {
		t.Errorf("缺席必须返回 false")
	}
	if err := writeUpdateMarkerTo(p, "v9.9.9", 12345); err != nil {
		t.Fatalf("写标记失败: %v", err)
	}
	to, ok := consumeUpdateMarkerFrom(p)
	if !ok || to != "v9.9.9" {
		t.Errorf("读回应为 v9.9.9/true，实际 %q/%v", to, ok)
	}
	if _, err := os.Stat(p); !os.IsNotExist(err) {
		t.Errorf("消费后文件必须消失")
	}
	if err := os.WriteFile(p, []byte("{坏掉"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, ok := consumeUpdateMarkerFrom(p); ok {
		t.Errorf("损坏必须返回 false")
	}
	if _, err := os.Stat(p); !os.IsNotExist(err) {
		t.Errorf("损坏文件也必须删掉，不重复触发")
	}
}

// waitPortFree：空端口即 true；被占端口在超时内 false。
func TestWaitPortFree(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Skipf("起监听失败: %v", err)
	}
	port := strconv.Itoa(ln.Addr().(*net.TCPAddr).Port)
	if waitPortFree(port, 500*time.Millisecond) {
		t.Errorf("被占端口在超时内必须返回 false")
	}
	_ = ln.Close()
	if !waitPortFree(port, 5*time.Second) {
		t.Errorf("空端口必须返回 true")
	}
}

// 杀后复验：stop 成功但端口仍被占 → 如实报错且不调 start（此前"弹回 running"的假成功根因）。
func TestRollOneKillUnverified(t *testing.T) {
	old := rollKillGrace
	rollKillGrace = 100 * time.Millisecond
	defer func() { rollKillGrace = old }()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Skipf("起监听失败: %v", err)
	}
	defer ln.Close()
	port := ln.Addr().(*net.TCPAddr).Port
	var calls []string
	tgt := staleTarget{def: pluginDef{ID: "tj", Name: "团结", Port: port}, pid: 1111}
	msg := rollOne(tgt,
		func(id string) error { calls = append(calls, "stop"); return nil },
		func(id string) error { calls = append(calls, "start"); return nil },
		func(d pluginDef) bool { calls = append(calls, "healthy"); return true },
		time.Second,
	)
	if msg == "" || !strings.Contains(msg, "杀不掉") {
		t.Errorf("应如实报杀不掉，实际 msg=%q calls=%v", msg, calls)
	}
	for _, c := range calls {
		if c == "start" || c == "healthy" {
			t.Errorf("杀不掉时 start/health 一律不得调用，calls=%v", calls)
		}
	}
}
