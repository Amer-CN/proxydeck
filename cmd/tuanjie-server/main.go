// Command tuanjie-server 是团结 Cowork 的本地 OpenAI 兼容网关（Go 重写版）。
// 把 codely-cli 的 cli_api_key 兑换 + LiteLLM 转发封装成标准 /v1 端点，
// 默认监听 127.0.0.1:8788，供 codex++ / ZCode 等 agent 直接接入。
package main

import (
	"flag"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/Amer-CN/proxydeck/internal/tuanjie"
)

func main() {
	host := flag.String("host", "127.0.0.1", "监听地址")
	port := flag.String("port", "8788", "监听端口")
	flag.Parse()

	srv := tuanjie.NewServer()
	log.Printf("tuanjie-server: listening on %s:%s (backend codely-litellm.tuanjie.cn)", *host, *port)
	if err := srv.Start(*host, *port); err != nil {
		log.Fatalf("tuanjie-server: %v", err)
	}

	// 等待 Ctrl+C / 被父进程终止
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, os.Interrupt, syscall.SIGTERM)
	<-ch
	srv.Stop()
	log.Printf("tuanjie-server: stopped")
}
