// util.go —— 容错解包与类型收敛小工具（对齐 vibex2api.py 的 _loads/_unwrap_obj/_apps_from 等）。
package vibex

import (
	"encoding/json"
	"fmt"
)

// str 把任意 JSON 值收敛成字符串（数字/布尔用 fmt.Sprint，nil 给空串）。
func str(v any) string {
	switch t := v.(type) {
	case nil:
		return ""
	case string:
		return t
	case float64:
		return trimFloat(t)
	case bool:
		if t {
			return "true"
		}
		return "false"
	default:
		return fmt.Sprint(t)
	}
}

// trimFloat 让 403.0 显示成 "403"（错误码比较/展示都要）。
func trimFloat(f float64) string {
	if f == float64(int64(f)) {
		return fmt.Sprint(int64(f))
	}
	return fmt.Sprint(f)
}

// truthy 报告 JSON 值是否为真（isApiErrorMessage / balance_insufficient 用）。
func truthy(v any) bool {
	switch t := v.(type) {
	case nil:
		return false
	case bool:
		return t
	case float64:
		return t != 0
	case string:
		return t != ""
	default:
		return true
	}
}

// toMap 把 JSON 值收敛成 map。
func toMap(v any) map[string]any {
	m, _ := v.(map[string]any)
	return m
}

// toList 把 JSON 值收敛成切片。
func toList(v any) []any {
	l, _ := v.([]any)
	return l
}

// jsonUnmarshal 容错解析：失败返回 nil（对应 python 的 _loads）。
func jsonUnmarshal(b []byte) any {
	var v any
	if json.Unmarshal(b, &v) != nil {
		return nil
	}
	return v
}

// unwrapObj app 对象可能在顶层，也可能被 .data / .app 包裹。
func unwrapObj(v any) any {
	if m := toMap(v); m != nil {
		for _, k := range []string{"data", "app"} {
			if inner := toMap(m[k]); inner != nil {
				return inner
			}
		}
	}
	return v
}

// appsFrom 容错解包 apps 列表（顶层数组 / .apps / .items / .data，data 里可再套一层）。
func appsFrom(v any) []any {
	if list, ok := v.([]any); ok {
		return list
	}
	m := toMap(v)
	if m == nil {
		return nil
	}
	for _, k := range []string{"apps", "items", "data"} {
		switch inner := m[k].(type) {
		case []any:
			return inner
		case map[string]any:
			for _, k2 := range []string{"apps", "items"} {
				if list, ok := inner[k2].([]any); ok {
					return list
				}
			}
		}
	}
	return nil
}

// liveStatus 取 live.status（须是 dict/str 且无 error 键），否则 status_cached。
func liveStatus(app map[string]any) string {
	if app == nil {
		return ""
	}
	if live := toMap(app["live"]); live != nil {
		if _, hasErr := live["error"]; !hasErr {
			if s, ok := live["status"].(string); ok {
				return s
			}
		}
		return str(app["status_cached"])
	}
	return str(app["status_cached"])
}

// portsOK 报告 host_ports 里 9000 与 7000 是否都 > 0。
func portsOK(app map[string]any) bool {
	hp := toMap(app["host_ports"])
	if hp == nil {
		return false
	}
	return numOK(hp["9000"]) && numOK(hp["7000"])
}

func numOK(v any) bool {
	switch t := v.(type) {
	case float64:
		return t > 0
	case int:
		return t > 0
	case int64:
		return t > 0
	case json.Number:
		f, err := t.Float64()
		return err == nil && f > 0
	}
	return false
}

// initRootMeta 从 app 对象提取 init_root 的 meta（SPEC-T1 §3.5）。
func initRootMeta(app map[string]any) map[string]any {
	meta := map[string]any{}
	for _, k := range []string{"app_id", "name", "app_type", "image", "host_ip",
		"host_ports", "created_at", "pocketbase_url"} {
		meta[k] = app[k]
	}
	if s, ok := app["flow"].(string); ok {
		meta["flow"] = s
	}
	if list, ok := app["enabled_capabilities"].([]any); ok {
		meta["enabled_capabilities"] = list
	}
	return meta
}
