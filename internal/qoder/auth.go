package qoder

import (
	"crypto/aes"
	"crypto/cipher"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"syscall"
	"time"
	"unsafe"
)

// authDir 返回 Qoder CN App 登录态目录（auth.v1.dat / Local State 所在）。
func authDir() string {
	return filepath.Join(os.Getenv("APPDATA"), "com.qodercn.app.stable")
}

// qoderAuth 是解密后的登录态。
type qoderAuth struct {
	Token     string         // dt- OAuth token（fetch_job_token 回调回给 worker）
	ExpiresAt time.Time      // token 过期时间（解不出为零值）
	User      map[string]any // user 字段（name/额度等，原样保留）
}

// dataBlob 对应 Windows CRYPT_INTEGER_BLOB { DWORD cbData; BYTE* pbData; }
// （pbData 用 unsafe.Pointer 保持与 C 指针相同的 8 字节布局，且对 GC 可见）。
type dataBlob struct {
	cbData uint32
	pbData unsafe.Pointer
}

var (
	procCryptUnprotectData = syscall.NewLazyDLL("crypt32.dll").NewProc("CryptUnprotectData")
	procLocalFree          = syscall.NewLazyDLL("kernel32.dll").NewProc("LocalFree")
)

// dpapiUnprotect 调 Windows DPAPI（CurrentUser 作用域）解密。
func dpapiUnprotect(data []byte) ([]byte, error) {
	var in, out dataBlob
	in.cbData = uint32(len(data))
	in.pbData = unsafe.Pointer(unsafe.SliceData(data))
	r, _, err := procCryptUnprotectData.Call(
		uintptr(unsafe.Pointer(&in)), 0, 0, 0, 0, 0, uintptr(unsafe.Pointer(&out)))
	if r == 0 {
		return nil, fmt.Errorf("CryptUnprotectData: %v", err)
	}
	defer procLocalFree.Call(uintptr(out.pbData))
	return unsafe.Slice((*byte)(out.pbData), out.cbData), nil
}

// loadAuth 读取并解密 Qoder CN 登录态。解密链（实测）：
// 同目录 Local State 的 os_crypt.encrypted_key（base64，去 DPAPI 前缀 5 字节）→
// DPAPI CurrentUser unprotect 得 32 字节 AES key → auth.v1.dat 前 3 字节为 "v10"，
// nonce=[3:15]、密文+tag=[15:] → AES-256-GCM 解出 JSON。
// 文件缺失 / 解密失败返回错误，调用方据此回 no_license。
func loadAuth() (*qoderAuth, error) {
	dir := authDir()
	ls, err := os.ReadFile(filepath.Join(dir, "Local State"))
	if err != nil {
		return nil, fmt.Errorf("读取 Local State 失败: %w", err)
	}
	var lsJ struct {
		OsCrypt struct {
			EncryptedKey string `json:"encrypted_key"`
		} `json:"os_crypt"`
	}
	if err := json.Unmarshal(ls, &lsJ); err != nil {
		return nil, fmt.Errorf("Local State 解析失败: %w", err)
	}
	raw, err := base64.StdEncoding.DecodeString(lsJ.OsCrypt.EncryptedKey)
	if err != nil || len(raw) < 6 || string(raw[:5]) != "DPAPI" {
		return nil, fmt.Errorf("encrypted_key 格式异常")
	}
	key, err := dpapiUnprotect(raw[5:])
	if err != nil {
		return nil, fmt.Errorf("DPAPI 解密失败: %w", err)
	}

	dat, err := os.ReadFile(filepath.Join(dir, "auth.v1.dat"))
	if err != nil {
		return nil, fmt.Errorf("读取 auth.v1.dat 失败: %w", err)
	}
	if len(dat) < 19 || string(dat[:3]) != "v10" {
		return nil, fmt.Errorf("auth.v1.dat 格式异常")
	}
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	pt, err := gcm.Open(nil, dat[3:15], dat[15:], nil)
	if err != nil {
		return nil, fmt.Errorf("auth.v1.dat AES-GCM 解密失败: %w", err)
	}
	var parsed struct {
		Token     string         `json:"token"`
		ExpiresAt string         `json:"expiresAt"`
		User      map[string]any `json:"user"`
	}
	if err := json.Unmarshal(pt, &parsed); err != nil || parsed.Token == "" {
		return nil, fmt.Errorf("登录态 JSON 解析失败（token 为空）")
	}
	exp, _ := time.Parse(time.RFC3339, parsed.ExpiresAt)
	return &qoderAuth{Token: parsed.Token, ExpiresAt: exp, User: parsed.User}, nil
}
