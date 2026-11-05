use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ConversationError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    Message(String),
}

impl std::fmt::Display for ConversationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversationError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            ConversationError::Io(e) => write!(f, "io error: {e}"),
            ConversationError::Json(e) => write!(f, "json error: {e}"),
            ConversationError::Message(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ConversationError {}

impl From<rusqlite::Error> for ConversationError {
    fn from(e: rusqlite::Error) -> Self {
        ConversationError::Sqlite(e)
    }
}

impl From<std::io::Error> for ConversationError {
    fn from(e: std::io::Error) -> Self {
        ConversationError::Io(e)
    }
}

impl From<serde_json::Error> for ConversationError {
    fn from(e: serde_json::Error) -> Self {
        ConversationError::Json(e)
    }
}

const CLR_RESET: &str = "\x1b[0m";
const CLR_BOLD: &str = "\x1b[1m";
const CLR_GREEN: &str = "\x1b[32m";
const CLR_YELLOW: &str = "\x1b[33m";

#[derive(Debug, Clone)]
pub struct ConversationRecord {
    pub id: String,
    pub title: String,
    pub dirs: Vec<String>,
    pub last_modified: i64,
}

fn path_unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut res = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(val) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            res.push(val);
            i += 3;
            continue;
        }
        res.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(res).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// FindLatestConversationForDir finds the most recent conversation ID matching start_dir (or its parent project),
/// ordered strictly by last_modified_time DESC, across ALL Google accounts.
/// Returns Ok((cid, title, matched_dir)).
pub fn find_latest_conversation_for_dir<P: AsRef<Path>>(
    start_dir: P,
) -> Result<(String, String, String), ConversationError> {
    let start_path = start_dir.as_ref();
    let start_dir_buf = if start_path.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        start_path.to_path_buf()
    };

    let real_start =
        std::fs::canonicalize(&start_dir_buf).unwrap_or_else(|_| crate::config::clean_path(&start_dir_buf));

    let home_dir = if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        let p = PathBuf::from(h);
        std::fs::canonicalize(&p).unwrap_or_else(|_| crate::config::clean_path(&p))
    } else {
        let p = crate::config::real_host_user_home();
        std::fs::canonicalize(&p).unwrap_or_else(|_| crate::config::clean_path(&p))
    };

    let db_path = crate::config::conversation_db_file();
    if !db_path.exists() {
        return Ok((String::new(), String::new(), String::new()));
    }

    // Connect to SQLite in read-only mode
    let conn = match rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return Ok((String::new(), String::new(), String::new())),
    };

    let _ = conn.execute("PRAGMA query_only = ON;", []);
    let _ = conn.execute("PRAGMA busy_timeout = 2000;", []);

    let mut stmt = match conn.prepare(
        "
		SELECT conversation_id, title, workspace_uris, last_modified_time
		FROM conversation_summaries
		ORDER BY last_modified_time DESC
	",
    ) {
        Ok(s) => s,
        Err(_) => return Ok((String::new(), String::new(), String::new())),
    };

    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return Ok((String::new(), String::new(), String::new())),
    };

    let mut convs = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        let cid: Option<String> = row.get(0).ok();
        let title: Option<String> = row.get(1).ok();
        let uris_raw: Option<String> = row.get(2).ok();
        let mtime: Option<i64> = row.get(3).ok();

        let Some(cid) = cid else {
            continue;
        };
        if cid.is_empty() {
            continue;
        }

        let mut dirs = Vec::new();
        if let Some(raw) = uris_raw
            && !raw.is_empty()
            && let Ok(uris) = serde_json::from_str::<Vec<String>>(&raw)
        {
            for u in uris {
                if u.is_empty() {
                    continue;
                }
                let clean = u.strip_prefix("file://").unwrap_or(&u);
                let unesc = path_unescape(clean);
                let clean = unesc.trim_end_matches('/');
                let path = PathBuf::from(clean);
                let real_d = std::fs::canonicalize(&path).unwrap_or_else(|_| crate::config::clean_path(&path));
                dirs.push(real_d.to_string_lossy().into_owned());
            }
        }

        convs.push(ConversationRecord {
            id: cid,
            title: title.unwrap_or_default(),
            dirs,
            last_modified: mtime.unwrap_or(0),
        });
    }

    // Traverse upwards from real_start matching any conversation workspace
    let mut curr = real_start;
    loop {
        let curr_str = curr.to_string_lossy();
        let target_path = if curr_str == "/" {
            "/".to_string()
        } else {
            curr_str.trim_end_matches('/').to_string()
        };

        for conv in &convs {
            for d in &conv.dirs {
                if target_path == *d {
                    return Ok((conv.id.clone(), conv.title.clone(), target_path));
                }
            }
        }

        if curr == home_dir || curr_str == "/" {
            break;
        }
        match curr.parent() {
            Some(parent) if parent != curr => {
                curr = parent.to_path_buf();
            }
            _ => break,
        }
    }

    Ok((String::new(), String::new(), String::new()))
}

