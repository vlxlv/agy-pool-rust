use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

static GO_BIN: OnceLock<PathBuf> = OnceLock::new();

fn get_go_bin() -> &'static Path {
    GO_BIN.get_or_init(|| {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root");
        let tmp = tempfile::tempdir().expect("create tempdir for go binary");
        let bin_path = tmp.path().join("agy-pool-go");
        let go_tool = std::env::var("HOME")
            .map(|h| PathBuf::from(h).join("sdk/go1.26.10/bin/go"))
            .ok()
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("go"));
        let out = Command::new(go_tool)
            .args(["build", "-o", bin_path.to_str().unwrap(), "./cmd/agy-pool"])
            .current_dir(repo_root)
            .env("AGY_TEST_MODE", "1")
            .output()
            .expect("execute go build");
        assert!(
            out.status.success(),
            "go build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::mem::forget(tmp);
        bin_path
    })
}

fn get_rust_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_agy-pool"))
}

#[derive(Debug)]
struct RunResult {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cli_case(bin: &Path, args: &[String]) -> RunResult {
    let tmp = tempfile::tempdir().expect("create fresh tempdir");
    let home_dir = tmp.path().join("home");
    let data_dir = tmp.path().join("data");
    let config_file = tmp.path().join("config.json");
    let gemini_dir = tmp.path().join("gemini");
    std::fs::create_dir_all(&home_dir).expect("create home dir");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::create_dir_all(&gemini_dir).expect("create gemini dir");

