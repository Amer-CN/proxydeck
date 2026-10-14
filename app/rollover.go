//go:build windows

package main

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/Amer-CN/proxydeck/internal/appdir"
	"golang.org/x/sys/windows"
)

// staleTarget 是一台要换血的后端：哪个插件、占位进程是谁、跑在哪个旧镜像上。
type staleTarget struct {
	def pluginDef
	pid int
	exe string
}

// isStaleImage 判定进程镜像是否为已被 supersede 的旧包。
//
// 只认 ProxyDeck.exe.old* 这个茎（大小写不敏感）：更新腾位（唯一名）和手动腾位的
// 旧文件都长这样。ProxyDeck_2.exe 这类改名副本、下载中的 ProxyDeck.exe.new、
// 毫不相干的 foo.old.exe 一律不算——换血只动"确定是旧包"的进程。
func isStaleImage(exePath string) bool {
	base := strings.ToLower(filepath.Base(exePath))
	return strings.HasPrefix(base, "proxydeck.exe.old")
}

// imagePathOfPID 返回进程的镜像全路径。打不开进程、读不出路径一律返回错误，
// 调用方必须按"未知=不动"处理（SYSTEM/受保护进程等本来就读不出来）。
func imagePathOfPID(pid int) (string, error) {
	h, err := windows.OpenProcess(windows.PROCESS_QUERY_LIMITED_INFORMATION, false, uint32(pid))
	if err != nil {
		return "", err
	}
	defer windows.CloseHandle(h)
	var buf [4096]uint16
	size := uint32(len(buf))
	if err := windows.QueryFullProcessImageName(h, 0, &buf[0], &size); err != nil {
		return "", err
	}
	return windows.UTF16ToString(buf[:size]), nil
}

// parseNetstatListeners 从 netstat -ano 输出里解析 port -> pids（仅 LISTENING 的 TCP）。
// 与 killByPort 同一套字段语义（f[1] 地址后缀、f[3] 状态、f[4] pid），抽出来只为可单测。
func parseNetstatListeners(out string) map[string][]int {
	holders := map[string][]int{}
	seen := map[string]map[int]bool{}
	for _, line := range strings.Split(out, "\n") {
		f := strings.Fields(line)
		if len(f) < 5 || !strings.EqualFold(f[3], "LISTENING") {
			continue
		}
		// f[1] 形如 127.0.0.1:8788 或 [::]:8788：取最后一个冒号后的端口
		idx := strings.LastIndex(f[1], ":")
		if idx < 0 {
			continue
		}
		port := f[1][idx+1:]
		pid, err := strconv.Atoi(f[4])
		if err != nil || pid <= 0 {
			continue
		}
		if seen[port] == nil {
			seen[port] = map[int]bool{}
		}
		if !seen[port][pid] {
			seen[port][pid] = true
			holders[port] = append(holders[port], pid)
		}
	}
	return holders
}

// snapshotListeners 快照一次 TCP 监听表（单次 netstat，供换血判定一次用完）。
func snapshotListeners() (map[string][]int, error) {
	out, err := hiddenCmd("netstat", "-ano", "-p", "tcp").Output()
	if err != nil {
		return nil, err
	}
	return parseNetstatListeners(string(out)), nil
}

// classifyHolders 决定换血名单：已知插件端口上、跑在旧包镜像上的占位进程。
//
// imageOf 由调用方提供（生产走 imagePathOfPID，单测走假表），读不出镜像的一律按
// "未知=不动"；currentExe 自身、selfPid 自身永远排除；非插件端口的占位进程看都不看
// （55990 主核心、测试端口、外部程序全部免疫）。
func classifyHolders(defs []pluginDef, snap map[string][]int, imageOf func(int) (string, bool), currentExe string, selfPid int) []staleTarget {
	var out []staleTarget
	for _, d := range defs {
		port := strconv.Itoa(d.Port)
		for _, pid := range snap[port] {
			if pid == selfPid {
				continue
			}
			exe, ok := imageOf(pid)
			if !ok || exe == "" {
				continue
			}
			if strings.EqualFold(exe, currentExe) {
				continue
			}
			if !isStaleImage(exe) {
				continue
			}
			out = append(out, staleTarget{def: d, pid: pid, exe: exe})
			break // 同一端口只换一次（pluginStop 按端口杀，pluginStart 重拉一个）
		}
	}
	return out
}

// waitBackendHealthy 轮询插件 health 直到绿（或超时）。interval/timeout 可注入，单测用毫秒级。
// rollKillGrace 杀后等端口空出的宽限（包变量，单测可调小；生产 5s）。
var rollKillGrace = 5 * time.Second

// updateMarkerPath 换血标记路径（数据目录，exe 换版也不丢；便携模式跟数据走）。
func updateMarkerPath() string { return appdir.Path(".update-rollover.json") }

// writeUpdateMarker 记录一次成功替换（to=新版本，at=纳秒时间戳），供新 GUI 换血用。
func writeUpdateMarker(to string) error {
	return writeUpdateMarkerTo(updateMarkerPath(), to, time.Now().UnixNano())
}

// writeUpdateMarkerTo 同上，路径可注入（单测走临时目录）。
func writeUpdateMarkerTo(path, to string, at int64) error {
	b, _ := json.Marshal(map[string]any{"to": to, "at": at})
	return os.WriteFile(path, b, 0o600)
}

// consumeUpdateMarker 读走换血标记（读到即删，保证只换一轮；解析失败也删，不重复触发）。
// 返回 ok=true = 刚发生过更新，调用方应对全部插件后端换血。
func consumeUpdateMarker() (string, bool) { return consumeUpdateMarkerFrom(updateMarkerPath()) }

