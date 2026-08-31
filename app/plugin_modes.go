// plugin_modes.go —— 插件子模式：进程内直接运行对应插件服务（GUI 托管时 spawn 本模式）。
package main

import (
	"flag"
	"log"
	"os"
	"path/filepath"

	"github.com/Amer-CN/proxydeck/internal/bai"
	"github.com/Amer-CN/proxydeck/internal/codebuddy"
	"github.com/Amer-CN/proxydeck/internal/comate"
	"github.com/Amer-CN/proxydeck/internal/qoder"
	"github.com/Amer-CN/proxydeck/internal/tuanjie"
)

var (
	flagPluginTuanjie     = flag.Bool("plugin-tuanjie", false, "团结 Cowork (Codely) 插件服务模式（GUI 托管时自动 spawn）")
	flagPluginCodebuddy   = flag.Bool("plugin-codebuddy", false, "CodeBuddy/WorkBuddy 插件服务模式（GUI 托管时自动 spawn；--desensitize 可选）")
	flagDesensitize       = flag.Bool("desensitize", false, "CodeBuddy 插件：对 system/developer/tools 做零宽脱敏，缓解腾讯审核误拦")
	flagPluginBai         = flag.Bool("plugin-bai", false, "B.AI 插件服务模式（本地转发到 api.b.ai，OpenAI 兼容）")
	flagPluginComate      = flag.Bool("plugin-comate", false, "Comate 插件服务模式（托管 zulu serve，本地 OpenAI 兼容 8786）")
	flagPluginQoder       = flag.Bool("plugin-qoder", false, "Qoder 插件服务模式（托管官方 agent SDK worker，本地 OpenAI 兼容 8785）")
)

// runPluginMode 处理 --plugin-tuanjie / --plugin-codebuddy / --plugin-bai / --plugin-comate
// 子模式：进程内直接跑对应插件服务（无窗口，关 GUI 不受影响）。
func runPluginMode() int {
	// 团结插件服务模式：进程内直接跑 internal/tuanjie 服务。
	if *flagPluginTuanjie {
		srv := tuanjie.NewServer()
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			_ = os.WriteFile(filepath.Join(exeDir(), "tuanjie-plugin-error.log"),
				[]byte(err.Error()), 0o600)
			os.Exit(1)
		}
		select {}
	}

	// CodeBuddy 插件服务模式：读桌面端登录态直连腾讯后端。
	if *flagPluginCodebuddy {
		srv, err := codebuddy.NewServer(*flagDesensitize)
		if err != nil {
			_ = os.WriteFile(filepath.Join(exeDir(), "codebuddy-plugin-error.log"),
				[]byte(err.Error()), 0o600)
			os.Exit(1)
		}
		log.Printf("codebuddy-plugin: listening on %s:%s (backend copilot.tencent.com, desensitize=%v)",
			*flagHost, *flagPort, *flagDesensitize)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			_ = os.WriteFile(filepath.Join(exeDir(), "codebuddy-plugin-error.log"),
				[]byte(err.Error()), 0o600)
			os.Exit(1)
		}
		select {}
	}

	// B.AI 插件服务模式：本地 Go 栈转发 api.b.ai（OpenAI 兼容）。
	if *flagPluginBai {
		srv := bai.NewServer()
		log.Printf("bai-plugin: starting on %s:%s", *flagHost, *flagPort)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			_ = os.WriteFile(filepath.Join(exeDir(), "bai-plugin-error.log"),
				[]byte(err.Error()), 0o600)
			os.Exit(1)
		}
		select {}
	}

	// Comate 插件服务模式：托管 zulu serve 子进程，本地 OpenAI 兼容（8786）。
	if *flagPluginComate {
		srv := comate.NewServer()
		log.Printf("comate-plugin: starting on %s:%s (zulu serve transport)", *flagHost, *flagPort)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			_ = os.WriteFile(filepath.Join(exeDir(), "comate-plugin-error.log"),
				[]byte(err.Error()), 0o600)
			os.Exit(1)
		}
		select {}
	}

	// Qoder 插件服务模式：每请求 spawn 官方 agent SDK worker 子进程，本地 OpenAI 兼容（8785）。
	if *flagPluginQoder {
		srv := qoder.NewServer()
		log.Printf("qoder-plugin: starting on %s:%s (official worker transport)", *flagHost, *flagPort)
		if err := srv.Start(*flagHost, *flagPort); err != nil {
			_ = os.WriteFile(filepath.Join(exeDir(), "qoder-plugin-error.log"),
				[]byte(err.Error()), 0o600)
			os.Exit(1)
		}
		select {}
	}
	return 0
}
