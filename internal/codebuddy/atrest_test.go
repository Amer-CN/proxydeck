// atrest_test.go —— at-rest 加密登录态单测：AAD golden / 密钥派生 /
// 加密往返 / keyId 不一致 / suite 拒绝 / 坏信封先验（坏信封不得触发取 key）。
// 密钥一律经 WORKBUDDY_AT_REST_SECRET 注入合成值，真机 exe 只在验收时跑，
// 单测不依赖本机环境；auth 目录沿用 CODEBUDDY_AUTH_DIR 隔离惯例。
package codebuddy

import (
	"bytes"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"strings"
	"testing"
)

// testSecretA/B 合成 secret（base64 串，与真机无关）。
var (
	testSecretA = base64.StdEncoding.EncodeToString([]byte("workbuddy-atrest-test-secret-AAAAAAAAAAAAAAAA"))
	testSecretB = base64.StdEncoding.EncodeToString([]byte("workbuddy-atrest-test-secret-BBBBBBBBBBBBBBBB"))
)

// withAtrestSecret 注入合成 secret 并清 at-rest 缓存（测试间隔离）。
func withAtrestSecret(t *testing.T, secret string) {
	t.Helper()
	t.Setenv("WORKBUDDY_AT_REST_SECRET", secret)
	atrestMu.Lock()
	atrestKeys, atrestErr = nil, nil
	atrestMu.Unlock()
	t.Cleanup(func() {
		atrestMu.Lock()
		atrestKeys, atrestErr = nil, nil
		atrestMu.Unlock()
	})
}

// AAD golden：keyId 取本机实测信封里的真值 "9127dea1b44020a7"，期望为手算 hex
//（"WB-AAD\x00"+01+u32(5)WBEV1+u32(6)sym-v1+u32(1)+u32(16)keyId+02 00 00）。
func TestAtrestAADGolden(t *testing.T) {
	got := aadForKeyID("9127dea1b44020a7")
	want, err := hex.DecodeString("57422d41414400010000000557424556310000000673796d2d7631" +
		"000000010000001039313237646561316234343032306137020000")
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, want) {
		t.Fatalf("AAD 不符:\n got %x\nwant %x", got, want)
	}
	if len(got) != 54 {
		t.Fatalf("AAD 应 54 字节，got %d", len(got))
	}
}

// 密钥派生：protector=sha256(secret utf8)；keyId=hex(sha256(protector))[:16]；
// 同 secret 两次派生一致；不同 secret 的 keyId 不同。
func TestAtrestKeyDerivation(t *testing.T) {
	a1, a2 := deriveAtrestKey(testSecretA, "t"), deriveAtrestKey(testSecretA, "t")
	if a1.keyID != a2.keyID || a1.key != a2.key {
		t.Fatal("同 secret 派生应一致")
	}
	if len(a1.keyID) != 16 {
		t.Fatalf("keyId 应 16 hex，got %q", a1.keyID)
	}
	b := deriveAtrestKey(testSecretB, "t")
	if a1.keyID == b.keyID {
		t.Fatal("不同 secret 的 keyId 应不同")
	}
}

// 加密往返：明文 → 信封 → 解回原文；两次加密 nonce 不同（密文不同）；
// 外层形态为 {"$wbEncrypted":1,"envelope":...}；JWT 形长文本可走全程。
func TestAtrestRoundtrip(t *testing.T) {
	withAtrestSecret(t, testSecretA)
	pt := "eyJhbGciOiJIUzI1NiJ9.eyJ1aWQiOiJ0ZXN0LXVpZCJ9.c2lnbmF0dXJlLWZvci10ZXN0LW9ubHk"
	enc1, err := EncryptLike(nil, pt, RegionCN)
	if err != nil {
		t.Fatalf("加密失败: %v", err)
	}
	if f, _ := enc1["$wbEncrypted"].(int); f != 1 {
		t.Fatalf("外层缺 $wbEncrypted=1: %v", enc1)
	}
	envStr, _ := enc1["envelope"].(string)
	if envStr == "" {
		t.Fatalf("外层缺 envelope: %v", enc1)
	}
	raw1, _ := json.Marshal(enc1)
	back, err := decryptEnvelopeRaw(raw1, RegionCN)
	if err != nil {
		t.Fatalf("解密失败: %v", err)
	}
	if back != pt {
		t.Fatal("往返明文不一致")
	}
	// nonce 随机：同明文两次加密密文不同，但都能解回
	enc2, err := EncryptLike(nil, pt, RegionCN)
	if err != nil {
		t.Fatal(err)
	}
	if enc1["envelope"] == enc2["envelope"] {
		t.Fatal("两次加密 envelope 应不同（nonce 随机）")
	}
	raw2, _ := json.Marshal(enc2)
	if back2, err := decryptEnvelopeRaw(raw2, RegionCN); err != nil || back2 != pt {
		t.Fatalf("第二次往返失败: %v", err)
	}
	// EncryptLike 沿用原信封 keyId：用 enc1 作原形态加密，内层 keyId 应一致
	enc3, err := EncryptLike(raw1, "new-plaintext-token", RegionCN)
	if err != nil {
		t.Fatal(err)
	}
	raw3, _ := json.Marshal(enc3)
	env3, err := parseWbEnvelope(raw3)
	if err != nil {
		t.Fatal(err)
	}
	env1, _ := parseWbEnvelope(raw1)
	if env3.KeyID != env1.KeyID {
		t.Fatalf("应沿用原信封 keyId: got %s want %s", env3.KeyID, env1.KeyID)
	}
	if back3, err := decryptEnvelopeRaw(raw3, RegionCN); err != nil || back3 != "new-plaintext-token" {
		t.Fatalf("沿用 keyId 的往返失败: %v", err)
	}
}

