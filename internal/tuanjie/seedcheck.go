package tuanjie

import (
	"archive/tar"
	"compress/gzip"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// 种子卫兵：官方人员确认封禁轴心是 session 签名，我们的签名种子硬编码逆向自
// 官方 CLI（client.go codelySigningSeedHex）。官方发新版轮换种子/改签名方案时
// 本机已装 CLI 的 bundle 会先变——周期核对 bundle/gemini.js 里两个特征串
// （签名种子 hex 与 header 名 X-Codely-Signature）是否仍存在：任一消失即置告警
// （/health seed_alert + GUI 警示条），核验恢复命中自动解除。
// 扫不到 bundle（未装 CLI / 读失败）不算告警，只记非告警状态，避免误报。
// 在线层（第二保险）：本机 CLI 永不更新时上面的扫描永远只看旧包——每轮先查
// npm registry 的 dist-tags.latest，版本号有变化才下载该版本 tgz、解出
// bundle/gemini.js 做同样的特征核验（官方当前真实在用的签名方案）。在线层
// 查询/下载失败只记状态不算告警（网络问题≠官方换锁），与本机层独立、任一层
// 特征消失即告警。

// seedFeatureHeaderName 官方 CLI 签名头名（client.go 签名实现写入的请求头，
// 此处只做 bundle 内存在性核对，不参与签名）。
const seedFeatureHeaderName = "X-Codely-Signature"

// 在线核对相关常量：registry 是 npm 公网，走普通 HTTP，不伪装、不走 utls。
const (
	seedRegistryPackumentURL = "https://registry.npmjs.org/@unity-china/codely-cli"
	seedBundleRelPath        = "bundle/gemini.js"
	seedRegistryTimeout      = 15 * time.Second // registry 查询（packument / 单版本文档）超时
	seedDownloadTimeout      = 15 * time.Minute // tgz 下载兜底超时（16MB 级，慢网也能完成）
	seedRegistryBodyLimit    = 4 << 20          // packument 响应读全上限 4MB
	seedVersionDocLimit      = 1 << 20          // 单版本文档响应上限（响应很小，防异常）
	seedTarballMaxBytes      = 64 << 20         // tarball 下载/解包大小上限 64MB
)

// seedRegClient 在线核验专用独立 client（零值 transport = http.DefaultTransport），
// 与 client.go 的 utls 伪装通道完全无关——伪装通道专用于官方上游。
var seedRegClient = &http.Client{}

// seedAlert 官方 CLI 签名种子轮换告警状态（runSeedGuardCheck 更新，
// handleHealth 快照给 GUI）。字段与 judgmentAlert 对齐；在线层各字段独立记录
// （latest/online 对外展示，onlineActive/onlineSignal 是在线层自己的告警结论，
// checkedVersion 是已完成特征核验的版本号，避免同版本反复下载 16MB tgz）。
type seedAlert struct {
	mu     sync.Mutex
	active bool
	since  time.Time
	signal string // 告警时为原因；非告警时为最近一次扫描状态（"ok"/"cli_not_found"）

	latest         string // registry 查到的 latest 版本号（查不到为空）
	online         string // 在线层状态："ok"/"registry_error"/"download_error"/"seed_missing"/"unchecked"
	onlineActive   bool   // 在线层核验结论：新包特征消失 → 告警
	onlineSignal   string // 在线层告警原因（带版本号）
	checkedVersion string // 已完成特征核验的版本号（内存即可，重启后首验重建基线）
}

// localCliBundlePaths 本机官方 CLI bundle（gemini.js）候选路径。与
// detectLocalCliVersion 的候选逻辑对齐（APPDATA 环境变量与 home\AppData\Roaming\npm
// 两个 npm 用户级全局安装位置），拼上 bundle/gemini.js 子路径。
// 注：路径解析在此独立实现，不改动 client.go。
func localCliBundlePaths() []string {
	var candidates []string
	if appdata := os.Getenv("APPDATA"); appdata != "" {
		candidates = append(candidates, filepath.Join(appdata, "npm", "node_modules", "@unity-china", "codely-cli", "bundle", "gemini.js"))
	}
	if home, err := os.UserHomeDir(); err == nil {
		candidates = append(candidates, filepath.Join(home, "AppData", "Roaming", "npm", "node_modules", "@unity-china", "codely-cli", "bundle", "gemini.js"))
	}
	return candidates
}

// checkBundleSeedFeatures 核对 bundle 文件里的两个签名特征串：签名种子 hex 与
// 签名头名，各自独立返回是否命中；读文件失败返回错误。纯函数（只读入参文件），
// 供种子卫兵后台循环与单测共用。
func checkBundleSeedFeatures(path string) (seedFound, headerFound bool, err error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return false, false, err
	}
	s := string(b)
	return strings.Contains(s, codelySigningSeedHex), strings.Contains(s, seedFeatureHeaderName), nil
}

