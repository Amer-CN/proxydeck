package appdir

import (
	"io"
	"os"
	"path/filepath"
	"strings"
)

// stateFile 描述一个历史状态文件（旧版全部落在 exe 同目录）。
// credential=true 表示文件内含 token 一类的凭据，需 DPAPI 加密 + 收紧 ACL。
type stateFile struct {
	name       string
	credential bool
}

// legacyStateFiles 需要搬迁的历史状态文件清单（credential=true 的还会被加密升级）。
// 只列状态/日志：plugins/ 目录、ProxyDeck.exe.* 残骸是程序本体，不搬。
var legacyStateFiles = []stateFile{
	// 主代理（app）
	{name: "api-key.txt", credential: true},
	{name: "stats.json"},
	{name: "keymap.json"},
	{name: "close_mode.txt"},
	{name: "notice_dismissed.flag"},
	{name: "close_hint_dismissed.flag"},
	{name: "headless-error.log"},
	{name: "calibration.json"},
	// 团结 8788
	{name: "tuanjie-accounts.json", credential: true},
	{name: "tuanjie-providers.json", credential: true},
	{name: "tuanjie-media.json"},
	{name: "tuanjie-vision.json"},
	{name: "tuanjie-water.json"},
	{name: "tuanjie-water-channels.json", credential: true},
	{name: "tuanjie-baselines.json"},
	{name: "tuanjie-stats.json"},
	{name: "tuanjie-pacing.json"},
	// WorkBuddy CN 8787 / INTL 8789
	{name: "codebuddy-pool.json"},
	{name: "codebuddy-pool-intl.json"},
	{name: "codebuddy-stats.json"},
	{name: "codebuddy-stats-intl.json"},
	{name: "codebuddy-failover.json"},
	{name: "codebuddy-failover-intl.json"},
	// VibeX 8790
	{name: "vibex-config.json", credential: true},
	{name: "vibex-stats.json"},
	// B.AI 8891
	{name: "bai-stats.json"},
	// 各插件启动失败诊断
	{name: "vibex-plugin-error.log"},
	{name: "tuanjie-plugin-error.log"},
	{name: "codebuddy-plugin-error.log"},
	{name: "codebuddy-intl-plugin-error.log"},
	{name: "bai-plugin-error.log"},
	{name: "comate-plugin-error.log"},
	{name: "qoder-plugin-error.log"},
}

// legacyCredentialFiles 返回需加密升级的历史凭据文件名（供 UpgradePlaintext 用）。
func legacyCredentialFiles() []string {
	var out []string
	for _, f := range legacyStateFiles {
		if f.credential {
			out = append(out, f.name)
		}
	}
	return out
}

// Migrate 把 srcDir（exe 同目录）里的历史状态文件一次性搬进 dstDir。
//
// 语义（报告第六节第 2 条）：
//   - 目标已存在同名文件 → 保留目标（用户目录里的才是权威），旧文件改名 *.migrated
//   - 搬走成功 → 原位置不留残渣；改名失败 → 删除旧文件（数据已在目标目录，
//     绝不因为删不掉而留下两份分叉状态）
//   - 目录（如 zulu-sessions）：目标无同名目录时整目录改名过去，否则只做改名标记
//   - 返回可在日志里打印的短句（不含文件内容）
func Migrate(dstDir, srcDir string) []string {
	if dstDir == "" || srcDir == "" || sameDir(dstDir, srcDir) {
		return nil
	}
	if st, err := os.Stat(srcDir); err != nil || !st.IsDir() {
		return nil
	}
	if err := os.MkdirAll(dstDir, 0o700); err != nil {
		return []string{"数据目录创建失败，迁移跳过：" + err.Error()}
	}
	var notes []string
	for _, f := range legacyStateFiles {
		src := filepath.Join(srcDir, f.name)
		fi, err := os.Lstat(src)
		if err != nil {
			continue // 不在旧位置：跳过（绝大多数文件都是这样）
		}
		dst := filepath.Join(dstDir, f.name)
		if fi.IsDir() {
			if _, err := os.Stat(dst); err == nil {
				note := retire(src, "目标已有同名目录，旧的只做改名")
				notes = append(notes, "目录 "+f.name+" 未搬迁："+note)
				continue
			}
			if err := os.Rename(src, dst); err != nil {
				notes = append(notes, "目录 "+f.name+" 搬迁失败："+err.Error())
			} else {
				notes = append(notes, "目录 "+f.name+" 已搬入数据目录")
			}
			continue
		}
		if fileExists(dst) {
			notes = append(notes, f.name+" 目标已存在，旧文件"+retire(src, "改名保留"))
			continue
		}
		tmp := dst + ".migrating"
		_ = os.Remove(tmp)
		if err := copyFile(src, tmp); err != nil {
			notes = append(notes, f.name+" 拷贝失败，保留在原处："+err.Error())
			continue
		}
		if err := os.Rename(tmp, dst); err != nil {
			_ = os.Remove(tmp)
			notes = append(notes, f.name+" 落位失败，保留在原处："+err.Error())
			continue
		}
		notes = append(notes, f.name+" 已搬入数据目录")
		if err := os.Remove(src); err != nil {
			// 删不掉（占用/权限）：改名让位，下次启动不再把它当权威旧文件
			notes = append(notes, f.name+" 旧文件删不掉，"+retire(src, "改名让位"))
		}
	}
	return notes
}

// UpgradePlaintext 把数据目录里仍是明文的凭据文件升级成 DPAPI 密文（读兼容的写侧收口）。
// 已是密文 / 不存在 / 读取失败一律跳过；加密失败时原文件原样保留，不丢账号。
func UpgradePlaintext() []string {
	var notes []string
	for _, name := range legacyCredentialFiles() {
		p := Path(name)
		b, err := os.ReadFile(p)
		if err != nil || len(b) == 0 {
			continue
		}
		if isProtected(b) {
			continue
		}
		if err := WriteFile(p, b); err != nil {
			notes = append(notes, name+" 明文升级失败（保持原样）："+err.Error())
			continue
		}
		notes = append(notes, name+" 已升级为 DPAPI 密文")
	}
	return notes
}

// retire 把 src 改名成 *.migrated；改名失败则直接删除（内容已在数据目录）。
func retire(src, why string) string {
	dst := src + ".migrated"
	if err := os.Rename(src, dst); err == nil {
		return why + "为 " + filepath.Base(dst)
	}
	if err := os.Remove(src); err == nil {
		return why + "失败，已删除（内容已在数据目录）"
	}
	return why + "失败，原样留在 " + src
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(dst, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	if err := out.Sync(); err != nil {
		out.Close()
		return err
	}
	return out.Close()
}

func fileExists(p string) bool {
	fi, err := os.Stat(p)
	return err == nil && !fi.IsDir()
}

func sameDir(a, b string) bool {
	ca, err1 := filepath.Abs(a)
	cb, err2 := filepath.Abs(b)
	if err1 != nil || err2 != nil {
		return false
	}
	return strings.EqualFold(filepath.Clean(ca), filepath.Clean(cb))
}
