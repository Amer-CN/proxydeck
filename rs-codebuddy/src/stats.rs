//! stats.rs —— 消耗统计（`internal/codebuddy/server.go` 的 `modelStat` / `addStat` /
//! `loadStats` / `saveStats` / `handleStats` 逐字移植）。
//!
//! 口径（以 Go 现状为准）：
//! * 三者（in/out/total）全 0 不入账；
//! * `total>0 ? total : in+out`；
//! * **每笔 addStat 之后立即原子落盘**（tmp + rename）——Go 侧**没有节流**（简报提到的
//!   「节流落盘」在本仓 `internal/codebuddy` 里不存在；`internal/proxy/stats.go` 的 ≥2s 节流
//!   属主代理，不属本插件），故此处如实对齐 Go 现状，不额外加节流；
//! * 落盘结构是 `map[model]*modelStat`，字段声明序恰好等于键序（calls/inputTokens/
//!   outputTokens/totalTokens）——所以 Go 的结构体序列化与 Rust 的键序序列化**逐字节相同**。
//! * 读盘时字段按 Go 的 `int64` 字面量规则解（`strconv.ParseInt`：拒绝 `1e2`/`1.0`/溢出，
//!   `null` 是 no-op），见 `load_from_disk` 与 `int64_literals_follow_go_parse_int`。
//!
//! **与 Go 的一处差异（登记在 README）**：文件内容为 JSON `null` 时，Go 的
//! `loadStats` 会把 `s.stats` 换成 nil map，随后第一次 `addStat` 直接
//! `panic: assignment to entry in nil map`（与 bai/P1 发现的同一族缺陷）。Rust 侧一律按
//! 「非法形态 → 空表」处理、不 panic。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::gjson;

/// `modelStat` 单模型用量累计。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelStat {
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

impl ModelStat {
    fn to_json(&self) -> Value {
        // 字段声明序 = 键序（calls/inputTokens/outputTokens/totalTokens）。
        gjson::obj(vec![
            ("calls", Value::Number(self.calls.into())),
            ("inputTokens", Value::Number(self.input_tokens.into())),
            ("outputTokens", Value::Number(self.output_tokens.into())),
            ("totalTokens", Value::Number(self.total_tokens.into())),
        ])
    }
}

/// 统计表（对应 Go `Server.stats` + `statsMu` + `statsPath`）。
pub struct Stats {
    models: Mutex<BTreeMap<String, ModelStat>>,
    path: PathBuf,
}

impl Stats {
    /// 空表 + 落盘路径（`path` 为空 = 不落盘，供测试缝用）。
    pub fn new(path: PathBuf) -> Stats {
        Stats {
            models: Mutex::new(BTreeMap::new()),
            path,
        }
    }

