package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// 群友实测的失败场景（2026-10-14）：「替换失败(旧程序改名.old不成): Access is denied」。
// 根因是腾位名写死 ProxyDeck.exe.old——只要上一次更新留下的 .old 仍在被运行（旧窗口没关完、
// 或后端子进程还跑在它身上），os.Remove 删不掉、改名继续撞 Access denied，二次更新必挂。
// 现在腾位名唯一，必须在这种"裸 .old 被锁死"的环境里照样成功。
func TestReplaceRunningExeDodgesLockedLegacyOld(t *testing.T) {
	dir := t.TempDir()
	exe := filepath.Join(dir, "ProxyDeck.exe")
	newf := filepath.Join(dir, "ProxyDeck.exe.new")
	if err := os.WriteFile(exe, []byte("old-image"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(newf, []byte("new-image"), 0o755); err != nil {
		t.Fatal(err)
	}

	// 造一个被运行中进程占住的旧残骸：把系统 ping 复制成裸 .old 名并跑起来
	locked := filepath.Join(dir, "ProxyDeck.exe.old")
	src := filepath.Join(os.Getenv("SystemRoot"), "System32", "ping.exe")
	b, err := os.ReadFile(src)
	if err != nil {
		t.Skipf("跳过：读不到 ping.exe：%v", err)
	}
	if err := os.WriteFile(locked, b, 0o755); err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(locked, "-n", "30", "127.0.0.1")
	if err := cmd.Start(); err != nil {
		t.Fatalf("起占位进程失败: %v", err)
	}
	defer func() { _ = cmd.Process.Kill(); _, _ = cmd.Process.Wait() }()
	time.Sleep(600 * time.Millisecond)

	// 前提校验：这个残骸此刻必须真的删不掉，否则本测试没覆盖到目标场景
	if err := os.Remove(locked); err == nil {
		t.Fatalf("占位进程没能锁住 %s，测试前提不成立", locked)
	}
	// 前提校验 2：旧实现（写死裸 .old）在同一环境下必然改名失败——证明本用例确实能抓到回归
	if err := os.Rename(exe, locked); err == nil {
		t.Fatalf("写死名的腾位竟然成功了，说明锁没生效、场景未复现")
	}

	old, err := replaceRunningExe(exe, newf)
	if err != nil {
		t.Fatalf("腾位仍失败（群友场景未修复）: %v", err)
	}
	if strings.HasSuffix(old, "ProxyDeck.exe.old") {
		t.Fatalf("腾位名不应再写死成裸 .old：%s", old)
	}
	if _, err := os.Stat(old); err != nil {
		t.Fatalf("腾位后的旧文件不存在: %v", err)
	}
	if _, err := os.Stat(exe); err == nil {
		t.Fatalf("原路径应已被腾空，实际仍存在")
	}

	// 唯一性：连续两次腾位不得重名（重名正是「二次更新必挂」的成因）
	if err := os.WriteFile(exe, []byte("old2"), 0o755); err != nil {
		t.Fatal(err)
	}
	old2, err := replaceRunningExe(exe, newf)
	if err != nil {
		t.Fatalf("第二次腾位失败: %v", err)
	}
	if old2 == old {
		t.Fatalf("两次腾位名相同：%s", old)
	}
}
