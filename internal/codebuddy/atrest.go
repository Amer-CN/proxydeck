// atrest.go —— WorkBuddy 桌面端 at-rest 加密登录态（$wbEncrypted）解密。
//
// 背景：桌面端 5.7 起 auth .info 里的 accessToken / refreshToken 由明文字符串
// 变为加密信封 {"$wbEncrypted":1,"envelope":"..."}（配方来源：GitHub
// ShouZhuo0413/codebuddy2api core/workbuddy_atrest_crypto.py，issue #23；
// 本机 CN/INTL 双区信封结构已实测一致：suite=1 / nonce 12B / authTag 16B）。
//
// 密钥链（与配方逐字节一致）：
//  1. protector = sha256(atRestSecretKey 字符串 utf8)，32 字节。secret 由桌面端
//     Electron 原生层提供（ELECTRON_RUN_AS_NODE=1 <exe> -e loggerGet…，
//     吐 {"version":1,"atRestSecretKey":"44字符base64",…}）。
//  2. keyId = hex(sha256(protector))[:16]（envelope.keyId，全安装静态一致，须校验）。
//  3. AAD（sym-v1 field framing）：
//     "WB-AAD\x00" + "\x01" + u32be(5)+"WBEV1" + u32be(6)+"sym-v1" +
//     u32be(1) + u32be(16)+keyId(ascii) + "\x02" + "\x00" + "\x00"。
//  4. AES-256-GCM（标准库 crypto/aes+cipher，无新依赖）；刷新后加密回写
//     （nonce 随机，保持桌面端可读）。
//
// 取 key 路径纪律（简报红线）：一律动态发现，禁止写死默认安装位置——
// exe 来源只有三处：WORKBUDDY_ELECTRON_PATH 环境变量（逃生口，命中即唯一）
// 与注册表卸载表推导（实现见 atrest_win.go；InstallLocation 为空时从
// DisplayIcon/UninstallString 反推真实路径）与 WORKBUDDY_AT_REST_SECRET
// 直接给 secret。LOCALAPPDATA\Programs 这类默认路径一律不拼。
//
// 凭据纪律：所有错误只给形态与来源（"信封损坏"/"keyId不一致"/"取密钥失败"），
// 绝不回显 secret / token / envelope 内容；成功日志只打 JWT 头部前 20 字＋长度。
package codebuddy

import (
	"context"
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"sync"
	"time"
)

// loggerGetJS 经桌面端自带 Electron（run-as-node）调用原生层取 key 的脚本
//（与配方一致）。
const loggerGetJS = "const b = process._linkedBinding('electron_browser_workbuddy_storage');" +
	"process.stdout.write(b.loggerGet().toString('utf8'));"

// atrestKey 一把派生好的 at-rest 密钥。
type atrestKey struct {
	key   [32]byte // protector = sha256(secret utf8)
	keyID string   // hex(sha256(protector))[:16]
	src   string   // 脱敏来源（"env" / exe 文件名），日志用
}

// electronCand 一个候选桌面端可执行文件（实现见 atrest_win.go / atrest_other.go）。
type electronCand struct {
	path  string // 完整路径
	label string // "CN" / "INTL"（DisplayName/文件名判定，仅用于排序）
}

var (
	atrestMu    sync.Mutex
	atrestKeys  []atrestKey
	atrestErr   error
	atrestErrAt time.Time
)

// atrestErrTTL 取 key 失败的负缓存（扫描循环逐文件调 ensure 时不至于每个文件
// 都起一次 exe；成功无 TTL，常驻复用）。
const atrestErrTTL = time.Minute

// deriveAtrestKey 由 secret 原串派生（纯函数，单测直测）。
func deriveAtrestKey(secret, src string) atrestKey {
	k := sha256.Sum256([]byte(secret))
	id := sha256.Sum256(k[:])
	return atrestKey{key: k, keyID: hex.EncodeToString(id[:])[:16], src: src}
}

