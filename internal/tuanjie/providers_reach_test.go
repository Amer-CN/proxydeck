// providers_reach_test.go —— 可达性口径：OK 以 /models 连通性为准，
// 计费端点超时/404 不再判不可达（v3.10.0 Zen 冷启动误标「不可达」的根治）。
package tuanjie

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func shortFetchTimeout(t *testing.T) {
	t.Helper()
	old := providerFetchTimeout
	providerFetchTimeout = 150 * time.Millisecond
	t.Cleanup(func() { providerFetchTimeout = old })
}

// 计费端点挂起（网络级超时）+ /models 正常 → 仍可达，超时文案留在状态字段
func TestFetchProviderInfo_BillingTimeoutKeepsReachable(t *testing.T) {
	shortFetchTimeout(t)
	mux := http.NewServeMux()
	mux.HandleFunc("/v1/models", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"data": []any{map[string]any{"id": "m1"}}})
	})
	mux.HandleFunc("/v1/dashboard/billing/subscription", func(w http.ResponseWriter, r *http.Request) {
		<-r.Context().Done() // 挂起直到客户端超时断开
	})
	mux.HandleFunc("/v1/dashboard/billing/usage", func(w http.ResponseWriter, r *http.Request) {
		<-r.Context().Done()
	})
	up := httptest.NewServer(mux)
	defer up.Close()

	info := fetchProviderInfo(ExternalProvider{Name: "zen", BaseURL: up.URL + "/v1", APIKey: "sk-test", Models: []string{"m1"}})
	if !info.OK {
		t.Fatalf("计费超时不应判不可达：ok=false, error=%q", info.Error)
	}
	if len(info.Models) != 1 || info.Models[0] != "m1" {
		t.Fatalf("模型列表应来自 /models：%v", info.Models)
	}
	if !strings.Contains(info.UsageStatus, "Timeout") && !strings.Contains(info.UsageStatus, "deadline") &&
		!strings.Contains(info.UsageStatus, "timeout") {
		t.Fatalf("UsageStatus 应保留超时文案：%q", info.UsageStatus)
	}
}

// /models 连不通（网络级超时）→ 不可达（真死）
func TestFetchProviderInfo_ModelsFailUnreachable(t *testing.T) {
	shortFetchTimeout(t)
	mux := http.NewServeMux()
	mux.HandleFunc("/v1/dashboard/billing/subscription", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"hard_limit_usd": 100.0})
	})
	mux.HandleFunc("/v1/dashboard/billing/usage", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"total_usage": 0.0})
	})
	mux.HandleFunc("/v1/models", func(w http.ResponseWriter, r *http.Request) {
		<-r.Context().Done()
	})
	up := httptest.NewServer(mux)
	defer up.Close()

	info := fetchProviderInfo(ExternalProvider{Name: "dead", BaseURL: up.URL + "/v1", APIKey: "sk-test"})
	if info.OK {
		t.Fatalf("/models 不通应判不可达")
	}
	if len(info.Models) != 0 {
		t.Fatalf("失败时模型列表应为空：%v", info.Models)
	}
}

// opencode.ai 稳态回归：计费恒 404 + /models 200 → 可达，状态如实报 404
func TestFetchProviderInfo_Billing404KeepsReachable(t *testing.T) {
	mux := http.NewServeMux()
	mux.HandleFunc("/v1/models", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"data": []any{map[string]any{"id": "muse-spark-1.3-contributor-free"}}})
	})
	mux.HandleFunc("/v1/dashboard/billing/subscription", func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "not found", http.StatusNotFound)
	})
	mux.HandleFunc("/v1/dashboard/billing/usage", func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "not found", http.StatusNotFound)
	})
	up := httptest.NewServer(mux)
	defer up.Close()

	info := fetchProviderInfo(ExternalProvider{Name: "zen", BaseURL: up.URL + "/v1", APIKey: "sk-test"})
	if !info.OK {
		t.Fatalf("计费 404 不应判不可达")
	}
	if info.SubStatus != "HTTP 404" || info.UsageStatus != "HTTP 404" {
		t.Fatalf("计费状态应如实报 404：%q / %q", info.SubStatus, info.UsageStatus)
	}
}
