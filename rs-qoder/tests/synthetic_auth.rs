//! A13：Qoder 登录态测试可在运行时合成（当前用户 DPAPI），不依赖采集机密文；
//! 截断/非法输入必须安全报错，不能解出半个 token。

#[path = "common/mod.rs"]
mod common;

#[test]
fn synthetic_auth_fixture_is_decryptable_on_this_machine() {
    // 即使本机就是采集机（冻结夹具可用），也强制走一次运行时合成路径：
    // 证明换机后业务测试仍能执行真实解密链。
    let dir = common::TestDir::new("auth-synthetic");
    rs_qoder::auth::write_synthetic_fixture(
        &dir.path,
        "dt-synthetic-qoder-token",
        "2099-01-01T00:00:00Z",
    )
    .expect("当前用户 DPAPI 应能生成合成夹具");
    let auth = rs_qoder::auth::load_auth(&dir.path).expect("合成夹具必须能被真实解密链解开");
    assert_eq!(auth.token, "dt-synthetic-qoder-token");
    assert!(auth.expires_at_ms.is_some(), "expiresAt 应解析成毫秒");

    // 业务公共入口的夹具同样可解（换机时=合成，采集机上=冻结）。
    let shared = common::auth_fixture_dir();
    let shared_auth = rs_qoder::auth::load_auth(&shared).expect("业务夹具必须可解");
    assert!(!shared_auth.token.is_empty());
    assert!(common::auth_fixture_usable());
}

#[test]
fn truncated_or_invalid_auth_files_fail_safely() {
    let dir = common::TestDir::new("auth-invalid");
    // 1) 缺 Local State
    assert!(rs_qoder::auth::load_auth(&dir.path).is_err());
    // 2) Local State 合法但 auth.v1.dat 截断（<19 字节）
    std::fs::write(
        dir.join("Local State"),
        br#"{"os_crypt":{"encrypted_key":"REMOVED-DPAPI"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("auth.v1.dat"), b"v10short").unwrap();
    let err = rs_qoder::auth::load_auth(&dir.path).unwrap_err();
    assert!(!err.contains("dt-"), "错误信息不得泄露 token: {err}");
}