/// ResolveContinueArg intercepts -c / --continue and resolves it directly to --conversation <cid>
/// for the current directory's most recent conversation across all accounts.
pub fn resolve_continue_arg(extra_args: &[String], err_out: &mut impl Write) -> Vec<String> {
    let mut has_continue = false;
    let mut new_args = Vec::new();
    let mut skip_next = false;

    for arg in extra_args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-c" || arg == "--continue" {
            has_continue = true;
        } else if arg == "--conversation" || arg.starts_with("--conversation=") {
            // If user explicitly passed --conversation, do not interfere
            return extra_args.to_vec();
        } else {
            new_args.push(arg.clone());
        }
    }

    if !has_continue {
        return extra_args.to_vec();
    }

    let (cid, title, matched_dir) = match find_latest_conversation_for_dir(Path::new("")) {
        Ok((c, t, m)) => (c, t, m),
        Err(_) => return extra_args.to_vec(),
    };

    if cid.is_empty() {
        return extra_args.to_vec();
    }

    let lock_file = crate::config::presence_lock_file(&cid);
    let mut is_locked = false;
    if lock_file.exists()
        && let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(&lock_file)
    {
        use std::os::unix::io::AsRawFd;
        let fd = f.as_raw_fd();
        // SAFETY: fd is a valid open file descriptor
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret != 0 {
            is_locked = true;
        } else {
            // SAFETY: fd was successfully locked, now unlock
            unsafe {
                libc::flock(fd, libc::LOCK_UN);
            }
        }
    }

    let dir_name = if !matched_dir.is_empty() {
        Path::new(&matched_dir)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
    } else {
        ""
    };
    let dir_name = if dir_name.is_empty() || dir_name == "." || dir_name == "/" {
        "workspace"
    } else {
        dir_name
    };

    let display_title = title.trim();
    let display_title = if display_title.is_empty() {
        if cid.len() >= 8 { &cid[..8] } else { &cid }
    } else {
        display_title
    };

    let cid_short = if cid.len() > 8 { &cid[..8] } else { &cid };

    if is_locked {
        let _ = writeln!(
            err_out,
            "{CLR_YELLOW}[agy-pool] Notice: Conversation '{display_title}' ({cid_short}...) is active in another process.{CLR_RESET}"
        );
    } else {
        let _ = writeln!(
            err_out,
            "{CLR_GREEN}✓ [agy-pool] Resuming last conversation in {dir_name}: {CLR_BOLD}{display_title}{CLR_RESET}"
        );
    }

    let mut result = vec!["--conversation".to_string(), cid];
    result.extend(new_args);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
            crate::config::reset_native_agy_dir();
        }
    }

    fn setup_test_env() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join(".gemini");
        std::fs::create_dir_all(&state_dir).unwrap();
        crate::config::set_native_agy_dir(&state_dir);
        TestEnv { _lock: lock, dir }
    }

    fn create_test_db(db_path: &Path) -> rusqlite::Connection {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT,
                title TEXT,
                workspace_uris TEXT,
                last_modified_time INTEGER
            );",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn find_latest_conversation_missing_db() {
        let _env = setup_test_env();
        let (cid, title, matched_dir) = find_latest_conversation_for_dir(Path::new("/some/dir")).unwrap();
        assert_eq!(cid, "");
        assert_eq!(title, "");
        assert_eq!(matched_dir, "");
    }

    #[test]
    fn find_latest_conversation_ordering_and_hierarchy() {
        let env = setup_test_env();
        let db_path = crate::config::conversation_db_file();
        let conn = create_test_db(&db_path);

        let proj_dir = env.dir.path().join("workspace").join("my-project");
        let sub_dir = proj_dir.join("src").join("deep");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let proj_dir_str = proj_dir.to_str().unwrap();
        let other_dir = env.dir.path().join("other");
        let other_dir_str = other_dir.to_str().unwrap();

        let proj_uri = serde_json::to_string(&vec![format!("file://{}", proj_dir_str)]).unwrap();
        let other_uri = serde_json::to_string(&vec![format!("file://{}", other_dir_str)]).unwrap();

        // Older conversation in projDir
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["cid-old", "Old Project Conv", proj_uri, 1000],
        )
        .unwrap();

        // Newer conversation in otherDir
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["cid-other", "Other Conv", other_uri, 3000],
        )
        .unwrap();

        // Newest conversation in projDir
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["cid-newest", "Newest Project Conv", proj_uri, 2000],
        )
        .unwrap();
        drop(conn);

        // Test matching from subDir walking up to projDir
        let (cid, title, matched_dir) = find_latest_conversation_for_dir(&sub_dir).unwrap();
        assert_eq!(cid, "cid-newest");
        assert_eq!(title, "Newest Project Conv");
        let expected_proj = std::fs::canonicalize(&proj_dir).unwrap_or(proj_dir);
        assert_eq!(matched_dir, expected_proj.to_str().unwrap());

        // Record stat before and after to ensure DB is read-only
        let fi1 = std::fs::metadata(&db_path).unwrap().modified().unwrap();
        let _ = find_latest_conversation_for_dir(&sub_dir).unwrap();
        let fi2 = std::fs::metadata(&db_path).unwrap().modified().unwrap();
        assert_eq!(fi1, fi2, "database file was modified during read!");
    }

    #[test]
    fn find_latest_conversation_url_encoding() {
        let env = setup_test_env();
        let db_path = crate::config::conversation_db_file();
        let conn = create_test_db(&db_path);

        let proj_dir = env.dir.path().join("workspace").join("my special project");
        std::fs::create_dir_all(&proj_dir).unwrap();
        let proj_dir_str = proj_dir.to_str().unwrap();

        // Encoded URI: spaces -> %20
        let enc_uri = format!("file://{}", proj_dir_str.replace(' ', "%20"));
        let raw_json = serde_json::to_string(&vec![enc_uri]).unwrap();

        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["cid-special", "Special Project", raw_json, 5000],
        )
        .unwrap();
        drop(conn);

        let (cid, _title, matched_dir) = find_latest_conversation_for_dir(&proj_dir).unwrap();
        assert_eq!(cid, "cid-special");
        let expected_proj = std::fs::canonicalize(&proj_dir).unwrap_or(proj_dir);
        assert_eq!(matched_dir, expected_proj.to_str().unwrap());
    }

    #[test]
    fn test_resolve_continue_arg() {
        let _env = setup_test_env();

        // 1. No continue flag
        let orig_args = vec!["foo".to_string(), "--bar".to_string(), "baz".to_string()];
        let mut buf = Vec::new();
        let res = resolve_continue_arg(&orig_args, &mut buf);
        assert_eq!(res, orig_args);

        // 2. Explicit --conversation takes precedence
        let explicit = vec![
            "--conversation".to_string(),
            "explicit-123".to_string(),
            "-c".to_string(),
        ];
        buf.clear();
        let res = resolve_continue_arg(&explicit, &mut buf);
        assert_eq!(res, explicit);

        let explicit_eq = vec!["--conversation=explicit-123".to_string(), "--continue".to_string()];
        buf.clear();
        let res = resolve_continue_arg(&explicit_eq, &mut buf);
        assert_eq!(res, explicit_eq);

        // 3. Populate DB and test resolution
        let db_path = crate::config::conversation_db_file();
        let conn = create_test_db(&db_path);
        let cwd = std::env::current_dir().unwrap();
        let cwd_str = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let proj_uri = serde_json::to_string(&vec![format!("file://{}", cwd_str.to_str().unwrap())]).unwrap();

        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["cid-resolved-456", "Active Dev Session", proj_uri, 9999],
        )
        .unwrap();
        drop(conn);

        buf.clear();
        let res = resolve_continue_arg(&["-c".to_string(), "--extra-flag".to_string()], &mut buf);
        assert_eq!(
            res,
            vec![
                "--conversation".to_string(),
                "cid-resolved-456".to_string(),
                "--extra-flag".to_string()
            ]
        );
        let out = String::from_utf8(buf.clone()).unwrap();
        assert!(out.contains("Resuming last conversation in"));

        // 4. Presence Lock Active
        let presence_dir = crate::config::presence_dir();
        let _ = std::fs::create_dir_all(&presence_dir);
        let lock_file = crate::config::presence_lock_file("cid-resolved-456");
        let lock_f = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_file)
            .unwrap();
        use std::os::unix::io::AsRawFd;
        let lock_fd = lock_f.as_raw_fd();
        // SAFETY: lock_fd is a valid file descriptor.
        let ret = unsafe { libc::flock(lock_fd, libc::LOCK_EX) };
        assert_eq!(ret, 0, "failed to flock");

        buf.clear();
        let res = resolve_continue_arg(&["--continue".to_string()], &mut buf);
        assert_eq!(res, vec!["--conversation".to_string(), "cid-resolved-456".to_string()]);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Notice: Conversation 'Active Dev Session' (cid-reso...) is active in another process."));

        // SAFETY: unlock
        unsafe {
            libc::flock(lock_fd, libc::LOCK_UN);
        }
    }
}
