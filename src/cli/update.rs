use crate::cli::Io;
use crate::cli::{CLR_GREEN, CLR_RED, CLR_RESET};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Reports whether the binary `update` would replace is owned by the agy-pool Debian package.
pub fn is_dpkg_managed() -> bool {
    let Ok(list) = std::fs::read_to_string(dpkg_file_list()) else {
        return false;
    };
    let exe = update_target_path();
    let exe = exe.to_string_lossy();
    list.lines().any(|l| l == exe)
}

fn deb_asset_name(version: &str) -> String {
    format!("agy-pool_{version}_{}.deb", current_go_arch())
}

pub fn current_go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

pub fn current_go_arch() -> &'static str {
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

        // GitHub's API rejects requests without a User-Agent (403).
        let mut req_builder = hyper::http::Request::builder()
            .method(hyper::http::Method::GET)
            .uri(&uri)
            .header(hyper::header::USER_AGENT, format!("agy-pool/{}", crate::VERSION));

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub release_url: String,
    /// The binary belongs to the Debian package, so the one-click upgrade cannot replace it.
    #[serde(default)]
    pub package_managed: bool,
    /// The release's `.deb` for this architecture, when package-managed and an update exists.
    #[serde(default)]
    pub deb_package_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartMode {
    Immediate,
    Delayed(std::time::Duration),
    None,
}

static UPDATE_CACHE: Mutex<Option<(std::time::Instant, UpdateCheckResult)>> = Mutex::new(None);
const UPDATE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

// Even forced checks query GitHub at most once a minute: the unauthenticated API allows 60 calls an hour.
static LAST_RELEASE_QUERY: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const MIN_RELEASE_QUERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
static APPLY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

pub fn clear_update_cache() {
    clear_release_cache();
    APPLY_IN_FLIGHT.store(false, Ordering::Release);
}

fn clear_release_cache() {
    *UPDATE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *LAST_RELEASE_QUERY.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `agy-pool restart` as an operator would type it. The gateway's daemon-child variables must not leak in:
/// AGY_POOL_DAEMON_CHILD would make it a second gateway, and the port variable would pin the old port.
fn restart_command(exe: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("restart")
        .env_remove(crate::daemon::ENV_DAEMON_CHILD)
        .env_remove(crate::daemon::ENV_DAEMON_PORT);
    cmd
}

/// Reports whether release version `latest` is newer than `current` ("X.Y.Z", pre-release suffixes ignored).
/// Unparsable versions fall back to "differs", the old behaviour.
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<(u64, u64, u64)> {
        let core = v.split(['-', '+']).next()?;
        let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
        let parsed = (it.next()??, it.next()??, it.next()??);
        it.next().is_none().then_some(parsed)
    }
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => latest != current,
    }
}

pub async fn check_latest_update(force_refresh: bool) -> Result<UpdateCheckResult, String> {
    if let Some((ts, ref res)) = *UPDATE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) {
        let age = ts.elapsed();
        if age < UPDATE_CACHE_TTL && (!force_refresh || age < MIN_RELEASE_QUERY_INTERVAL) {
            return Ok(res.clone());
        }
    }
    {
        let mut last = LAST_RELEASE_QUERY.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < MIN_RELEASE_QUERY_INTERVAL) {
            return Err("release check rate-limited; try again in a minute".to_string());
        }
        *last = Some(std::time::Instant::now());
    }

    let token = github_token();
    let api_url = releases_api();
    let body = download(&api_url, &token)
        .await
        .map_err(|e| format!("Cannot query releases: {e}"))?;
    let releases: Vec<Release> =
        serde_json::from_slice(&body).map_err(|e| format!("Cannot parse releases response: {e}"))?;
    if releases.is_empty() {
        return Err("Cannot parse releases response: empty releases list".to_string());
    }

    let rel = &releases[0];
    let latest = rel.tag_name.strip_prefix('v').unwrap_or(&rel.tag_name).to_string();
    let current = crate::VERSION.to_string();
    let has_update = is_newer(&latest, &current);
    let release_url = format!("https://github.com/vlxlv/agy-pool-rust/releases/tag/{}", rel.tag_name);
    let package_managed = is_dpkg_managed();
    let deb = deb_asset_name(&latest);
    let deb_package_name = (package_managed && has_update && rel.assets.iter().any(|a| a.name == deb)).then_some(deb);

    let res = UpdateCheckResult {
        current_version: current,
        latest_version: latest,
        has_update,
        release_url,
        package_managed,
        deb_package_name,
    };

    let mut lock = UPDATE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *lock = Some((std::time::Instant::now(), res.clone()));
    Ok(res)
}

