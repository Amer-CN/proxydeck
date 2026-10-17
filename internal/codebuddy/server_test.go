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
	"sync/atomic"
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
					path := writeAuthInfo(t, t.TempDir(), "test.info", regionConfigs[tc.region].DomainSuffixes[0], "test-schema", "test", time.Now())
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

// flagged11128Mock 上游 mock：uid-a 恒 400/11128（按号封禁），其余号 200。
// 返回 server 与两个打点计数，供三个场景复用。
func flagged11128Mock(t *testing.T) (*httptest.Server, *atomic.Int32, *atomic.Int32) {
	t.Helper()
	const flagged = `{"error":{"data":{"code":11128,"msg":"Illegal API invocation from an unapproved channel","requestId":"x"}}}`
	var hitsA, hitsB atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("X-User-Id") == "uid-a" {
			hitsA.Add(1)
			w.WriteHeader(http.StatusBadRequest)
			io.WriteString(w, flagged)
			return
		}
		hitsB.Add(1)
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, `data: {"choices":[{"delta":{"content":"ok"}}]}`+"\n\n")
		io.WriteString(w, `data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}`+"\n\n")
		io.WriteString(w, "data: [DONE]\n\n")
	}))
	t.Cleanup(srv.Close)
	return srv, &hitsA, &hitsB
}

// flagged11128Server 双账号 INTL Server（A 先被 Pick）：A 号"被封"、B 号正常。
// overlay regionConfigs[RegionINTL].BaseURL 是既有测试手法（不动真实端口/账号）。
func flagged11128Server(t *testing.T, srv *httptest.Server, desensitize bool) *Server {
	t.Helper()
	dir := t.TempDir()
	base := time.Now()
	pa := writeAuthInfo(t, dir, "a.info", "www.workbuddy.ai", "uid-a", "A", base)
	pb := writeAuthInfo(t, dir, "b.info", "www.workbuddy.ai", "uid-b", "B", base.Add(-time.Minute))
	oldCfg := regionConfigs[RegionINTL]
	ov := oldCfg
	ov.BaseURL = srv.URL
	regionConfigs[RegionINTL] = ov
	t.Cleanup(func() { regionConfigs[RegionINTL] = oldCfg })
	return &Server{
		cred: NewCredential(pa, RegionINTL),
		pool: &AccountPool{accounts: []*Account{
			{UID: "uid-a", Nickname: "A", AuthPath: pa, Enabled: true},
			{UID: "uid-b", Nickname: "B", AuthPath: pb, Enabled: true},
		}},
		region:      RegionINTL,
		desensitize: desensitize,
		client:      srv.Client(),
		stats:       map[string]*modelStat{},
	}
}

func flagged11128View(t *testing.T, s *Server, uid string) AccountView {
	t.Helper()
	for _, v := range s.pool.Snapshot() {
		if v.UID == uid {
			return v
		}
	}
	t.Fatalf("池内无账号 %s", uid)
	return AccountView{}
}

