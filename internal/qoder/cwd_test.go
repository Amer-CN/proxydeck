package qoder

import "testing"

func TestExtractCwdFromPrompt(t *testing.T) {
	cases := []struct{ name, prompt, want string }{
		{
			"英文标记+反斜杠路径",
			"[System]\nYou are a coding agent. Working directory: F:\\AIXM\\command\n[User]\nhi",
			"F:\\AIXM\\command",
		},
		{
			"中文标记+全角冒号",
			"[System]\n工作目录：F:\\AIXM\\command\n[User]\nhi",
			"F:\\AIXM\\command",
		},
		{
			"cwd 标记+正斜杠",
			"[System]\ncwd: F:/AIXM/command\n[User]\nhi",
			"F:/AIXM/command",
		},
		{
			"不存在的路径必须拒绝（回落兜底）",
			"[System]\nWorking directory: F:\\AIXM\\no-such-dir-xyz\n[User]\nhi",
			"",
		},
		{
			"无标记 → 空",
			"[System]\nyou are helpful\n[User]\nhi",
			"",
		},
	}
	for _, c := range cases {
		if got := extractCwdFromPrompt(c.prompt); got != c.want {
			t.Errorf("%s: extractCwdFromPrompt = %q, want %q", c.name, got, c.want)
		}
	}
}
