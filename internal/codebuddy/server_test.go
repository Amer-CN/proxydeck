// server_test.go —— tool_choice 兼容与工具 schema 出站链路回归。
// 上游使用内存 RoundTripper，凭据用临时目录造假，不访问真实服务或账号。
package codebuddy

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestNormalizeToolChoiceFunctionToRequired(t *testing.T) {
	body := map[string]any{
		"tools":       []any{map[string]any{"type": "function", "function": map[string]any{"name": "get_weather"}}},
		"tool_choice": map[string]any{"type": "function", "function": map[string]any{"name": "get_weather"}},
	}
	normalizeToolChoice(body)
	if got, _ := body["tool_choice"].(string); got != "required" {
		t.Fatalf("强制指定式应降级为 required, got %#v", body["tool_choice"])
	}
}

func TestNormalizeToolChoiceTypeAuto(t *testing.T) {
	body := map[string]any{
		"tools":       []any{map[string]any{"type": "function"}},
		"tool_choice": map[string]any{"type": "auto"},
	}
	normalizeToolChoice(body)
	if got, _ := body["tool_choice"].(string); got != "auto" {
		t.Fatalf("{\"type\":\"auto\"} 应为 auto, got %#v", body["tool_choice"])
	}
}

func TestNormalizeToolChoiceStringUnchanged(t *testing.T) {
	body := map[string]any{
		"tools":       []any{map[string]any{"type": "function"}},
		"tool_choice": "required",
	}
	normalizeToolChoice(body)
	if got, _ := body["tool_choice"].(string); got != "required" {
		t.Fatalf("字符串式应原样保留, got %#v", body["tool_choice"])
	}
}

func TestNormalizeToolChoiceDroppedWithoutTools(t *testing.T) {
	body := map[string]any{
		"tool_choice": map[string]any{"type": "function", "function": map[string]any{"name": "x"}},
	}
	normalizeToolChoice(body)
	if _, ok := body["tool_choice"]; ok {
		t.Fatalf("无 tools 时应删除 tool_choice, got %#v", body["tool_choice"])
	}
}

type toolSchemaRoundTripper func(*http.Request) (*http.Response, error)

func (f toolSchemaRoundTripper) RoundTrip(r *http.Request) (*http.Response, error) {
	return f(r)
}

// 覆盖真实 handleChat：客户端 tools → 元数据清理 → 上游校验 → tool_calls 回传。
// 内存上游拒绝 properties 丢失的必填参数，因此旧清理逻辑在这里也必须失败，
// 不能只靠直接调用清理函数的单测证明生产转发链路已接入修复。
func TestChatPreservesToolSchemaAndToolCalls(t *testing.T) {
	cases := []struct {
		region Region
		model  string
	}{
		{RegionINTL, "gpt-6-astra"},
		{RegionINTL, "deepseek-v4.1-flash"},
		{RegionCN, "deepseek-v4.1-flash"},
	}
	for _, tc := range cases {
		for _, stream := range []bool{false, true} {
			for _, clean := range []bool{false, true} {
				name := fmt.Sprintf("%s/%s/stream=%v/clean=%v", tc.region, tc.model, stream, clean)
				t.Run(name, func(t *testing.T) {
					path := writeAuthInfo(t, t.TempDir(), "test.info", regionConfigs[tc.region].DomainSuffix, "test-schema", "test", time.Now())
					hits := 0
					s := &Server{
						region: tc.region, desensitize: clean,
						cred:  NewCredential(path, tc.region),
						stats: map[string]*modelStat{},
						client: &http.Client{Transport: toolSchemaRoundTripper(func(r *http.Request) (*http.Response, error) {
							hits++
							defer r.Body.Close()
							if got := r.URL.String(); got != regionConfigs[tc.region].BaseURL+"/v2/chat/completions" {
								return nil, fmt.Errorf("unexpected route: %s", got)
							}
							var sent map[string]any
							if err := json.NewDecoder(r.Body).Decode(&sent); err != nil {
								return nil, err
							}
							if sent["model"] != tc.model || sent["stream"] != true || sent["tool_choice"] != "required" {
								return nil, fmt.Errorf("model, stream or tool_choice changed unexpectedly")
							}
							tools, ok := sent["tools"].([]any)
							if !ok || len(tools) != 1 {
								return nil, fmt.Errorf("tools lost")
							}
							fn := tools[0].(map[string]any)["function"].(map[string]any)
							params := fn["parameters"].(map[string]any)
							props := params["properties"].(map[string]any)
							for _, name := range params["required"].([]any) {
								if _, ok := props[name.(string)]; !ok {
									return nil, fmt.Errorf("invalid tools[0].parameters: missing required property %s", name)
								}
							}
							if len(props) != 4 || params["additionalProperties"] != false || fn["name"] != "Agent" {
								return nil, fmt.Errorf("tool schema contract changed")
							}
							if _, exists := fn["strict"]; exists {
								return nil, fmt.Errorf("strict must not be injected as a workaround")
							}
							_, hasDescription := fn["description"]
							if hasDescription == clean {
								return nil, fmt.Errorf("unexpected tool annotation handling")
							}
							response := "data: {\"id\":\"local-schema-test\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_schema\",\"type\":\"function\",\"function\":{\"name\":\"Agent\",\"arguments\":\"{\\\"description\\\":\\\"Check schema\\\",\\\"prompt\\\":\\\"Check only\\\"}\"}}]},\"finish_reason\":null}]}\n\n" +
								"data: {\"id\":\"local-schema-test\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"
							return &http.Response{
								StatusCode: http.StatusOK,
								Header:     http.Header{"Content-Type": []string{"text/event-stream"}},
								Body:       io.NopCloser(strings.NewReader(response)),
								Request:    r,
							}, nil
						})},
					}
					payload := map[string]any{
						"model": tc.model, "stream": stream,
						"messages":    []any{map[string]any{"role": "user", "content": "Check the schema."}},
						"tools":       []any{toolFixture(t, zcodeAgentToolFixture)},
						"tool_choice": map[string]any{"type": "function", "function": map[string]any{"name": "Agent"}},
					}
					encoded, err := json.Marshal(payload)
					if err != nil {
						t.Fatal(err)
					}
					rr := httptest.NewRecorder()
					s.handleChat(rr, httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(string(encoded))))
					if rr.Code != http.StatusOK || hits != 1 {
						t.Fatalf("expected one accepted request: status=%d hits=%d body=%s", rr.Code, hits, rr.Body.String())
					}
					if stream {
						if !strings.Contains(rr.Body.String(), `"name":"Agent"`) || !strings.Contains(rr.Body.String(), "data: [DONE]") {
							t.Fatalf("stream tool call or end marker lost: %s", rr.Body.String())
						}
					} else {
						var result map[string]any
						if err := json.Unmarshal(rr.Body.Bytes(), &result); err != nil {
							t.Fatal(err)
						}
						choice := result["choices"].([]any)[0].(map[string]any)
						calls := choice["message"].(map[string]any)["tool_calls"].([]any)
						fn := calls[0].(map[string]any)["function"].(map[string]any)
						var args map[string]any
						if err := json.Unmarshal([]byte(fn["arguments"].(string)), &args); err != nil {
							t.Fatal(err)
						}
						if choice["finish_reason"] != "tool_calls" || fn["name"] != "Agent" || args["description"] != "Check schema" || args["prompt"] != "Check only" {
							t.Fatalf("tool call did not round-trip: %s", rr.Body.String())
						}
					}
				})
			}
		}
	}
}
