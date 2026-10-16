//! stats.rs —— 按模型累计的用量统计 + `tuanjie-stats.json` 落盘。
//! 逐条对应 `internal/tuanjie/server.go` 的 `modelStat` / `addStat` / `loadStats` /
//! `saveStats` / `handleStats`（`:57-62`、`:1827-1864`、`:1918-1948`）。
//!
//! 三处口径与 Go 逐字节对齐（差异清单见 crate README）：
//!
//! 1. **落盘形态**：`json.Marshal(map[string]*modelStat)` → 键按字节序、结构体按**声明序**
//!    （calls / inputTokens / outputTokens / totalTokens）、无换行、无缩进；
//!    `tmp + rename` 原子替换（与 `saveStats` 同）。
//! 2. **三者全 0 不入账**（`server.go:1829-1831`）；
//!    `total>0 ? total : in+out`（`server.go:1841-1845`）。
//! 3. **读回**：文件缺失 / JSON 非法一律保持空表（Go 是 `json.Unmarshal` 失败即返回，
//!    Rust 侧同）；`null` 内容按空表处理（Go 侧会解成 nil map 并在下次 `addStat` panic，
//!    那属 Go 的既有缺陷，Rust 不复制——见 README 差异）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::gojson::go_string;
use crate::server::GJ;

/// 单模型用量累计 —— `modelStat`（`server.go:57-62`）。字段声明序即 JSON 键序。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelStat {
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

impl ModelStat {
    /// `json.Marshal(modelStat)` 的逐字节等价物（声明序 + int64 原样）。
    pub fn to_go_json(&self) -> String {
        format!(
            "{{\"calls\":{},\"inputTokens\":{},\"outputTokens\":{},\"totalTokens\":{}}}",
            self.calls, self.input_tokens, self.output_tokens, self.total_tokens
        )
    }

    /// `/v1/stats` 里的一条（一个对象的四个 int64）。
    pub fn to_gj(&self) -> GJ {
        GJ::Raw(self.to_go_json())
    }
}

/// 统计表 —— Go 侧 `Server.stats` + `statsPath`。
pub struct Stats {
    inner: Mutex<BTreeMap<String, ModelStat>>,
    path: Option<PathBuf>,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    /// 生产构造：从数据目录的 `tuanjie-stats.json` 读回 —— `NewServer` 里的
    /// `s.statsPath = appdir.Path("tuanjie-stats.json")` + `s.loadStats()`（`server.go:81-82`）。
    pub fn new() -> Self {
        Self::with_path(Some(crate::pacing::data_path("tuanjie-stats.json")))
    }

    /// 显式指定（或不指定）落盘路径 —— 测试用（Go 侧 `statsPath == ""` 时同样不读不写）。
    pub fn with_path(path: Option<PathBuf>) -> Self {
        let s = Stats {
            inner: Mutex::new(BTreeMap::new()),
            path,
        };
        s.load();
        s
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ModelStat>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 累计一次调用 —— `addStat`（`server.go:1828-1849`）。
    pub fn add(&self, model: &str, input: i64, output: i64, total: i64) {
        if total == 0 && input == 0 && output == 0 {
            return;
        }
        {
            let mut g = self.lock();
            let st = g.entry(model.to_string()).or_default();
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
        crate::logf(&format!(
            "[tuanjie] stat model={model} in={input} out={output}"
        ));
    }

    /// 当前累计表的快照（拷贝，对应 `handleStats` 里那份复制）。
    pub fn snapshot(&self) -> BTreeMap<String, ModelStat> {
        self.lock().clone()
    }

    /// `/v1/stats` 的 `models` 字段（键按字节序、值按声明序）。
    pub fn snapshot_obj(&self) -> BTreeMap<String, GJ> {
        self.snapshot()
            .into_iter()
            .map(|(k, v)| (k, v.to_gj()))
            .collect()
    }

    /// `loadStats`（`server.go:1918-1931`）：读不到 / 解不开一律保持空表。
    fn load(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Ok(b) = std::fs::read(&path) else {
            return;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) else {
            return;
        };
        let Some(obj) = v.as_object() else {
            return;
        };
        let mut g = self.lock();
        g.clear();
        for (model, st) in obj {
            let get = |k: &str| st.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
            g.insert(
                model.clone(),
                ModelStat {
                    calls: get("calls"),
                    input_tokens: get("inputTokens"),
                    output_tokens: get("outputTokens"),
                    total_tokens: get("totalTokens"),
                },
            );
        }
    }

    /// `saveStats`（`server.go:1934-1948`）：`json.Marshal(map[string]*modelStat)` 的
    /// 字节形态（键字节序、值声明序、无换行），`tmp + rename` 原子落盘。
    pub fn save(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let bytes = self.marshal_go();
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// 落盘字节（也用于门禁直比）。
    pub fn marshal_go(&self) -> Vec<u8> {
        let g = self.lock();
        let mut out = String::from("{");
        for (i, (k, v)) in g.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&go_string(k));
            out.push(':');
            out.push_str(&v.to_go_json());
        }
        out.push('}');
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_totals_are_not_recorded() {
        let s = Stats::with_path(None);
        s.add("m", 0, 0, 0);
        assert!(s.snapshot().is_empty());
    }

    #[test]
    fn total_zero_falls_back_to_in_plus_out() {
        let s = Stats::with_path(None);
        s.add("m", 3, 4, 0);
        let st = s.snapshot().get("m").cloned().unwrap();
        assert_eq!(st.total_tokens, 7);
        assert_eq!(st.calls, 1);
    }

    #[test]
    fn marshal_is_declaration_order_and_key_sorted() {
        let s = Stats::with_path(None);
        s.add("b", 1, 2, 3);
        s.add("a", 1, 2, 3);
        assert_eq!(
            String::from_utf8(s.marshal_go()).unwrap(),
            "{\"a\":{\"calls\":1,\"inputTokens\":1,\"outputTokens\":2,\"totalTokens\":3},\
             \"b\":{\"calls\":1,\"inputTokens\":1,\"outputTokens\":2,\"totalTokens\":3}}"
        );
    }
}