// scanSeedBundle 扫单个 bundle 路径：scanned=false 表示文件不存在或读失败
// （未装 CLI / 权限问题）——不算告警，与「文件在但特征消失」严格区分。
func scanSeedBundle(path string) (scanned, seedFound, headerFound bool) {
	if _, err := os.Stat(path); err != nil {
		return false, false, false
	}
	sf, hf, err := checkBundleSeedFeatures(path)
	if err != nil {
		return false, false, false
	}
	return true, sf, hf
}

// evalSeedScan 把一次扫描结果映射为卫兵状态（纯函数，可单测）：
// 扫不到 bundle（scanned=false）不算告警；两特征齐全 → ok；
// 任一消失 → 告警并说明缺哪个特征（=官方改了签名方案或轮换了种子）。
func evalSeedScan(scanned, seedFound, headerFound bool) (active bool, signal string) {
	switch {
	case !scanned:
		return false, "cli_not_found"
	case seedFound && headerFound:
		return false, "ok"
	case !seedFound && !headerFound:
		return true, "签名种子与签名头名均未命中"
	case !seedFound:
		return true, "签名种子未命中（官方可能已轮换种子）"
	default:
		return true, "签名头名未命中（官方可能已改签名方案）"
	}
}

// queryRegistryLatest 查 npm registry packument，只取 dist-tags.latest。
// 响应带完整版本历史可能很大：读全但限 4MB，Unmarshal 时只声明 dist-tags
// 字段，其余版本详情由 json 丢弃。失败由调用方记 registry_error（不算告警）。
func queryRegistryLatest(ctx context.Context, packumentURL string) (string, error) {
	qctx, cancel := context.WithTimeout(ctx, seedRegistryTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(qctx, http.MethodGet, packumentURL, nil)
	if err != nil {
		return "", err
	}
	resp, err := seedRegClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("registry 状态 %d", resp.StatusCode)
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, seedRegistryBodyLimit))
	if err != nil {
		return "", err
	}
	var doc struct {
		DistTags struct {
			Latest string `json:"latest"`
		} `json:"dist-tags"`
	}
	if err := json.Unmarshal(body, &doc); err != nil {
		return "", err
	}
	if doc.DistTags.Latest == "" {
		return "", fmt.Errorf("dist-tags 无 latest")
	}
	return doc.DistTags.Latest, nil
}

// queryVersionTarball 查具体版本文档拿 dist.tarball（响应小，省流量——不解析
// 整个 packument 的 versions 表）。失败记 download_error（不算告警）。
func queryVersionTarball(ctx context.Context, packumentURL, version string) (string, error) {
	qctx, cancel := context.WithTimeout(ctx, seedRegistryTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(qctx, http.MethodGet, packumentURL+"/"+version, nil)
	if err != nil {
		return "", err
	}
	resp, err := seedRegClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("版本文档状态 %d", resp.StatusCode)
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, seedVersionDocLimit))
	if err != nil {
		return "", err
	}
	var doc struct {
		Dist struct {
			Tarball string `json:"tarball"`
		} `json:"dist"`
	}
	if err := json.Unmarshal(body, &doc); err != nil {
		return "", err
	}
	if doc.Dist.Tarball == "" {
		return "", fmt.Errorf("版本文档无 dist.tarball")
	}
	return doc.Dist.Tarball, nil
}

// limitWriter 累计写入超过上限即报错（下载/解包防超大响应撑爆磁盘）。
type limitWriter struct {
	w   io.Writer
	max int64
	n   int64
}

func (l *limitWriter) Write(p []byte) (int, error) {
	if l.n+int64(len(p)) > l.max {
		return 0, fmt.Errorf("超过 %d 字节上限", l.max)
	}
	n, err := l.w.Write(p)
	l.n += int64(n)
	return n, err
}

