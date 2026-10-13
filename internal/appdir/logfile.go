package appdir

import (
	"bytes"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strconv"
	"time"
)

// 日志轮转参数（报告第六节第 4 条：按大小轮转 1 MB × 3）。
const (
	// LogMaxBytes 单份日志上限 1 MB，超过即轮转。
	LogMaxBytes int64 = 1 << 20
	// LogBackups 保留的历史份数（.1/.2/.3）。
	LogBackups = 3
)

// OpenLog 打开（不存在则创建）追加写日志文件。写失败不阻断主流程
// （返回 nil，调用方按「无日志」继续）。
func OpenLog(path string) *os.File {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil
	}
	rotateLog(path)
	f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		return nil
	}
	return f
}

// AppendLog 往日志追加一行带时间戳的记录（先轮转后写）。
func AppendLog(path, msg string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	rotateLog(path)
	return appendLine(path, msg)
}

// Logf 往应用日志写一行（落在数据目录，不污染 exe 同目录）。
// 日志写失败绝不能变成启动失败原因，故一律吞掉错误。
func Logf(format string, args ...any) {
	_ = AppendLog(filepath.Join(DataDir(), "appdir.log"), fmt.Sprintf(format, args...))
}

// StampedWriter 在每行前加时间戳（子进程 stdout/stderr 落盘用）。
type StampedWriter struct {
	f   *os.File
	buf []byte
}

// NewStampedWriter 包装一个日志文件；f 为 nil 时丢弃全部输出。
func NewStampedWriter(f *os.File) *StampedWriter { return &StampedWriter{f: f} }

func (w *StampedWriter) Write(p []byte) (int, error) {
	if w == nil || w.f == nil {
		return len(p), nil
	}
	w.buf = append(w.buf, p...)
	out := make([]byte, 0, len(p)+24)
	for len(w.buf) > 0 {
		line := w.buf
		if i := bytes.IndexByte(line, '\n'); i >= 0 {
			line, w.buf = line[:i], line[i+1:]
		} else {
			w.buf = nil // 无换行的尾巴：本行已带时间戳，剩余内容等下一次写入（可能永远等不到，无害）
		}
		if len(line) == 0 {
			continue
		}
		out = append(out, time.Now().Format("2006-01-02 15:04:05")...)
		out = append(out, ' ')
		out = append(out, line...)
		out = append(out, '\n')
	}
	if _, err := w.f.Write(out); err != nil {
		return 0, err
	}
	rotateIfBig(w.f.Name())
	return len(p), nil
}

func appendLine(path, msg string) error {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	defer f.Close()
	_, err = f.WriteString(time.Now().Format("2006-01-02 15:04:05") + " " + msg + "\n")
	return err
}

// rotateIfBig 写完后按大小轮转（长驻日志用；一次 Stat 的开销可忽略）。
func rotateIfBig(path string) {
	if fi, err := os.Stat(path); err == nil && fi.Size() > LogMaxBytes {
		rotateLog(path)
	}
}

// rotateLog 超过上限就把 path 滚成 .1，旧的依次后移，最老的丢弃。
// 轮转失败不动原文件（日志继续写原文件，不丢）。
func rotateLog(path string) {
	fi, err := os.Stat(path)
	if err != nil || fi.Size() <= LogMaxBytes {
		return
	}
	_ = os.Remove(path + "." + strconv.Itoa(LogBackups))
	for i := LogBackups - 1; i >= 1; i-- {
		_ = os.Rename(path+"."+strconv.Itoa(i), path+"."+strconv.Itoa(i+1))
	}
	if err := os.Rename(path, path+".1"); err != nil {
		log.Printf("[appdir] 日志轮转失败（继续写原文件）：%v", err)
	}
}