    let mut cmd = Command::new(bin);
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("AGY_TEST_MODE", "1")
        .env("HOME", &home_dir)
        .env("AGY_DATA_DIR", &data_dir)
        .env("AGY_CONFIG_FILE", &config_file)
        .env("AGY_GEMINI_DIR", &gemini_dir)
        .current_dir(tmp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd.output().expect("execute CLI command");
    RunResult {
        exit_code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct GoldenCase {
    name: String,
    args: Vec<String>,
    exit_code: i32,
    #[serde(default)]
    stdout_contains: Vec<String>,
    #[serde(default)]
    stderr_contains: Vec<String>,
    #[serde(default)]
    stdout_contains_go: Vec<String>,
    #[serde(default)]
    stdout_not_contains_go: Vec<String>,
    #[serde(default)]
    _exempt_py_parity: bool,
}

/// golden_cases.json predates Go commit 16d8e34 (sanitized lifecycle errors) and nothing else reads it,
/// so these cases are pinned to the current Go binary here instead of editing the shared fixture.
fn apply_current_go_behavior(cases: &mut [GoldenCase]) {
    for case in cases.iter_mut() {
        let (exit_code, stdout, stderr): (i32, &[&str], &[&str]) = match case.name.as_str() {
            "rename_not_found" => (1, &["[Error] The selected account was not found."], &[]),
            "remove_not_found" => (1, &[], &["[Error] Could not remove the selected account."]),
            "switch_not_found" => (1, &[], &["[Error] Account pool is empty."]),
            _ => continue,
        };
        case.exit_code = exit_code;
        case.stdout_contains = stdout.iter().map(|s| s.to_string()).collect();
        case.stderr_contains = stderr.iter().map(|s| s.to_string()).collect();
    }
}

#[test]
fn test_cli_golden_cases() {
    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root");
    let fixtures_path = repo_root.join("testdata/cli/golden_cases.json");
    let content = std::fs::read_to_string(&fixtures_path).expect("read golden_cases.json");
    let mut cases: Vec<GoldenCase> = serde_json::from_str(&content).expect("parse golden_cases.json");

    assert_eq!(cases.len(), 17, "expected exactly 17 golden cases");
    apply_current_go_behavior(&mut cases);

    let mut errors = Vec::new();

    for case in &cases {
        let go_res = run_cli_case(go_bin, &case.args);
        let rust_res = run_cli_case(rust_bin, &case.args);

        println!("CASE: {} args={:?}", case.name, case.args);
        println!(
            "  GO:   rc={:?} stdout={:?} stderr={:?}",
            go_res.exit_code, go_res.stdout, go_res.stderr
        );
        println!(
            "  RUST: rc={:?} stdout={:?} stderr={:?}",
            rust_res.exit_code, rust_res.stdout, rust_res.stderr
        );

        // Assert on both: exit_code
        if go_res.exit_code != Some(case.exit_code) {
            errors.push(format!(
                "Go exit code mismatch on case '{}': got {:?}, want {}",
                case.name, go_res.exit_code, case.exit_code
            ));
        }
        if rust_res.exit_code != Some(case.exit_code) {
            errors.push(format!(
                "Rust exit code mismatch on case '{}': got {:?}, want {}",
                case.name, rust_res.exit_code, case.exit_code
            ));
        }

        // Assert on both: stdout_contains
        for pat in &case.stdout_contains {
            if !go_res.stdout.contains(pat) {
                errors.push(format!(
                    "Go stdout on case '{}' missing pattern: {:?}\nGot: {:?}",
                    case.name, pat, go_res.stdout
                ));
            }
            if !rust_res.stdout.contains(pat) {
                errors.push(format!(
                    "Rust stdout on case '{}' missing pattern: {:?}\nGot: {:?}",
                    case.name, pat, rust_res.stdout
                ));
            }
        }

        // Assert on both: stderr_contains
        for pat in &case.stderr_contains {
            if !go_res.stderr.contains(pat) {
                errors.push(format!(
                    "Go stderr on case '{}' missing pattern: {:?}\nGot: {:?}",
                    case.name, pat, go_res.stderr
                ));
            }
            if !rust_res.stderr.contains(pat) {
                errors.push(format!(
                    "Rust stderr on case '{}' missing pattern: {:?}\nGot: {:?}",
                    case.name, pat, rust_res.stderr
                ));
            }
        }

        // Assert on both: stdout_contains_go
        for pat in &case.stdout_contains_go {
            if !go_res.stdout.contains(pat) {
                errors.push(format!(
                    "Go stdout on case '{}' missing pattern_go: {:?}\nGot: {:?}",
                    case.name, pat, go_res.stdout
                ));
            }
            if !rust_res.stdout.contains(pat) {
                errors.push(format!(
                    "Rust stdout on case '{}' missing pattern_go: {:?}\nGot: {:?}",
                    case.name, pat, rust_res.stdout
                ));
            }
        }

        // Assert on both: stdout_not_contains_go
        for pat in &case.stdout_not_contains_go {
            if go_res.stdout.contains(pat) {
                errors.push(format!(
                    "Go stdout on case '{}' contains forbidden pattern_go: {:?}\nGot: {:?}",
                    case.name, pat, go_res.stdout
                ));
            }
            if rust_res.stdout.contains(pat) {
                errors.push(format!(
                    "Rust stdout on case '{}' contains forbidden pattern_go: {:?}\nGot: {:?}",
                    case.name, pat, rust_res.stdout
                ));
            }
        }

        // Also assert Go and Rust stdout are identical after stripping ANSI escapes
        // for every case except doctor_check and status_summary.
        if case.name != "doctor_check" && case.name != "status_summary" {
            let go_clean = agy_pool::cli::width::strip_ansi(&go_res.stdout);
            let rust_clean = agy_pool::cli::width::strip_ansi(&rust_res.stdout);
            if go_clean != rust_clean {
                errors.push(format!(
                    "case '{}' stdout mismatch after strip_ansi.\n--- GO ---\n{}\n--- RUST ---\n{}",
                    case.name, go_clean, rust_clean
                ));
            }
            let go_err = agy_pool::cli::width::strip_ansi(&go_res.stderr);
            let rust_err = agy_pool::cli::width::strip_ansi(&rust_res.stderr);
            if go_err != rust_err {
                errors.push(format!(
                    "case '{}' stderr mismatch after strip_ansi.\n--- GO ---\n{}\n--- RUST ---\n{}",
                    case.name, go_err, rust_err
                ));
            }
        }
    }

    if !errors.is_empty() {
        panic!("Found {} failures:\n{}", errors.len(), errors.join("\n\n"));
    }
}