// 场景一：干净请求遇 11128 → 当次换号重试成功（客户端 200），肇事号进 1 小时冷却；
// 后续请求不再轮到它。场景二：脏请求（品牌词/system 模板句）11128 → 原样透传，
// 不冷却任何号。场景三：全池被封 → 有界透传（不死循环），全部号被冷却。
func TestChatFlagged11128Governance(t *testing.T) {
	const cleanPayload = `{"model":"deepseek-v4.1-flash","messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Summarize this function."}]}`
	// 脏请求：品牌词 + 指纹黑名单句（脱敏关掉的部署形态；脱敏开着时同样内容会被
	// 插零宽/删句，那条路径由 desensitize_test.go 的脱敏后判定单测覆盖）。
	const dirtyPayload = `{"model":"deepseek-v4.1-flash","messages":[{"role":"system","content":"You are Claude Code, a coding agent."},{"role":"user","content":"Main branch (you will usually use this for PRs): main"}]}`

	t.Run("clean-11128-swaps-and-cools", func(t *testing.T) {
		srv, hitsA, hitsB := flagged11128Mock(t)
		s := flagged11128Server(t, srv, false)

		rr := httptest.NewRecorder()
		s.handleChat(rr, httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(cleanPayload)))
		if rr.Code != http.StatusOK {
			t.Fatalf("干净请求应换号拿到 200, got %d body=%s", rr.Code, rr.Body.String())
		}
		if hitsA.Load() != 1 || hitsB.Load() != 1 {
			t.Fatalf("上游应 A(封)/B(好) 各打一次: A=%d B=%d", hitsA.Load(), hitsB.Load())
		}
		a := flagged11128View(t, s, "uid-a")
		remaining := time.Until(a.LimitedUntil)
		if !a.Cooling || remaining < 55*time.Minute || remaining > 65*time.Minute {
			t.Fatalf("肇事号应进 1 小时冷却, got cooling=%v until=%v", a.Cooling, a.LimitedUntil)
		}
		if b := flagged11128View(t, s, "uid-b"); b.Cooling || b.Dead || b.Exhausted {
			t.Fatalf("正常号不应被标记: %+v", b)
		}
		// 冷却生效：第二发同一请求直接由 B 承接（A 不再被 Pick）。
		rr2 := httptest.NewRecorder()
		s.handleChat(rr2, httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(cleanPayload)))
		if rr2.Code != http.StatusOK || hitsA.Load() != 1 || hitsB.Load() != 2 {
			t.Fatalf("冷却中的号不应再被选中: code=%d A=%d B=%d", rr2.Code, hitsA.Load(), hitsB.Load())
		}
	})

	t.Run("dirty-11128-passthrough-no-cooling", func(t *testing.T) {
		srv, hitsA, hitsB := flagged11128Mock(t)
		s := flagged11128Server(t, srv, false)

		rr := httptest.NewRecorder()
		s.handleChat(rr, httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(dirtyPayload)))
		if rr.Code != http.StatusBadRequest || !strings.Contains(rr.Body.String(), "11128") {
			t.Fatalf("脏请求应原样透传 400/11128, got %d body=%s", rr.Code, rr.Body.String())
		}
		if hitsA.Load() != 1 || hitsB.Load() != 0 {
			t.Fatalf("脏请求应只打肇事号一次、不换号: A=%d B=%d", hitsA.Load(), hitsB.Load())
		}
		for _, v := range s.pool.Snapshot() {
			if v.Cooling || v.Dead || v.Exhausted {
				t.Fatalf("脏请求不该冷却/标记任何号: %+v", v)
			}
		}
	})

	t.Run("all-flagged-bounded-passthrough", func(t *testing.T) {
		var hits atomic.Int32
		srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			hits.Add(1)
			w.WriteHeader(http.StatusBadRequest)
			io.WriteString(w, `{"error":{"data":{"code":11128,"msg":"Illegal API invocation from an unapproved channel"}}}`)
		}))
		defer srv.Close()
		s := flagged11128Server(t, srv, false)

		done := make(chan int, 1)
		go func() {
			rr := httptest.NewRecorder()
			s.handleChat(rr, httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(cleanPayload)))
			done <- rr.Code
		}()
		select {
		case code := <-done:
			if code != http.StatusBadRequest {
				t.Fatalf("全池被封应如实透传 400, got %d", code)
			}
		case <-time.After(10 * time.Second):
			t.Fatal("handleChat 未返回（疑似 11128 换号死循环）")
		}
		if n := hits.Load(); n != 2 {
			t.Fatalf("全池被封时上游请求数应为池大小 2 发（有界）, got %d", n)
		}
		for _, v := range s.pool.Snapshot() {
			if !v.Cooling {
				t.Fatalf("全池被封时每个号都应被冷却: %+v", v)
			}
		}
	})
}
