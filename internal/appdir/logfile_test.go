package appdir

import (
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

// 轮转：超过 1 MB 时旧文件滚成 .1，内容从新文件开始，且总量有界（1 MB × 4 上限）。
func TestAppendLogRotates(t *testing.T) {
	p := filepath.Join(t.TempDir(), "tuanjie-plugin-error.log")
	big := strings.Repeat("x", int(LogMaxBytes)) // 一次写满上限，下一次写入前必触发轮转
	if err := AppendLog(p, big); err != nil {
		t.Fatalf("AppendLog: %v", err)
	}
	if _, err := os.Stat(p + ".1"); !os.IsNotExist(err) {
		t.Fatalf("未超限就不该轮转")
	}
	if err := AppendLog(p, "第二次"); err != nil {
		t.Fatalf("AppendLog: %v", err)
	}
	if _, err := os.Stat(p + ".1"); err != nil {
		t.Fatalf("超限后应轮转出 .1: %v", err)
	}
	cur, err := os.ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(cur), "第二次") {
		t.Fatalf("轮转后新文件应只含本次写入: %q", firstBytes(cur, 60))
	}
	// 时间戳前缀（报告第六节第 4 条）
	if len(cur) < 20 || cur[4] != '-' || cur[7] != '-' {
		t.Fatalf("日志行应带时间戳前缀: %q", firstBytes(cur, 40))
	}
}

// 轮转保留 3 份：连写 5 次满额内容后，.1/.2/.3 在、.4 不存在（总量 1 MB × 4）。
func TestAppendLogKeepsThreeBackups(t *testing.T) {
	p := filepath.Join(t.TempDir(), "x-plugin-error.log")
	big := strings.Repeat("y", int(LogMaxBytes))
	for i := 0; i < 5; i++ {
		if err := AppendLog(p, big); err != nil {
			t.Fatalf("第 %d 次 AppendLog: %v", i, err)
		}
	}
	for i := 1; i <= LogBackups; i++ {
		if _, err := os.Stat(p + "." + strconv.Itoa(i)); err != nil {
			t.Fatalf("应保留 .%d: %v", i, err)
		}
	}
	if _, err := os.Stat(p + ".4"); !os.IsNotExist(err) {
		t.Fatalf("最多保留 %d 份备份，.4 不该存在", LogBackups)
	}
}

// OpenLog 追加写（不截断既有内容）。
func TestOpenLogAppends(t *testing.T) {
	p := filepath.Join(t.TempDir(), "sub", "codebuddy.log")
	f := OpenLog(p)
	if f == nil {
		t.Fatal("OpenLog 应建出目录与文件")
	}
	_, _ = f.WriteString("第一行\n")
	_ = f.Close()
	f2 := OpenLog(p)
	if f2 == nil {
		t.Fatal("OpenLog 二次打开失败")
	}
	_, _ = f2.WriteString("第二行\n")
	_ = f2.Close()
	b, err := os.ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(b), "第一行") || !strings.Contains(string(b), "第二行") {
		t.Fatalf("追加写不该截断历史内容: %q", b)
	}
}

// StampedWriter：每行加时间戳前缀（子进程 stdout/stderr 落盘用）。
func TestStampedWriterPrefixesLines(t *testing.T) {
	p := filepath.Join(t.TempDir(), "plugins.log")
	f := OpenLog(p)
	if f == nil {
		t.Fatal("OpenLog 失败")
	}
	w := NewStampedWriter(f)
	if _, err := w.Write([]byte("hello\nworld\n")); err != nil {
		t.Fatalf("Write: %v", err)
	}
	_ = f.Close()
	b, err := os.ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimRight(string(b), "\n"), "\n")
	if len(lines) != 2 {
		t.Fatalf("应写两行，实得 %d: %q", len(lines), b)
	}
	for i, l := range lines {
		if !strings.Contains(l, "hello") && !strings.Contains(l, "world") {
			t.Fatalf("第 %d 行内容不对: %q", i, l)
		}
		if len(l) < 20 || l[4] != '-' {
			t.Fatalf("第 %d 行应带时间戳前缀: %q", i, l)
		}
	}
}

// 无文件（f==nil）时写入静默丢弃，不 panic——日志写失败不能变成启动失败。
func TestStampedWriterNilSafe(t *testing.T) {
	var w *StampedWriter
	if n, err := w.Write([]byte("x\n")); err != nil || n != 2 {
		t.Fatalf("nil 写入应为无操作: n=%d err=%v", n, err)
	}
	if w := NewStampedWriter(nil); func() bool { _, err := w.Write([]byte("x\n")); return err != nil }() {
		t.Fatal("nil 文件写入不该报错")
	}
}
