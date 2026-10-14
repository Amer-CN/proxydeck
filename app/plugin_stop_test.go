//go:build windows

package main

import (
	"net"
	"strconv"
	"strings"
	"testing"
	"time"
)

// 临时 def 注册（用完即删）：pluginStop 只认 pluginDefs 名单＋states 表。
func withTempDef(t *testing.T, a *app, id string, port int) {
	t.Helper()
	pluginDefs = append(pluginDefs, pluginDef{ID: id, Name: "TX-" + id, Port: port})
	a.plugins[id] = &pluginState{running: true}
	t.Cleanup(func() {
		pluginDefs = pluginDefs[:len(pluginDefs)-1]
		delete(a.plugins, id)
	})
}

// 杀得掉：端口空出，返回 nil，状态落 running=false。
func TestPluginStopSuccess(t *testing.T) {
	old := stopVerifyWait
	stopVerifyWait = 300 * time.Millisecond
	defer func() { stopVerifyWait = old }()
	a := &app{}
	a.pluginStates()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Skipf("起监听失败: %v", err)
	}
	port := ln.Addr().(*net.TCPAddr).Port
	_ = ln.Close() // 关掉：保证端口空闲（被抢的概率忽略不计）
	withTempDef(t, a, "test-stop-ok", port)
	if err := a.pluginStop("test-stop-ok"); err != nil {
		t.Fatalf("空闲端口停堆应成功，实际 %v", err)
	}
	st := a.plugins["test-stop-ok"]
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.running || st.cmd != nil || st.lastErr != "" {
		t.Errorf("成功后状态应为 idle 干净，实际 running=%v cmd=%v lastErr=%q",
			st.running, st.cmd != nil, st.lastErr)
	}
}

// 杀不掉：占位监听跑在测试进程自身（killByPort 自排除 pid，杀了也白杀），
// 必须如实报错且恢复 running 跟踪——此前这里回 nil，拉杆先假装关掉再弹回 running。
func TestPluginStopKillFailsHonest(t *testing.T) {
	old := stopVerifyWait
	stopVerifyWait = 300 * time.Millisecond
	defer func() { stopVerifyWait = old }()
	a := &app{}
	a.pluginStates()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Skipf("起监听失败: %v", err)
	}
	defer ln.Close() // 全程占住：模拟杀不掉的旧进程（自排除，他人无恙）
	port := ln.Addr().(*net.TCPAddr).Port
	withTempDef(t, a, "test-stop-fail", port)
	err = a.pluginStop("test-stop-fail")
	if err == nil {
		t.Fatalf("kill-fail path must return non-nil error")
	} else if !strings.Contains(err.Error(), "杀不掉") {
		t.Fatalf("error text must say the process could not be killed, got: %v", err)
	}
	if !strings.Contains(err.Error(), strconv.Itoa(port)) {
		t.Errorf("报错应带端口号，实际 %v", err)
	}
	st := a.plugins["test-stop-fail"]
	st.mu.Lock()
	defer st.mu.Unlock()
	if !st.running {
		t.Errorf("杀不掉时 running 必须恢复 true（它还活着），否则下次探活又演" +
			"先 idle 后弹回")
	}
}

// 未知 id 维持原行为。
func TestPluginStopUnknown(t *testing.T) {
	a := &app{}
	a.pluginStates()
	if err := a.pluginStop("no-such-plugin"); err == nil {
		t.Errorf("未知插件应报错")
	}
}
