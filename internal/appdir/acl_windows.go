package appdir

import (
	"fmt"
	"os"
	"os/exec"
)

// aclUserOnly 给文件补一条「仅当前用户」的显式授权：/grant:r 只替换该用户自己的条目，
// 不动其它条目，所以**绝不会把访问权收没了**（去掉继承再授权会在授权失败时留下空
// ACL，实测会把文件锁死——曾把 keymap.json/close_mode.txt 收成谁都读不了）。
// 拿不到用户名时返回错误，调用方只记日志、不阻断读写。
func aclUserOnly(path string) error {
	user := os.Getenv("USERNAME")
	if user == "" {
		return fmt.Errorf("USERNAME 环境变量为空，无法收紧 ACL")
	}
	// 域账户用 DOMAIN\user；本机账户（USERDOMAIN == 机器名）直接用用户名即可。
	if dom := os.Getenv("USERDOMAIN"); dom != "" && dom != os.Getenv("COMPUTERNAME") {
		user = dom + `\` + user
	}
	cmd := exec.Command("icacls", path, "/grant:r", user+":(F)")
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("icacls 失败: %v (%s)", err, trimOutput(out))
	}
	return nil
}

func trimOutput(b []byte) string {
	s := string(b)
	if len(s) > 300 {
		s = s[:300] + "…"
	}
	return s
}