// ensureAtrestKeys 取本机 at-rest 密钥（缓存；region 只决定候选 exe 的尝试顺序，
// 解密时以 envelope.keyId 匹配为准，不跨区误用）。
func ensureAtrestKeys(region Region) ([]atrestKey, error) {
	atrestMu.Lock()
	defer atrestMu.Unlock()
	if len(atrestKeys) > 0 {
		return atrestKeys, nil
	}
	if atrestErr != nil && time.Since(atrestErrAt) < atrestErrTTL {
		return nil, atrestErr
	}
	fail := func(err error) ([]atrestKey, error) {
		atrestErr, atrestErrAt = err, time.Now()
		return nil, err
	}
	// 1. 环境变量直接给 secret（逃生口；测试注入同口）。
	if secret := os.Getenv("WORKBUDDY_AT_REST_SECRET"); secret != "" {
		atrestKeys = []atrestKey{deriveAtrestKey(secret, "env")}
		atrestErr = nil
		return atrestKeys, nil
	}
	// 2. 候选 exe 逐个 run-as-node 取 secret。
	cands := electronCandidates(region)
	if len(cands) == 0 {
		return fail(fmt.Errorf("找不到 WorkBuddy 桌面端可执行文件" +
			"（注册表卸载表无条目），可用 WORKBUDDY_ELECTRON_PATH 指定"))
	}
	var keys []atrestKey
	var lastErr error
	for _, c := range cands {
		secret, err := fetchAtrestSecret(c.path)
		if err != nil {
			lastErr = err
			continue
		}
		keys = append(keys, deriveAtrestKey(secret, c.path))
	}
	if len(keys) == 0 {
		msg := "取本机 at-rest 密钥失败"
		if lastErr != nil {
			msg += "：" + truncate(lastErr.Error(), 200)
		}
		return fail(fmt.Errorf("%s", msg))
	}
	atrestKeys = keys
	atrestErr = nil
	return atrestKeys, nil
}

// fetchAtrestSecret 对单个 exe 跑 loggerGet 取 atRestSecretKey（20s 超时）。
func fetchAtrestSecret(exe string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, exe, "-e", loggerGetJS)
	cmd.Env = append(os.Environ(), "ELECTRON_RUN_AS_NODE=1")
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("%s: %v", exe, err)
	}
	var payload struct {
		AtRestSecretKey string `json:"atRestSecretKey"`
	}
	if err := json.Unmarshal(out, &payload); err != nil || payload.AtRestSecretKey == "" {
		return "", fmt.Errorf("%s: loggerGet 未返回 atRestSecretKey", exe)
	}
	if _, err := base64.StdEncoding.DecodeString(payload.AtRestSecretKey); err != nil {
		return "", fmt.Errorf("%s: atRestSecretKey 非法 base64", exe)
	}
	return payload.AtRestSecretKey, nil
}

// wbEnvelope 信封内层（envelope 字段 base64 解出的 JSON）。
type wbEnvelope struct {
	Suite      int    `json:"suite"`
	KeyID      string `json:"keyId"`
	Nonce      string `json:"nonce"`
	AuthTag    string `json:"authTag"`
	Ciphertext string `json:"ciphertext"`
}

// isEncryptedField 值是否为 $wbEncrypted 信封形态（不碰密钥，纯形态判定）。
func isEncryptedField(v any) bool {
	m, ok := v.(map[string]any)
	if !ok {
		return false
	}
	f, ok := m["$wbEncrypted"].(float64)
	if !ok || f != 1 {
		return false
	}
	_, ok = m["envelope"].(string)
	return ok
}

// aadForKeyID 构造 sym-v1 field framing AAD（与配方逐字节一致，单测有 golden）。
func aadForKeyID(keyID string) []byte {
	aad := []byte("WB-AAD\x00")
	aad = append(aad, 0x01)
	aad = append(aad, lp("WBEV1")...)
	aad = append(aad, lp("sym-v1")...)
	var u [4]byte
	binary.BigEndian.PutUint32(u[:], 1)
	aad = append(aad, u[:]...)
	aad = append(aad, lp(keyID)...)
	aad = append(aad, 0x02, 0x00, 0x00)
	return aad
}

// lp = u32be(len)+bytes。
func lp(s string) []byte {
	var u [4]byte
	binary.BigEndian.PutUint32(u[:], uint32(len(s)))
	return append(u[:], s...)
}

// parseWbEnvelope 解信封结构（不碰密钥：形态坏的假信封在此失败，不触发 exe）。
func parseWbEnvelope(raw json.RawMessage) (*wbEnvelope, error) {
	var outer map[string]any
	if err := json.Unmarshal(raw, &outer); err != nil || !isEncryptedField(outer) {
		return nil, fmt.Errorf("非 $wbEncrypted 信封形态")
	}
	enc, _ := outer["envelope"].(string)
	inner, err := base64.StdEncoding.DecodeString(enc)
	if err != nil {
		return nil, fmt.Errorf("信封 base64 损坏")
	}
	var env wbEnvelope
	if err := json.Unmarshal(inner, &env); err != nil {
		return nil, fmt.Errorf("信封内层 JSON 损坏")
	}
	if env.Suite != 1 {
		return nil, fmt.Errorf("不支持的加密 suite=%d", env.Suite)
	}
	if len(env.KeyID) != 16 {
		return nil, fmt.Errorf("信封 keyId 形态异常")
	}
	return &env, nil
}

