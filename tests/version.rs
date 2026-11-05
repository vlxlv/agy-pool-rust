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
