use std::process::Command;

#[test]
fn version_matches_go_golden_case() {
    let out = Command::new(env!("CARGO_BIN_EXE_agy-pool"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("agy-pool"),
        "golden case `version` expects stdout to contain agy-pool, got {stdout:?}"
    );
    assert!(stdout.contains(agy_pool::VERSION));
}

#[test]
fn version_matches_build_time_override_or_cargo_version() {
    let want = option_env!("AGY_POOL_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agy-pool"))
        .arg("--version")
        .env("AGY_TEST_MODE", "1")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("agy-pool {want}\n"));
}
