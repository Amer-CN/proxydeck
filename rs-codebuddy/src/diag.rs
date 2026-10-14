//! diag.rs —— Go `server.go` 的三段现场诊断：`requestFingerprint` / `summarizeToolsForDiag` /
//! `dumpRequestBody`。
//!
//! 三者的**文本形态**都被黄金门禁逐字锁住（指纹串里有 `fmt.Sprintf("%v")` 的输出，
//! Go 与 Rust 的默认格式化不同，必须自己照 Go 的规则写）：
//! * `%v` 打 `map[string]int` → 键**排序**后 `map[a:1 b:2]`；
//! * `%v` 打 `[]string` → `[a b]`（空切片是 `[]`）；
//! * `%v` 打 `any` → nil 是 `<nil>`；float64 走 `%g`（`strconv.FormatFloat(f,'g',-1,64)`，
//!   指数区间 `exp < -4 || exp >= 21` 用 `1e+06` 形，其余定点）；
//! * `sha=%x` 是**整份出站体** `json.Marshal` 后 sha256 的**前 6 字节**十六进制。

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::gjson;
use crate::godecode::GoMap;

/// Go `requestFingerprint`：给出站请求体打一份**不含正文**的可 grep 指纹。
pub fn request_fingerprint(body: &GoMap) -> String {
    let mut role_tally: Map<String, Value> = Map::new();
    let msgs: &[Value] = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    for m in msgs {
        if let Some(mm) = m.as_object() {
            let role = mm.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let cur = role_tally.get(role).and_then(|v| v.as_i64()).unwrap_or(0);
            role_tally.insert(role.to_string(), Value::Number((cur + 1).into()));
        }
    }
    let mut names: Vec<String> = Vec::new();
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            let Some(tm) = t.as_object() else { continue };
            let Some(fnv) = tm.get("function").and_then(|v| v.as_object()) else {
                continue;
            };
            if let Some(n) = fnv.get("name").and_then(|v| v.as_str()) {
                names.push(n.to_string());
            }
        }
    }
    let keys: Vec<String> = {
        let mut k: Vec<String> = body.keys().cloned().collect();
        k.sort();
        k
    };
    let raw = gjson::to_go(&Value::Object(body.clone()));
    let sum = Sha256::digest(raw.as_bytes());
    format!(
        "bytes={} sha={} msgs={} roles={} keys={} tools={} tool_names={} effort={} max_tokens={} stream={}",
        raw.len(),
        hex_lower(&sum[..6]),
        msgs.len(),
        go_fmt_map(&role_tally),
        go_fmt_list(&keys),
        names.len(),
        go_fmt_list(&names),
        go_fmt_any(body.get("reasoning_effort").unwrap_or(&Value::Null)),
        go_fmt_any(body.get("max_tokens").unwrap_or(&Value::Null)),
        go_fmt_any(body.get("stream").unwrap_or(&Value::Null)),
    )
}

fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

/// Go `fmt` 打 `map[string]int`：键排序、`k:v` 空格分隔。
fn go_fmt_map(m: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let body: Vec<String> = keys
        .iter()
        .map(|k| format!("{}:{}", k, m[*k].as_i64().unwrap_or(0)))
        .collect();
    format!("map[{}]", body.join(" "))
}

/// Go `fmt` 打 `[]string`。
fn go_fmt_list(items: &[String]) -> String {
    format!("[{}]", items.join(" "))
}

/// Go `fmt.Sprintf("%v", v)` 在本合同上会出现的形态。
pub fn go_fmt_any(v: &Value) -> String {
    match v {
        Value::Null => "<nil>".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_f64() {
            Some(f) => go_fmt_float_g(f),
            None => "0".to_string(),
        },
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(go_fmt_any).collect();
            format!("[{}]", parts.join(" "))
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", k, go_fmt_any(&m[*k])))
                .collect();
            format!("map[{}]", parts.join(" "))
        }
    }
}

/// `strconv.FormatFloat(f, 'g', -1, 64)`（Go `%v` 打 float64 用的就是它）。
pub fn go_fmt_float_g(v: f64) -> String {
    if !v.is_finite() {
        return match v.is_nan() {
            true => "NaN".to_string(),
            false if v.is_sign_negative() => "-Inf".to_string(),
            false => "+Inf".to_string(),
        };
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }
    let e = format!("{:e}", v);
    let (mant, exp) = e.split_once('e').expect("`{:e}` 必带 e");
    let exp: i32 = exp.parse().unwrap_or(0);
    // Go 的 `strconv.FormatFloat(f,'g',-1,64)`：`shortest` 时把决策精度当 6 用
    // （`formatDigits` 里 `if shortest { eprec = 6 }`），故 `exp < -4 || exp >= 6` 走指数形。
    // 与 `encoding/json` 的阈值（`abs < 1e-6 || abs >= 1e21`）**不是**同一条规则，别混用。
    if exp < -4 || exp >= 6 {
        format!(
            "{}e{}{:02}",
            mant,
            if exp < 0 { "-" } else { "+" },
            exp.abs()
        )
    } else {
        format!("{}", v)
    }
}

