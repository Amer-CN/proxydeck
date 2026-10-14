// golden_adapt.rs —— 黄金对照门禁：Rust 的请求改写必须与 Go 侧逐字节一致。
//
// 基准分两档（详见 `rs-bai/tests/golden/README.md`）：
//   * **小号基准**（入库）：`tests/golden/golden-adapt-go.small.json` —— 常规用例
//     F1~F6、F9~F12 共 10 条。干净检出上必跑；读不到就是失败。
//   * **全量基准**（不入库）：`golden-adapt-go.json` —— 比小号版多 F7/F8 两条超大用例
//     （入参体 20MB / 7.5MB，两个文件合计 65MB 工作树体积），只留在 gitignore 的
//     `.work/rust-rewrite/` 下。不在时**显式跳过并计数**（逐个打 SKIP(oversized) 行），
//     绝不静默——「干净检出少覆盖了两条」这件事必须看得见。
//
// 两档都由 `.work/cli/go-adapter`（`internal/bai/server.go` 里 `adaptQuirks` /
// `truncateToContext` / `estTokens` 的逐字副本，provenance.mjs 可校验）对同一批
// fixture 跑出来；再生成路径见 `tests/golden/README.md`。
// 同一组 fixture 在这里跑 Rust 版，两边出站字节必须完全相同
// （含 developer→system、8192 钳制、effort 改写、超窗截断）。
use serde_json::Value;

/// 两条超大用例的 id（入参体 > 1 MiB）：入库的小号基准不含它们，只有全量基准才含。
const OVERSIZED_IDS: [&str; 2] = ["F7-truncate-oversized", "F8-truncate-tool-pairing"];

/// 显式跳过提示。libtest 默认捕获通过用例的 stdout，要看这几行得加 `-- --nocapture`；
/// 但无论看不看得到，跳过这件事都必须留下痕迹（而不是让测试"悄悄少跑两条"）。
const SKIP_NOTICE: &str =
    "SKIP(oversized): baseline not vendored — run .work/rust-rewrite/mkfixtures.mjs to regenerate";

fn read_golden(path: &std::path::Path) -> Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "读不到黄金基准 {}: {e}（小号基准入库在 rs-bai/tests/golden/；\
             再生成：node .work/rust-rewrite/mkfixtures.mjs → go run ./.work/cli/go-adapter \
             → node .work/rust-rewrite/mksmall.mjs，见 tests/golden/README.md）",
            path.display()
        )
    });
    let doc: Value = serde_json::from_str(&raw).expect("golden JSON 解析失败");
    let results = doc["results"].as_array().expect("golden 缺少 results 数组");
    assert!(!results.is_empty(), "golden results 为空");
    doc
}

/// 逐条比对：Rust 改写后的出站字节必须与 Go 基准逐字节相同。返回比对条数。
fn check_results(results: &[Value]) -> usize {
    let mut checked = 0usize;
    for r in results {
        let id = r["id"].as_str().unwrap_or("<no-id>");
        let input = r["in"].as_str().expect("golden 缺 in");
        let expected = r["out"].as_str().expect("golden 缺 out");

        let got = rs_bai::adapt::adapt_quirks_body(input.as_bytes());
        let got_str = String::from_utf8(got.clone()).expect("改写后必须是合法 UTF-8");

        assert_eq!(
            got_str, expected,
            "[{id}] 出站字节与 Go 不一致\n--- Go  ---\n{expected}\n--- Rust ---\n{got_str}"
        );
        // 未被改写时必须是原样字节（不重排 JSON、不改空白）
        if r["changed"].as_bool() == Some(false) {
            assert_eq!(
                got,
                input.as_bytes(),
                "[{id}] 无改写却动了字节（应当原样透传）"
            );
        }
        checked += 1;
    }
    checked
}

#[test]
fn rust_adapt_matches_go_golden_byte_for_byte() {
    // ---- 第一档：小号基准（入库，必跑） ----
    let small_path = rs_bai::golden_path("golden-adapt-go.small.json");
    let small = read_golden(&small_path);
    let small_results = small["results"].as_array().expect("小号基准缺少 results");
    for r in small_results {
        let id = r["id"].as_str().unwrap_or("<no-id>");
        assert!(
            !OVERSIZED_IDS.contains(&id),
            "[{id}] 超大用例不该出现在入库的小号基准里（它只在 .work/ 的全量基准中）"
        );
    }
    let mut checked = check_results(small_results);

    // ---- 第二档：全量基准里的超大用例（基准在则跑，缺则显式跳过并计数） ----
    let full_path = rs_bai::golden_path("golden-adapt-go.json");
    let mut skipped = 0usize;
    if full_path.exists() {
        let full = read_golden(&full_path);
        let oversized: Vec<Value> = full["results"]
            .as_array()
            .expect("全量基准缺少 results")
            .iter()
            .filter(|r| OVERSIZED_IDS.contains(&r["id"].as_str().unwrap_or("")))
            .cloned()
            .collect();
        assert_eq!(
            oversized.len(),
            OVERSIZED_IDS.len(),
            "全量基准 {} 里应含 {} 条超大用例，实得 {} 条——基准与用例集已不同步",
            full_path.display(),
            OVERSIZED_IDS.len(),
            oversized.len()
        );
        checked += check_results(&oversized);
    } else {
        for id in OVERSIZED_IDS {
            println!("{SKIP_NOTICE} [{id}]");
            skipped += 1;
        }
    }

    println!(
        "golden 对照汇总：{checked} 条 fixture 出站字节与 Go 逐字节一致；\
         显式跳过超大用例 {skipped} 条{}",
        if skipped > 0 {
            "（基准未入库，见 rs-bai/tests/golden/README.md）"
        } else {
            ""
        }
    );
}
