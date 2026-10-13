package appdir

import (
	"bytes"
	"encoding/base64"
	"fmt"
	"runtime"
	"syscall"
	"unsafe"
)

// 密文自描述前缀：文件以此开头即为 DPAPI 密文，否则按明文看待（读兼容优先）。
var cryptPrefix = []byte("PDAPI1\n")

// dataBlob 对应 Windows CRYPT_INTEGER_BLOB { DWORD cbData; BYTE* pbData; }。
// pbData 用 unsafe.Pointer（而非 uintptr）：保持与 C 指针相同的 8 字节布局，
// 同时对 GC 可见，且全程无需 uintptr → Pointer 的回转。
type dataBlob struct {
	cbData uint32
	pbData unsafe.Pointer
}

const cryptprotectUIForbidden = 0x1 // 失败即报错，绝不弹系统对话框

var (
	procCryptProtectData   = syscall.NewLazyDLL("crypt32.dll").NewProc("CryptProtectData")
	procCryptUnprotectData = syscall.NewLazyDLL("crypt32.dll").NewProc("CryptUnprotectData")
	procLocalFree          = syscall.NewLazyDLL("kernel32.dll").NewProc("LocalFree")
)

// callDPAPI 调 CryptProtectData / CryptUnprotectData，把系统新分配的输出块拷成 Go 切片、
// 立刻 LocalFree 后返回。
//
// 两条实测结论（都踩过）：
//  1. 入参整块交给 Go 字节切片，由编译器保证调用期间不被回收/移动；借道 uintptr 转手
//     会让 26 字节明文保护出来的密文自己都解不开（The data is invalid），24 字节的却正常。
//  2. 输出必须当次拷走再释放：把系统内存当切片返回，调用方一有分配就可能踩到已释放内存，
//     密文头 8 字节被复用成指针样值，解出来同样是 The data is invalid。
func callDPAPI(proc *syscall.LazyProc, in []byte, what string) ([]byte, error) {
	if len(in) == 0 {
		return nil, fmt.Errorf("%s: 空内容", what)
	}
	inBlob := dataBlob{cbData: uint32(len(in)), pbData: unsafe.Pointer(unsafe.SliceData(in))}
	var outBlob dataBlob
	r, _, callErr := proc.Call(
		uintptr(unsafe.Pointer(&inBlob)), 0, 0, 0, 0, cryptprotectUIForbidden,
		uintptr(unsafe.Pointer(&outBlob)))
	runtime.KeepAlive(in)      // 入参活过整次系统调用
	runtime.KeepAlive(&inBlob) // 入参结构体同理（系统调用期间读它）
	if r == 0 {
		return nil, fmt.Errorf("%s: %v", what, callErr)
	}
	if outBlob.pbData == nil || outBlob.cbData == 0 {
		return nil, fmt.Errorf("%s: 系统返回空数据", what)
	}
	out := append([]byte(nil), unsafe.Slice((*byte)(outBlob.pbData), outBlob.cbData)...)
	procLocalFree.Call(uintptr(outBlob.pbData))
	return out, nil
}

// protect 用 DPAPI（当前用户作用域）加密：其他 Windows 账户 / 拷到别的机器都解不开。
func protect(data []byte) ([]byte, error) {
	return callDPAPI(procCryptProtectData, data, "CryptProtectData")
}

// unprotect 解密 DPAPI 密文（当前用户作用域）。
func unprotect(cipher []byte) ([]byte, error) {
	return callDPAPI(procCryptUnprotectData, cipher, "CryptUnprotectData")
}

// protectFileBytes 把明文编成落盘形态（前缀 + base64 密文，单行）。
func protectFileBytes(plain []byte) ([]byte, error) {
	ct, err := protect(plain)
	if err != nil {
		return nil, err
	}
	out := make([]byte, 0, len(cryptPrefix)+base64.StdEncoding.EncodedLen(len(ct))+1)
	out = append(out, cryptPrefix...)
	out = base64.StdEncoding.AppendEncode(out, ct)
	return append(out, '\n'), nil
}

// unprotectFileBytes 解回落盘形态；密文格式坏掉时如实报错（不返回半截数据）。
func unprotectFileBytes(raw []byte) ([]byte, error) {
	body := bytes.TrimSpace(raw[len(cryptPrefix):])
	ct, err := base64.StdEncoding.DecodeString(string(body))
	if err != nil {
		return nil, fmt.Errorf("密文 base64 损坏: %w", err)
	}
	return unprotect(ct)
}

// isProtected 报告字节流是不是 DPAPI 密文形态。
func isProtected(b []byte) bool { return bytes.HasPrefix(b, cryptPrefix) }