// consumeUpdateMarkerFrom 同上，路径可注入（单测走临时目录）。
func consumeUpdateMarkerFrom(path string) (string, bool) {
	b, err := os.ReadFile(path)
	if err != nil {
		return "", false
	}
	_ = os.Remove(path)
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		return "", false
	}
	to, _ := m["to"].(string)
	return to, true
}

// waitPortFree 等端口空出来（只看 LISTENING，TIME_WAIT 残留不算）。单测可用临时监听。
func waitPortFree(port string, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for {
		if !portBusy(port) {
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(300 * time.Millisecond)
	}
}

func waitBackendHealthy(healthy func() bool, timeout, interval time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for {
		if healthy() {
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(interval)
	}
}

// rollStaleBackends 更新后零残留的关键一步：把还跑在旧包（*.old*）上的插件后端逐个换血。
//
// 背景：更新只重启 GUI，后端是独立常驻进程，还跑在腾位前的旧镜像上（改名后叫
// ProxyDeck.exe.old-<ts>）。旧文件被咬住就删不掉，"零残留"无从谈起。
// 本函数只在新 GUI 启动时后台跑一次：对已知插件端口逐个检查占位进程镜像，凡跑在
// 旧包上的就地换血（停旧→用新包拉起→等 health 绿），全部换完再扫一次残留。
// 判定缺一不可：①已知插件端口（pluginDefs 名单，55990 主核心、测试端口一律不动）；
// ②占位进程镜像命中 *.old*（大小写不敏感；现包/外部路径/读不出的未知一律不动）。
// 逐个串行，一路失败只记日志不连累其他路；全程异步，不阻塞界面；UI 经由既有
// pluginList 轮询自然从 starting 翻成 running，无需动 ui.html。
func (a *app) rollStaleBackends() {
	// 1) 换血标记（更新器刚换完包）：全量滚，不猜镜像名——文件改名后运行中进程
	// 上报的还是原名，靠名字永远认不出刚换过包的后端，只能靠标记。
	if to, ok := consumeUpdateMarker(); ok {
		log.Printf("[rollover] 检测到更新换血标记（→ %s），开始全量换血", to)
		a.rollAllBackends()
		cleanupOldExe()
		return
	}
	// 2) 无标记：沿用镜像名判定（手动腾位/旧残留场景）。
	snap, err := snapshotListeners()
	if err != nil {
		log.Printf("[rollover] 监听表快照失败，本次跳过换血检查：%v", err)
		return
	}
	if len(snap) == 0 {
		return
	}
	selfExe, _ := os.Executable()
	targets := classifyHolders(pluginDefs, snap, func(pid int) (string, bool) {
		exe, err := imagePathOfPID(pid)
		if err != nil {
			return "", false
		}
		return exe, true
	}, selfExe, os.Getpid())
	if len(targets) == 0 {
		return
	}
	for _, t := range targets {
		log.Printf("[rollover] %s（端口 %d）还跑在旧包上（pid=%d），开始换血", t.def.Name, t.def.Port, t.pid)
		if msg := rollOne(t,
			a.pluginStop,
			a.pluginStart,
			func(d pluginDef) bool { return httpOK(a.pluginHealthURL(d)) },
			90*time.Second,
		); msg != "" {
			log.Print(msg)
			continue
		}
		log.Printf("[rollover] %s 换血完成（已跑在新包上）", t.def.Name)
	}
	cleanupOldExe()
}

// rollAllBackends 把全部插件后端逐个换血（更新刚完成时用：所有后端都跑着旧包，
// 无需逐个判定）。逐个串行，一路失败只记日志不连累其他路；最后扫一次残留。
func (a *app) rollAllBackends() {
	for _, d := range pluginDefs {
		log.Printf("[rollover] %s 开始换血（更新后全量）", d.Name)
		if msg := rollOne(staleTarget{def: d},
			a.pluginStop,
			a.pluginStart,
			func(dd pluginDef) bool { return httpOK(a.pluginHealthURL(dd)) },
			90*time.Second,
		); msg != "" {
			log.Print(msg)
			continue
		}
		log.Printf("[rollover] %s 换血完成（已跑在新包上）", d.Name)
	}
}

// rollOne 给一台后端换血：停旧→确认端口空出→用新包拉起→等 health 绿，返回 "" 表示成功。
// stop/start/healthy 全部由调用方注入：生产传真实实现，单测传 mock（绝不碰真实端口）。
// 任一步失败只返回一行日志文案，由调用方记日志后继续下一台——失败隔离，不中断整轮。
// 其中杀后复验端口是双保险：pluginStop 自身已如实报错（杀不掉回非 nil 错误），
// 这里再验一次，防其他 stop 路径的假成功导致"拉杆弹回 running"；杀不掉就如实报错跳过。
func rollOne(t staleTarget,
	stop func(id string) error,
	start func(id string) error,
	healthy func(d pluginDef) bool,
	timeout time.Duration,
) string {
	if err := stop(t.def.ID); err != nil {
		return fmt.Sprintf("[rollover] %s 停止旧实例失败，跳过换血：%v", t.def.Name, err)
	}
	if !waitPortFree(strconv.Itoa(t.def.Port), rollKillGrace) {
		return fmt.Sprintf("[rollover] %s 旧进程杀不掉（端口 %d 仍被占，可能权限不足），跳过换血", t.def.Name, t.def.Port)
	}
	if err := start(t.def.ID); err != nil {
		return fmt.Sprintf("[rollover] %s 用新包拉起失败，请手动拉杆：%v", t.def.Name, err)
	}
	if !waitBackendHealthy(func() bool { return healthy(t.def) }, timeout, time.Second) {
		return fmt.Sprintf("[rollover] %s 拉起后 %v 仍未健康，请手动检查", t.def.Name, timeout)
	}
	return ""
}
