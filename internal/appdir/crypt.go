package appdir

import (
	"fmt"
	"os"
	"path/filepath"
)

// ReadFile 读数据文件，凭据文件自动识别新旧两种落盘形态（读兼容优先）：
//   - 明文（旧版落的文件）→ 原样返回，调用方行为与升级前完全一致
//   - DPAPI 密文（PDAPI1 前缀）→ 解密后返回
//
// 解密失败（换机 / 换了 Windows 账户 / 文件损坏）如实返回错误，绝不返回半截内容——
// 调用方据此报错，不把空账号池当成「本来就没账号」。
func ReadFile(path string) ([]byte, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if !isProtected(b) {
		return b, nil
	}
	return unprotectFileBytes(b)
}

// WriteFile 写数据文件：落盘前一律 DPAPI 加密（明文绝不落盘）+ 收紧 ACL 到当前用户。
// 加密失败即返回错误且**不改动原文件**（写走临时文件 + 改名，不截断既有数据）。
func WriteFile(path string, plain []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	enc, err := protectFileBytes(plain)
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, enc, 0o600); err != nil {
		return err
	}
	if err := aclUserOnly(tmp); err != nil {
		// ACL 收紧失败不阻断写入（数据本体仍然加密），但如实留痕。
		Logf("ACL 收紧失败（文件仍为密文）：%s: %v", filepath.Base(path), err)
	}
	if err := os.Rename(tmp, path); err != nil {
		_ = os.Remove(tmp)
		return err
	}
	return nil
}

// Remove 删除数据文件（缺席不算错误，与 os.Remove 的语义差就差这一条）。
func Remove(path string) error {
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return err
	}
	return nil
}

// Describe 给日志/报错用的一句话：区域（数据目录）在哪、查找过的候选路径有哪些。
func Describe(looked ...string) string {
	dir := DataDir()
	mode := "用户数据目录"
	if portable {
		mode = "便携模式（exe 同目录）"
	}
	out := fmt.Sprintf("状态目录 %s（%s）", dir, mode)
	if len(looked) > 0 {
		out += "；候选路径：" + joinSemi(looked)
	}
	return out
}

func joinSemi(list []string) string {
	out := ""
	for i, s := range list {
		if i > 0 {
			out += "; "
		}
		out += s
	}
	return out
}
