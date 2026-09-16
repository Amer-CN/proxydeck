// jwt.go —— Rh-Accesstoken 的 JWT payload 解码（SPEC-T1 §3.2）。
package vibex

import (
	"encoding/base64"
	"encoding/json"
	"strings"
	"time"
)

// jwtInfo 是解码后我们用得到的字段。
type jwtInfo struct {
	Sub      string    // sub（userId）
	Username string    // nickName / user_name / username / mobile
	Exp      time.Time // exp；零值 = 无 exp 或解析不出
}

// hasExp 报告 exp 是否有效。
func (j *jwtInfo) hasExp() bool { return j != nil && !j.Exp.IsZero() }

// expired 报告 token 是否已过期（无 exp 视为不过期，交由上游判 401）。
func (j *jwtInfo) expired(now time.Time) bool {
	return j.hasExp() && now.After(j.Exp)
}

// decodeJWT 解出 JWT 的 payload 段；不是合法 JWT 返回 nil。
func decodeJWT(token string) *jwtInfo {
	token = strings.TrimSpace(token)
	if token == "" {
		return nil
	}
	parts := strings.Split(token, ".")
	if len(parts) < 2 {
		return nil
	}
	seg := strings.TrimRight(parts[1], "=")
	raw, err := base64.RawURLEncoding.DecodeString(seg)
	if err != nil {
		return nil
	}
	var payload map[string]any
	if json.Unmarshal(raw, &payload) != nil {
		return nil
	}
	info := &jwtInfo{Sub: str(payload["sub"])}
	for _, k := range []string{"nickName", "user_name", "username", "mobile"} {
		if v := str(payload[k]); v != "" {
			info.Username = v
			break
		}
	}
	if info.Username == "" {
		info.Username = info.Sub
	}
	info.Exp = unixTime(payload["exp"])
	return info
}

// unixTime 把 JWT 里的 exp（JSON number）转成时间；非法值返回零值。
func unixTime(v any) time.Time {
	switch t := v.(type) {
	case float64:
		return time.Unix(int64(t), 0)
	case int64:
		return time.Unix(t, 0)
	case json.Number:
		if n, err := t.Int64(); err == nil {
			return time.Unix(n, 0)
		}
	}
	return time.Time{}
}
