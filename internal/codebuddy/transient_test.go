package codebuddy

import "testing"

// 上游瞬时拒绝（11133）：必须重试；其他错误码不得误判（否则会把真错误也重试一遍）。
func TestIsTransientReject(t *testing.T) {
	const real11133 = `{"code":11133,"msg":"Invalid request parameters","requestId":"x",` +
		`"extError":{"code":"400001","message":"the request parameters were rejected by the model provider"}}`
	cases := []struct {
		name    string
		errBody string
		want    bool
	}{
		{"真 11133", real11133, true},
		{"空体", "", false},
		{"prompt 过长 11115", `{"code":11115,"msg":"prompt is too long: 1398108 tokens > 1048576 maximum"}`, false},
		{"内容指纹 11128", `{"code":11128,"msg":"Illegal API invocation from an unapproved channel"}`, false},
		{"工具定义 11129", `{"code":11129,"msg":"invalid_function_parameters"}`, false},
		{"额度耗尽 14018", `{"code":14018,"msg":"Credits exhausted"}`, false},
		{"数字巧合不得误判", `{"code":111330,"msg":"other"}`, false},
	}
	for _, c := range cases {
		if got := isTransientReject(c.errBody); got != c.want {
			t.Errorf("%s: isTransientReject = %v, want %v", c.name, got, c.want)
		}
	}
}
