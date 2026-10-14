//! util.rs —— 容错解包与类型收敛小工具（对应 Go `internal/vibex/util.go`）。
//!
//! 全部按 Go 的口径来：数字一律当 `float64`（`encoding/json` 解进 `any` 的结果），
//! 容器类值走 `fmt.Sprint` 风格的字符串化（`str`/`shapeOf` 里会用到）。
use serde_json::{Map, Value};

/// `str(v any)`：nil→""；string→原样；float64→`trimFloat`；bool→"true"/"false"；
/// 其它→`fmt.Sprint`。
pub fn str_v(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => trim_float(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        other => go_sprint(other),
    }
}

/// 便捷入口：`map.get(k)` 的版本（缺键与 nil 同义）。
pub fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(v) => str_v(v),
        None => String::new(),
    }
}

/// `trimFloat`：整数值显示成整数串（`403.0`→`"403"`），否则走 Go 的 `fmt.Sprint(float64)`。
pub fn trim_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.223372036854776e18 {
        return format!("{}", f as i64);
    }
    go_float_v(f)
}

/// Go `fmt.Sprint(float64)`（即 `%v` → `strconv.FormatFloat(f,'g',-1,64)`）的近似：
/// 定点区间 `1e-4 <= |f| < 1e21` 用最短十进制，之外用科学计数法（Go 的 `e` 指数带符号、
/// 不补零）。
fn go_float_v(f: f64) -> String {
    if !f.is_finite() {
        return if f.is_nan() {
            "NaN".into()
        } else if f > 0.0 {
            "+Inf".into()
        } else {
            "-Inf".into()
        };
    }
    let abs = f.abs();
    if abs == 0.0 || (abs >= 1e-4 && abs < 1e21) {
        return format!("{}", f);
    }
    let s = format!("{:e}", f);
    let (mant, exp) = s.split_once('e').unwrap_or((s.as_str(), "0"));
    let mut exp = exp.to_string();
    let neg = exp.starts_with('-');
    if neg || exp.starts_with('+') {
        exp.remove(0);
    }
    let trimmed = exp.trim_start_matches('0');
    let exp = if trimmed.is_empty() { "0" } else { trimmed };
    format!("{}e{}{}", mant, if neg { '-' } else { '+' }, exp)
}

/// Go `fmt.Sprint` 对容器值的近似（map 键排序、空格分隔、`map[k:v]`/`[a b]`）。
fn go_sprint(v: &Value) -> String {
    match v {
        Value::Null => "<nil>".into(),
        Value::Bool(b) => format!("{b}"),
        Value::Number(n) => go_float_v(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(go_sprint).collect();
            format!("[{}]", parts.join(" "))
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", k, go_sprint(&m[*k])))
                .collect();
            format!("map[{}]", parts.join(" "))
        }
    }
}

/// `truthy`：nil→false；bool；float64≠0；string≠""；其它→true。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `toMap`：对象才返回，其它 None。
pub fn to_map(v: &Value) -> Option<&Map<String, Value>> {
    v.as_object()
}