/// Replaces the binary with the latest release when it is newer. Returns the installed version, or `None` when
/// already up to date.
pub async fn apply_latest_update(restart: RestartMode) -> Result<Option<String>, String> {
    if APPLY_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return Err("an update is already in progress".to_string());
    }
    let guard = ApplyGuard;
    let token = github_token();
    let api_url = releases_api();
    let body = download(&api_url, &token)
        .await
        .map_err(|e| format!("Cannot query releases: {e}"))?;
    let releases: Vec<Release> =
        serde_json::from_slice(&body).map_err(|e| format!("Cannot parse releases response: {e}"))?;
    if releases.is_empty() {
        return Err("Cannot parse releases response: empty releases list".to_string());
    }

    let rel = &releases[0];
    let latest = rel.tag_name.strip_prefix('v').unwrap_or(&rel.tag_name);

    if !is_newer(latest, crate::VERSION) {
        return Ok(None);
    }

    let exe = update_target_path();
    if is_dpkg_managed() {
        let deb = deb_asset_name(latest);
        return Err(format!(
            "{} is installed by the agy-pool Debian package; download {deb} from release {} and run `sudo apt install ./{deb}`",
            exe.display(),
            rel.tag_name
        ));
    }

    let bin_name = format!("agy-pool-{}-{}", current_go_os(), current_go_arch());
    let mut assets = std::collections::HashMap::new();
    for a in &rel.assets {
        assets.insert(a.name.clone(), a.url.clone());
    }

    let bin_url = match assets.get(&bin_name) {
        Some(u) => u.clone(),
        None => {
            return Err(format!(
                "Release {} has no {} or SHA256SUMS asset",
                rel.tag_name, bin_name
            ));
        }
    };

    let sums_url = match assets.get("SHA256SUMS") {
        Some(u) => u.clone(),
        None => {
            return Err(format!(
                "Release {} has no {} or SHA256SUMS asset",
                rel.tag_name, bin_name
            ));
        }
    };

    let sums = download(&sums_url, &token)
        .await
        .map_err(|e| format!("Cannot download SHA256SUMS: {e}"))?;

    let want = checksum_for(&sums, &bin_name);
    if want.is_empty() {
        return Err(format!("SHA256SUMS has no entry for {bin_name}"));
    }

    let bin = download(&bin_url, &token)
        .await
        .map_err(|e| format!("Cannot download {bin_name}: {e}"))?;

    let mut hasher = Sha256::new();
    hasher.update(&bin);
    let got = hex::encode(hasher.finalize());
    if got != want {
        return Err(format!("Checksum mismatch for {bin_name}; binary not replaced"));
    }

    let was_running = crate::daemon::is_daemon_running(None, 0);
    replace_file(&exe, &bin).map_err(|e| format!("Cannot replace {}: {e}", exe.display()))?;

    // Not clear_update_cache: APPLY_IN_FLIGHT must stay set until the restart below is done.
    clear_release_cache();

    match restart {
        RestartMode::Immediate if was_running => {
            let status = restart_command(&exe).status();
            if status.is_err() || !status.as_ref().unwrap().success() {
                let err_str = status
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "non-zero exit".to_string());
                return Err(format!(
                    "Updated, but gateway restart failed: {err_str} (run `agy-pool restart`)"
                ));
            }
        }
        RestartMode::Delayed(delay) if was_running => {
            // The task owns the guard: a second click cannot download again and queue a second restart, and
            // a failed restart releases it so the operator can retry.
            let exe_clone = exe.clone();
            tokio::spawn(async move {
                let _guard = guard;
                tokio::time::sleep(delay).await;
                let mut cmd = restart_command(&exe_clone);
                cmd.stdin(std::process::Stdio::null());
                if let Ok(log_fd) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(crate::config::log_file())
                {
                    if let Ok(log_err) = log_fd.try_clone() {
                        cmd.stdout(log_fd);
                        cmd.stderr(log_err);
                    } else {
                        cmd.stdout(std::process::Stdio::null());
                        cmd.stderr(std::process::Stdio::null());
                    }
                } else {
                    cmd.stdout(std::process::Stdio::null());
                    cmd.stderr(std::process::Stdio::null());
                }
                #[cfg(unix)]
                {
                    cmd.process_group(0);
                }
                match cmd.spawn() {
                    Ok(_) => {
                        crate::observability::log_message_level(
                            crate::observability::LogLevel::Info,
                            "UPDATE",
                            "Spawned detached `agy-pool restart`; daemon will now yield to new instance",
                        );
                    }
                    Err(e) => {
                        crate::observability::log_message_level(
                            crate::observability::LogLevel::Error,
                            "UPDATE",
                            &format!(
                                "binary updated but gateway restart failed to spawn ({e}); run `agy-pool restart`"
                            ),
                        );
                    }
                }
            });
        }
        _ => {}
    }

    Ok(Some(latest.to_string()))
}