/// Go `summarizeToolsForDiag`（11129 结构诊断；只取出站 tools 定义与 tool_choice）。
pub fn summarize_tools_for_diag(backend_body: &GoMap) -> String {
    let tools: &[Value] = backend_body
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let mut summary: Vec<Value> = Vec::with_capacity(tools.len());
    for (i, raw) in tools.iter().enumerate() {
        let Some(tm) = raw.as_object() else {
            summary.push(gjson::obj(vec![
                ("index", Value::Number((i as i64).into())),
                ("note", gjson::s("non-object tool")),
            ]));
            continue;
        };
        let mut entry: Map<String, Value> = Map::new();
        entry.insert("index".to_string(), Value::Number((i as i64).into()));
        if let Some(t) = tm.get("type").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                entry.insert("type".to_string(), gjson::s(t));
            }
        }
        if let Some(fnv) = tm.get("function").and_then(|v| v.as_object()) {
            if let Some(name) = fnv.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    entry.insert("name".to_string(), gjson::s(name));
                }
            }
            if let Some(desc) = fnv.get("description").and_then(|v| v.as_str()) {
                if !desc.is_empty() {
                    entry.insert("description".to_string(), gjson::s(desc));
                }
            }
            match fnv.get("parameters") {
                Some(p) if !p.is_null() => {
                    entry.insert("parameters".to_string(), p.clone());
                }
                _ => {
                    entry.insert("parameters".to_string(), gjson::s("missing"));
                }
            }
            if let Some(strict) = fnv.get("strict") {
                if !strict.is_null() {
                    entry.insert("strict".to_string(), strict.clone());
                }
            }
        } else {
            if let Some(name) = tm.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    entry.insert("name".to_string(), gjson::s(name));
                }
            }
            if let Some(p) = tm.get("parameters") {
                if !p.is_null() {
                    entry.insert("parameters".to_string(), p.clone());
                }
            }
        }
        summary.push(Value::Object(entry));
    }
    let mut out: Map<String, Value> = Map::new();
    out.insert(
        "tool_count".to_string(),
        Value::Number((tools.len() as i64).into()),
    );
    out.insert("tools".to_string(), Value::Array(summary));
    if let Some(tc) = backend_body.get("tool_choice") {
        if !tc.is_null() {
            out.insert("tool_choice".to_string(), tc.clone());
        }
    }
    gjson::to_go(&Value::Object(out))
}

/// Go `dumpRequestBody`：把出站请求体整份落到数据目录（0600）。上限 10 份；写失败一律
/// 静默（诊断绝不能影响转发）。返回落盘路径（未落盘返回空串）。
///
/// `dir` 由调用方给定（生产 = 启动时解析的 `appdir.data_dir()`；测试 = 临时目录），
/// 对齐 Go 时**只看「同一目录、同一命名、同样 10 份上限」这三条合同**。
pub fn dump_request_body(body: &GoMap, dir: &std::path::Path) -> String {
    let mut existing = 0usize;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("diag-11133-") && name.ends_with(".json") {
                existing += 1;
            }
        }
    }
    if existing >= 10 {
        return String::new();
    }
    let raw = gjson::to_go(&Value::Object(body.clone()));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = dir.join(format!("diag-11133-{}.json", stamp));
    if std::fs::create_dir_all(dir).is_err() {
        return String::new();
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(&p) {
        Ok(mut f) => {
            use std::io::Write;
            if f.write_all(raw.as_bytes()).is_err() {
                return String::new();
            }
            p.to_string_lossy().to_string()
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> GoMap {
        crate::godecode::unmarshal_map(s.as_bytes()).unwrap()
    }

    // 期望值由真 Go 采收（tests/golden/golden-codebuddy-go.json 的 pure.fingerprint 段）
    #[test]
    fn fingerprint_matches_go_samples() {
        assert_eq!(
            request_fingerprint(&parse(
                r#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":true}"#
            )),
            "bytes=74 sha=625943e2c106 msgs=1 roles=map[user:1] keys=[messages model stream] tools=0 tool_names=[] effort=<nil> max_tokens=<nil> stream=true"
        );
        assert_eq!(
            request_fingerprint(&parse(r#"{"model":"auto"}"#)),
            "bytes=16 sha=3263355984f8 msgs=0 roles=map[] keys=[model] tools=0 tool_names=[] effort=<nil> max_tokens=<nil> stream=<nil>"
        );
        assert_eq!(
            request_fingerprint(&parse(
                r#"{"model":"auto","messages":[{"role":"user","content":"<a>&b"}],"stream":true}"#
            )),
            "bytes=92 sha=f1534c174b38 msgs=1 roles=map[user:1] keys=[messages model stream] tools=0 tool_names=[] effort=<nil> max_tokens=<nil> stream=true"
        );
    }

    #[test]
    fn go_fmt_matches_go() {
        assert_eq!(go_fmt_float_g(4096.0), "4096");
        assert_eq!(go_fmt_float_g(0.7), "0.7");
        assert_eq!(go_fmt_float_g(1_000_000.0), "1e+06");
        assert_eq!(go_fmt_float_g(1e-5), "1e-05");
        assert_eq!(go_fmt_float_g(1e21), "1e+21");
        assert_eq!(go_fmt_float_g(-0.0), "-0");
    }
}
