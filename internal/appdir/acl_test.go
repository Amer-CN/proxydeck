package appdir

import (
	"os"
	"os/exec"
	"strings"
	"testing"
)

// ACL 收紧：数据目录与凭据文件的 ACL 只留当前用户（Administrators 允许出现，
// 它本来就是机器管理员组，不是「所有用户」）。
func TestCredentialFileACLRestrictedToCurrentUser(t *testing.T) {
	dir := t.TempDir()
	t.Setenv(DataDirEnv, dir)
	Init(false)

	p := Path("api-key.txt")
	if err := WriteFile(p, []byte("sk-acl-probe")); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	out, err := exec.Command("icacls", p).CombinedOutput()
	if err != nil {
		t.Fatalf("icacls 查询失败: %v (%s)", err, out)
	}
	acl := string(out)
	user := os.Getenv("USERNAME")
	if user == "" {
		t.Skip("无 USERNAME，跳过 ACL 断言")
	}
	if !strings.Contains(acl, user) {
		t.Fatalf("ACL 里应有当前用户 %s：\n%s", user, acl)
	}
	// 关键：收紧后文件必须仍可读可写（不能让 ACL 收紧变成锁死）
	if _, err := ReadFile(p); err != nil {
		t.Fatalf("收紧 ACL 后必须仍能读：%v", err)
	}
	if err := WriteFile(p, []byte("sk-acl-probe-2")); err != nil {
		t.Fatalf("收紧 ACL 后必须仍能写：%v", err)
	}
}