// verifySeedTarball 下载 tarball 流式落系统临时目录，gzip+tar 解包只找
// bundle/gemini.js（其余文件跳过不解；条目名含 .. 或绝对路径直接跳过，防路径
// 穿越），找到后走 checkBundleSeedFeatures 做特征核验；defer 清理整个临时目录。
// 返回（种子命中，头名命中，文件找到，错误）；err 非空=下载/解包层失败（记
// download_error），err 空且 found=false=包里没有该文件（官方包结构变了）。
func verifySeedTarball(ctx context.Context, tarballURL string) (seedFound, headerFound, fileFound bool, err error) {
	dir, err := os.MkdirTemp("", "seedguard-")
	if err != nil {
		return false, false, false, err
	}
	defer os.RemoveAll(dir)

	dctx, cancel := context.WithTimeout(ctx, seedDownloadTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(dctx, http.MethodGet, tarballURL, nil)
	if err != nil {
		return false, false, false, err
	}
	resp, err := seedRegClient.Do(req)
	if err != nil {
		return false, false, false, err
	}
	if resp.StatusCode != http.StatusOK {
		resp.Body.Close()
		return false, false, false, fmt.Errorf("tarball 状态 %d", resp.StatusCode)
	}
	f, err := os.Create(filepath.Join(dir, "pkg.tgz"))
	if err != nil {
		resp.Body.Close()
		return false, false, false, err
	}
	_, copyErr := io.Copy(&limitWriter{w: f, max: seedTarballMaxBytes}, resp.Body)
	closeErr := f.Close()
	resp.Body.Close()
	if copyErr != nil {
		return false, false, false, copyErr
	}
	if closeErr != nil {
		return false, false, false, closeErr
	}

	gf, err := os.Open(filepath.Join(dir, "pkg.tgz"))
	if err != nil {
		return false, false, false, err
	}
	defer gf.Close()
	gz, err := gzip.NewReader(gf)
	if err != nil {
		return false, false, false, err
	}
	defer gz.Close()
	tr := tar.NewReader(gz)
	for {
		hdr, err := tr.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return false, false, false, err
		}
		name := path.Clean(hdr.Name)
		if path.IsAbs(name) || name == ".." || strings.HasPrefix(name, "../") {
			continue // 路径穿越防护：npm 包正常不会有，但代码要防
		}
		if hdr.Typeflag != tar.TypeReg {
			continue
		}
		rel := strings.TrimPrefix(name, "package/") // npm 包 tar 条目统一带 package/ 前缀
		if rel != seedBundleRelPath {
			continue // 只解 bundle/gemini.js，其余跳过不解
		}
		out, err := os.Create(filepath.Join(dir, "gemini.js"))
		if err != nil {
			return false, false, false, err
		}
		_, copyErr := io.Copy(&limitWriter{w: out, max: seedTarballMaxBytes}, tr)
		closeErr := out.Close()
		if copyErr != nil {
			return false, false, false, copyErr
		}
		if closeErr != nil {
			return false, false, false, closeErr
		}
		sf, hf, err := checkBundleSeedFeatures(filepath.Join(dir, "gemini.js"))
		if err != nil {
			return false, false, false, err
		}
		return sf, hf, true, nil
	}
	return false, false, false, nil // tar 读完没有 bundle/gemini.js
}

// seedOnlineVerdict 取在线层当前告警结论快照（失败轮次沿用上次结论：
// registry 挂了不解除、也不误报在线层已核实的告警）。
func (s *Server) seedOnlineVerdict() (active bool, signal string) {
	s.seedAlert.mu.Lock()
	defer s.seedAlert.mu.Unlock()
	return s.seedAlert.onlineActive, s.seedAlert.onlineSignal
}

