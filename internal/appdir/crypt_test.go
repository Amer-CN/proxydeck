package appdir

import (
	"os"
	"path/filepath"
	"testing"
)

// 读兼容：旧明文文件原样读回（升级后行为不能变）。
func TestReadFilePlaintextCompat(t *testing.T) {
	p := filepath.Join(t.TempDir(), "tuanjie-accounts.json")
	plain := []byte(`{"accounts":[{"user_id":"u1","access_token":"tok-1","enabled":true}]}`)
	if err := os.WriteFile(p, plain, 0o600); err != nil {
		t.Fatal(err)
	}
	got, err := ReadFile(p)
	if err != nil {
		t.Fatalf("明文应能读: %v", err)
	}
	if string(got) != string(plain) {
		t.Fatalf("明文读回不一致: %q", got)
	}
}

// 写加密读解密：写进去的是密文，读出来是原文。
func TestWriteFileEncryptsAndReadsBack(t *testing.T) {
	p := filepath.Join(t.TempDir(), "api-key.txt")
	secret := []byte("sk-very-secret-token-value")
	if err := WriteFile(p, secret); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	raw, err := os.ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if !isProtected(raw) {
		t.Fatalf("落盘应为密文（前缀 %q），实得 %q", cryptPrefix, firstBytes(raw, 16))
	}
	if contains(raw, secret) {
		t.Fatalf("密文里不该包含明文内容")
	}
	got, err := ReadFile(p)
	if err != nil {
		t.Fatalf("ReadFile: %v", err)
	}
	if string(got) != string(secret) {
		t.Fatalf("往返不一致: %q", got)
	}
}

// 密文损坏：如实报错，绝不返回半截数据（调用方据此报错而不是当空账号池）。
func TestReadFileCorruptCipherErrors(t *testing.T) {
	p := filepath.Join(t.TempDir(), "vibex-config.json")
	if err := os.WriteFile(p, append(append([]byte{}, cryptPrefix...), []byte("!!!not-base64!!!\n")...), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := ReadFile(p); err == nil {
		t.Fatal("损坏的密文必须报错，不能当空内容")
	}
}

// 覆盖写：第二次写新内容后读回新内容（不是追加）。
func TestWriteFileOverwrites(t *testing.T) {
	p := filepath.Join(t.TempDir(), "keymap.json")
	if err := WriteFile(p, []byte("first")); err != nil {
		t.Fatal(err)
	}
	if err := WriteFile(p, []byte("second")); err != nil {
		t.Fatal(err)
	}
	got, err := ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != "second" {
		t.Fatalf("覆盖写后应为 second，实得 %q", got)
	}
}

// Remove 缺席不报错（与 os.Remove 的差别就在这里）。
func TestRemoveMissingIsNoError(t *testing.T) {
	if err := Remove(filepath.Join(t.TempDir(), "nope.txt")); err != nil {
		t.Fatalf("删除不存在的文件不该报错: %v", err)
	}
}

func contains(hay, needle []byte) bool {
	if len(needle) == 0 {
		return true
	}
	for i := 0; i+len(needle) <= len(hay); i++ {
		if string(hay[i:i+len(needle)]) == string(needle) {
			return true
		}
	}
	return false
}
