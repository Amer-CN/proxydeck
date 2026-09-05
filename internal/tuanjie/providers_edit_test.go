// providers_edit_test.go —— 外部账号编辑（ProviderStore.Update + handler action=edit）：
// key/protocol 留空保留原值（旧配置磁盘空值不回填成 chat，Zen 兜底依赖空值）、
// 显式给值才归一化、Models 与名称不动、账号不存在或 base_url 空拒绝。
// 全部走 t.TempDir + exeDirOverride，绝不碰真实 tuanjie-*.json，
// 也不对运行中的 8788 端口发任何请求。
package tuanjie

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// overrideExeDir 把 exe 目录指到临时目录（providers.json 落盘隔离），返回该目录。
func overrideExeDir(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	t.Cleanup(func() { exeDirOverride = old })
	return dir
}

// TestProviderUpdateBranches Update 全分支：base_url 归一化更新、key/protocol
// 留空保留、显式给值归一化覆盖、Models 原样、结果落盘、非法输入拒绝。
func TestProviderUpdateBranches(t *testing.T) {
	dir := overrideExeDir(t)
	ps := NewProviderStore()
	if !ps.Add(ExternalProvider{Name: "a", BaseURL: "https://a.example/v1/", APIKey: "sk-a", Models: []string{"m1", " m2 "}, Protocol: " Responses "}) {
		t.Fatal("seed Add 失败")
	}
	if e := ps.List()[0]; e.BaseURL != "https://a.example/v1" || e.Protocol != "responses" || e.APIKey != "sk-a" {
		t.Fatalf("seed 基线不对: %+v", e)
	}

	// key/protocol 留空：保留原值；base_url 更新且尾斜杠归一化；Models 不动
	if !ps.Update("a", ExternalProvider{Name: "a", BaseURL: "https://new.example/v2/"}) {
		t.Fatal("Update 应成功")
	}
	e := ps.List()[0]
	if e.BaseURL != "https://new.example/v2" {
		t.Fatalf("base_url 未更新或未去尾斜杠: %q", e.BaseURL)
	}
	if e.APIKey != "sk-a" {
		t.Fatalf("key 留空应保留原值: %q", e.APIKey)
	}
	if e.Protocol != "responses" {
		t.Fatalf("protocol 留空应保留原值: %q", e.Protocol)
	}
	if len(e.Models) != 2 || e.Models[0] != "m1" || e.Models[1] != "m2" {
		t.Fatalf("Models 不应被改动: %+v", e.Models)
	}

	// 显式给值：key 覆盖、protocol trim+小写归一化
	if !ps.Update("a", ExternalProvider{BaseURL: "https://new.example/v2", APIKey: " sk-b ", Protocol: " ANTHROPIC "}) {
		t.Fatal("Update 应成功")
	}
	if e := ps.List()[0]; e.APIKey != "sk-b" || e.Protocol != "anthropic" {
		t.Fatalf("显式 key/protocol 应覆盖并归一化: %+v", e)
	}

	// 落盘：改动写进临时目录的 tuanjie-providers.json
	raw, err := os.ReadFile(filepath.Join(dir, "tuanjie-providers.json"))
	if err != nil {
		t.Fatalf("read persisted: %v", err)
	}
	if !strings.Contains(string(raw), `"base_url": "https://new.example/v2"`) || !strings.Contains(string(raw), "sk-b") {
		t.Fatalf("落盘内容不对: %s", raw)
	}

	// 拒绝分支：账号不存在 / base_url 空 / 名称空，且失败不改动数据
	if ps.Update("nope", ExternalProvider{BaseURL: "https://x/v1"}) {
		t.Fatal("不存在的账号应返回 false")
	}
	if ps.Update("a", ExternalProvider{BaseURL: "   "}) {
		t.Fatal("base_url 为空应返回 false")
	}
	if ps.Update("  ", ExternalProvider{BaseURL: "https://x/v1"}) {
		t.Fatal("名称为空应返回 false")
	}
	if e := ps.List()[0]; e.APIKey != "sk-b" || e.Protocol != "anthropic" || e.BaseURL != "https://new.example/v2" {
		t.Fatalf("失败的 Update 不得改动数据: %+v", e)
	}
}

