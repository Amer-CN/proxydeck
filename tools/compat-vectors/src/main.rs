//! T12 证据工具：把**同一组输入**喂给各 crate 的 Go 字符串转义实现，比较字节。
//!
//! 输入覆盖：控制字符（\u0000/\u001f）、HTML（<>&）、Unicode（中文/emoji/U+2028/U+2029）、
//! 引号/反斜杠、DEL(0x7f)、合法与非法 UTF-8 由 &str 保证合法；数字/对象不属本原语。
//!
//! 退出码：全部一致 0；任何一处不同 1（打印差异）。

fn main() {
    let cases: &[&str] = &[
        "",
        "plain",
        "\u{0}\u{1}\u{8}\u{b}\u{c}\u{1f}\u{7f}",
        "<a> & \"b\" \\ c",
        "中文🙂\u{2028}\u{2029}",
        "line\nbreak\r\ttab",
        "\u{fffd}\u{10ffff}",
        "a/b",
    ];

    let mut mismatches = 0usize;
    for input in cases {
        let proxy = rs_proxy::gjson::to_go(&serde_json::Value::String(input.to_string()));
        let codebuddy = rs_codebuddy::gjson::to_go(&serde_json::Value::String(input.to_string()));
        let qoder = rs_qoder::gjson::s((*input).to_string()).to_go();
        let tuanjie = rs_tuanjie::gojson::go_string(input);
        let comate = rs_comate::gjson::s((*input).to_string()).to_go();
        let vibex = rs_vibex::gjson::s((*input).to_string()).to_go();
        let all = [
            ("proxy", proxy.clone()),
            ("codebuddy", codebuddy.clone()),
            ("qoder", qoder.clone()),
            ("tuanjie", tuanjie.clone()),
            ("comate", comate.clone()),
            ("vibex", vibex.clone()),
        ];
        let mut ok = true;
        for (name, got) in &all {
            if got != &proxy {
                ok = false;
                mismatches += 1;
                println!("MISMATCH input={input:?} {name}={got:?} proxy={proxy:?}");
            }
        }
        if ok {
            println!("OK input={input:?}");
        }
    }

    if mismatches > 0 {
        eprintln!("{mismatches} 处字符串转义不一致");
        std::process::exit(1);
    }
    println!("全部 {} 组输入在 6 个转义实现上逐字节一致", cases.len());
}
