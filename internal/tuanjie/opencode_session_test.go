package tuanjie

import (
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/google/uuid"
)

// TestApplyOpenCodeSession helper 契约：baseURL 指向 opencode.ai 时附加
// x-opencode-session（进程级稳定、UUID v4）；其他 provider 不附加；nil 请求不 panic。
func TestApplyOpenCodeSession(t *testing.T) {
	reqZen, _ := http.NewRequest(http.MethodPost, "http://up.test/v1/chat/completions", nil)
	applyOpenCodeSession(reqZen, "https://opencode.ai/zen/v1")
	gotZen := reqZen.Header.Get("x-opencode-session")
	if gotZen == "" {
		t.Fatal("opencode.ai 出站请求应带 x-opencode-session 头")
	}
	id, err := uuid.Parse(gotZen)
	if err != nil || id.Version() != 4 {
		t.Fatalf("会话标识应为 UUID v4，got %q (err=%v, version=%d)", gotZen, err, id.Version())
	}
	reqZen2, _ := http.NewRequest(http.MethodPost, "http://up.test/v1/chat/completions", nil)
	applyOpenCodeSession(reqZen2, "https://opencode.ai/zen/v1")
	if again := reqZen2.Header.Get("x-opencode-session"); again != gotZen {
		t.Fatalf("进程级会话标识应稳定，两次不一致: %q vs %q", gotZen, again)
	}
	reqAgnes, _ := http.NewRequest(http.MethodPost, "http://up.test/v1/chat/completions", nil)
	applyOpenCodeSession(reqAgnes, "https://apihub.agnes-ai.com/v1")
	if v := reqAgnes.Header.Get("x-opencode-session"); v != "" {
		t.Fatalf("非 opencode.ai provider 不应带会话头，got %q", v)
	}
	applyOpenCodeSession(nil, "https://opencode.ai/v1") // 不应 panic
}

// TestForwardExternalPathsOpenCodeSession 三条外部转发路径（chat / responses /
// anthropic）端到端：httptest 回环上游捕获出站请求头——
// 1) BaseURL 指向 opencode.ai（借 query 片段触发 Contains 判定，真实连接仍回环）
//    → 出站带 x-opencode-session，值为 UUID v4 且进程内稳定；
// 2) BaseURL 非 opencode.ai → 出站不带。
func TestForwardExternalPathsOpenCodeSession(t *testing.T) {
	responsesOK := `{"id":"resp_z","object":"response","created_at":1,"status":"completed","model":"muse-spark-1.3-contributor-free",
		"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}],
		"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}`
	anthropicOK := `{"id":"msg_z","type":"message","role":"assistant","model":"m",
		"content":[{"type":"text","text":"pong"}],"stop_reason":"end_turn",
		"usage":{"input_tokens":1,"output_tokens":2}}`
	chatOK := `{"choices":[{"message":{"role":"assistant","content":"pong"}}]}`

	cases := []struct {
		name         string
		model        string
		upstreamBody string
		call         func(s *Server, w http.ResponseWriter, r *http.Request, body []byte, prov *ExternalProvider) int
	}{
		{
			name:         "chat",
			model:        "m",
			upstreamBody: chatOK,
			call: func(s *Server, w http.ResponseWriter, r *http.Request, body []byte, prov *ExternalProvider) int {
				return s.forwardExternal(w, r, body, "m", false, prov, "/chat/completions")
			},
		},
		{
			name:         "responses",
			model:        zenResponsesModelName,
			upstreamBody: responsesOK,
			call: func(s *Server, w http.ResponseWriter, r *http.Request, body []byte, prov *ExternalProvider) int {
				return s.forwardExternalResponses(w, r, body, zenResponsesModelName, false, prov, time.Now())
			},
		},
		{
			name:         "anthropic",
			model:        "claude-sonnet-4",
			upstreamBody: anthropicOK,
			call: func(s *Server, w http.ResponseWriter, r *http.Request, body []byte, prov *ExternalProvider) int {
				return s.forwardExternalAnthropic(w, r, body, "claude-sonnet-4", false, prov, time.Now())
			},
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			var mu sync.Mutex
			var captured []string
			up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				mu.Lock()
				captured = append(captured, r.Header.Get("x-opencode-session"))
				mu.Unlock()
				w.Header().Set("Content-Type", "application/json")
				fmt.Fprint(w, tc.upstreamBody)
			}))
			defer up.Close()

			for _, isOpencode := range []bool{true, false} {
				base := up.URL
				if isOpencode {
					// query 片段让 Contains(baseURL,"opencode.ai") 成立，
					// 真实 TCP 连接仍回环到 httptest 上游
					base += "?opencode.ai"
				}
				prov := &ExternalProvider{Name: "P", BaseURL: base, APIKey: "sk-test"}
				s := newTestServer()
				chatBody := mustJSON(t, map[string]any{
					"model": tc.model,
					"messages": []any{
						map[string]any{"role": "user", "content": "hi"},
					},
					"stream": false,
				})
				for i := 0; i < 2; i++ { // 两次出站，验证会话头进程级稳定
					r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
					w := httptest.NewRecorder()
					if st := tc.call(s, w, r, []byte(chatBody), prov); st != 0 {
						t.Fatalf("isOpencode=%v 第%d次 failStatus = %d, want 0", isOpencode, i+1, st)
					}
				}
				mu.Lock()
				n := len(captured)
				first, second := captured[n-2], captured[n-1]
				mu.Unlock()
				if isOpencode {
					if first == "" || second == "" {
						t.Fatalf("opencode.ai 出站应带会话头，captured=%q", captured)
					}
					if first != second {
						t.Fatalf("会话头应进程级稳定，两次不一致: %q vs %q", first, second)
					}
					if id, err := uuid.Parse(first); err != nil || id.Version() != 4 {
						t.Fatalf("会话头应为 UUID v4，got %q (err=%v)", first, err)
					}
				} else if first != "" || second != "" {
					t.Fatalf("非 opencode.ai provider 不应带会话头，captured=%q", captured)
				}
			}
		})
	}
}
