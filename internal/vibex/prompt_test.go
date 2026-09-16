// prompt_test.go —— 纯函数单测：jwt_decode / build_prompt / extract_text / claude_event 折叠。
package vibex

import (
	"encoding/base64"
	"encoding/json"
	"strings"
	"testing"
	"time"
)

// b64u 手工造 JWT 段（对齐 SPEC §3.2 的自测方式，不需要真实 token）。
func b64u(t *testing.T, v any) string {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return strings.TrimRight(base64.URLEncoding.EncodeToString(b), "=")
}

func TestDecodeJWT(t *testing.T) {
	token := b64u(t, map[string]any{"alg": "HS256", "typ": "JWT"}) + "." +
		b64u(t, map[string]any{"sub": "user-123", "nickName": "tester", "exp": 4102444800}) + ".sig"
	info := decodeJWT(token)
	if info == nil {
		t.Fatal("decodeJWT 应解出 payload")
	}
	if info.Sub != "user-123" {
		t.Fatalf("sub 解析错误: %q", info.Sub)
	}
	if info.Username != "tester" {
		t.Fatalf("username 解析错误: %q", info.Username)
	}
	if !info.hasExp() || info.Exp.Unix() != 4102444800 {
		t.Fatalf("exp 解析错误: %v", info.Exp)
	}
	if info.expired(time.Unix(4102444799, 0)) {
		t.Fatal("过期判定提前了")
	}
	if !info.expired(time.Unix(4102444801, 0)) {
		t.Fatal("过期判定漏了")
	}
	for _, bad := range []string{"", "not-a-jwt", "only.two"} {
		if decodeJWT(bad) != nil {
			t.Fatalf("非法 token 应返回 nil: %q", bad)
		}
	}
	// 无 nickName 时 username 退回 sub；无 exp 视为不过期。
	token2 := b64u(t, map[string]any{"alg": "none"}) + "." + b64u(t, map[string]any{"sub": "u9"}) + ".x"
	info2 := decodeJWT(token2)
	if info2 == nil || info2.Username != "u9" {
		t.Fatalf("username 应退回 sub: %+v", info2)
	}
	if info2.hasExp() || info2.expired(time.Now().Add(100*time.Hour)) {
		t.Fatal("无 exp 不应判过期")
	}
}

func TestBuildPrompt(t *testing.T) {
	msgs := []any{
		map[string]any{"role": "system", "content": "你是一个严谨的助手"},
		map[string]any{"role": "user", "content": "1+1=？"},
		map[string]any{"role": "assistant", "content": "2"},
		map[string]any{"role": "user", "content": "那 2+2 呢"},
	}
	got := buildPrompt(msgs, "")
	for _, needle := range []string{"【系统设定】", "【对话记录】", "【用户】", "1+1=？", "【助手】", "【任务】"} {
		if !strings.Contains(got, needle) {
			t.Fatalf("build_prompt 缺少 %q\n%s", needle, got)
		}
	}
	if !(strings.Index(got, "【系统设定】") < strings.Index(got, "【用户】") &&
		strings.Index(got, "【用户】") < strings.Index(got, "1+1=？")) {
		t.Fatalf("块顺序不对:\n%s", got)
	}
	if !strings.Contains(got, defaultInstruction) {
		t.Fatal("缺省指令未注入【任务】块")
	}

	// 特例：仅 1 条 user（无 system）→ 直接透传原文
	single := []any{map[string]any{"role": "user", "content": "只问一句"}}
	if got := buildPrompt(single, ""); got != "只问一句" {
		t.Fatalf("单条 user 特例应返回原文，得到 %q", got)
	}
	if got := buildPrompt(single, "自定义指令"); got != "只问一句\n\n【任务】\n自定义指令" {
		t.Fatalf("自定义指令应附加，得到 %q", got)
	}

	// tool 角色标「工具输出」，developer 归入系统设定
	mixed := []any{
		map[string]any{"role": "developer", "content": "D"},
		map[string]any{"role": "tool", "content": "T"},
	}
	out := buildPrompt(mixed, "")
	if !strings.Contains(out, "【系统设定】\nD") || !strings.Contains(out, "【工具输出】\nT") {
		t.Fatalf("角色映射不对:\n%s", out)
	}
}

func TestExtractText(t *testing.T) {
	if got := extractText("纯文本"); got != "纯文本" {
		t.Fatalf("字符串内容应原样返回: %q", got)
	}
	got := extractText([]any{
		map[string]any{"type": "text", "text": "你好"},
		map[string]any{"type": "image_url", "image_url": map[string]any{"url": "http://x/y.png"}},
		"尾巴",
	})
	if !strings.Contains(got, "你好") || !strings.Contains(got, imagePlaceholder) || !strings.Contains(got, "尾巴") {
		t.Fatalf("多模态提取不对: %q", got)
	}
	if got := extractText(map[string]any{"text": "对象文本"}); got != "对象文本" {
		t.Fatalf("对象内容提取不对: %q", got)
	}
	if got := extractText(nil); got != "" {
		t.Fatalf("nil 应给空串: %q", got)
	}
}

