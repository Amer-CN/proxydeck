package main

import (
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// 群友反馈「拉杆拉了显示启动中、根本起不来」：后端秒退时状态必须复位，
// 否则 pluginList 的 running 恒为 true，界面永远停在 starting。

func TestWatchPluginExitMarksFailure(t *testing.T) {
	d := pluginDef{ID: "watchtest-exit", Name: "WatchTest", Port: 1}
	st := &pluginState{}
	cmd := exec.Command("cmd", "/c", "exit 3")
	if err := cmd.Start(); err != nil {
		t.Fatalf("起测试子进程失败: %v", err)
	}
	st.running = true
	st.cmd = cmd

	a := &app{}
	a.watchPluginExit(st, cmd, d)

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		st.mu.Lock()
		running, lastErr, stillCmd := st.running, st.lastErr, st.cmd
		st.mu.Unlock()
		if !running {
			if stillCmd != nil {
				t.Fatalf("复位后 cmd 应为 nil")
			}
			if lastErr == "" {
				t.Fatalf("复位后必须带失败原因，否则界面无从提示")
			}
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("子进程已退出但 running 未复位——界面会永远停在「启动中」")
}

func TestWatchPluginExitIgnoresDeliberateStop(t *testing.T) {
	d := pluginDef{ID: "watchtest-stop", Name: "WatchTest", Port: 1}
	st := &pluginState{}
	cmd := exec.Command("cmd", "/c", "ping -n 30 127.0.0.1")
	if err := cmd.Start(); err != nil {
		t.Fatalf("起测试子进程失败: %v", err)
	}
	st.running = true
	st.cmd = cmd
	a := &app{}
	a.watchPluginExit(st, cmd, d)

	// 模拟 pluginStop：先把状态复位并摘掉 cmd，再杀进程
	st.mu.Lock()
	st.running = false
	st.cmd = nil
	st.mu.Unlock()
	_ = cmd.Process.Kill()
	time.Sleep(600 * time.Millisecond)

	st.mu.Lock()
	lastErr := st.lastErr
	st.mu.Unlock()
	if lastErr != "" {
		t.Fatalf("主动停堆不该写失败原因，实际: %q", lastErr)
	}
}

func TestPluginExitReasonPrefersDiagnosticFile(t *testing.T) {
	p := filepath.Join(t.TempDir(), "x-plugin-error.log")
	if err := os.WriteFile(p, []byte(" 借据失效：请在桌面端重新登录 \n"), 0o644); err != nil {
		t.Fatal(err)
	}
	got := pluginExitReason(p, errors.New("exit status 1"), pluginDef{Name: "X"})
	if !strings.Contains(got, "重新登录") {
		t.Fatalf("应优先用子进程诊断文件，实际: %q", got)
	}
	if !strings.HasPrefix(got, "启动失败：") {
		t.Fatalf("前缀应为「启动失败：」，实际: %q", got)
	}
}

func TestPluginExitReasonFallsBackToWaitError(t *testing.T) {
	got := pluginExitReason(filepath.Join(t.TempDir(), "missing.log"), errors.New("exit status 3"), pluginDef{Name: "X"})
	if !strings.Contains(got, "exit status 3") || !strings.Contains(got, "X") {
		t.Fatalf("无诊断文件时应回退出错误并带插件名，实际: %q", got)
	}
}