// checkSeedRegistry 在线层一轮：查 registry latest → 版本号没变什么都不下载；
// 变了（或重启后首验）才拿 dist.tarball 下载解包核验。查询/下载失败只改
// online 状态不告警；特征核验失败（任一特征消失）→ 在线层告警，signal 带版本号。
func (s *Server) checkSeedRegistry(ctx context.Context, packumentURL string) (active bool, signal string) {
	latest, err := queryRegistryLatest(ctx, packumentURL)
	if err != nil {
		s.seedAlert.mu.Lock()
		s.seedAlert.latest = ""
		s.seedAlert.online = "registry_error"
		s.seedAlert.mu.Unlock()
		return s.seedOnlineVerdict()
	}
	s.seedAlert.mu.Lock()
	checked := s.seedAlert.checkedVersion
	s.seedAlert.latest = latest
	if checked == latest {
		// 版本没变 → 不下载（禁每轮无条件拉 16MB tgz）。
		// online 必须按已核结论重算：上一轮可能刚经历 registry_error 被置脏，
		// 直接沿用会让 /health 永久输出 seed_latest 有值而 seed_online 卡在
		// registry_error 的自相矛盾状态（核验结论本身在 onlineActive 里，不丢）。
		s.seedAlert.online = "ok"
		if s.seedAlert.onlineActive {
			s.seedAlert.online = "seed_missing"
		}
		active, signal := s.seedAlert.onlineActive, s.seedAlert.onlineSignal
		s.seedAlert.mu.Unlock()
		return active, signal
	}
	s.seedAlert.mu.Unlock()

	tarballURL, err := queryVersionTarball(ctx, packumentURL, latest)
	if err != nil {
		s.seedAlert.mu.Lock()
		s.seedAlert.online = "download_error"
		s.seedAlert.mu.Unlock()
		return s.seedOnlineVerdict()
	}
	sf, hf, found, err := verifySeedTarball(ctx, tarballURL)
	if err != nil {
		s.seedAlert.mu.Lock()
		s.seedAlert.online = "download_error"
		s.seedAlert.mu.Unlock()
		return s.seedOnlineVerdict()
	}

	// 特征核验走到了结果（无论命中与否都记已核版本，同版本告警期间不重复下载）。
	active, signal = false, ""
	if !found {
		active, signal = true, latest+" 包内未找到 "+seedBundleRelPath
	} else if a, esig := evalSeedScan(true, sf, hf); a {
		active, signal = true, latest+" "+esig
	}
	online := "ok"
	if active {
		online = "seed_missing"
	}
	s.seedAlert.mu.Lock()
	s.seedAlert.checkedVersion = latest
	s.seedAlert.online = online
	s.seedAlert.onlineActive = active
	s.seedAlert.onlineSignal = signal
	s.seedAlert.mu.Unlock()
	return active, signal
}

// combineSeedAlerts 合成本机层与在线层：任一层特征消失即告警；在线层告警
// 优先（反映官方当前真实在用的包），两层都正常时信号用本机层的
// （"ok"/"cli_not_found"）。registry_error/download_error 不改变告警结论。
func combineSeedAlerts(localActive bool, localSignal string, onlineActive bool, onlineSignal string) (active bool, signal string) {
	switch {
	case onlineActive:
		return true, onlineSignal
	case localActive:
		return true, localSignal
	default:
		return false, localSignal
	}
}

// runSeedGuardCheck 扫一轮：先本机官方 CLI bundle（既有逻辑），再 registry
// 在线核对，合成后更新告警状态。本机层取第一个扫得到的候选路径，全部扫不到
// 按 cli_not_found 处理（非告警）。
func (s *Server) runSeedGuardCheck(ctx context.Context) {
	localActive, localSignal := false, "cli_not_found"
	for _, p := range localCliBundlePaths() {
		scanned, sf, hf := scanSeedBundle(p)
		if !scanned {
			continue
		}
		localActive, localSignal = evalSeedScan(scanned, sf, hf)
		break
	}
	onlineActive, onlineSignal := s.checkSeedRegistry(ctx, seedRegistryPackumentURL)
	active, signal := combineSeedAlerts(localActive, localSignal, onlineActive, onlineSignal)
	s.applySeedAlert(active, signal)
}

// applySeedAlert 更新种子卫兵状态：active 翻转时各打一行日志（置位/解除）；
// 置位记 since，解除清 since；非告警状态下仅 signal 变化不打日志。
func (s *Server) applySeedAlert(active bool, signal string) {
	s.seedAlert.mu.Lock()
	defer s.seedAlert.mu.Unlock()
	if s.seedAlert.active == active && s.seedAlert.signal == signal {
		return
	}
	wasActive := s.seedAlert.active
	s.seedAlert.active = active
	s.seedAlert.signal = signal
	if active && !wasActive {
		s.seedAlert.since = time.Now()
		log.Printf("[tuanjie] seedguard 官方 CLI 签名特征已变化，告警置位：%s", signal)
	} else if !active && wasActive {
		s.seedAlert.since = time.Time{}
		log.Printf("[tuanjie] seedguard 告警解除（官方 bundle 签名特征恢复命中）")
	}
}

// SeedGuardLoop 种子卫兵后台循环：启动先扫一次（本机层 + registry 在线层），
// 之后每 1 小时扫一次。接线仿 ResumeBudgetLoop：只在 Start 里起 goroutine，
// lifeCtx cancel 退出，测试直接构造 Server 不起循环。
func (s *Server) SeedGuardLoop(ctx context.Context) {
	s.runSeedGuardCheck(ctx)
	t := time.NewTicker(time.Hour)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			s.runSeedGuardCheck(ctx)
		}
	}
}