func TestClaudeEvents(t *testing.T) {
	cfg := &Config{}
	cases := []struct {
		name string
		ev   any
		want []outEvent
	}{
		{
			name: "text_delta",
			ev: map[string]any{"type": "stream_event", "event": map[string]any{
				"type": "content_block_delta", "delta": map[string]any{"type": "text_delta", "text": "增量"}}},
			want: []outEvent{{Kind: "text", Text: "增量"}},
		},
		{
			name: "thinking_delta",
			ev: map[string]any{"type": "stream_event", "event": map[string]any{
				"type": "content_block_delta", "delta": map[string]any{"type": "thinking_delta", "thinking": "想"}}},
			want: []outEvent{{Kind: "thinking", Text: "想"}},
		},
		{
			name: "stream_event 非 delta",
			ev: map[string]any{"type": "stream_event", "event": map[string]any{
				"type": "message_start"}},
			want: nil,
		},
		{
			name: "assistant 普通文本",
			ev: map[string]any{"type": "assistant", "message": map[string]any{
				"content": []any{map[string]any{"type": "text", "text": "完整消息"}}}},
			want: []outEvent{{Kind: "assistant_text", Text: "完整消息"}},
		},
		{
			name: "assistant 报错文本（双记）",
			ev: map[string]any{"type": "assistant", "isApiErrorMessage": true, "message": map[string]any{
				"content": []any{map[string]any{"type": "text", "text": "上游报错"}}}},
			want: []outEvent{{Kind: "assistant_text", Text: "上游报错"}, {Kind: "error_text", Text: "上游报错"}},
		},
		{
			name: "assistant tool_use（expose_tools 关）",
			ev: map[string]any{"type": "assistant", "message": map[string]any{
				"content": []any{map[string]any{"type": "tool_use", "name": "Bash"}}}},
			want: nil,
		},
		{
			name: "result",
			ev: map[string]any{"type": "result", "subtype": "success", "result": "答案",
				"usage": map[string]any{"input_tokens": float64(3), "output_tokens": float64(4)}},
			want: []outEvent{{Kind: "result", Text: "答案",
				Usage: map[string]any{"input_tokens": float64(3), "output_tokens": float64(4)}}},
		},
		{name: "system 忽略", ev: map[string]any{"type": "system"}, want: nil},
		{name: "非对象忽略", ev: "字符串", want: nil},
	}
	for _, tc := range cases {
		got := claudeEvents(tc.ev, cfg)
		if len(got) != len(tc.want) {
			t.Fatalf("%s: 事件数 %d != %d (%+v)", tc.name, len(got), len(tc.want), got)
		}
		for i := range got {
			if got[i].Kind != tc.want[i].Kind || got[i].Text != tc.want[i].Text {
				t.Fatalf("%s: 第 %d 条 %+v != %+v", tc.name, i, got[i], tc.want[i])
			}
		}
	}

	// expose_tools 打开时 tool_use → "[tool: 名]"
	cfgTools := &Config{ExposeTools: true}
	got := claudeEvents(map[string]any{"type": "assistant", "message": map[string]any{
		"content": []any{map[string]any{"type": "tool_use", "name": "Bash"}}}}, cfgTools)
	if len(got) != 1 || got[0].Kind != "tool" || got[0].Text != "[tool: Bash]" {
		t.Fatalf("expose_tools 行为不对: %+v", got)
	}
}

func TestFoldSync(t *testing.T) {
	content, _, _, _ := foldSync([]outEvent{
		{Kind: "assistant_text", Text: "合成错误 A"},
		{Kind: "assistant_text", Text: "合成错误 B"},
		{Kind: "done"},
	})
	if content != "合成错误 A合成错误 B" {
		t.Fatalf("assistant_text 兜底不对: %q", content)
	}
	content, _, _, _ = foldSync([]outEvent{{Kind: "text", Text: "真实回答"}, {Kind: "assistant_text", Text: "重复"}})
	if content != "真实回答" {
		t.Fatalf("有 text 流时不应使用 assistant_text: %q", content)
	}
	content, usage, flags, lastErr := foldSync([]outEvent{
		{Kind: "error_text", Text: "错误文本"},
		{Kind: "result", Usage: map[string]any{"input_tokens": float64(5), "output_tokens": float64(6)}},
		{Kind: "done", Flags: map[string]any{"balance_insufficient": true}},
		{Kind: "error", Text: "轮内错误"},
	})
	if content != "错误文本" {
		t.Fatalf("error_text 行为应保持: %q", content)
	}
	if u := oaiUsage(usage); u.PromptTokens != 5 || u.CompletionTokens != 6 || u.TotalTokens != 11 {
		t.Fatalf("usage 映射不对: %+v", u)
	}
	if !truthy(flags["balance_insufficient"]) {
		t.Fatal("done flags 未透传")
	}
	if lastErr != "轮内错误" {
		t.Fatalf("错误未记录: %q", lastErr)
	}
}

func TestJWTHeadersAndExpiry(t *testing.T) {
	// token 池应从 JWT exp 解出过期时间，并跳过已过期 token。
	expired := b64u(t, map[string]any{"alg": "none"}) + "." +
		b64u(t, map[string]any{"sub": "old", "exp": 1000000000}) + ".x"
	fresh := b64u(t, map[string]any{"alg": "none"}) + "." +
		b64u(t, map[string]any{"sub": "new", "exp": time.Now().Add(time.Hour).Unix()}) + ".x"
	pool := NewPool([]string{expired, fresh, fresh}, tokenCooldown)
	if pool.Len() != 2 {
		t.Fatalf("池应去重为 2，得到 %d", pool.Len())
	}
	tok, ok := pool.acquire()
	if !ok || tok.Value != fresh {
		t.Fatalf("应跳过已过期 token，得到 %v", ok)
	}
	cfg := &Config{BaseURL: defaultBaseURL, TenantID: "t-1"}
	c := NewClient(cfg, pool)
	h := c.headers(tok)
	if h["Authorization"] != "Bearer "+fresh || h["RH-TOKEN"] != fresh {
		t.Fatal("鉴权头不对")
	}
	if h["userId"] != "new" || h["username"] != "new" {
		t.Fatalf("JWT 派生头不对: %v", h)
	}
	if h["X-Tenant-Id"] != "t-1" || h["X-Vibex-Locale"] != "zh-CN" {
		t.Fatalf("租户/语言头不对: %v", h)
	}
}
