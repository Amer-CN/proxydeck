// Package appdir 统一解析首次写入的「数据目录」——GUI 与全部后端子进程（--plugin-*、
// -headless、--fax）共用同一套判定，前端与后端绝不会读写两个目录。
//
// 规则（外部反馈报告第六节第 1 条）：
//   - 默认：%APPDATA%\ProxyDeck\（exe 目录视作只读，桌面/Program Files 均不再长文件）
//   - 便携模式：exe 同目录存在 `portable` 标记文件，或启动带 `--portable`，数据仍写 exe 同目录
//
// 子进程判定「便携」时看的是**自己的** exe：插件与 GUI 是同一个 exe，目录一致，
// 因此父子进程解析结果必然相同（GUI 另外会把 --portable 传给子进程，双保险）。
//
// 环境变量 PROXYDECK_DATA_DIR 可覆盖数据目录（单测隔离与临时实例用；真实运行不带该变量）。
package appdir

import (
	"os"
	"path/filepath"
	"strings"
)

const (
	// AppName 是 %APPDATA% 下的数据目录名。
	AppName = "ProxyDeck"
	// PortableMarker 是 exe 同目录里的便携标记文件名。
	PortableMarker = "portable"
	// DataDirEnv 覆盖数据目录（测试/临时实例用）。
	DataDirEnv = "PROXYDECK_DATA_DIR"
)

// portable 为真 = 便携模式（数据写 exe 同目录）。由 Init 按 flag 与标记文件解析后写入。
var portable bool

// markerExists 报告 exe 同目录是否存在 portable 标记。
func markerExists() bool {
	fi, err := os.Stat(filepath.Join(ExeDir(), PortableMarker))
	return err == nil && !fi.IsDir()
}

// Portable 报告当前是否便携模式。
func Portable() bool { return portable }

// PortableArgs 返回要传给子进程（-headless / --plugin-* / --fax）的便携参数：
// 便携模式返回 ["--portable"]，否则 nil。GUI 起的后端都在子进程里读状态文件，
// 不带这个参数就会出现「GUI 写 exe 同目录、后端写 %APPDATA%」的目录撕裂。
func PortableArgs() []string {
	if portable {
		return []string{"--portable"}
	}
	return nil
}

// Init 解析便携模式：显式 --portable 优先，其次 exe 同目录的 portable 标记。
// 必须在任何数据文件读写之前调用（GUI 在 main()、插件子进程在 runPluginMode 之前）。
func Init(portableFlag bool) {
	portable = portableFlag || markerExists()
}

// ExeDir 返回 exe 所在目录（go run / 测试时退回工作目录）。便携标记与程序残骸仍看这里。
func ExeDir() string {
	if p, err := os.Executable(); err == nil {
		if rp, err := filepath.EvalSymlinks(p); err == nil {
			p = rp
		}
		return filepath.Dir(p)
	}
	if wd, err := os.Getwd(); err == nil {
		return wd
	}
	return "."
}

// DataDir 返回数据目录（不存在则创建）。失败时退回 exe 同目录——宁可退回也不要让调用方拿到空串。
func DataDir() string {
	dir := dataDir()
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return ExeDir()
	}
	return dir
}

// Path 拼一个数据目录下的文件路径（不建目录；调用方写盘前自行 MkdirAll，
// 或直接走 appdir.WriteFile）。
func Path(name string) string { return filepath.Join(DataDir(), name) }

func dataDir() string {
	if d := strings.TrimSpace(os.Getenv(DataDirEnv)); d != "" {
		return d
	}
	if portable {
		return ExeDir()
	}
	if appData := strings.TrimSpace(os.Getenv("APPDATA")); appData != "" {
		return filepath.Join(appData, AppName)
	}
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		return filepath.Join(home, "AppData", "Roaming", AppName)
	}
	return ExeDir()
}
