package tuanjie

import (
	"testing"
	"time"
)

// TestSignLitellm 用固定输入断言签名值。
// 参考值由 Node crypto 独立实现计算得出（2026-08-21 实测上游 200 通过）。
func TestSignLitellm(t *testing.T) {
	got := SignLitellm("/v1/chat/completions", "sk-test-key-1234567890", time.Unix(1750000000, 0))
	want := "v1.1750000000.SvPELRsS7YUqeErP10Z_rK3_dExT699l9GYyFalB3uU"
	if got != want {
		t.Fatalf("签名不匹配:\n got=%s\nwant=%s", got, want)
	}
}

// TestSignLitellmSensitivity path/时间/key 任一变化都应改变签名。
func TestSignLitellmSensitivity(t *testing.T) {
	base := SignLitellm("/v1/models", "sk-a", time.Unix(1750000000, 0))
	cases := map[string]string{
		"path":  SignLitellm("/v1/chat/completions", "sk-a", time.Unix(1750000000, 0)),
		"key":   SignLitellm("/v1/models", "sk-b", time.Unix(1750000000, 0)),
		"time":  SignLitellm("/v1/models", "sk-a", time.Unix(1750000001, 0)),
	}
	for name, v := range cases {
		if v == base {
			t.Fatalf("%s 变化未改变签名", name)
		}
	}
}