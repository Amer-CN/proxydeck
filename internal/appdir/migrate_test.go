package appdir

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// 迁移主路径：旧文件搬进数据目录，原位置不留残渣，内容逐字节一致。
func TestMigrateMovesLegacyFiles(t *testing.T) {
	src := t.TempDir()
	dst := t.TempDir()
	content := `{"accounts":[{"user_id":"u1","access_token":"tok-abc","enabled":true}]}`
	if err := os.WriteFile(filepath.Join(src, "tuanjie-accounts.json"), []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(src, "keymap.json"), []byte(`[["cmd"]]`), 0o600); err != nil {
		t.Fatal(err)
	}
	// 不相干的文件（程序侧）不该被动
	if err := os.WriteFile(filepath.Join(src, "ProxyDeck.exe"), []byte("exe"), 0o600); err != nil {
		t.Fatal(err)
	}

	notes := Migrate(dst, src)

	if _, err := os.Stat(filepath.Join(src, "tuanjie-accounts.json")); !os.IsNotExist(err) {
		t.Fatalf("旧位置不该还有 tuanjie-accounts.json（err=%v）", err)
	}
	got, err := os.ReadFile(filepath.Join(dst, "tuanjie-accounts.json"))
	if err != nil {
		t.Fatalf("数据目录里应有搬过来的账号文件: %v", err)
	}
	if string(got) != content {
		t.Fatalf("搬运后内容不一致:\n实得 %q\n期望 %q", got, content)
	}
	if b, err := os.ReadFile(filepath.Join(dst, "keymap.json")); err != nil || string(b) != `[["cmd"]]` {
		t.Fatalf("keymap 应一并搬走: %q err=%v", b, err)
	}
	if _, err := os.Stat(filepath.Join(src, "ProxyDeck.exe")); err != nil {
		t.Fatalf("程序本体不该被搬动: %v", err)
	}
	// 无临时残留
	if _, err := os.Stat(filepath.Join(dst, "tuanjie-accounts.json.migrating")); !os.IsNotExist(err) {
		t.Fatalf("不该留下 .migrating 临时文件")
	}
	if len(notes) == 0 || !strings.Contains(strings.Join(notes, "；"), "已搬入数据目录") {
		t.Fatalf("应回报搬迁结果，实得 %v", notes)
	}
}

// 目标已存在同名文件：不动目标（用户目录里的才是权威），旧文件改名 *.migrated。
func TestMigrateKeepsExistingTargetAndRetiresLegacy(t *testing.T) {
	src := t.TempDir()
	dst := t.TempDir()
	if err := os.WriteFile(filepath.Join(src, "stats.json"), []byte(`{"old":1}`), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dst, "stats.json"), []byte(`{"new":2}`), 0o600); err != nil {
		t.Fatal(err)
	}

	Migrate(dst, src)

	got, err := os.ReadFile(filepath.Join(dst, "stats.json"))
	if err != nil || string(got) != `{"new":2}` {
		t.Fatalf("目标文件必须保持原样: %q err=%v", got, err)
	}
	if _, err := os.Stat(filepath.Join(src, "stats.json")); !os.IsNotExist(err) {
		t.Fatalf("旧文件应已被改名/清除，不该原样留在 exe 同目录")
	}
	if _, err := os.Stat(filepath.Join(src, "stats.json.migrated")); err != nil {
		t.Fatalf("旧文件应改名为 stats.json.migrated: %v", err)
	}
}

// 便携模式或同目录调用：什么都不做（不搬、不报错）。
func TestMigrateSkipsSameDir(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "keymap.json")
	if err := os.WriteFile(p, []byte(`[["tj"]]`), 0o600); err != nil {
		t.Fatal(err)
	}
	if notes := Migrate(dir, dir); len(notes) != 0 {
		t.Fatalf("同目录不该有动作，实得 %v", notes)
	}
	if _, err := os.Stat(p); err != nil {
		t.Fatalf("同目录调用不得改动文件: %v", err)
	}
}

// 旧位置没有文件：静默返回（常态，不写日志）。
func TestMigrateNoLegacyFiles(t *testing.T) {
	notes := Migrate(t.TempDir(), t.TempDir())
	if len(notes) != 0 {
		t.Fatalf("无历史文件时应静默，实得 %v", notes)
	}
}

// 明文凭据升级：数据目录里旧的明文账号文件被换成 DPAPI 密文，且回读内容不变。
func TestUpgradePlaintextEncryptsCredentialFiles(t *testing.T) {
	dir := t.TempDir()
	t.Setenv(DataDirEnv, dir)
	Init(false)

	plain := []byte(`{"accounts":[{"user_id":"u1","access_token":"tok-secret","enabled":true}]}`)
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-accounts.json"), plain, 0o600); err != nil {
		t.Fatal(err)
	}
	// 非凭据文件不该被动
	if err := os.WriteFile(filepath.Join(dir, "stats.json"), []byte(`{"x":1}`), 0o600); err != nil {
		t.Fatal(err)
	}

	notes := UpgradePlaintext()
	if !strings.Contains(strings.Join(notes, "；"), "已升级为 DPAPI 密文") {
		t.Fatalf("应回报升级，实得 %v", notes)
	}
	raw, err := os.ReadFile(filepath.Join(dir, "tuanjie-accounts.json"))
	if err != nil {
		t.Fatal(err)
	}
	if !isProtected(raw) {
		t.Fatalf("落盘内容应为 DPAPI 密文（%q 前缀），实得 %q", cryptPrefix, firstBytes(raw, 16))
	}
	if strings.Contains(string(raw), "tok-secret") {
		t.Fatalf("密文里不该出现明文 token")
	}
	back, err := ReadFile(filepath.Join(dir, "tuanjie-accounts.json"))
	if err != nil {
		t.Fatalf("加密后应能读回: %v", err)
	}
	if string(back) != string(plain) {
		t.Fatalf("升级后内容不一致:\n实得 %q\n期望 %q", back, plain)
	}
	if b, _ := os.ReadFile(filepath.Join(dir, "stats.json")); string(b) != `{"x":1}` {
		t.Fatalf("非凭据文件不该被加密: %q", b)
	}
	// 二次调用幂等（已是密文就跳过）
	if notes := UpgradePlaintext(); strings.Contains(strings.Join(notes, "；"), "tuanjie-accounts.json") {
		t.Fatalf("已是密文不该重复升级，实得 %v", notes)
	}
}

func firstBytes(b []byte, n int) string {
	if len(b) < n {
		return string(b)
	}
	return string(b[:n])
}
