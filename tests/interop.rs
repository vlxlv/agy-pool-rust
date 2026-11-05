use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn goside(dir: &std::path::Path) -> Command {
    let mut c = Command::new("go");
    c.current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .args(["run", "./rust/interop/goside"])
        .arg(dir)
        .env("AGY_TEST_MODE", "1");
    c
}

#[test]
fn go_and_rust_share_leases_and_pool() {
    let go_check = Command::new("go").arg("version").output();
    match go_check {
        Ok(out) if out.status.success() => {}
        _ => panic!("'go' binary not found on PATH or failed to execute. Ensure Go is installed and on PATH."),
    }

    let dir = tempfile::tempdir().unwrap();
    agy_pool::config::set_test_mode(true);
    agy_pool::config::configure_data_dir(dir.path()).unwrap();

    // Go creates the DB; Rust reads it and renames an account.
    let out = goside(dir.path())
        .arg("seed")
        .output()
        .expect("failed to execute 'go run ./rust/interop/goside seed'");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("OK <nil>"),
        "seed failed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    agy_pool::storage::pool_transaction(|p| {
        p.accounts.iter_mut().find(|a| a.id == "acc_2").unwrap().name = "Renamed".into();
        Ok(())
    })
    .unwrap();
    let names = goside(dir.path())
        .arg("names")
        .output()
        .expect("failed to execute 'go run ./rust/interop/goside names'");
    assert!(
        String::from_utf8_lossy(&names.stdout).contains("NAME acc_2 Renamed"),
        "names failed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&names.stdout),
        String::from_utf8_lossy(&names.stderr)
    );

    // Go holds a lease; Rust must see it as remote in-flight.
    let mut held = goside(dir.path())
        .arg("hold")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn 'go run ./rust/interop/goside hold'");
    let mut line = String::new();
    BufReader::new(held.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "HELD");
    let mut seen = -1;
    let (_, rust_lease) = agy_pool::storage::reserve_in_flight(|remote| {
        seen = remote.get("acc_1").copied().unwrap_or(0);
        "acc_2".into()
    })
    .unwrap();
    assert_eq!(seen, 1, "Rust must see the Go process's lease on acc_1");

    // Reverse direction: release both, then a Rust lease on acc_1 must show up in Go's remote view.
    held.stdin.take().unwrap().write_all(b"\n").unwrap();
    held.wait().unwrap();
    drop(rust_lease);
    let _rust_acc1 = agy_pool::storage::lease_in_flight("acc_1").unwrap();
    let remote = goside(dir.path())
        .arg("remote")
        .output()
        .expect("failed to execute 'go run ./rust/interop/goside remote'");
    assert!(
        String::from_utf8_lossy(&remote.stdout).contains("REMOTE acc_1=1"),
        "remote failed: stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&remote.stdout),
        String::from_utf8_lossy(&remote.stderr)
    );
}
