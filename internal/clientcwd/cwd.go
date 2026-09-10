// Package clientcwd 提供从扁平化 prompt 中提取客户端会话工作目录的共享实现。
// 本实现原在 internal/qoder/worker.go，由 comate 与 qoder 共用：
// 上游客户端（ZCode 等）系统提示自带 Working directory/工作目录 标记，
// agent 的文件读写/命令就落在客户端同一个项目里，工具执行、路径解析与用户
// 所见完全一致；不跟随的话 agent 工具只能在临时目录里空转，能力等于聊天模型。
// 提取到的路径必须真实存在且是目录才算数（防提示词里随机路径误匹配）。
package clientcwd

import (
	"os"
	"strings"
)

// cwdMarkers 客户端系统提示里常见的工作目录标记（ZCode/Claude Code 系均为此格式）。
var cwdMarkers = []string{
	"working directory", "current directory", "工作目录", "当前工作目录", "cwd",
}

// Extract 从扁平化 prompt 中提取客户端会话的工作目录；提取不到返回 ""。
func Extract(prompt string) string {
	lower := strings.ToLower(prompt)
	for _, marker := range cwdMarkers {
		idx := 0
		for {
			i := strings.Index(lower[idx:], marker)
			if i < 0 {
				break
			}
			start := idx + i + len(marker)
			seg := prompt[start:]
			// 跳过冒号/空白
			trimmed := strings.TrimLeft(seg, ":： \t\r\n")
			// 抓取盘符绝对路径（到行尾/引号/反引号为止）
			j := 0
			for j < len(trimmed) {
				c := trimmed[j]
				if c == '\r' || c == '\n' || c == '"' || c == '`' || c == '\'' {
					break
				}
				j++
			}
			cand := strings.TrimRight(trimmed[:j], " \t.,；;")
			if len(cand) >= 3 && cand[1] == ':' {
				if fi, err := os.Stat(cand); err == nil && fi.IsDir() {
					return cand
				}
			}
			idx = start + j
			if idx >= len(prompt) {
				break
			}
		}
	}
	return ""
}

// Dir 返回可用工作目录：Extract 命中优先，否则回退到 envName 指定的环境变量
// （须真实存在且是目录），再回退 os.TempDir()。
func Dir(prompt, envName string) string {
	if p := Extract(prompt); p != "" {
		return p
	}
	if p := strings.TrimSpace(os.Getenv(envName)); p != "" {
		if fi, err := os.Stat(p); err == nil && fi.IsDir() {
			return p
		}
	}
	return os.TempDir()
}