// keyId 不一致诚实报错：A 加密、B 解密 → 错里带"keyId"且不带明文。
func TestAtrestKeyIDMismatch(t *testing.T) {
	withAtrestSecret(t, testSecretA)
	enc, err := EncryptLike(nil, "secret-plaintext", RegionCN)
	if err != nil {
		t.Fatal(err)
	}
	raw, _ := json.Marshal(enc)
	withAtrestSecret(t, testSecretB)
	if _, err := decryptEnvelopeRaw(raw, RegionCN); err == nil ||
		!strings.Contains(err.Error(), "keyId") {
		t.Fatalf("应报 keyId 不一致: %v", err)
	} else if strings.Contains(err.Error(), "secret-plaintext") {
		t.Fatalf("错误不得带明文: %v", err)
	}
}

// suite 拒绝：suite=2 的信封结构合法但须拒。
func TestAtrestSuiteMismatch(t *testing.T) {
	withAtrestSecret(t, testSecretA)
	enc, err := EncryptLike(nil, "x", RegionCN)
	if err != nil {
		t.Fatal(err)
	}
	inner, _ := base64.StdEncoding.DecodeString(enc["envelope"].(string))
	var m map[string]any
	if err := json.Unmarshal(inner, &m); err != nil {
		t.Fatal(err)
	}
	m["suite"] = 2
	ib, _ := json.Marshal(m)
	enc["envelope"] = base64.StdEncoding.EncodeToString(ib)
	raw, _ := json.Marshal(enc)
	if _, err := decryptEnvelopeRaw(raw, RegionCN); err == nil ||
		!strings.Contains(err.Error(), "suite") {
		t.Fatalf("应拒非 1 suite: %v", err)
	}
}

// 坏信封先验：形态坏的 envelope 在取 key 之前就失败——把
// WORKBUDDY_ELECTRON_PATH 指到不存在的路径，若仍报信封错而非取 key 错，
// 即证明未调 exe（真机无 secret 时同样快败）。
func TestAtrestBadEnvelopeBeforeKey(t *testing.T) {
	t.Setenv("WORKBUDDY_AT_REST_SECRET", "")
	t.Setenv("WORKBUDDY_ELECTRON_PATH", `Z:\根本不存在\WorkBuddy.exe`)
	atrestMu.Lock()
	atrestKeys, atrestErr = nil, nil
	atrestMu.Unlock()
	t.Cleanup(func() {
		atrestMu.Lock()
		atrestKeys, atrestErr = nil, nil
		atrestMu.Unlock()
	})
	for name, raw := range map[string]string{
		"garbage":   `{"$wbEncrypted":1,"envelope":"deadbeef"}`,
		"not-b64":   `{"$wbEncrypted":1,"envelope":"%%%"}`,
		"not-obj":   `"plain"`,
		"no-marker": `{"envelope":"e30="}`,
	} {
		if _, err := decryptEnvelopeRaw(json.RawMessage(raw), RegionCN); err == nil {
			t.Fatalf("%s 应失败", name)
		} else if strings.Contains(err.Error(), "密钥") || strings.Contains(err.Error(), "可执行文件") {
			t.Fatalf("%s 应报信封错、不得走到取 key: %v", name, err)
		}
	}
}

// wbToken JSON：明文原样；信封记形态不触发解密；数字/null 置空不报错。
func TestWbTokenJSON(t *testing.T) {
	var pt wbToken
	if err := json.Unmarshal([]byte(`"tok-abc"`), &pt); err != nil {
		t.Fatal(err)
	}
	if pt.IsEncrypted() {
		t.Fatal("明文不应标加密")
	}
	if s, err := pt.Resolve(RegionCN); err != nil || s != "tok-abc" {
		t.Fatalf("明文应原样回: %q %v", s, err)
	}
	var enc wbToken
	if err := json.Unmarshal([]byte(`{"$wbEncrypted":1,"envelope":"deadbeef"}`), &enc); err != nil {
		t.Fatal(err)
	}
	if !enc.IsEncrypted() {
		t.Fatal("信封应标加密")
	}
	for _, raw := range []string{`5`, `null`, `[1]`} {
		var v wbToken
		if err := json.Unmarshal([]byte(raw), &v); err != nil {
			t.Fatalf("%s 不应报错", raw)
		}
		if v.IsEncrypted() {
			t.Fatalf("%s 不应标加密", raw)
		}
		if s, _ := v.Resolve(RegionCN); s != "" {
			t.Fatalf("%s 应置空", raw)
		}
	}
}
