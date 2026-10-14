//! `internal/qoder/server.go:99-160` 的 Rust 等价物：积分账本 `~/.qoder-cli-credits.json`。
//!
//! 口径（Go 注释原文的精神，别自作主张扩大）：
//! * 服务端是**多桶计费**（套餐 Credits + 按厂商专属包，各有有效期与扣减优先级），
//!   代理无从得知每次消耗落在哪个桶 ⇒ 账本**只记累计消耗**，不假装知道「剩余」；
//! * 累计的来源只有一处：worker `result.total_credits`（服务端逐请求下发的真实消耗）；
//! * 文件形态 `{"spent":<float64>,"updatedAt":<unix 毫秒>}`；缺失/损坏一律读成 `0`。

use std::path::{Path, PathBuf};

use crate::gjson::{f, i, obj};

/// 账本文件名（与 Go `creditsLedgerPath()` 同值）。
pub const LEDGER_FILE: &str = ".qoder-cli-credits.json";

/// Go `os.UserHomeDir()` 在 Windows 上的判定（`C:\Program Files\Go\src\os\file.go` 实测）：
/// **只看 `USERPROFILE`**（Go 1.26.5 已经没有 HOMEDRIVE/HOMEPATH 兜底），取不到即报错
/// ——调用方把它当「定位不到主目录」。这里回 `None` 表示同一件事。
pub fn home_dir() -> Option<PathBuf> {
    match std::env::var("USERPROFILE") {
        Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// 账本路径；定位不到主目录回 `None`（Go 回空串，调用方据此放弃读/写）。
pub fn ledger_path_in(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|h| h.join(LEDGER_FILE))
}

/// 读累计消耗：文件不存在 / 读不出 / JSON 坏 / 类型不符 → `0`（Go `readCreditsSpent`）。
pub fn read_spent(path: Option<&Path>) -> f64 {
    let Some(p) = path else { return 0.0 };
    let Ok(b) = std::fs::read(p) else { return 0.0 };
    parse_spent(&b)
}

/// 从账本字节里取 `spent`（Go 把这段交给 `json.Unmarshal` 到一个 `struct{Spent float64}`）。
///
/// 与 Go 同语义的几条：
/// * `null` → 零值（无错）；数组/字符串等 → 类型错 → 0；
/// * 字段匹配**先精确、后忽略大小写**，且**按文档顺序处理、后者覆盖前者**
///   （`{"spent":1,"SPENT":2}` → Go 实测 **2**，黄金基准锁住了这条）；
/// * 值必须是数字（`bool`/`string`/对象都算类型错 → 0）；整数与浮点都接受；
/// * 未知字段一律忽略。
///
/// 依赖 `serde_json` 的 `preserve_order`（见 Cargo.toml 的说明）——没有它，
/// 对象键会被按键排序，上面那条「后者生效」就反了。
pub fn parse_spent(b: &[u8]) -> f64 {
    let v: serde_json::Value = match serde_json::from_slice(b) {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    let obj = match v {
        serde_json::Value::Null => return 0.0,
        serde_json::Value::Object(m) => m,
        _ => return 0.0,
    };
    let mut hit: Option<&serde_json::Value> = None;
    for (k, val) in obj.iter() {
        if k == "spent" || k.eq_ignore_ascii_case("spent") {
            hit = Some(val);
        }
    }
    match hit {
        // 超出 float64 表示范围的数字字面量：Go 的 Unmarshal 报 ErrRange → 0
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// 写账本：`{"spent":<float64>,"updatedAt":<unix 毫秒>}`（Go `writeCreditsSpent`）。
/// 键序、浮点形状都走 Go `json.Marshal` 的那套（见 `gjson`）。**没有末尾换行**。
pub fn write_spent(path: &Path, spent: f64, now_ms: i64) -> Result<(), String> {
    let body = obj(vec![("spent", f(spent)), ("updatedAt", i(now_ms))]).to_go();
    std::fs::write(path, body).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spent_matches_go_shapes() {
        assert_eq!(parse_spent(b"{\"spent\":1.5}"), 1.5);
        assert_eq!(parse_spent(b"{\"spent\":0}"), 0.0);
        assert_eq!(parse_spent(b"{\"spent\":3,\"updatedAt\":99}"), 3.0);
        assert_eq!(parse_spent(b"{\"Spent\":2.5}"), 2.5, "字段匹配忽略大小写");
        assert_eq!(
            parse_spent(b"{\"spent\":1,\"SPENT\":2}"),
            2.0,
            "Go 按文档顺序、后者覆盖前者（黄金基准锁的实测值）"
        );
        assert_eq!(parse_spent(b"{\"spent\":1,\"extra\":true}"), 1.0, "未知字段忽略");
        assert_eq!(parse_spent(b"null"), 0.0, "null 是零值不是错");
        assert_eq!(parse_spent(b"{}"), 0.0, "缺字段即零值");
        assert_eq!(parse_spent(b"[]"), 0.0, "顶层类型不符");
        assert_eq!(parse_spent(b"{\"spent\":\"x\"}"), 0.0, "类型不符");
        assert_eq!(parse_spent(b"{not json"), 0.0, "坏 JSON");
        assert_eq!(parse_spent(b""), 0.0, "空文件");
    }

    #[test]
    fn write_shape_is_go_marshal() {
        let dir = std::env::temp_dir().join("rs-qoder-credits-shape");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ledger.json");
        write_spent(&p, 1.5, 1700000000123).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "{\"spent\":1.5,\"updatedAt\":1700000000123}"
        );
        write_spent(&p, 0.0, 1).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "{\"spent\":0,\"updatedAt\":1}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
