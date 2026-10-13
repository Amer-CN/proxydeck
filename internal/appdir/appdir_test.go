package appdir

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// clearEnv 清掉会干扰解析的环境变量（测试机可能带着它们跑）。
func clearEnv(t *testing.T) {
	t.Helper()
	for _, k := range []string{DataDirEnv, "APPDATA"} {
		old, had := os.LookupEnv(k)
		_ = os.Unsetenv(k)
		t.Cleanup(func() {
			if had {
				_ = os.Setenv(k, old)
				return
			}
			_ = os.Unsetenv(k)
		})
	}
}

// 默认（非便携、无覆盖）：数据目录 = %APPDATA%\ProxyDeck。
func TestDataDirDefaultsToAppData(t *testing.T) {
	clearEnv(t)
	appData := t.TempDir()
	t.Setenv("APPDATA", appData)
	Init(false)
	if Portable() {
		t.Fatal("无 portable 标记、未传 --portable 时不该是便携模式")
	}
	want := filepath.Join(appData, AppName)
	if got := DataDir(); got != want {
		t.Fatalf("数据目录应为 %s，实得 %s", want, got)
	}
	if !dirExists(want) {
		t.Fatalf("DataDir 应顺带把目录建出来：%s 不存在", want)
	}
}

// 便携模式：数据目录回到 exe 同目录（U 盘场景与升级前一致）。
func TestDataDirPortableFlag(t *testing.T) {
	clearEnv(t)
	t.Setenv("APPDATA", t.TempDir())
	Init(true)
	t.Cleanup(func() { Init(false) })
	if !Portable() {
		t.Fatal("--portable 应进入便携模式")
	}
	if got, want := DataDir(), ExeDir(); !strings.EqualFold(got, want) {
		t.Fatalf("便携模式数据目录应为 exe 同目录 %s，实得 %s", want, got)
	}
	if args := PortableArgs(); len(args) != 1 || args[0] != "--portable" {
		t.Fatalf("便携模式应给子进程带 --portable，实得 %v", args)
	}
}

// 非便携模式的子进程参数为空（子进程自行按 exe 同目录判定）。
func TestPortableArgsEmptyWhenNotPortable(t *testing.T) {
	clearEnv(t)
	t.Setenv("APPDATA", t.TempDir())
	Init(false)
	if args := PortableArgs(); len(args) != 0 {
		t.Fatalf("非便携模式不该带 --portable，实得 %v", args)
	}
}

// PROXYDECK_DATA_DIR 覆盖（测试/临时实例用），并优先于便携模式。
func TestDataDirEnvOverride(t *testing.T) {
	clearEnv(t)
	t.Setenv("APPDATA", t.TempDir())
	dir := t.TempDir()
	t.Setenv(DataDirEnv, dir)
	Init(true) // 覆盖优先于便携
	t.Cleanup(func() { Init(false) })
	if got := DataDir(); got != dir {
		t.Fatalf("环境变量覆盖应生效，实得 %s", got)
	}
	if p := Path("x.json"); p != filepath.Join(dir, "x.json") {
		t.Fatalf("Path 应拼在数据目录下，实得 %s", p)
	}
}

// Describe 要能写清「区域 + 候选路径」（报告第六节第 4 条的错误文案要求）。
func TestDescribeMentionsDirAndCandidates(t *testing.T) {
	clearEnv(t)
	t.Setenv("APPDATA", t.TempDir())
	Init(false)
	got := Describe(`C:\a\b`, `D:\c`)
	if !strings.Contains(got, DataDir()) || !strings.Contains(got, "用户数据目录") {
		t.Fatalf("Describe 应含数据目录与模式，实得 %s", got)
	}
	if !strings.Contains(got, `C:\a\b`) || !strings.Contains(got, `D:\c`) {
		t.Fatalf("Describe 应列出候选路径，实得 %s", got)
	}
}

func TestDataDirFallsBackToUserHome(t *testing.T) {
	clearEnv(t)
	home := t.TempDir()
	t.Setenv("USERPROFILE", home)
	t.Setenv("HOMEDRIVE", "")
	t.Setenv("HOMEPATH", "")
	Init(false)
	got := DataDir()
	if !strings.Contains(got, AppName) {
		t.Fatalf("无 APPDATA 时应退回用户主目录下的数据目录，实得 %s", got)
	}
}

func dirExists(p string) bool {
	fi, err := os.Stat(p)
	return err == nil && fi.IsDir()
}
