//! stats.rs —— 本地消耗统计（对应 Go `internal/vibex/stats.go`）。
//!
//! C74~C76：`/v1/stats` + `vibex-stats.json`（**明文 JSON**，不是 DPAPI 密文——
//! Go 侧用的是 `os.ReadFile`/`os.WriteFile`，不是 `appdir.*`）。
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::gjson::{st, J};
use crate::logf;
use crate::prompt::int_of;
use crate::util::str_of;
use serde_json::{Map, Value};

/// 单模型用量累计（字段名与 bai/tuanjie/codebuddy 同形，GUI 共用一套渲染）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelStat {
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

impl ModelStat {
    /// Go struct 的字段顺序（`calls`/`inputTokens`/`outputTokens`/`totalTokens`）。
    pub fn to_j(&self) -> J {
        st(vec![
            ("calls", J::Int(self.calls)),
            ("inputTokens", J::Int(self.input_tokens)),
            ("outputTokens", J::Int(self.output_tokens)),
            ("totalTokens", J::Int(self.total_tokens)),
        ])
    }
}

/// 统计表 + 落盘路径。
pub struct Stats {
    table: Mutex<BTreeMap<String, ModelStat>>,
    pub path: PathBuf,
    /// 启动时刻（`uptimeSec` 用）。
    pub started_at_ms: i64,
}

impl Stats {
    /// 建统计表并读回历史（文件缺失/损坏即空表，不阻断启动）。
    pub fn new(path: PathBuf, started_at_ms: i64) -> Stats {
        let s = Stats {
            table: Mutex::new(BTreeMap::new()),
            path,
            started_at_ms,
        };
        s.load();
        s
    }

    /// `loadStats`：坏文件当空表；`null` 也当空表（Go 里是 `map` 变 nil，后续 addStat 有兜底）。
    fn load(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let b = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(_) => return,
        };
        let v = crate::util::json_unmarshal(&b);
        let obj = match v.as_object() {
            Some(o) => o,
            None => return,
        };
        let mut table = self.table.lock().unwrap();
        for (k, val) in obj {
            if let Some(m) = val.as_object() {
                table.insert(
                    k.clone(),
                    ModelStat {
                        calls: int_of(m.get("calls")),
                        input_tokens: int_of(m.get("inputTokens")),
                        output_tokens: int_of(m.get("outputTokens")),
                        total_tokens: int_of(m.get("totalTokens")),
                    },
                );
            }
        }
    }

    /// `handleStats` 的响应体（`{"models":{…},"uptimeSec":N}`）。
    pub fn body(&self, now_ms: i64) -> String {
        let table = self.table.lock().unwrap();
        let models = J::Obj(
            table
                .iter()
                .map(|(k, v)| (k.clone(), v.to_j()))
                .collect(),
        );
        J::Obj(vec![
            ("models".into(), models),
            (
                "uptimeSec".into(),
                J::Int((now_ms - self.started_at_ms) / 1000),
            ),
        ])
        .to_go()
    }

    /// `recordUsage`：把一轮 `result` 事件的 usage 真值入账（nil → 不入账）。
    pub fn record_usage(&self, model: &str, usage: Option<&Map<String, Value>>) {
        let usage = match usage {
            Some(u) => u,
            None => return,
        };
        let input = int_of(usage.get("input_tokens"));
        let output = int_of(usage.get("output_tokens"));
        self.add_stat(model, input, output, input + output);
    }

    /// `addStat`：三者全 0 不入账（不估算，避免脏数据）；`total>0 ? total : in+out`。
    pub fn add_stat(&self, model: &str, input: i64, output: i64, total: i64) {
        if total == 0 && input == 0 && output == 0 {
            return;
        }
        {
            let mut table = self.table.lock().unwrap();
            let ent = table.entry(model.to_string()).or_default();
            ent.calls += 1;
            ent.input_tokens += input;
            ent.output_tokens += output;
            if total > 0 {
                ent.total_tokens += total;
            } else {
                ent.total_tokens += input + output;
            }
        }
        self.save();
        logf(&format!("stat model={model} in={input} out={output}"));
    }

    /// `saveStats`：原子落盘（tmp + rename，0644）。Go 侧不建目录，这里同样不建。
    fn save(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let text = {
            let table = self.table.lock().unwrap();
            J::Obj(
                table
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_j()))
                    .collect(),
            )
            .to_go()
        };
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        if std::fs::write(&tmp, text.as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// 便于测试/日志：统计表的只读快照（`model` → `(calls,in,out,total)`）。
pub fn snapshot(table: &BTreeMap<String, ModelStat>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in table {
        parts.push(format!(
            "{k}:{},{},{},{}",
            v.calls, v.input_tokens, v.output_tokens, v.total_tokens
        ));
    }
    parts.join("|")
}

/// `/v1/stats` 里 `models` 为空时的形态（供断言用）。
pub fn empty_models_shape() -> String {
    str_of(Some(&Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_usage_is_not_recorded() {
        let dir = std::env::temp_dir().join("rs-vibex-stats-test-a");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("vibex-stats.json");
        let _ = std::fs::remove_file(&p);
        let s = Stats::new(p.clone(), 0);
        s.add_stat("m", 0, 0, 0);
        assert!(!p.exists(), "全 0 不应入账、不应落盘");
        s.add_stat("m", 3, 4, 0);
        let text = s.body(0);
        assert!(text.contains("\"totalTokens\":7"), "{text}");
        assert!(text.contains("\"calls\":1"), "{text}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn field_order_and_sorted_keys() {
        let dir = std::env::temp_dir().join("rs-vibex-stats-test-b");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("vibex-stats.json");
        let _ = std::fs::remove_file(&p);
        let s = Stats::new(p.clone(), 0);
        s.add_stat("b", 1, 1, 2);
        s.add_stat("a", 5, 5, 10);
        let text = s.body(0);
        assert_eq!(
            text,
            "{\"models\":{\"a\":{\"calls\":1,\"inputTokens\":5,\"outputTokens\":5,\"totalTokens\":10},\
             \"b\":{\"calls\":1,\"inputTokens\":1,\"outputTokens\":1,\"totalTokens\":2}},\"uptimeSec\":0}"
        );
        let _ = std::fs::remove_file(&p);
    }
}
