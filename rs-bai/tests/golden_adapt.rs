// golden_adapt.rs —— 黄金对照门禁：Rust 的请求改写必须与 Go 侧逐字节一致。
//
// 基准文件 `.work/rust-rewrite/golden-adapt-go.json` 由 `.work/cli/go-adapter`（`internal/bai/server.go`
// 里 `adaptQuirks` / `truncateToContext` / `estTokens` 的逐字副本，provenance.mjs 可校验）
// 对 `.work/rust-rewrite/fixtures.json` 跑出来。同一组 fixture 在这里跑 Rust 版，
// 两边出站字节必须完全相同（含 developer→system、8192 钳制、effort 改写、超窗截断）。
//
// 文件缺失时测试**失败**而不是跳过：黄金门禁是这条路线的前提，静默跳过等于没有门禁。
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/rs-bai
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rs-bai 应有父目录")
        .to_path_buf()
}

#[test]
fn rust_adapt_matches_go_golden_byte_for_byte() {
    let golden_path = repo_root().join(".work/rust-rewrite/golden-adapt-go.json");
    let raw = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
        panic!(
            "读不到黄金基准 {}: {e}（先生成：node .work/rust-rewrite/mkfixtures.mjs && .work/cli/go-adapter.exe）",
            golden_path.display()
        )
    });
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("golden JSON 解析失败");
    let results = doc["results"].as_array().expect("golden 缺少 results 数组");
    assert!(!results.is_empty(), "golden results 为空");

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
    println!("golden 对照通过：{checked} 条 fixture 出站字节与 Go 逐字节一致");
}
