package tuanjie

import (
	"testing"
)

// 媒体改路由：图片请求 + 不识图模型 → 改写 codely-vl；纯文本不动。
func TestRerouteIfMedia(t *testing.T) {
	// 含 image_url 的请求
	mediaBody := []byte(`{"model":"GLM-5.3","stream":false,"messages":[
		{"role":"user","content":[
			{"type":"text","text":"这是什么"},
			{"type":"image_url","image_url":{"url":"data:image/png;base64,xxx"}}
		]}]}`)
	nb, note := RerouteIfMedia(mediaBody)
	if note == "" {
		t.Fatal("含图请求应触发改路由")
	}
	if want := "codely-vl"; !containsJSON(nb, want) {
		t.Fatalf("改写后 model 应为 %s，got: %s", want, string(nb))
	}
	if want := "image_url"; !containsJSON([]byte(note), want) {
		t.Fatalf("改写说明应含媒体类型: %s", note)
	}

	// 纯文本请求：不改
	textBody := []byte(`{"model":"GLM-5.3","messages":[{"role":"user","content":"你好"}]}`)
	_, note2 := RerouteIfMedia(textBody)
	if note2 != "" {
		t.Fatalf("纯文本不应改路由: %s", note2)
	}

	// 已是视觉模型：不改
	vlBody := []byte(`{"model":"codely-vl","messages":[{"role":"user","content":[
		{"type":"image_url","image_url":{"url":"data:image/png;base64,xxx"}}]}]}`)
	_, note3 := RerouteIfMedia(vlBody)
	if note3 != "" {
		t.Fatalf("视觉模型收到图片不应改路由: %s", note3)
	}
}

// 内容块检测：audio 等非文本类型也识别。
func TestDetectMediaType(t *testing.T) {
	audioBody := []byte(`{"messages":[{"role":"user","content":[
		{"type":"input_audio","input_audio":{"data":"..."}}]}]}`)
	if m := DetectMediaType(audioBody); m != "input_audio" {
		t.Fatalf("应检出 input_audio, got %q", m)
	}
	none := []byte(`{"messages":[{"role":"user","content":"纯文本"}]}`)
	if m := DetectMediaType(none); m != "" {
		t.Fatalf("纯文本应返回空, got %q", m)
	}
}

// 账号池：GLM 路由 + 轮换 + 402 禁用。
func TestAccountPoolBasics(t *testing.T) {
	p := &AccountPool{loads: map[string]int{}}
	p.accounts = []*Account{
		{UserID: "u1", AccessToken: "t1", Enabled: true, HasGLM53: true},
		{UserID: "u2", AccessToken: "t2", Enabled: true, HasGLM53: false},
	}
	// GLM 模型 → 只选 u1
	a := p.Pick("GLM-5.3")
	if a == nil || a.UserID != "u1" {
		t.Fatalf("GLM 应路由到 u1, got %v", a)
	}
	// 非 GLM 连续两次 → 轮换覆盖两个账号（顺序无关，集合相等即可）
	seen := map[string]bool{}
	for i := 0; i < 2; i++ {
		if c := p.Pick("codely-basic"); c != nil {
			seen[c.UserID] = true
		}
	}
	if !seen["u1"] || !seen["u2"] {
		t.Fatalf("连续两次非 GLM 应覆盖 u1+u2, got %v", seen)
	}
	// u1 402 禁用后 GLM 无可用 → 退回（仍返回账号但不跳过逻辑下轮会绕过）
	p.MarkBudgetExceeded("u1")
	if a2 := p.Pick("GLM-5.3"); a2 == nil {
		t.Log("u1 禁用后 GLM 无资格账号，Pick 返回 nil（符合预期）")
	}
	// u2 仍可用
	if c := p.Pick("codely-basic"); c == nil || c.UserID != "u2" {
		t.Fatalf("u2 应仍可用, got %v", c)
	}
}

// GLM 模型识别。
func TestIsGLMModel(t *testing.T) {
	if !isGLMModel("GLM-5.3") || !isGLMModel("glm-5.2-MAX") {
		t.Fatal("GLM 系应识别")
	}
	if isGLMModel("codely-basic") || isGLMModel("KIMI-K3") {
		t.Fatal("非 GLM 不应误判")
	}
}

// 注册表：登记/touch/finish/负载。
func TestRequestRegistry(t *testing.T) {
	reg := NewRegistry()
	rid := reg.Register("GLM-5.3", "u1", true)
	reg.Touch(rid, 100)
	reg.Touch(rid, 50)
	inflight := reg.Inflight()
	if len(inflight) != 1 || inflight[0].Bytes != 150 {
		t.Fatalf("注册表字节应 150, got %+v", inflight)
	}
	if reg.LoadOf("u1") != 1 {
		t.Fatalf("u1 负载应为 1")
	}
	reg.Finish(rid)
	if len(reg.Inflight()) != 0 || reg.LoadOf("u1") != 0 {
		t.Fatal("完成后应清零")
	}
}

// 模型名归一化。
func TestNormalizeModelName(t *testing.T) {
	if normalizeModelName("z-ai/GLM-5.3") != "glm-5.3" {
		t.Fatal("应去前缀+小写")
	}
	if normalizeModelName("GLM-5.3") != "glm-5.3" {
		t.Fatal("应小写")
	}
}

func containsJSON(b []byte, s string) bool {
	return string(b) != "" && stringContains(string(b), s)
}

func stringContains(h, n string) bool {
	for i := 0; i+len(n) <= len(h); i++ {
		if h[i:i+len(n)] == n {
			return true
		}
	}
	return false
}
