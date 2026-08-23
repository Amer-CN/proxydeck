package server

import (
	"log"
	"net"
	"net/http"
	"time"

	"github.com/Amer-CN/proxydeck/internal/proxy"
)

const defaultPort = "55990"
const defaultHost = "127.0.0.1"

// Server represents the HTTP server
type Server struct {
	Port    string
	Host    string
	Proxy   *proxy.Proxy
	Handler http.Handler
}

// NewServer creates a new server instance
func NewServer(proxy *proxy.Proxy) *Server {
	mux := http.NewServeMux()
	mux.HandleFunc("/v1/chat/completions", logger(proxy.HandleChatCompletions))
	mux.HandleFunc("/chat/completions", logger(proxy.HandleChatCompletions))
	mux.HandleFunc("/v1/responses", logger(proxy.HandleResponses))
	mux.HandleFunc("/v1/messages", logger(proxy.HandleMessages))
	mux.HandleFunc("/messages", logger(proxy.HandleMessages))
	mux.HandleFunc("/v1/models", logger(proxy.HandleModels))
	mux.HandleFunc("/v1/usage", logger(proxy.HandleUsage))
	mux.HandleFunc("/v1/stats", logger(proxy.HandleStats))
	mux.HandleFunc("/v1/calibration", logger(proxy.HandleCalibration))
	mux.HandleFunc("/health", logger(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		w.Write([]byte(`{"status":"ok"}`))
	}))

	return &Server{
		Port:    defaultPort,
		Host:    defaultHost,
		Proxy:   proxy,
		Handler: mux,
	}
}

// SetPort sets the port for the server
func (s *Server) SetPort(port string) {
	if port != "" {
		s.Port = port
	}
}

// SetHost sets the host for the server
func (s *Server) SetHost(host string) {
	if host != "" {
		s.Host = host
	}
}

// GetPort returns the server port
func (s *Server) GetPort() string {
	return s.Port
}

// GetHost returns the server host
func (s *Server) GetHost() string {
	return s.Host
}

// logger is a middleware for logging requests
func logger(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		log.Printf("[%s] %s %s", r.Method, r.URL.Path, r.RemoteAddr)
		next(w, r)
		log.Printf("[%s] %s done in %v", r.Method, r.URL.Path, time.Since(start))
	}
}

// Start starts the HTTP server
func (s *Server) Start() {
	addr := s.Host + ":" + s.Port
	if err := http.ListenAndServe(addr, s.Handler); err != nil {
		log.Fatalf("Server failed: %v", err)
	}
}

// StartWithListener serves on an existing listener (used by headless mode
// where port binding is handled by the caller).
func (s *Server) StartWithListener(ln net.Listener) {
	if err := http.Serve(ln, s.Handler); err != nil {
		log.Fatalf("Server failed: %v", err)
	}
}
