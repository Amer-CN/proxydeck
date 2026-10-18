// stats.rs —— `/v1/stats` 与 `bai-stats.json` 的按模型累计，逐条对齐 Go `internal/bai/server.go`。
//
// Go 侧口径（逐条复刻）：
//   * 结构：`map[model]modelStat`，`modelStat` 字段 `calls / inputTokens / outputTokens / totalTokens`
//   * `addStat(model,in,out,total)`：in/out/total **三者全为 0 时直接返回**（不建条目）；
//     `calls++`；`total>0 ? total : in+out` 累进 totalTokens
//   * 落盘：`json.Marshal(map)`（无缩进、无换行）→ 写 `<path>.tmp` → rename 覆盖（原子）
//   * 读回：启动时读 `bai-stats.json`；文件不存在 / 坏 JSON / `null` → 沿用空表，不报错
//   * `/v1/stats`：`{"models":{...},"uptimeSec":N}`，每次读都回一份**副本**
//   * 落盘时机：每笔 addStat 之后立即落盘（**未做节流**——与 Go 现状一致，如实对齐；
//     简报提到的「节流落盘」在本仓 Go 版 `internal/bai` 里并不存在，`internal/proxy/stats.go`
//     的 ≥2s 节流属COMMAND 代理，不属 bai。此处以 Go 现状为准，不额外加节流）
//
// 与 Go 的已知差异（登记在 crate README）：Go 用 `encoding/json` 的 map 序，本模块的
// `/v1/stats` 响应**显式按 key 升序**输出（Go 的 map 序对键集合做排序，实测与升序一致，
// 见 tests/p1_golden.rs 的逐字节对照）。
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::push_go_string;

/// 单模型用量累计（字段名与 Go `modelStat` 同形，GUI 共用一套渲染）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct ModelStat {
    pub calls: i64,
    #[serde(rename = "inputTokens")]
    pub input_tokens: i64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: i64,
    #[serde(rename = "totalTokens")]
    pub total_tokens: i64,
}

/// 统计表：内存累计 + 落盘路径（对应 Go `Server.stats` + `Server.statsPath`）。
pub struct Stats {
    inner: Mutex<BTreeMap<String, ModelStat>>,
    path: PathBuf,
    started_at: Instant,
}

impl Stats {
    /// 建表并从 `bai-stats.json` 读回历史（对应 Go `NewServer` 里的 `loadStats`）。
    pub fn load(path: PathBuf) -> Self {
        let mut map: BTreeMap<String, ModelStat> = BTreeMap::new();
        if let Ok(b) = std::fs::read(&path) {
            // 坏 JSON / null 一律沿用空表（Go: `json.Unmarshal(b,&m)==nil` 才采纳）
            if let Ok(m) = serde_json::from_slice::<BTreeMap<String, ModelStat>>(&b) {
                map = m;
            }
        }
        Self {
            inner: Mutex::new(map),
            path,
            started_at: Instant::now(),
        }
    }

    /// 用给定的累计表直接建表（黄金回放/测试用：把基准记的那张表按同一套落盘口径写出去）。
    pub fn from_snapshot(path: PathBuf, snapshot: BTreeMap<String, ModelStat>) -> Self {
        Self {
            inner: Mutex::new(snapshot),
            path,
            started_at: Instant::now(),
        }
    }

    /// 落盘路径（测试与文档用）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 运行时长（秒），对应 Go `int64(time.Since(s.startedAt).Seconds())`。
    pub fn uptime_secs(&self) -> i64 {
        self.started_at.elapsed().as_secs() as i64
    }