struct ApplyGuard;

impl Drop for ApplyGuard {
    fn drop(&mut self) {
        APPLY_IN_FLIGHT.store(false, Ordering::Release);
    }
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

    let check = match check_latest_update(true).await {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, e, CLR_RESET);
            return 1;
        }
    };

    if !check.has_update {
        let _ = writeln!(
            io.stdout,
            "{}✓ agy-pool {} is up to date{}",
            CLR_GREEN, check.current_version, CLR_RESET
        );
        return 0;
    }

    let _ = writeln!(
        io.stdout,
        "Update available: {} → {}",
        check.current_version, check.latest_version
    );
    if check_only {
        return 0;
    }

    let exe = update_target_path();
    match apply_latest_update(RestartMode::Immediate).await {
        Ok(None) => {
            let _ = writeln!(
                io.stdout,
                "{}✓ agy-pool {} is up to date{}",
                CLR_GREEN, check.current_version, CLR_RESET
            );
            0
        }
        Ok(Some(latest)) => {
            let _ = writeln!(
                io.stdout,
                "{}✓ Updated {} to {}{}",
                CLR_GREEN,
                exe.display(),
                latest,
                CLR_RESET
            );
            0
        }
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] {}{}", CLR_RED, e, CLR_RESET);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_command_drops_daemon_child_env() {
        let cmd = restart_command(Path::new("/bin/agy-pool"));
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["restart"]);
        let removed: Vec<_> = cmd.get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k).collect();
        assert!(removed.contains(&std::ffi::OsStr::new(crate::daemon::ENV_DAEMON_CHILD)));
        assert!(removed.contains(&std::ffi::OsStr::new(crate::daemon::ENV_DAEMON_PORT)));
    }

    #[test]
    fn is_newer_compares_versions_not_strings() {
        assert!(is_newer("0.3.7", "0.3.6"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.3.6", "0.3.7"), "never offer a downgrade");
        assert!(!is_newer("0.3.7", "0.3.7"));
        assert!(!is_newer("0.3.7", "0.3.7-dev"), "same core version is not an update");
        assert!(is_newer("1.0.0", "0.1.1-alpha.2"));
        assert!(
            is_newer("nightly", "0.3.7"),
            "unparsable versions fall back to 'differs'"
        );
    }
    use crate::cli::test_support::{cli_test_env, run_cli};
    use std::sync::Arc;

    #[tokio::test]
    async fn download_sends_user_agent() {
        // api.github.com answers 403 to requests without a User-Agent; Go's client always sent one.
        let seen = Arc::new(Mutex::new(None));
        let seen_c = seen.clone();
        let server = crate::testutil::TestServer::start(move |req| {
            *seen_c.lock().unwrap() = req.header("user-agent").map(str::to_string);
            (200, vec![], b"[]".to_vec())
        });
        download(&format!("{}/releases", server.url()), "").await.unwrap();
        let ua = seen.lock().unwrap().clone();
        assert_eq!(ua.as_deref(), Some(format!("agy-pool/{}", crate::VERSION).as_str()));
    }

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
                        let deb = deb_asset_name(tag.trim_start_matches('v'));
                        let body = format!(
                            r#"[{{"tag_name":"{tag}","assets":[{{"name":"{bin_name}","url":"http://127.0.0.1:{port}/bin"}},{{"name":"SHA256SUMS","url":"http://127.0.0.1:{port}/sums"}},{{"name":"{deb}","url":"http://127.0.0.1:{port}/deb"}}]}}]"#
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
        std::fs::write(&dpkg_file, "/.\n/usr/bin/other\n").unwrap();
        set_dpkg_file_list(Some(dpkg_file.clone()));
        assert!(!is_dpkg_managed(), "a dpkg list without the target must not count");
        std::fs::write(&dpkg_file, format!("/.\n{}\n", target.display())).unwrap();
        assert!(is_dpkg_managed());

        let deb = format!("agy-pool_9.9.9_{}.deb", current_go_arch());
        clear_update_cache();
        let check = check_latest_update(true).await.unwrap();
        assert!(check.package_managed && check.has_update, "{check:?}");
        assert_eq!(check.deb_package_name.as_deref(), Some(deb.as_str()));

        let (code, out) = run_update(&target, &[]).await;
        assert_eq!(code, 1, "dpkg-owned code mismatch: {out}");
        assert!(out.contains("Debian package"), "dpkg message missing: {out}");
        assert!(
            out.contains(&format!("sudo apt install ./{deb}")),
            "dpkg message lacks the .deb: {out}"
        );
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