    /// 启动时读回历史统计（`loadStats`）：缺失/坏 JSON → 保持当前表（空表）。
    pub fn load(path: PathBuf) -> Stats {
        let s = Stats::new(path);
        s.load_from_disk();
        s
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 启动时读回历史统计（`loadStats`）：缺失/坏 JSON → 保持当前表（空表）。
    ///
    /// 逐字段按 **Go `json.Unmarshal` 的匹配规则**解析：键名大小写不敏感（tag 名或结构体
    /// 字段名都认）、未知字段忽略、**任一字段类型不对 → 整份按失败处理**（保持空表）。
    /// 值为 `null` 时 Go 对非指针标量是 **no-op**（字段保持零值、不算错），这里照做。
    fn load_from_disk(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let Ok(b) = std::fs::read(&self.path) else {
            return;
        };
        // 顶层不是对象（含 `null`）→ Go 会把它解成 nil map（随后 addStat panic），
        // 本实现按「非法形态 → 空表」处理、不 panic（差异登记在 README）。
        let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(&b) else {
            return;
        };
        let mut out: BTreeMap<String, ModelStat> = BTreeMap::new();
        for (k, v) in &root {
            let mut st = ModelStat::default();
            match v {
                // Go 的 `map[string]*modelStat` 允许 null 条目（指针为 nil）。
                Value::Null => {}
                Value::Object(entry) => {
                    for (fk, fv) in entry {
                        let field = match fk.to_ascii_lowercase().as_str() {
                            "calls" => 0,
                            "inputtokens" | "inputtok" => 1,
                            "outputtokens" | "outputtok" => 2,
                            "totaltokens" | "totaltok" => 3,
                            _ => continue,
                        };
                        // Go 的 `int64` 字段解码 = `strconv.ParseInt(字面量, 10, 64)`：
                        // **只认十进制整数字面量**——`1e2` / `1.0` / `1.5` 这类带指数或小数点
                        // 的字面量报错，超出 int64 也报错（任一字段出错 ⇒ 整份加载失败）。
                        // serde_json 的 `Number` 保留「整数形 / 浮点形」之分（带 `.`/`e`，或
                        // 超出 u64 的字面量一律落成 `Float`），所以 `as_i64()` 恰好等价于
                        // Go 的判据；**不能**走 `as_f64()`（那会接受 `1e2`、对大整数丢精度、
                        // 对溢出饱和）。期望值由真 Go 采收：`.work/cli/jsonint`。
                        if fv.is_null() {
                            continue; // Go：`null` 对非指针标量是 no-op（字段保持零值）
                        }
                        let Some(val) = fv.as_i64() else { return };
                        match field {
                            0 => st.calls = val,
                            1 => st.input_tokens = val,
                            2 => st.output_tokens = val,
                            _ => st.total_tokens = val,
                        }
                    }
                }
                _ => return,
            }
            out.insert(k.clone(), st);
        }
        *self.models.lock().unwrap_or_else(|e| e.into_inner()) = out;
    }

    /// `addStat`：累计一次调用的用量并落盘。
    pub fn add_stat(&self, model: &str, input: i64, output: i64, total: i64) {
        if total == 0 && input == 0 && output == 0 {
            return;
        }
        {
            let mut m = self.models.lock().unwrap_or_else(|e| e.into_inner());
            let st = m.entry(model.to_string()).or_default();
            st.calls += 1;
            st.input_tokens += input;
            st.output_tokens += output;
            if total > 0 {
                st.total_tokens += total;
            } else {
                st.total_tokens += input + output;
            }
        }
        self.save();
        println!("[codebuddy] stat model={model} in={input} out={output}");
    }

    /// `saveStats`：原子落盘（tmp + rename）。写盘失败只丢统计、不报错。
    pub fn save(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let text = {
            let m = self.models.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = String::from("{");
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                gjson::push_go_string(&mut out, k);
                out.push(':');
                out.push_str(&gjson::to_go(&v.to_json()));
            }
            out.push('}');
            out
        };
        let tmp = format!("{}.tmp", self.path.display());
        if std::fs::write(&tmp, text.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// `handleStats` 的响应体：`{"models":{…},"uptimeSec":N}`（键序 = Go 的 map 键序）。
    pub fn response(&self, uptime_secs: i64) -> Value {
        let mut models = Map::new();
        {
            let m = self.models.lock().unwrap_or_else(|e| e.into_inner());
            for (k, v) in m.iter() {
                models.insert(k.clone(), v.to_json());
            }
        }
        gjson::obj(vec![
            ("models", Value::Object(models)),
            ("uptimeSec", Value::Number(uptime_secs.into())),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_skips_zero_and_round_trips_file() {
        let dir = std::env::temp_dir().join(format!(
            "rs-codebuddy-stats-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("codebuddy-stats.json");
        let s = Stats::new(p.clone());
        s.add_stat("m", 0, 0, 0); // 全 0 不入账
        assert_eq!(gjson::to_go(&s.response(0)), r#"{"models":{},"uptimeSec":0}"#);
        s.add_stat("m", 7, 2, 9);
        s.add_stat("m", 1, 1, 0); // total=0 → in+out
        assert_eq!(
            gjson::to_go(&s.response(3)),
            r#"{"models":{"m":{"calls":2,"inputTokens":8,"outputTokens":3,"totalTokens":11}},"uptimeSec":3}"#
        );
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            on_disk,
            r#"{"m":{"calls":2,"inputTokens":8,"outputTokens":3,"totalTokens":11}}"#
        );
        // 读回
        let s2 = Stats::load(p.clone());
        assert_eq!(gjson::to_go(&s2.response(0)), gjson::to_go(&s.response(0)));
        // 坏形态（含 JSON null）一律按空表，不 panic
        std::fs::write(&p, b"null").unwrap();
        let s3 = Stats::load(p.clone());
        s3.add_stat("m", 1, 1, 2);
        assert_eq!(
            gjson::to_go(&s3.response(0)),
            r#"{"models":{"m":{"calls":1,"inputTokens":1,"outputTokens":1,"totalTokens":2}},"uptimeSec":0}"#
        );
        std::fs::write(&p, b"{not json").unwrap();
        let s4 = Stats::load(p.clone());
        assert_eq!(gjson::to_go(&s4.response(0)), r#"{"models":{},"uptimeSec":0}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `int64` 字段的字面量规则（Go = `strconv.ParseInt(字面量, 10, 64)`）：
    /// **只认十进制整数字面量**，`1e2` / `1.0` / 溢出**一律报错 ⇒ 整份加载失败**（保持空表）；
    /// 值为 `null` 时 Go 对非指针标量是 no-op（字段保持零值）。期望值由真 Go 采收：
    /// `.work/cli/jsonint/main.go`（`go run ./.work/cli/jsonint`）。
    #[test]
    fn int64_literals_follow_go_parse_int() {
        let dir = std::env::temp_dir().join(format!(
            "rs-codebuddy-int64-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("codebuddy-stats.json");
        let loads = |lit: &str| -> bool {
            std::fs::write(
                &p,
                format!(r#"{{"m":{{"calls":1,"inputTokens":{lit},"outputTokens":0,"totalTokens":0}}}}"#),
            )
            .unwrap();
            !Stats::load(p.clone()).response(0)["models"]
                .as_object()
                .unwrap()
                .is_empty()
        };

        for good in [
            "100",
            "-100",
            "0",
            "9223372036854775807",
            "-9223372036854775808",
        ] {
            assert!(loads(good), "Go 接受整数字面量 {good}，本实现也必须能加载");
        }
        for bad in [
            "1.0",
            "1e2",
            "1E2",
            "1e-2",
            "1.5",
            "-1e2",
            "0.0",
            "1e18",
            "1e19",
            "9223372036854775808",
            "-9223372036854775809",
            "18446744073709551615",
            "18446744073709551616",
            "true",
            r#""100""#,
        ] {
            assert!(!loads(bad), "Go 对 {bad} 报错（整份加载失败），本实现也必须拒绝");
        }

        // `null`：Go 对非指针标量是 no-op（字段保持零值），不报错。
        std::fs::write(
            &p,
            r#"{"m":{"calls":null,"inputTokens":1,"outputTokens":2,"totalTokens":3}}"#,
        )
        .unwrap();
        let s = Stats::load(p.clone());
        assert_eq!(
            gjson::to_go(&s.response(0)),
            r#"{"models":{"m":{"calls":0,"inputTokens":1,"outputTokens":2,"totalTokens":3}},"uptimeSec":0}"#
        );

        // 大整数必须按 int64 精确解出（不经 f64：不丢精度、不饱和）。
        std::fs::write(
            &p,
            r#"{"m":{"calls":9223372036854775807,"inputTokens":1,"outputTokens":1,"totalTokens":1}}"#,
        )
        .unwrap();
        let s = Stats::load(p.clone());
        assert_eq!(
            s.response(0)["models"]["m"]["calls"].as_i64(),
            Some(i64::MAX),
            "Go 的 int64 解码是精确的（`strconv.ParseInt`），不能经 f64 中转"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
