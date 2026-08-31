package qoder

import "testing"

func TestResolveModelDisplayNames(t *testing.T) {
	cases := []struct{ in, want string }{
		// 截图目录（2026-08-30 服务端实时列表，13 条）
		{"", "Auto"},
		{"auto", "Auto"},
		{"qmodel_38max", "qmodel_38max"},
		{"Qwen3.8-Max", "qmodel_38max"},
		{"Qwen3.8-Flash", "qfmodel"},
		{"Qwen3.7-Max", "qmodel_latest"},
		{"Qwen3.7-Plus", "qmodel"},
		{"DeepSeek-V4-Pro", "dmodel"},
		{"DeepSeek-V4-Flash", "dfmodel"},
		{"GLM-5.3", "gmodel"},
		{"GLM-5.2", "gm51model"},
		{"Kimi-K2.7-Code", "kmodel"},
		{"MiniMax-M3", "mmodel"},
		// 代号经用户实切验证（App 日志 --model 实锤）：直连不映射
		{"Qwen3.7-Flash", "q37fmodel"},
		{"q37fmodel", "q37fmodel"},
		{"GLM-5.3-Flash", "gfmodel"},
		{"gfmodel", "gfmodel"},
		// 已下架的旧档必须拒绝（截图为准原则）
		{"performance", ""},
		{"efficient", ""},
		{"Cantus", ""},
		{"cmodel", ""},
		{"Kimi-K3", ""},
		{"kmodel_latest", ""},
		{"quest-ultimate", ""},
		{"Ultimate", ""},
		{"Qwen3.8-Max-Preview", ""},
		{"qmodel_preview", ""},
		{"not-a-model", ""},
	}
	for _, c := range cases {
		if got := resolveModel(c.in); got != c.want {
			t.Errorf("resolveModel(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}
