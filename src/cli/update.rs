use crate::cli::Io;
use crate::cli::{CLR_GREEN, CLR_RED, CLR_RESET};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DEFAULT_RELEASES_API: &str = "https://api.github.com/repos/vlxlv/agy-pool-rust/releases?per_page=1";
const DEFAULT_DPKG_FILE_LIST: &str = "/var/lib/dpkg/info/agy-pool.list";

static RELEASES_API: Mutex<Option<String>> = Mutex::new(None);
static UPDATE_TARGET_PATH: Mutex<Option<fn() -> PathBuf>> = Mutex::new(None);
static DPKG_FILE_LIST: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn set_releases_api(url: Option<String>) {
    let mut lock = RELEASES_API.lock().unwrap_or_else(|e| e.into_inner());
    *lock = url;
}

pub fn set_update_target_path(f: Option<fn() -> PathBuf>) {
    let mut lock = UPDATE_TARGET_PATH.lock().unwrap_or_else(|e| e.into_inner());
    *lock = f;
}

pub fn set_dpkg_file_list(path: Option<PathBuf>) {
    let mut lock = DPKG_FILE_LIST.lock().unwrap_or_else(|e| e.into_inner());
    *lock = path;
}

fn releases_api() -> String {
    let lock = RELEASES_API.lock().unwrap_or_else(|e| e.into_inner());
    lock.clone().unwrap_or_else(|| DEFAULT_RELEASES_API.to_string())
}

fn update_target_path() -> PathBuf {
    let lock = UPDATE_TARGET_PATH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = *lock {
        f()
    } else {
        #[cfg(test)]
        panic!("default update target path called in test without mock");
        #[cfg(not(test))]
        crate::daemon::entrypoint()
    }
}

fn dpkg_file_list() -> PathBuf {
    let lock = DPKG_FILE_LIST.lock().unwrap_or_else(|e| e.into_inner());
    lock.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_DPKG_FILE_LIST))
}

fn current_go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn current_go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "arm",
        other => other,
    }
}

fn github_token() -> String {
    for k in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(k)
            && !v.is_empty()
        {
            return v;
        }
    }
    // Tests must not read the host's real GitHub credentials.
    if cfg!(test) {
        return String::new();
    }
    if let Ok(output) = std::process::Command::new("gh").args(["auth", "token"]).output()
        && output.status.success()
    {
        return String::from_utf8_lossy(&output.stdout).trim().to_string();
    }
    String::new()
}

async fn download(url_str: &str, token: &str) -> Result<Vec<u8>, String> {
    let mut current_url = url_str.to_string();
    let orig_host = url::Url::parse(url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();

    for _ in 0..10 {
        let uri: hyper::http::Uri = current_url
            .parse()
            .map_err(|e| format!("invalid URL {current_url}: {e}"))?;

        let current_host = uri.host().unwrap_or("").to_string();

        let mut req_builder = hyper::http::Request::builder()
            .method(hyper::http::Method::GET)
            .uri(&uri);

        if current_url.contains("/releases/assets/") {
            req_builder = req_builder.header(hyper::header::ACCEPT, "application/octet-stream");
        }
        if !token.is_empty() && (current_host == orig_host || orig_host.is_empty()) {
            req_builder = req_builder.header(hyper::header::AUTHORIZATION, format!("Bearer {token}"));
        }

        let req = req_builder
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .map_err(|e| e.to_string())?;

        let conn = crate::upstream::connect(&uri)
            .await
            .map_err(|e| format!("connect error: {e}"))?;

        let resp = conn.send(req).await.map_err(|e| format!("send error: {e}"))?;

        let status = resp.status();
        if status.is_redirection() {
            if let Some(loc) = resp.headers().get(hyper::header::LOCATION) {
                let loc_str = loc.to_str().map_err(|e| e.to_string())?;
                let next_url = if loc_str.starts_with("http://") || loc_str.starts_with("https://") {
                    loc_str.to_string()
                } else {
                    let base = url::Url::parse(&current_url).map_err(|e| e.to_string())?;
                    base.join(loc_str).map_err(|e| e.to_string())?.to_string()
                };
                current_url = next_url;
                continue;
            } else {
                return Err(format!("redirect without Location header: {status}"));
            }
        }

        if status != hyper::http::StatusCode::OK {
            return Err(format!("GET {current_url}: {status}"));
        }

        let body_bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("reading body: {e}"))?
            .to_bytes();

        return Ok(body_bytes.to_vec());
    }

    Err("too many redirects".to_string())
}

