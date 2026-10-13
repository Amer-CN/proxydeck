// plugin_modes.go —— 插件子模式：进程内直接运行对应插件服务（GUI 托管时 spawn 本模式）。
package main

import (
	"flag"
	"log"
	"os"
	"strconv"

	"github.com/Amer-CN/proxydeck/internal/appdir"
	"github.com/Amer-CN/proxydeck/internal/bai"
	"github.com/Amer-CN/proxydeck/internal/codebuddy"
	"github.com/Amer-CN/proxydeck/internal/comate"
	"github.com/Amer-CN/proxydeck/internal/qoder"
	"github.com/Amer-CN/proxydeck/internal/tuanjie"
	"github.com/Amer-CN/proxydeck/internal/vibex"
)

var (
	flagPluginTuanjie       = flag.Bool("plugin-tuanjie", false, "团结 Cowork (Codely) 插件服务模式（GUI 托管时自动 spawn）")
	flagPluginCodebuddy     = flag.Bool("plugin-codebuddy", false, "CodeBuddy/WorkBuddy 插件服务模式（GUI 托管时自动 spawn；--desensitize 可选）")
	flagPluginCodebuddyIntl = flag.Bool("plugin-codebuddy-intl", false, "WorkBuddy 国际版插件服务模式（GUI 托管时自动 spawn；--desensitize 可选）")
	flagDesensitize         = flag.Bool("desensitize", false, "CodeBuddy 插件：对 system/developer/tools 做零宽脱敏，缓解腾讯审核误拦")
	flagPluginBai           = flag.Bool("plugin-bai", false, "B.AI 插件服务模式（本地转发到 api.b.ai，OpenAI 兼容）")
	flagPluginComate        = flag.Bool("plugin-comate", false, "Comate 插件服务模式（托管 zulu serve，本地 OpenAI 兼容 8786）")
	flagPluginQoder         = flag.Bool("plugin-qoder", false, "Qoder 插件服务模式（托管官方 agent SDK worker，本地 OpenAI 兼容 8785）")

	// flagPluginVibex 由下面的 vibexDispatchFlag 置位（--plugin-vibex）。
	flagPluginVibex bool
)

// vibexDispatchFlag 实现 flag.Value：解析 --plugin-vibex 时同时置位 flagPluginBai。
//
// 为什么这么绕：main() 的插件子模式分发条件是六个既有 flag 的或
// （app/main.go，本轮改动白名单不含它），单加一个 flag 进不了 runPluginMode()。
// 借道已有标志后，runPluginMode 里的 vibex 分支排在最前，故 --plugin-vibex 的
// 行为与"新增一个分发项"等价；带上 --plugin-bai 一起用时以 vibex 为准。
type vibexDispatchFlag struct{}

func (vibexDispatchFlag) String() string   { return "false" }
func (vibexDispatchFlag) IsBoolFlag() bool { return true }

func (vibexDispatchFlag) Set(s string) error {
	b, err := strconv.ParseBool(s)
	if err != nil {
		return err
	}
	flagPluginVibex = b
	if b {
		*flagPluginBai = true
	}
	return nil
}

// --plugin-vibex：VibeX（RunningHub）插件服务模式，本地 OpenAI 兼容 8790。
func init() {
	flag.Var(vibexDispatchFlag{}, "plugin-vibex", "VibeX 插件服务模式（REST+WS 协议客户端，本地 OpenAI 兼容 8790）")
}

// pluginFailLog 写插件启动失败诊断：带时间戳、追加写、按 1 MB × 3 轮转
// （数据目录，与 GUI app.pluginErrLog 读的路径同源）。
func pluginFailLog(name string, err error) {
	_ = appdir.AppendLog(appdir.Path(name+"-plugin-error.log"), "启动失败："+err.Error())
}

// runPluginMode 处理 --plugin-tuanjie / --plugin-codebuddy / --plugin-bai / --plugin-comate
// 子模式：进程内直接跑对应插件服务（无窗口，关 GUI 不受影响）。
func runPluginMode() int {
	// 状态目录口径与 GUI 完全一致（默认 %APPDATA%\ProxyDeck\，便携模式写 exe 同目录）。
	// 子进程的 exe 就是 GUI 的 exe，判定必然相同；GUI 另外会传 --portable 作双保险。
	// 本行必须在任何 NewServer()（读账号池/统计/配置）之前。
	appdir.Init(*flagPortable)

	// VibeX 插件服务模式：REST + WS 私有协议客户端，对外 OpenAI 兼容（8790）。
	// 注意：必须排在 --plugin-bai 分支之前——vibexDispatchFlag 是借 flagPluginBai
	// 进入本函数的（见该类型注释）。
	if flagPluginVibex {
		srv := vibex.NewServer()
		log.Printf("vibex-plugin: starting on %s:%s (VibeX REST+WS)", *flagHost, *flagPort)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			pluginFailLog("vibex", err)
			os.Exit(1)
		}
		select {}
	}

	// 团结插件服务模式：进程内直接跑 internal/tuanjie 服务。
	if *flagPluginTuanjie {
		srv := tuanjie.NewServer()
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			pluginFailLog("tuanjie", err)
			os.Exit(1)
		}
		select {}
	}

	// CodeBuddy 插件服务模式：读桌面端登录态直连腾讯后端。
	if *flagPluginCodebuddy {
		srv, err := codebuddy.NewServer(*flagDesensitize)
		if err != nil {
			pluginFailLog("codebuddy", err)
			os.Exit(1)
		}
		log.Printf("codebuddy-plugin: listening on %s:%s (backend copilot.tencent.com, desensitize=%v)",
			*flagHost, *flagPort, *flagDesensitize)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			pluginFailLog("codebuddy", err)
			os.Exit(1)
		}
		select {}
	}

	// CodeBuddy 国际版插件服务模式：直连 www.codebuddy.ai（凭据按 workbuddy.ai 过滤，
	// 不刷新 token，首条 system 强制，免费模型池）。
	if *flagPluginCodebuddyIntl {
		srv, err := codebuddy.NewServerForRegion(codebuddy.RegionINTL, *flagDesensitize)
		if err != nil {
			pluginFailLog("codebuddy-intl", err)
			os.Exit(1)
		}
		log.Printf("codebuddy-intl-plugin: listening on %s:%s (backend www.codebuddy.ai, desensitize=%v)",
			*flagHost, *flagPort, *flagDesensitize)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			pluginFailLog("codebuddy-intl", err)
			os.Exit(1)
		}
		select {}
	}

	// B.AI 插件服务模式：本地 Go 栈转发 api.b.ai（OpenAI 兼容）。
	if *flagPluginBai {
		srv := bai.NewServer()
		log.Printf("bai-plugin: starting on %s:%s", *flagHost, *flagPort)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			pluginFailLog("bai", err)
			os.Exit(1)
		}
		select {}
	}

	// Comate 插件服务模式：托管 zulu serve 子进程，本地 OpenAI 兼容（8786）。
	if *flagPluginComate {
		srv := comate.NewServer()
		log.Printf("comate-plugin: starting on %s:%s (zulu serve transport)", *flagHost, *flagPort)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			pluginFailLog("comate", err)
			os.Exit(1)
		}
		select {}
	}

	// Qoder 插件服务模式：每请求 spawn 官方 agent SDK worker 子进程，本地 OpenAI 兼容（8785）。
	if *flagPluginQoder {
		srv := qoder.NewServer()
		log.Printf("qoder-plugin: starting on %s:%s (official worker transport)", *flagHost, *flagPort)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			pluginFailLog("qoder", err)
			os.Exit(1)
		}
		select {}
	}
	return 0
}
