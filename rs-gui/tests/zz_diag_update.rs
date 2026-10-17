//! 临时诊断（跑完即删）：验证更新检查的真实网络链路 + IPC 派发。
use std::sync::Arc;

#[test]
fn diag_update_real_fetch() {
    let app = Arc::new(rs_gui::state::App::new("127.0.0.1", "55990", ""));
    // 1) 直测 gh CLI 通道
    let gh = rs_gui::proc::hidden_output(
        "gh",
        &["api", "repos/Amer-CN/proxydeck/releases?per_page=100", "--jq", "."],
    );
    match &gh {
        Some(s) => println!("[diag] gh 通道: ok, {} 字节", s.len()),
        None => println!("[diag] gh 通道: 失败"),
    }
    // 2) 跑真拉取，看缓存落地
    let t0 = std::time::Instant::now();
    rs_gui::update::refresh_releases(&app);
    println!("[diag] refresh_releases 用时 {:?}", t0.elapsed());
    let cache = {
        let s = app.update.lock().unwrap();
        s.cache.clone()
    };
    println!("[diag] 缓存: {}", &cache[..cache.len().min(200)]);
    // 3) 前台两条入口
    println!("[diag] ccCheckUpdate      → {}", rs_gui::update::cc_check_update(&app));
    println!("[diag] ccCheckUpdateForce → {}", rs_gui::update::check_update_force(&app));
    // 4) IPC 派发（模拟前端 postMessage）
    let host: Arc<dyn rs_gui::host::Host> = Arc::new(rs_gui::host::NoWindowHost);
    let bridge = rs_gui::bridge::Bridge::new(Arc::clone(&app), host);
    for name in ["ccCheckUpdate", "ccCheckUpdateForce", "ccVersion", "ccState"] {
        let body = format!(r#"{{"id":1,"name":"{name}","args":[]}}"#);
        match rs_gui::webview::handle_ipc(&bridge, &body) {
            Some((id, text)) => println!("[diag] IPC {name} → id={id} 长度={} 头={}", text.len(), &text[..text.len().min(90)]),
            None => println!("[diag] IPC {name} → None（未派发！）"),
        }
    }
}