/// `jsonUnmarshal` 的容错语义：解析失败返回 `Value::Null`（Go 返回 nil interface）。
pub fn json_unmarshal(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

/// `shapeOf`：`array[N]` / `map{k[],k{},k}`（键排序）/ `non-container`。
pub fn shape_of(v: &Value) -> String {
    match v {
        Value::Array(items) => format!("array[{}]", items.len()),
        Value::Object(m) => {
            let mut keys: Vec<String> = Vec::with_capacity(m.len());
            for (k, item) in m {
                match item {
                    Value::Array(inner) => keys.push(format!("{k}[]=+{}", inner.len())),
                    Value::Object(_) => keys.push(format!("{k}{{}}")),
                    _ => keys.push(k.clone()),
                }
            }
            keys.sort();
            format!("map{{{}}}", keys.join(","))
        }
        _ => "non-container".into(),
    }
}

/// `unwrapObj`：顶层若含 `.data`/`.app` 对象则下沉一层。
pub fn unwrap_obj(v: &Value) -> Value {
    if let Some(m) = to_map(v) {
        for k in ["data", "app"] {
            if let Some(inner) = m.get(k).and_then(|x| x.as_object()) {
                return Value::Object(inner.clone());
            }
        }
    }
    v.clone()
}

/// `appsFrom`：顶层数组 / `.apps` / `.items` / `.data`，且 `data` 里可再套一层 `apps`/`items`。
pub fn apps_from(v: &Value) -> Option<Vec<Value>> {
    if let Value::Array(items) = v {
        return Some(items.clone());
    }
    let m = to_map(v)?;
    for k in ["apps", "items", "data"] {
        match m.get(k) {
            Some(Value::Array(items)) => return Some(items.clone()),
            Some(Value::Object(inner)) => {
                for k2 in ["apps", "items"] {
                    if let Some(Value::Array(items)) = inner.get(k2) {
                        return Some(items.clone());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// `liveStatus`：`live` 是对象且**无 `error` 键**且 `live.status` 是字符串 → 它；
/// 否则 `status_cached`。
pub fn live_status(app: Option<&Map<String, Value>>) -> String {
    let app = match app {
        Some(a) => a,
        None => return String::new(),
    };
    if let Some(Value::Object(live)) = app.get("live") {
        if live.contains_key("error") {
            return str_of(app.get("status_cached"));
        }
        if let Some(Value::String(s)) = live.get("status") {
            return s.clone();
        }
        return str_of(app.get("status_cached"));
    }
    str_of(app.get("status_cached"))
}

/// `portsOK`：`host_ports` 的 `9000` 与 `7000` 都 `> 0`（数值型才算）。
pub fn ports_ok(app: &Map<String, Value>) -> bool {
    let hp = match app.get("host_ports").and_then(to_map) {
        Some(hp) => hp,
        None => return false,
    };
    num_ok(hp.get("9000")) && num_ok(hp.get("7000"))
}

fn num_ok(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f > 0.0).unwrap_or(false),
        _ => false,
    }
}

/// `initRootMeta`：9 个固定键（缺失仍以 `null` 出现）+ 条件性的 `flow`/`enabled_capabilities`。
pub fn init_root_meta(app: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for k in [
        "app_id",
        "name",
        "app_type",
        "image",
        "host_ip",
        "host_ports",
        "created_at",
        "pocketbase_url",
    ] {
        out.insert(k.to_string(), app.get(k).cloned().unwrap_or(Value::Null));
    }
    if let Some(Value::String(s)) = app.get("flow") {
        out.insert("flow".into(), Value::String(s.clone()));
    }
    if let Some(v @ Value::Array(_)) = app.get("enabled_capabilities") {
        out.insert("enabled_capabilities".into(), v.clone());
    }
    Value::Object(out)
}

/// `clip`：TrimSpace 后截前 n **字节**（Go 的切片是字节，可能切进多字节字符中间——
/// 这里如实照做，故返回原始 `Vec<u8>` 再由调用方按 Go 的字符串拼接语义处理）。
pub fn clip_bytes(s: &str, n: usize) -> Vec<u8> {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() <= n {
        b.to_vec()
    } else {
        b[..n].to_vec()
    }
}

/// `clip` 的字符串形态（Go 侧是 `string(raw[:n])`，含非法 UTF-8 时 Go 的字符串仍原样携带
/// 那些字节；Rust 字符串不允许，故这里用 lossy 转换并在 README 登记这一处差异）。
pub fn clip(s: &str, n: usize) -> String {
    String::from_utf8_lossy(&clip_bytes(s, n)).to_string()
}

/// `firstNonEmpty`。
pub fn first_non_empty(vals: &[&str]) -> String {
    for v in vals {
        if !v.is_empty() {
            return (*v).to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn str_and_trim_float_like_go() {
        assert_eq!(str_v(&json!(null)), "");
        assert_eq!(str_v(&json!("hi")), "hi");
        assert_eq!(str_v(&json!(403)), "403");
        assert_eq!(str_v(&json!(403.0)), "403");
        assert_eq!(str_v(&json!(403.5)), "403.5");
        assert_eq!(str_v(&json!(true)), "true");
        assert_eq!(str_v(&json!(false)), "false");
        assert_eq!(str_v(&json!(1000)), "1000");
    }

    #[test]
    fn truthy_like_go() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!(false)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!(true)));
        assert!(truthy(&json!(-1)));
        assert!(truthy(&json!("x")));
        assert!(truthy(&json!([1])));
    }

    #[test]
    fn apps_from_and_unwrap() {
        assert_eq!(apps_from(&json!([{"a": 1}])).unwrap().len(), 1);
        assert_eq!(apps_from(&json!({"apps": [1, 2]})).unwrap().len(), 2);
        assert_eq!(apps_from(&json!({"data": {"apps": [1]}})).unwrap().len(), 1);
        assert_eq!(apps_from(&json!({"data": {"providers": []}})), None);
        assert_eq!(apps_from(&json!({"code": 0})), None);

        assert_eq!(
            unwrap_obj(&json!({"data": {"app_id": "a"}, "app_id": "t"})),
            json!({"app_id": "a"})
        );
        assert_eq!(unwrap_obj(&json!({"app_id": "c"})), json!({"app_id": "c"}));
    }

    #[test]
    fn live_status_and_ports() {
        assert_eq!(
            live_status(json!({"live": {"status": "running"}}).as_object()),
            "running"
        );
        assert_eq!(
            live_status(
                json!({"live": {"error": "x", "status": "running"}, "status_cached": "c"}).as_object()
            ),
            "c"
        );
        assert_eq!(live_status(json!({"status_cached": "c"}).as_object()), "c");
        assert_eq!(live_status(None), "");

        assert!(ports_ok(json!({"host_ports": {"9000": 1, "7000": 2}}).as_object().unwrap()));
        assert!(!ports_ok(json!({"host_ports": {"9000": 0, "7000": 2}}).as_object().unwrap()));
        assert!(!ports_ok(json!({"host_ports": {"9000": "1", "7000": 2}}).as_object().unwrap()));
        assert!(!ports_ok(json!({}).as_object().unwrap()));
    }

    #[test]
    fn shape_of_like_go() {
        assert_eq!(shape_of(&json!([1, 2])), "array[2]");
        assert_eq!(shape_of(&json!({"code": 0, "msg": "ok"})), "map{code,msg}");
        assert_eq!(shape_of(&json!("s")), "non-container");
        assert_eq!(shape_of(&json!({"a": [], "b": {}})), "map{a[]=+0,b{}}");
    }
}