// decryptEnvelopeRaw 解一个信封 raw（region 只定候选顺序；keyId 匹配才解）。
func decryptEnvelopeRaw(raw json.RawMessage, region Region) (string, error) {
	env, err := parseWbEnvelope(raw)
	if err != nil {
		return "", err
	}
	nonce, err := base64.StdEncoding.DecodeString(env.Nonce)
	if err != nil || len(nonce) != 12 {
		return "", fmt.Errorf("信封 nonce 异常")
	}
	tag, err := base64.StdEncoding.DecodeString(env.AuthTag)
	if err != nil || len(tag) != 16 {
		return "", fmt.Errorf("信封 authTag 异常")
	}
	ct, err := base64.StdEncoding.DecodeString(env.Ciphertext)
	if err != nil {
		return "", fmt.Errorf("信封 ciphertext 异常")
	}
	keys, err := ensureAtrestKeys(region)
	if err != nil {
		return "", err
	}
	for _, k := range keys {
		if k.keyID != env.KeyID {
			continue
		}
		block, err := aes.NewCipher(k.key[:])
		if err != nil {
			return "", fmt.Errorf("本地 AES 初始化失败")
		}
		gcm, err := cipher.NewGCM(block)
		if err != nil {
			return "", fmt.Errorf("本地 GCM 初始化失败")
		}
		pt, err := gcm.Open(nil, nonce, append(ct, tag...), aadForKeyID(env.KeyID))
		if err != nil {
			return "", fmt.Errorf("信封解密失败（文件损坏或桌面端已换密钥）")
		}
		return string(pt), nil
	}
	return "", fmt.Errorf("信封 keyId 与本机派生不一致（桌面端可能已更换密钥）")
}

// DecryptAuthField 解 .info 里的字段值：明文原样回；信封解密；其他置空不报错
//（昵称等显示字段沿用旧 flex 语义）。
func DecryptAuthField(raw json.RawMessage, region Region) (string, error) {
	var s string
	if err := json.Unmarshal(raw, &s); err == nil {
		return s, nil
	}
	var v any
	if err := json.Unmarshal(raw, &v); err != nil {
		return "", nil
	}
	if !isEncryptedField(v) {
		return "", nil
	}
	return decryptEnvelopeRaw(raw, region)
}

// EncryptLike 把明文加密回 $wbEncrypted 信封：originalRaw 若为信封则沿用其
// keyId 对应的本机 key（桌面端可读）；明文原文件则用首选 key。返回外层对象。
func EncryptLike(originalRaw json.RawMessage, plaintext string, region Region) (map[string]any, error) {
	keys, err := ensureAtrestKeys(region)
	if err != nil {
		return nil, err
	}
	key := keys[0]
	if len(originalRaw) > 0 {
		if env, perr := parseWbEnvelope(originalRaw); perr == nil {
			for _, k := range keys {
				if k.keyID == env.KeyID {
					key = k
					break
				}
			}
		}
	}
	block, err := aes.NewCipher(key.key[:])
	if err != nil {
		return nil, fmt.Errorf("本地 AES 初始化失败")
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return nil, fmt.Errorf("本地 GCM 初始化失败")
	}
	var nonce [12]byte
	if _, err := rand.Read(nonce[:]); err != nil {
		return nil, fmt.Errorf("随机 nonce 失败")
	}
	sealed := gcm.Seal(nil, nonce[:], []byte(plaintext), aadForKeyID(key.keyID))
	inner := map[string]any{
		"suite":      1,
		"keyId":      key.keyID,
		"nonce":      base64.StdEncoding.EncodeToString(nonce[:]),
		"authTag":    base64.StdEncoding.EncodeToString(sealed[len(sealed)-16:]),
		"ciphertext": base64.StdEncoding.EncodeToString(sealed[:len(sealed)-16]),
	}
	ib, err := json.Marshal(inner)
	if err != nil {
		return nil, fmt.Errorf("信封组装失败")
	}
	return map[string]any{
		"$wbEncrypted": 1,
		"envelope":     base64.StdEncoding.EncodeToString(ib),
	}, nil
}

// desensToken 脱敏描述（日志/汇报用）：JWT 头部前 20 字＋长度，零原文。
// 省略号用 ASCII（cmd 重定向日志是 UTF-16，宽字符会被拆散，见 truncate 同款）。
func desensToken(pt string) string {
	head := pt
	if len(head) > 20 {
		head = head[:20]
	}
	return fmt.Sprintf("JWT头部=%s... 长度=%d", head, len(pt))
}