    /// 累计一次调用并落盘（对应 Go `addStat`）。三者全 0 不入账。
    pub fn add(&self, model: &str, input: i64, output: i64, total: i64) {
        if total == 0 && input == 0 && output == 0 {
            return;
        }
        {
            let mut m = self.lock();
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
        println!("bai-plugin: stat model={model} in={input} out={output}");
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ModelStat>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 原子落盘（对应 Go `saveStats`：tmp + rename）。写盘失败只丢统计不报错。
    pub fn save(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let body = {
            let m = self.lock();
            let mut s = String::with_capacity(64 * m.len() + 2);
            s.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                push_go_string(&mut s, k);
                s.push_str(":{\"calls\":");
                s.push_str(&v.calls.to_string());
                s.push_str(",\"inputTokens\":");
                s.push_str(&v.input_tokens.to_string());
                s.push_str(",\"outputTokens\":");
                s.push_str(&v.output_tokens.to_string());
                s.push_str(",\"totalTokens\":");
                s.push_str(&v.total_tokens.to_string());
                s.push('}');
            }
            s.push('}');
            s.into_bytes()
        };
        let Some(dir) = self.path.parent() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(mut f) = std::fs::File::create(&tmp) {
            if f.write_all(&body).is_err() {
                return;
            }
            let _ = f.sync_all();
            drop(f);
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// 组装 `/v1/stats` 响应体（对应 Go `handleStats`：先取副本再编码）。
    pub fn snapshot_body(&self) -> Vec<u8> {
        let uptime = self.uptime_secs();
        let m = self.lock();
        let mut s = String::with_capacity(64 * m.len() + 32);
        s.push_str("{\"models\":{");
        for (i, (k, v)) in m.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            push_go_string(&mut s, k);
            s.push_str(":{\"calls\":");
            s.push_str(&v.calls.to_string());
            s.push_str(",\"inputTokens\":");
            s.push_str(&v.input_tokens.to_string());
            s.push_str(",\"outputTokens\":");
            s.push_str(&v.output_tokens.to_string());
            s.push_str(",\"totalTokens\":");
            s.push_str(&v.total_tokens.to_string());
            s.push('}');
        }
        s.push_str("},\"uptimeSec\":");
        s.push_str(&uptime.to_string());
        s.push('}');
        // Go 侧走 `json.NewEncoder(w).Encode(...)` —— Encoder 会在末尾补一个换行
        s.push('\n');
        s.into_bytes()
    }

    /// 处理一次 `/v1/stats`（同步：读表、拼体，无 await）。
    pub fn handle(&self, _method: &Method) -> Response {
        // Go 侧该路径只注册了 handler（不限方法），非 GET 也照常回统计——与 `/health`
        // 同口径，故这里不做 405。
        (
            StatusCode::OK,
            // 与 Go `w.Header().Set("Content-Type", "application/json")` 逐字一致：
            // 不带 `; charset=utf-8`（P1 审查点出的「一字之差」）。
            [(header::CONTENT_TYPE, "application/json")],
            self.snapshot_body(),
        )
            .into_response()
    }

    /// 给测试用：当前表副本。
    pub fn snapshot(&self) -> BTreeMap<String, ModelStat> {
        self.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_usage_is_ignored() {
        let dir = std::env::temp_dir().join("rs-bai-stats-zero");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bai-stats.json");
        let _ = std::fs::remove_file(&p);
        let s = Stats::load(p.clone());
        s.add("m", 0, 0, 0);
        assert!(s.snapshot().is_empty(), "三者全 0 不建条目");
        assert!(!p.exists(), "不入账就不该落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn accumulates_and_total_falls_back_to_in_plus_out() {
        let dir = std::env::temp_dir().join("rs-bai-stats-acc");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bai-stats.json");
        let _ = std::fs::remove_file(&p);
        let s = Stats::load(p.clone());
        s.add("m", 100, 50, 150);
        s.add("m", 10, 5, 0);
        let snap = s.snapshot();
        let v = snap.get("m").expect("应有条目");
        assert_eq!(v.calls, 2);
        assert_eq!(v.input_tokens, 110);
        assert_eq!(v.output_tokens, 55);
        assert_eq!(v.total_tokens, 165, "total=0 时应兜底 in+out");
        assert!(p.exists(), "入账后应落盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_reads_history_and_tolerates_broken_files() {
        let dir = std::env::temp_dir().join("rs-bai-stats-reload");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bai-stats.json");

        std::fs::write(
            &p,
            br#"{"glm-5.3":{"calls":2,"inputTokens":110,"outputTokens":55,"totalTokens":165}}"#,
        )
        .unwrap();
        let s = Stats::load(p.clone());
        assert_eq!(s.snapshot().get("glm-5.3").unwrap().calls, 2);

        std::fs::write(&p, b"{not json").unwrap();
        assert!(Stats::load(p.clone()).snapshot().is_empty(), "坏 JSON 当空表");

        std::fs::write(&p, b"null").unwrap();
        assert!(Stats::load(p.clone()).snapshot().is_empty(), "null 当空表");

        std::fs::remove_file(&p).unwrap();
        assert!(Stats::load(p.clone()).snapshot().is_empty(), "文件缺失当空表");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_shape_matches_go_marshal() {
        let dir = std::env::temp_dir().join("rs-bai-stats-shape");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bai-stats.json");
        let _ = std::fs::remove_file(&p);
        let s = Stats::load(p.clone());
        s.add("glm-5.3", 100, 50, 150);
        s.add("glm-5.3", 10, 5, 0);
        s.add("hy3", 7, 3, 10);
        let disk = std::fs::read(&p).unwrap();
        assert_eq!(
            String::from_utf8(disk).unwrap(),
            r#"{"glm-5.3":{"calls":2,"inputTokens":110,"outputTokens":55,"totalTokens":165},"hy3":{"calls":1,"inputTokens":7,"outputTokens":3,"totalTokens":10}}"#,
            "落盘形态必须与 Go json.Marshal(map) 一致：无缩进、无换行、键升序"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