// TestProviderInfosProtocolRaw 旧式 JSON（无 protocol 字段）加载后 Infos 透出
// 原始空串（baseInfo 不再归一化）；新式 chat 账号仍为 "chat"。旧式账号 Update
// 时 protocol 留空不得把磁盘空值回填成 chat。
func TestProviderInfosProtocolRaw(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{}`))
	}))
	defer srv.Close()

	dir := overrideExeDir(t)
	cfg := `[{"name":"old","base_url":"` + srv.URL + `/v1","api_key":"sk-old"},{"name":"new","base_url":"` + srv.URL + `/v1","api_key":"sk-new","protocol":"chat"}]`
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-providers.json"), []byte(cfg), 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}
	ps := NewProviderStore()
	infos := ps.Infos()
	if len(infos) != 2 {
		t.Fatalf("应有 2 条，实得 %d", len(infos))
	}
	if infos[0].Name != "old" || infos[0].Protocol != "" {
		t.Fatalf("旧式账号 Infos().Protocol 应为原始空串: %+v", infos[0])
	}
	if infos[1].Name != "new" || infos[1].Protocol != "chat" {
		t.Fatalf("新式 chat 账号 Infos().Protocol 应为 chat: %+v", infos[1])
	}

	// 旧式账号编辑（protocol 留空）：磁盘空值不回填，key/base_url 正常更新
	if !ps.Update("old", ExternalProvider{BaseURL: srv.URL + "/v2"}) {
		t.Fatal("Update 旧式账号应成功")
	}
	raw, err := os.ReadFile(filepath.Join(dir, "tuanjie-providers.json"))
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	var list []ExternalProvider
	if err := json.Unmarshal(raw, &list); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if list[0].Name != "old" || list[0].Protocol != "" {
		t.Fatalf("protocol 留空不得回填旧配置空值: %+v", list[0])
	}
	if list[0].BaseURL != srv.URL+"/v2" || list[0].APIKey != "sk-old" {
		t.Fatalf("Update 结果不对: %+v", list[0])
	}
	// Update 已清该账号缓存，Infos 再走 baseInfo，仍应透出空串
	if infos = ps.Infos(); len(infos) != 2 || infos[0].Protocol != "" {
		t.Fatalf("Update 后 Infos().Protocol 仍应为空串: %+v", infos)
	}
}

// TestHandleProvidersEditAction handler action=edit：成功 ok:true+提示并落盘；
// 失败（账号不存在 / base_url 空）ok:false+提示。
func TestHandleProvidersEditAction(t *testing.T) {
	dir := overrideExeDir(t)
	ps := NewProviderStore()
	if !ps.Add(ExternalProvider{Name: "p1", BaseURL: "https://h.example/v1", APIKey: "sk-old", Protocol: "chat"}) {
		t.Fatal("seed Add 失败")
	}
	s := &Server{providers: ps} // handleProviders 的 POST 路径只依赖 providers

	post := func(body string) map[string]any {
		t.Helper()
		rec := httptest.NewRecorder()
		s.handleProviders(rec, httptest.NewRequest(http.MethodPost, "/providers", strings.NewReader(body)))
		var out map[string]any
		if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
			t.Fatalf("响应不是 JSON: %v (%s)", err, rec.Body.String())
		}
		return out
	}

	// 成功：base_url 更新（去尾斜杠）、key/protocol 留空保留
	out := post(`{"action":"edit","name":"p1","base_url":"https://edited.example/v9/","api_key":"","protocol":""}`)
	if out["ok"] != true || out["msg"] != "外部账号已更新" {
		t.Fatalf("编辑成功响应不对: %v", out)
	}
	if e := ps.List()[0]; e.BaseURL != "https://edited.example/v9" || e.APIKey != "sk-old" || e.Protocol != "chat" {
		t.Fatalf("编辑结果不对: %+v", e)
	}
	raw, err := os.ReadFile(filepath.Join(dir, "tuanjie-providers.json"))
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if !strings.Contains(string(raw), "edited.example") {
		t.Fatalf("编辑未落盘: %s", raw)
	}

	// 失败：账号不存在
	out = post(`{"action":"edit","name":"ghost","base_url":"https://x/v1"}`)
	if out["ok"] != false || out["msg"] != "更新失败（账号不存在或 base_url 为空）" {
		t.Fatalf("编辑失败（账号不存在）响应不对: %v", out)
	}
	// 失败：base_url 为空
	out = post(`{"action":"edit","name":"p1","base_url":"   "}`)
	if out["ok"] != false {
		t.Fatalf("base_url 为空应失败: %v", out)
	}
	if e := ps.List()[0]; e.BaseURL != "https://edited.example/v9" {
		t.Fatalf("失败的编辑不得改动数据: %+v", e)
	}
}