fn checksum_for(sums: &[u8], name: &str) -> String {
    let s = String::from_utf8_lossy(sums);
    for line in s.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 2 && fields[1].trim_start_matches('*') == name {
            return fields[0].to_ascii_lowercase();
        }
    }
    String::new()
}

fn replace_file(dst: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = dst.parent().unwrap_or(Path::new("."));
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = dir.join(format!(".agy-pool-update-{pid}-{nanos}"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&tmp_path)?;
    if let Err(e) = file.write_all(data).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp_path, dst) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct ReleaseAsset {
    name: String,
    url: String,
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

pub async fn update_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut check_only = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check_only = true,
            "-h" | "--help" => {
                let _ = writeln!(io.stdout, "usage: agy-pool update [--check]");
                return 0;
            }
            _ => {
                let _ = writeln!(io.stderr, "agy-pool update: error: unrecognized argument: {arg}");
                return 2;
            }
        }
    }

    let token = github_token();
    let api_url = releases_api();
    let body = match download(&api_url, &token).await {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Cannot query releases: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let releases: Vec<Release> = match serde_json::from_slice::<Vec<Release>>(&body) {
        Ok(r) if !r.is_empty() => r,
        Ok(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Cannot parse releases response: empty releases list{}",
                CLR_RED, CLR_RESET
            );
            return 1;
        }
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Cannot parse releases response: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let rel = &releases[0];
    let latest = rel.tag_name.strip_prefix('v').unwrap_or(&rel.tag_name);

    if latest == crate::VERSION {
        let _ = writeln!(
            io.stdout,
            "{}✓ agy-pool {} is up to date{}",
            CLR_GREEN,
            crate::VERSION,
            CLR_RESET
        );
        return 0;
    }

    let _ = writeln!(io.stdout, "Update available: {} → {}", crate::VERSION, latest);
    if check_only {
        return 0;
    }

    let exe = update_target_path();
    let dpkg_path = dpkg_file_list();
    if let Ok(list) = std::fs::read_to_string(&dpkg_path) {
        let exe_str = exe.to_string_lossy();
        if list.lines().any(|l| l == exe_str) {
            let _ = writeln!(
                io.stderr,
                "{}[Error] {} is installed by the agy-pool Debian package; install agy-pool_{}_{}.deb from release {} with `sudo apt install ./<file>.deb`{}",
                CLR_RED,
                exe.display(),
                latest,
                current_go_arch(),
                rel.tag_name,
                CLR_RESET
            );
            return 1;
        }
    }

    let bin_name = format!("agy-pool-{}-{}", current_go_os(), current_go_arch());
    let mut assets = std::collections::HashMap::new();
    for a in &rel.assets {
        assets.insert(a.name.clone(), a.url.clone());
    }

    let bin_url = match assets.get(&bin_name) {
        Some(u) => u.clone(),
        None => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Release {} has no {} or SHA256SUMS asset{}",
                CLR_RED, rel.tag_name, bin_name, CLR_RESET
            );
            return 1;
        }
    };

    let sums_url = match assets.get("SHA256SUMS") {
        Some(u) => u.clone(),
        None => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Release {} has no {} or SHA256SUMS asset{}",
                CLR_RED, rel.tag_name, bin_name, CLR_RESET
            );
            return 1;
        }
    };

    let sums = match download(&sums_url, &token).await {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Cannot download SHA256SUMS: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let want = checksum_for(&sums, &bin_name);
    if want.is_empty() {
        let _ = writeln!(
            io.stderr,
            "{}[Error] SHA256SUMS has no entry for {}{}",
            CLR_RED, bin_name, CLR_RESET
        );
        return 1;
    }

    let bin = match download(&bin_url, &token).await {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Cannot download {}: {}{}",
                CLR_RED, bin_name, e, CLR_RESET
            );
            return 1;
        }
    };

    let mut hasher = Sha256::new();
    hasher.update(&bin);
    let got = hex::encode(hasher.finalize());
    if got != want {
        let _ = writeln!(
            io.stderr,
            "{}[Error] Checksum mismatch for {}; binary not replaced{}",
            CLR_RED, bin_name, CLR_RESET
        );
        return 1;
    }

    let was_running = crate::daemon::is_daemon_running(None, 0);
    if let Err(e) = replace_file(&exe, &bin) {
        let _ = writeln!(
            io.stderr,
            "{}[Error] Cannot replace {}: {}{}",
            CLR_RED,
            exe.display(),
            e,
            CLR_RESET
        );
        return 1;
    }

    let _ = writeln!(
        io.stdout,
        "{}✓ Updated {} to {}{}",
        CLR_GREEN,
        exe.display(),
        latest,
        CLR_RESET
    );

    if was_running {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("restart");
        let status = cmd.status();
        if status.is_err() || !status.as_ref().unwrap().success() {
            let err_str = status
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "non-zero exit".to_string());
            let _ = writeln!(
                io.stderr,
                "{}[Error] Updated, but gateway restart failed: {} (run `agy-pool restart`){}",
                CLR_RED, err_str, CLR_RESET
            );
            return 1;
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::{cli_test_env, run_cli};
    use std::sync::Arc;

    struct ResetUpdate;
    impl Drop for ResetUpdate {
        fn drop(&mut self) {
            set_releases_api(None);
            set_update_target_path(None);
            set_dpkg_file_list(None);
        }
    }

    struct MockServerState {
        tag: String,
        bin_name: String,
        new_bin: Vec<u8>,
        sums: String,
        fail_download: bool,
        bad_checksum: bool,
    }

    async fn start_mock_server(state: Arc<Mutex<MockServerState>>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let n = match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let req_str = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req_str.lines().next().unwrap_or("");
                    let path = first_line.split_whitespace().nth(1).unwrap_or("");

                    let (tag, bin_name, new_bin, sums, fail_download, bad_checksum) = {
                        let st = state.lock().unwrap();
                        (
                            st.tag.clone(),
                            st.bin_name.clone(),
                            st.new_bin.clone(),
                            st.sums.clone(),
                            st.fail_download,
                            st.bad_checksum,
                        )
                    };

                    if path.contains("/releases") {
                        let body = format!(
                            r#"[{{"tag_name":"{tag}","assets":[{{"name":"{bin_name}","url":"http://127.0.0.1:{port}/bin"}},{{"name":"SHA256SUMS","url":"http://127.0.0.1:{port}/sums"}}]}}]"#
                        );
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    } else if path.contains("/sums") {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            sums.len(),
                            sums
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                    } else if path.contains("/bin") {
                        if fail_download {
                            let header = "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\npartial";
                            let _ = socket.write_all(header.as_bytes()).await;
                        } else {
                            let resp_bytes = if bad_checksum {
                                b"mismatched content".to_vec()
                            } else {
                                new_bin
                            };
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                resp_bytes.len()
                            );
                            let _ = socket.write_all(header.as_bytes()).await;
                            let _ = socket.write_all(&resp_bytes).await;
                        }
                    } else {
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        (url, task)
    }

    static TEST_TARGET: Mutex<Option<PathBuf>> = Mutex::new(None);
    fn target_provider() -> PathBuf {
        let lock = TEST_TARGET.lock().unwrap();
        lock.clone().expect("TEST_TARGET must be set")
    }

    async fn run_update(target: &Path, args: &[&str]) -> (i32, String) {
        std::fs::write(target, b"old binary").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut cmd_args = vec!["update"];
        cmd_args.extend_from_slice(args);
        let (code, stdout, stderr) = run_cli(&cmd_args, "").await;
        let got = std::fs::read(target).unwrap();
        (code, format!("{stdout}{stderr}|{}", String::from_utf8_lossy(&got)))
    }

    #[tokio::test]
    async fn test_update_cmd() {
        let _env = cli_test_env();
        let _guard = ResetUpdate;

        let bin_name = format!("agy-pool-{}-{}", current_go_os(), current_go_arch());
        let new_bin = b"new binary".to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&new_bin);
        let sum_hex = hex::encode(hasher.finalize());
        let sums = format!("{sum_hex}  {bin_name}\n");
        let tag = "v9.9.9".to_string();

        let state = Arc::new(Mutex::new(MockServerState {
            tag,
            bin_name: bin_name.clone(),
            new_bin,
            sums,
            fail_download: false,
            bad_checksum: false,
        }));

        let (server_url, server_task) = start_mock_server(state.clone()).await;

        set_releases_api(Some(format!("{server_url}/releases")));

        let temp_dir = tempfile::tempdir().unwrap();
        let target = temp_dir.path().join("agy-pool");
        *TEST_TARGET.lock().unwrap() = Some(target.clone());
        set_update_target_path(Some(target_provider));

        let (code, out) = run_update(&target, &["--check"]).await;
        assert_eq!(code, 0, "--check code mismatch: {out}");
        assert!(out.contains("9.9.9"), "--check output: {out}");
        assert!(out.ends_with("|old binary"), "--check binary modified: {out}");

        let (code, out) = run_update(&target, &[]).await;
        assert_eq!(code, 0, "update code mismatch: {out}");
        assert!(out.ends_with("|new binary"), "update binary mismatch: {out}");

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "mode = {:o}, want 0755", mode);

        // dpkgFileList check
        let dpkg_file = temp_dir.path().join("agy-pool.list");
        std::fs::write(&dpkg_file, format!("/.\n{}\n", target.display())).unwrap();
        set_dpkg_file_list(Some(dpkg_file));

        let (code, out) = run_update(&target, &[]).await;
        assert_eq!(code, 1, "dpkg-owned code mismatch: {out}");
        assert!(out.contains("Debian package"), "dpkg message missing: {out}");
        assert!(out.ends_with("|old binary"), "dpkg replaced binary: {out}");

        set_dpkg_file_list(None);

        // Bad checksum check
        state.lock().unwrap().sums = format!("0000  {bin_name}\n");
        let (code, out) = run_update(&target, &[]).await;
        assert_eq!(code, 1, "bad checksum code mismatch: {out}");
        assert!(out.ends_with("|old binary"), "bad checksum replaced: {out}");

        // Up to date check
        state.lock().unwrap().tag = format!("v{}", crate::VERSION);
        let (code, out) = run_update(&target, &[]).await;
        assert_eq!(code, 0, "up to date code mismatch: {out}");
        assert!(out.contains("up to date"), "up to date message missing: {out}");

        server_task.abort();
    }

    #[tokio::test]
    async fn update_checksum_mismatch_leaves_binary_untouched() {
        let _env = cli_test_env();
        let _guard = ResetUpdate;

        let bin_name = format!("agy-pool-{}-{}", current_go_os(), current_go_arch());
        let new_bin = b"new binary".to_vec();
        let sums = format!("deadbeefdeadbeef  {bin_name}\n");
        let tag = "v9.9.9".to_string();

        let state = Arc::new(Mutex::new(MockServerState {
            tag,
            bin_name: bin_name.clone(),
            new_bin,
            sums,
            fail_download: false,
            bad_checksum: false,
        }));

        let (server_url, server_task) = start_mock_server(state).await;
        set_releases_api(Some(format!("{server_url}/releases")));

        let temp_dir = tempfile::tempdir().unwrap();
        let target = temp_dir.path().join("agy-pool");
        *TEST_TARGET.lock().unwrap() = Some(target.clone());
        set_update_target_path(Some(target_provider));

        let original_bytes = b"original untouched binary";
        std::fs::write(&target, original_bytes).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (code, _, stderr) = run_cli(&["update"], "").await;
        assert_ne!(code, 0, "expected non-zero exit code on checksum mismatch");
        assert!(
            stderr.contains("Checksum mismatch"),
            "expected checksum mismatch error, got: {stderr}"
        );

        let got = std::fs::read(&target).unwrap();
        assert_eq!(got, original_bytes, "target binary bytes were modified!");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "mode changed!");

        server_task.abort();
    }

    #[tokio::test]
    async fn update_download_failure_leaves_binary_untouched() {
        let _env = cli_test_env();
        let _guard = ResetUpdate;

        let bin_name = format!("agy-pool-{}-{}", current_go_os(), current_go_arch());
        let new_bin = b"new binary".to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&new_bin);
        let sum_hex = hex::encode(hasher.finalize());
        let sums = format!("{sum_hex}  {bin_name}\n");
        let tag = "v9.9.9".to_string();

        let state = Arc::new(Mutex::new(MockServerState {
            tag,
            bin_name: bin_name.clone(),
            new_bin,
            sums,
            fail_download: true,
            bad_checksum: false,
        }));

        let (server_url, server_task) = start_mock_server(state).await;
        set_releases_api(Some(format!("{server_url}/releases")));

        let temp_dir = tempfile::tempdir().unwrap();
        let target = temp_dir.path().join("agy-pool");
        *TEST_TARGET.lock().unwrap() = Some(target.clone());
        set_update_target_path(Some(target_provider));

        let original_bytes = b"original untouched binary";
        std::fs::write(&target, original_bytes).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (code, _, _) = run_cli(&["update"], "").await;
        assert_ne!(code, 0, "expected non-zero exit code on download failure");

        let got = std::fs::read(&target).unwrap();
        assert_eq!(got, original_bytes, "target binary bytes were modified!");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "mode changed!");

        server_task.abort();
    }
}
