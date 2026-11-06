#![allow(clippy::collapsible_if)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub mod roots;
pub use roots::*;

pub const MARKER_START: &str = "# >>> agy-pool integration >>>";
pub const MARKER_END: &str = "# <<< agy-pool integration <<<";
pub const ALIAS_BLOCK: &str = "\n# >>> agy-pool integration >>>\nalias agy='agy-pool run --'\nalias agy-orig='agy-raw'\n# <<< agy-pool integration <<<\n";
pub const MANAGED_SHIM_MARKER: &str = "# agy-pool managed shim v1";

pub const USER_AGY_SHIM_CONTENT: &str = r#"#!/bin/sh
# agy-pool managed shim v1
CONFIG_FILE="${AGY_CONFIG_FILE:-$HOME/.config/agy-pool/config.json}"
DATA_DIR="${AGY_DATA_DIR:-$HOME/.local/share/agy-pool}"
exec agy-pool run -c "$CONFIG_FILE" -D "$DATA_DIR" -- "$@"
"#;

pub const SYSTEM_AGY_SHIM_CONTENT: &str = r#"#!/bin/sh
# agy-pool managed shim v1
CONFIG_FILE="${AGY_CONFIG_FILE:-/etc/agy-pool/config.json}"
DATA_DIR="${AGY_DATA_DIR:-/var/lib/agy-pool}"
exec agy-pool run -c "$CONFIG_FILE" -D "$DATA_DIR" -- "$@"
"#;

pub const AGY_RAW_SHIM_CONTENT: &str = r#"#!/bin/sh
# agy-pool managed shim v1
exec agy-pool raw -- "$@"
"#;

#[derive(Default)]
pub struct Options<'a> {
    pub source_binary: PathBuf,
    pub target_dir: PathBuf,
    pub prefix: String,
    pub home_dir: PathBuf,
    pub is_system: bool,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub native_agy_path: PathBuf,
    pub skip_shell_rc: bool,
    pub clean_legacy_rc: bool,
    pub stdout: Option<&'a mut dyn Write>,
    pub stderr: Option<&'a mut dyn Write>,
}

pub fn determine_target_dir(
    prefix: impl AsRef<Path>,
    home: impl AsRef<Path>,
    is_usr_local_writable: Option<impl Fn() -> bool>,
) -> PathBuf {
    let prefix = prefix.as_ref();
    if !prefix.as_os_str().is_empty() {
        let p_bin = prefix.join("bin");
        if let Ok(fi) = fs::metadata(&p_bin) {
            if fi.is_dir() {
                return p_bin;
            }
        }
    }

    if let Some(check) = is_usr_local_writable {
        if check() {
            return PathBuf::from("/usr/local/bin");
        }
    }

    let home = home.as_ref();
    if !home.as_os_str().is_empty() {
        return home.join(".local/bin");
    }

    let roots = get_installer_roots();
    roots.bin_dir
}

pub fn determine_managed_native_path(target_dir: impl AsRef<Path>, is_system: bool) -> PathBuf {
    let roots = get_installer_roots();
    let target_dir = target_dir.as_ref();
    if is_system {
        if !roots.system_root.as_os_str().is_empty() {
            return roots.system_root.join("usr/local/libexec/agy-pool/agy-native");
        }
        if !target_dir.as_os_str().is_empty() && target_dir.file_name() == Some(std::ffi::OsStr::new("bin")) {
            if let Some(parent) = target_dir.parent() {
                return parent.join("libexec/agy-pool/agy-native");
            }
        }
        return PathBuf::from("/usr/local/libexec/agy-pool/agy-native");
    }
    if !target_dir.as_os_str().is_empty() {
        if target_dir.file_name() == Some(std::ffi::OsStr::new("bin")) {
            if let Some(parent) = target_dir.parent() {
                return parent.join("libexec/agy-pool/agy-native");
            }
        }
        return target_dir.join("libexec/agy-pool/agy-native");
    }
    if !roots.libexec_dir.as_os_str().is_empty() {
        return roots.libexec_dir.join("agy-native");
    }
    roots.user_home.join(".local/libexec/agy-pool/agy-native")
}

pub fn remove_managed_native_copy(managed_native: &Path, out: Option<&mut dyn Write>) -> Result<(), String> {
    let mut sink = io::sink();
    let out = out.unwrap_or(&mut sink);

    if managed_native.as_os_str().is_empty() {
        return Ok(());
    }

    crate::config::assert_safe_destructive_path(managed_native).map_err(|e| e.to_string())?;

    if managed_native.file_name() != Some(std::ffi::OsStr::new("agy-native")) {
        return Err(format!(
            "refusing to remove non-native path {}",
            managed_native.display()
        ));
    }
    let managed_dir = match managed_native.parent() {
        Some(d) => d,
        None => {
            return Err(format!(
                "refusing to remove native copy outside agy-pool directory: {}",
                managed_native.display()
            ));
        }
    };
    if managed_dir.file_name() != Some(std::ffi::OsStr::new("agy-pool")) {
        return Err(format!(
            "refusing to remove native copy outside agy-pool directory: {}",
            managed_native.display()
        ));
    }
    let libexec_dir = match managed_dir.parent() {
        Some(d) => d,
        None => {
            return Err(format!(
                "refusing to remove native copy outside libexec directory: {}",
                managed_native.display()
            ));
        }
    };
    if libexec_dir.file_name() != Some(std::ffi::OsStr::new("libexec")) {
        return Err(format!(
            "refusing to remove native copy outside libexec directory: {}",
            managed_native.display()
        ));
    }

    let st = match fs::symlink_metadata(managed_native) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };

    if !st.file_type().is_symlink() && !st.is_file() {
        return Err(format!(
            "refusing to remove irregular file {}",
            managed_native.display()
        ));
    }

    if let Err(e) = fs::remove_file(managed_native) {
        return Err(format!(
            "failed to remove managed native copy {}: {}",
            managed_native.display(),
            e
        ));
    }
    let _ = writeln!(out, "Removed managed native binary: {}", managed_native.display());

    let _ = fs::remove_dir(managed_dir);
    Ok(())
}

pub fn assert_safe_install_target(target_dir: &Path) -> Result<(), String> {
    crate::config::assert_safe_destructive_path(target_dir).map_err(|e| e.to_string())
}

pub fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    crate::config::assert_safe_destructive_path(dst).map_err(|e| e.to_string())?;
    let mut in_file = File::open(src).map_err(|e| e.to_string())?;

    let tmp_dst = format!("{}.tmp", dst.display());
    let tmp_path = Path::new(&tmp_dst);
    crate::config::assert_safe_destructive_path(tmp_path).map_err(|e| e.to_string())?;

    let mut out_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o755)
        .open(tmp_path)
        .map_err(|e| e.to_string())?;

    if let Err(e) = io::copy(&mut in_file, &mut out_file) {
        drop(out_file);
        let _ = fs::remove_file(tmp_path);
        return Err(e.to_string());
    }
    if let Err(e) = out_file.sync_all() {
        drop(out_file);
        let _ = fs::remove_file(tmp_path);
        return Err(e.to_string());
    }
    drop(out_file);

    if let Err(e) = fs::rename(tmp_path, dst) {
        let _ = fs::remove_file(tmp_path);
        return Err(e.to_string());
    }
    Ok(())
}

pub fn remove_legacy_rc_block(data: &[u8]) -> (Vec<u8>, bool) {
    let s = String::from_utf8_lossy(data);
    let idx_start = match s.find(MARKER_START) {
        Some(i) => i,
        None => return (data.to_vec(), false),
    };
    let idx_end = match s.find(MARKER_END) {
        Some(i) => i,
        None => return (data.to_vec(), false),
    };
    if idx_end < idx_start {
        return (data.to_vec(), false);
    }

    let mut end_block = idx_end + MARKER_END.len();
    if end_block < data.len() && data[end_block] == b'\r' {
        end_block += 1;
    }
    if end_block < data.len() && data[end_block] == b'\n' {
        end_block += 1;
    }

    let mut start_block = idx_start;
    if start_block > 0 && data[start_block - 1] == b'\n' {
        start_block -= 1;
        if start_block > 0 && data[start_block - 1] == b'\r' {
            start_block -= 1;
        }
    }

    let mut result = Vec::with_capacity(data.len().saturating_sub(end_block - start_block));
    result.extend_from_slice(&data[..start_block]);
    result.extend_from_slice(&data[end_block..]);
    (result, true)
}

pub fn append_alias_if_missing(rc_path: &Path) -> Result<(), String> {
    crate::config::assert_safe_destructive_path(rc_path).map_err(|e| e.to_string())?;
    let content = match fs::read(rc_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.to_string()),
    };
    let s = String::from_utf8_lossy(&content);
    if s.contains(MARKER_START) {
        return Ok(());
    }

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(rc_path)
        .map_err(|e| e.to_string())?;
    f.write_all(ALIAS_BLOCK.as_bytes()).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn remove_alias(rc_path: &Path) -> Result<(), String> {
    crate::config::assert_safe_destructive_path(rc_path).map_err(|e| e.to_string())?;
    let data = match fs::read(rc_path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let (cleaned, changed) = remove_legacy_rc_block(&data);
    if !changed {
        return Ok(());
    }
    fs::write(rc_path, cleaned).map_err(|e| e.to_string())
}

pub fn safe_remove_owned_entry(dir: &Path, name: &str, out: Option<&mut dyn Write>) -> Result<(), String> {
    let mut sink = io::sink();
    let out = out.unwrap_or(&mut sink);

    let full_path = dir.join(name);
    crate::config::assert_safe_destructive_path(&full_path).map_err(|e| e.to_string())?;

    let dir_file = match File::open(dir) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("failed to open dir {}: {}", dir.display(), e)),
    };
    use std::os::unix::io::AsRawFd;
    let dir_fd = dir_file.as_raw_fd();

    let st = match fs::symlink_metadata(&full_path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("failed to lstat {}: {}", full_path.display(), e)),
    };

    // 1. If symlink, ensure no-follow and verify ownership
    if st.file_type().is_symlink() {
        let target =
            fs::read_link(&full_path).map_err(|e| format!("failed to readlink {}: {}", full_path.display(), e))?;
        let target_str = target.to_string_lossy();
        if name == "agy-orig" {
            if target_str != "agy-raw" && !target_str.ends_with("/agy-raw") {
                let _ = writeln!(
                    out,
                    "Refusing to delete symlink {}: target is not agy-raw (points to {:?})",
                    full_path.display(),
                    target_str
                );
                return Ok(());
            }
        } else if !target_str.contains("agy-pool") && !target_str.contains("agy-raw") {
            let _ = writeln!(
                out,
                "Refusing to delete symlink {}: unverified target {:?}",
                full_path.display(),
                target_str
            );
            return Ok(());
        }
        let c_name = std::ffi::CString::new(name).map_err(|e| format!("invalid name {}: {}", name, e))?;
        let res = unsafe { libc::unlinkat(dir_fd, c_name.as_ptr(), 0) };
        if res != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ENOENT) {
                return Err(format!("unlinkat failed for {}: {}", full_path.display(), err));
            }
        }
        return Ok(());
    }

    // 2. Reject non-regular files (directories, sockets, devices)
    if !st.is_file() {
        let _ = writeln!(
            out,
            "Refusing to delete {}: not a regular file or symlink (mode: {:o})",
            full_path.display(),
            st.mode()
        );
        return Ok(());
    }

    // 3. Regular file: open with O_NOFOLLOW to avoid symlink swap race
    let c_name = std::ffi::CString::new(name).map_err(|e| format!("invalid name {}: {}", name, e))?;
    let fd = unsafe { libc::openat(dir_fd, c_name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW) };
    if fd < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            let _ = writeln!(
                out,
                "Refusing to delete {}: symlink race detected (O_NOFOLLOW failed)",
                full_path.display()
            );
            return Ok(());
        }
        return Err(format!("openat failed for {}: {}", full_path.display(), err));
    }

    struct FdGuard(libc::c_int);
    impl Drop for FdGuard {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
    let _guard = FdGuard(fd);

    let mut fd_st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut fd_st) } != 0 {
        return Err(format!("fstat failed: {}", io::Error::last_os_error()));
    }
    if (fd_st.st_ino as u64) != st.ino() || (fd_st.st_dev as u64) != st.dev() {
        let _ = writeln!(
            out,
            "Refusing to delete {}: file replaced during validation (TOCTOU detected)",
            full_path.display()
        );
        return Ok(());
    }

    // 4. Validate ownership
    if name == "agy-pool" {
        if let Some(native_agy) = crate::diagnostics::find_real_agy_binary() {
            if let Ok(native_meta) = fs::metadata(&native_agy) {
                if native_meta.ino() == (fd_st.st_ino as u64) && native_meta.dev() == (fd_st.st_dev as u64) {
                    let _ = writeln!(
                        out,
                        "CRITICAL: Refusing to delete {}: hard-linked to native agy!",
                        full_path.display()
                    );
                    return Ok(());
                }
            }
        }
    } else {
        // Shims (agy, agy-raw, agy-orig): verify exact ownership marker
        let mut buf = [0u8; 1024];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        let n = if n > 0 { n as usize } else { 0 };
        let header = String::from_utf8_lossy(&buf[..n]);
        if !header.contains(MANAGED_SHIM_MARKER) && !header.contains(MARKER_START) {
            let _ = writeln!(
                out,
                "Refusing to delete unowned file {}: missing agy-pool ownership marker",
                full_path.display()
            );
            return Ok(());
        }
    }

    // 5. Unlink verified regular file directly via parent dir fd
    let res = unsafe { libc::unlinkat(dir_fd, c_name.as_ptr(), 0) };
    if res != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOENT) {
            return Err(format!("unlinkat failed for {}: {}", full_path.display(), err));
        }
    }
    Ok(())
}

pub fn install(mut opts: Options<'_>) -> Result<(), String> {
    let mut sink = io::sink();
    let out = opts.stdout.as_deref_mut().unwrap_or(&mut sink);

    let roots = get_installer_roots();

    let prefix = if !opts.prefix.is_empty() {
        opts.prefix.clone()
    } else {
        std::env::var("PREFIX").unwrap_or_default()
    };
    let home = if !opts.home_dir.as_os_str().is_empty() {
        opts.home_dir.clone()
    } else {
        roots.user_home.clone()
    };

    let target_dir = if !opts.target_dir.as_os_str().is_empty() {
        opts.target_dir.clone()
    } else if opts.is_system {
        if !roots.system_root.as_os_str().is_empty() {
            roots.system_root.join("usr/local/bin")
        } else {
            PathBuf::from("/usr/local/bin")
        }
    } else {
        determine_target_dir(
            &prefix,
            &home,
            Some(|| {
                if crate::config::is_test_mode() {
                    return false;
                }
                // Probe /usr/local/bin writability
                let test_file = Path::new("/usr/local/bin/.agy_write_probe");
                if let Ok(f) = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(test_file)
                {
                    drop(f);
                    let _ = fs::remove_file(test_file);
                    return true;
                }
                false
            }),
        )
    };

    assert_safe_install_target(&target_dir)?;

    let managed_native = determine_managed_native_path(&target_dir, opts.is_system);
    crate::config::assert_safe_destructive_path(&managed_native).map_err(|e| e.to_string())?;

    // 1. Precondition: discover real native agy
    let native_agy = if !opts.native_agy_path.as_os_str().is_empty() {
        if crate::diagnostics::validate_native_candidate(&opts.native_agy_path).is_err() {
            return Err(
                "Native agy was not found.\nInstall the original Antigravity CLI first, then rerun agy-pool installer."
                    .to_string(),
            );
        }
        opts.native_agy_path.clone()
    } else {
        let target_agy = target_dir.join("agy");
        if crate::diagnostics::validate_native_candidate(&target_agy).is_ok() {
            target_agy
        } else if crate::diagnostics::validate_native_candidate(&managed_native).is_ok() {
            managed_native.clone()
        } else {
            match crate::diagnostics::resolve_native_agy_binary() {
                Ok((cand, _)) => cand,
                Err(_) => {
                    return Err("Native agy was not found.\nInstall the original Antigravity CLI first, then rerun agy-pool installer.".to_string());
                }
            }
        }
    };
    if crate::diagnostics::validate_native_candidate(&native_agy).is_err() {
        return Err(
            "Native agy was not found.\nInstall the original Antigravity CLI first, then rerun agy-pool installer."
                .to_string(),
        );
    }
    let _ = writeln!(out, "Found native agy: {}", native_agy.display());

    // 2. Safely preserve native agy into managed_native before publishing shims
    if native_agy != managed_native {
        let managed_dir = match managed_native.parent() {
            Some(d) => d,
            None => return Err(format!("invalid managed native path: {}", managed_native.display())),
        };
        crate::config::assert_safe_destructive_path(managed_dir).map_err(|e| e.to_string())?;
        fs::create_dir_all(managed_dir)
            .map_err(|e| format!("failed to create directory {}: {}", managed_dir.display(), e))?;

        let pid = std::process::id();
        let tmp_managed = managed_dir.join(format!(".agy-native.tmp.{}", pid));
        crate::config::assert_safe_destructive_path(&tmp_managed).map_err(|e| e.to_string())?;
        if let Err(e) = copy_file(&native_agy, &tmp_managed) {
            let _ = fs::remove_file(&tmp_managed);
            return Err(format!("failed to copy native agy to {}: {}", tmp_managed.display(), e));
        }
        if let Err(e) = crate::diagnostics::validate_native_candidate(&tmp_managed) {
            let _ = fs::remove_file(&tmp_managed);
            return Err(format!("preserved native agy candidate invalid: {}", e));
        }
        if let Err(e) = fs::rename(&tmp_managed, &managed_native) {
            let _ = fs::remove_file(&tmp_managed);
            return Err(format!(
                "failed to place managed native agy {}: {}",
                managed_native.display(),
                e
            ));
        }
        if let Err(e) = crate::diagnostics::validate_native_candidate(&managed_native) {
            return Err(format!(
                "managed native agy {} validation failed: {}",
                managed_native.display(),
                e
            ));
        }
        // Hardlink protection against target agy
        let target_agy = target_dir.join("agy");
        if let Ok(fi_target) = fs::metadata(&target_agy) {
            if let Ok(fi_managed) = fs::metadata(&managed_native) {
                if fi_target.ino() == fi_managed.ino() && fi_target.dev() == fi_managed.dev() {
                    return Err(format!(
                        "CRITICAL: managed native {} is hard-linked to target agy; refusing to proceed",
                        managed_native.display()
                    ));
                }
            }
        }
        let _ = writeln!(out, "Preserved native agy: {}", managed_native.display());
    } else if let Err(e) = crate::diagnostics::validate_native_candidate(&managed_native) {
        return Err(format!(
            "managed native agy {} validation failed: {}",
            managed_native.display(),
            e
        ));
    }

    fs::create_dir_all(&target_dir)
        .map_err(|e| format!("failed to create target directory {}: {}", target_dir.display(), e))?;

    // 3. Stage files in temporary files inside target_dir to prevent partial installs
    let pid = std::process::id();
    let tmp_pool = target_dir.join(format!(".agy-pool.tmp.{}", pid));
    let tmp_agy = target_dir.join(format!(".agy.tmp.{}", pid));
    let tmp_raw = target_dir.join(format!(".agy-raw.tmp.{}", pid));
    let tmp_orig = target_dir.join(format!(".agy-orig.tmp.{}", pid));

    let mut staged: Vec<PathBuf> = Vec::new();
    let cleanup_staged = |staged_list: &[PathBuf]| {
        for s in staged_list {
            let _ = fs::remove_file(s);
        }
    };

    let dst_pool = target_dir.join("agy-pool");
    if !opts.source_binary.as_os_str().is_empty() {
        if let Err(e) = crate::config::assert_safe_destructive_path(&tmp_pool) {
            return Err(e.to_string());
        }
        if let Err(e) = copy_file(&opts.source_binary, &tmp_pool) {
            cleanup_staged(&staged);
            return Err(format!("failed to stage binary to {}: {}", tmp_pool.display(), e));
        }
        staged.push(tmp_pool.clone());
    }

    let dst_agy = target_dir.join("agy");
    if let Err(e) = crate::config::assert_safe_destructive_path(&tmp_agy) {
        cleanup_staged(&staged);
        return Err(e.to_string());
    }
    let agy_content = if opts.is_system {
        SYSTEM_AGY_SHIM_CONTENT
    } else {
        USER_AGY_SHIM_CONTENT
    };
    let mut f = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o755)
        .open(&tmp_agy)
    {
        Ok(f) => f,
        Err(e) => {
            cleanup_staged(&staged);
            return Err(format!("failed to stage agy shim: {}", e));
        }
    };
    if let Err(e) = f.write_all(agy_content.as_bytes()) {
        cleanup_staged(&staged);
        return Err(format!("failed to stage agy shim: {}", e));
    }
    drop(f);
    staged.push(tmp_agy.clone());

    let dst_raw = target_dir.join("agy-raw");
    if let Err(e) = crate::config::assert_safe_destructive_path(&tmp_raw) {
        cleanup_staged(&staged);
        return Err(e.to_string());
    }
    let mut f = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o755)
        .open(&tmp_raw)
    {
        Ok(f) => f,
        Err(e) => {
            cleanup_staged(&staged);
            return Err(format!("failed to stage agy-raw script: {}", e));
        }
    };
    if let Err(e) = f.write_all(AGY_RAW_SHIM_CONTENT.as_bytes()) {
        cleanup_staged(&staged);
        return Err(format!("failed to stage agy-raw script: {}", e));
    }
    drop(f);
    staged.push(tmp_raw.clone());

    let dst_orig = target_dir.join("agy-orig");
    if let Err(e) = crate::config::assert_safe_destructive_path(&tmp_orig) {
        cleanup_staged(&staged);
        return Err(e.to_string());
    }
    let _ = fs::remove_file(&tmp_orig);
    if std::os::unix::fs::symlink("agy-raw", &tmp_orig).is_err() {
        let mut f = match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o755)
            .open(&tmp_orig)
        {
            Ok(f) => f,
            Err(e) => {
                cleanup_staged(&staged);
                return Err(format!("failed to stage agy-orig shim: {}", e));
            }
        };
        if let Err(e) = f.write_all(AGY_RAW_SHIM_CONTENT.as_bytes()) {
            cleanup_staged(&staged);
            return Err(format!("failed to stage agy-orig shim: {}", e));
        }
    }
    staged.push(tmp_orig.clone());

    // Atomically publish staged files into place
    if !opts.source_binary.as_os_str().is_empty() {
        if let Err(e) = crate::config::assert_safe_destructive_path(&dst_pool) {
            cleanup_staged(&staged);
            return Err(e.to_string());
        }
        if let Err(e) = fs::rename(&tmp_pool, &dst_pool) {
            cleanup_staged(&staged);
            return Err(format!("failed to publish binary to {}: {}", dst_pool.display(), e));
        }
    }
    if let Err(e) = crate::config::assert_safe_destructive_path(&dst_agy) {
        cleanup_staged(&staged);
        return Err(e.to_string());
    }
    if let Err(e) = fs::rename(&tmp_agy, &dst_agy) {
        cleanup_staged(&staged);
        return Err(format!("failed to publish agy shim to {}: {}", dst_agy.display(), e));
    }
    if let Err(e) = crate::config::assert_safe_destructive_path(&dst_raw) {
        cleanup_staged(&staged);
        return Err(e.to_string());
    }
    if let Err(e) = fs::rename(&tmp_raw, &dst_raw) {
        cleanup_staged(&staged);
        return Err(format!(
            "failed to publish agy-raw script to {}: {}",
            dst_raw.display(),
            e
        ));
    }
    if let Err(e) = crate::config::assert_safe_destructive_path(&dst_orig) {
        cleanup_staged(&staged);
        return Err(e.to_string());
    }
    let _ = fs::remove_file(&dst_orig);
    if let Err(e) = fs::rename(&tmp_orig, &dst_orig) {
        cleanup_staged(&staged);
        return Err(format!("failed to publish agy-orig to {}: {}", dst_orig.display(), e));
    }

    // 4. Initialize default config and data directory layout
    let (config_dir, data_dir) = if opts.is_system {
        let c = if !opts.config_dir.as_os_str().is_empty() {
            opts.config_dir.clone()
        } else if !roots.system_root.as_os_str().is_empty() {
            roots.system_root.join("etc/agy-pool")
        } else {
            PathBuf::from("/etc/agy-pool")
        };
        let d = if !opts.data_dir.as_os_str().is_empty() {
            opts.data_dir.clone()
        } else if !roots.system_root.as_os_str().is_empty() {
            roots.system_root.join("var/lib/agy-pool")
        } else {
            PathBuf::from("/var/lib/agy-pool")
        };
        (c, d)
    } else {
        let c = if !opts.config_dir.as_os_str().is_empty() {
            opts.config_dir.clone()
        } else if !home.as_os_str().is_empty() {
            home.join(".config/agy-pool")
        } else {
            PathBuf::new()
        };
        let d = if !opts.data_dir.as_os_str().is_empty() {
            opts.data_dir.clone()
        } else if !home.as_os_str().is_empty() {
            home.join(".local/share/agy-pool")
        } else {
            PathBuf::new()
        };
        (c, d)
    };

    if !config_dir.as_os_str().is_empty() {
        crate::config::assert_safe_destructive_path(&config_dir).map_err(|e| e.to_string())?;
        let _ = fs::create_dir_all(&config_dir);
        let cfg_file = config_dir.join("config.json");
        if !cfg_file.exists() {
            let default_cfg = r#"{
  "version": 1,
  "server": {
    "listen": "127.0.0.1",
    "port": 8899
  },
  "scheduler": {
    "strategy": "max_quota"
  },
  "native_agy": {
    "binary": null
  },
  "logging": {
    "max_size_bytes": 5242880,
    "backup_count": 1
  }
}
"#;
            crate::config::assert_safe_destructive_path(&cfg_file).map_err(|e| e.to_string())?;
            let _ = fs::write(&cfg_file, default_cfg);
        }
    }

    if !data_dir.as_os_str().is_empty() {
        crate::config::assert_safe_destructive_path(&data_dir).map_err(|e| e.to_string())?;
        let _ = fs::create_dir_all(data_dir.join("locks"));
    }

    // 5. Check if targetDir is in PATH
    let mut is_on_path = false;
    if let Some(path_env) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&path_env) {
            if p == target_dir {
                is_on_path = true;
                break;
            }
        }
    }
    if !is_on_path {
        let _ = writeln!(
            out,
            "Note: {} is not in your PATH. Add it to your PATH to run 'agy' directly.",
            target_dir.display()
        );
    }

    Ok(())
}

pub fn repair(opts: Options<'_>) -> Result<(), String> {
    install(opts)
}

pub fn uninstall(mut opts: Options<'_>) -> Result<(), String> {
    let mut sink = io::sink();
    let out = opts.stdout.as_deref_mut().unwrap_or(&mut sink);

    let roots = get_installer_roots();

    let prefix = if !opts.prefix.is_empty() {
        opts.prefix.clone()
    } else {
        std::env::var("PREFIX").unwrap_or_default()
    };
    let home = if !opts.home_dir.as_os_str().is_empty() {
        opts.home_dir.clone()
    } else {
        roots.user_home.clone()
    };

    let target_dir = if !opts.target_dir.as_os_str().is_empty() {
        opts.target_dir.clone()
    } else if opts.is_system {
        if !roots.system_root.as_os_str().is_empty() {
            roots.system_root.join("usr/local/bin")
        } else {
            PathBuf::from("/usr/local/bin")
        }
    } else {
        determine_target_dir(&prefix, &home, None::<fn() -> bool>)
    };

    assert_safe_install_target(&target_dir)?;

    // 1. Remove owned shims and binaries using safe_remove_owned_entry
    for name in ["agy", "agy-raw", "agy-orig", "agy-pool"] {
        safe_remove_owned_entry(&target_dir, name, Some(out))
            .map_err(|e| format!("failed to safely remove {}: {}", name, e))?;
    }

    // 2. Remove managed native copy from libexec/agy-pool if present
    let managed_native = determine_managed_native_path(&target_dir, opts.is_system);
    remove_managed_native_copy(&managed_native, Some(out))
        .map_err(|e| format!("failed to safely remove managed native binary: {}", e))?;

    // 3. Clean legacy shell rc files ONLY if explicitly requested
    if opts.clean_legacy_rc && !home.as_os_str().is_empty() {
        for rc in [home.join(".bashrc"), home.join(".zshrc")] {
            if rc.exists() {
                crate::config::assert_safe_destructive_path(&rc).map_err(|e| e.to_string())?;
                let _ = remove_alias(&rc);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    struct EnvRestore {
        key: &'static str,
        val: Option<String>,
    }
    impl EnvRestore {
        fn new(key: &'static str) -> Self {
            Self {
                key,
                val: std::env::var(key).ok(),
            }
        }
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.val {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn file_hash(path: &Path) -> String {
        let data = fs::read(path).unwrap_or_else(|e| panic!("failed to read {:?}: {}", path, e));
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&data);
        hex::encode(hasher.finalize())
    }

    #[test]
    fn test_determine_target_dir_precedence() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("fakehome");
        let fake_prefix = temp_dir.join("fakeprefix");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(fake_prefix.join("bin")).unwrap();

        // 1. Prefix with bin directory takes precedence
        let target = determine_target_dir(&fake_prefix, &fake_home, Some(|| true));
        let expected = fake_prefix.join("bin");
        assert_eq!(target, expected);

        // 2. Without prefix, writable /usr/local/bin takes precedence
        let target = determine_target_dir("", &fake_home, Some(|| true));
        assert_eq!(target, PathBuf::from("/usr/local/bin"));

        // 3. Without prefix and /usr/local/bin not writable, falls back to $HOME/.local/bin
        let target = determine_target_dir("", &fake_home, Some(|| false));
        let expected = fake_home.join(".local/bin");
        assert_eq!(target, expected);
    }

    #[test]
    fn test_install_idempotence_and_uninstallation() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("fakehome");
        let target_dir = temp_dir.join("install_bin");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&target_dir).unwrap();

        let src_bin = temp_dir.join("built-agy-pool");
        fs::write(&src_bin, "#!/bin/sh\necho agy-pool v1\n").unwrap();
        fs::set_permissions(&src_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let fake_native_agy = temp_dir.join("fake_native_agy");
        fs::write(&fake_native_agy, "#!/bin/sh\necho native agy v1.2.2\n").unwrap();
        fs::set_permissions(&fake_native_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let bashrc = fake_home.join(".bashrc");
        fs::write(&bashrc, "# Existing bashrc content\nexport FOO=bar\n").unwrap();

        let opts = Options {
            source_binary: src_bin,
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            native_agy_path: fake_native_agy,
            skip_shell_rc: false,
            ..Default::default()
        };

        // First install
        install(opts).unwrap();

        let installed_pool = target_dir.join("agy-pool");
        let installed_agy = target_dir.join("agy");
        let installed_raw = target_dir.join("agy-raw");
        let installed_orig = target_dir.join("agy-orig");

        let fi_pool = fs::metadata(&installed_pool).unwrap();
        assert_ne!(fi_pool.permissions().mode() & 0o111, 0);

        let fi_agy = fs::metadata(&installed_agy).unwrap();
        assert_ne!(fi_agy.permissions().mode() & 0o111, 0);

        let agy_shim_bytes = fs::read(&installed_agy).unwrap();
        let agy_shim_str = String::from_utf8_lossy(&agy_shim_bytes);
        assert!(agy_shim_str.contains("agy-pool run"));
        assert!(agy_shim_str.contains("-c"));

        let fi_raw = fs::metadata(&installed_raw).unwrap();
        assert_ne!(fi_raw.permissions().mode() & 0o111, 0);
        let raw_shim_bytes = fs::read(&installed_raw).unwrap();
        let raw_shim_str = String::from_utf8_lossy(&raw_shim_bytes);
        assert!(raw_shim_str.contains("agy-pool raw"));

        let target_link = fs::read_link(&installed_orig).unwrap();
        assert!(
            target_link == Path::new("agy-raw") || target_link == installed_raw,
            "expected agy-orig symlink to agy-raw, got {:?}",
            target_link
        );

        let user_cfg = fake_home.join(".config/agy-pool/config.json");
        assert!(user_cfg.exists(), "expected default config file at {:?}", user_cfg);

        let user_locks = fake_home.join(".local/share/agy-pool/locks");
        assert!(user_locks.is_dir(), "expected user data locks dir at {:?}", user_locks);

        // Upgrade / Second Install (Idempotence)
        let opts2 = Options {
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            ..Default::default()
        };
        install(opts2).unwrap();

        // Uninstallation
        let opts3 = Options {
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            ..Default::default()
        };
        uninstall(opts3).unwrap();

        assert!(!installed_pool.exists());
        assert!(!installed_agy.exists());
        assert!(!installed_raw.exists());
        assert!(fs::symlink_metadata(&installed_orig).is_err());
        assert!(
            user_locks.exists(),
            "mutable data directory was deleted during uninstall"
        );
    }

    #[test]
    fn test_install_precondition_fails_when_native_agy_missing() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let target_dir = temp_dir.join("install_bin");
        fs::create_dir_all(&target_dir).unwrap();

        let opts = Options {
            target_dir,
            home_dir: temp_dir.to_path_buf(),
            native_agy_path: PathBuf::from("/nonexistent/path/to/agy"),
            ..Default::default()
        };

        let err = install(opts).unwrap_err();
        assert!(
            err.contains("Native agy was not found"),
            "expected error containing 'Native agy was not found', got: {err}"
        );
    }

    #[test]
    fn test_install_system_mode_synthetic_root() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_native_agy = temp_dir.join("fake_native_agy");
        fs::write(&fake_native_agy, "#!/bin/sh\necho native agy\n").unwrap();
        fs::set_permissions(&fake_native_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let target_dir = temp_dir.join("sys_bin");
        let cfg_dir = temp_dir.join("sys_etc");
        let data_dir = temp_dir.join("sys_var");

        let opts = Options {
            is_system: true,
            target_dir: target_dir.clone(),
            config_dir: cfg_dir.clone(),
            data_dir: data_dir.clone(),
            native_agy_path: fake_native_agy,
            skip_shell_rc: true,
            ..Default::default()
        };

        install(opts).unwrap();

        let agy_shim = target_dir.join("agy");
        let content = fs::read_to_string(&agy_shim).unwrap();
        assert!(
            content.contains("/etc/agy-pool/config.json") && content.contains("/var/lib/agy-pool"),
            "expected system config/data paths in agy shim: {content}"
        );

        assert!(cfg_dir.join("config.json").exists(), "system config.json not created");
        assert!(data_dir.join("locks").exists(), "system locks dir not created");
    }

    #[test]
    fn test_assert_safe_install_target_fail_closed() {
        let _env = crate::cli::test_support::cli_test_env();

        let err = assert_safe_install_target(Path::new("/usr/local/bin")).unwrap_err();
        assert!(
            err.contains("FAIL-CLOSED GUARD"),
            "expected FAIL-CLOSED GUARD message, got {err}"
        );
    }

    #[test]
    fn test_install_and_uninstall_no_shell_rc_modification() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("home");
        fs::create_dir_all(&fake_home).unwrap();

        let bashrc_content = b"# User personal bashrc\nexport MY_VAR=123\nalias ll='ls -la'\n# End of bashrc\n";
        let zshrc_content = b"# User personal zshrc\nsetopt prompt_subst\nexport ZSH_THEME=agnoster\n";

        let bashrc = fake_home.join(".bashrc");
        let zshrc = fake_home.join(".zshrc");
        fs::write(&bashrc, bashrc_content).unwrap();
        fs::write(&zshrc, zshrc_content).unwrap();

        let target_dir = fake_home.join(".local/bin");
        let fake_native_agy = temp_dir.join("fake_native_agy");
        fs::write(&fake_native_agy, "#!/bin/sh\necho native\n").unwrap();
        fs::set_permissions(&fake_native_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let opts = Options {
            home_dir: fake_home.clone(),
            target_dir: target_dir.clone(),
            native_agy_path: fake_native_agy,
            clean_legacy_rc: false,
            ..Default::default()
        };

        install(opts).unwrap();

        let after_install_bashrc = fs::read(&bashrc).unwrap();
        assert_eq!(after_install_bashrc, bashrc_content, "install modified .bashrc!");

        let after_install_zshrc = fs::read(&zshrc).unwrap();
        assert_eq!(after_install_zshrc, zshrc_content, "install modified .zshrc!");

        let opts_un = Options {
            home_dir: fake_home.clone(),
            target_dir: target_dir.clone(),
            clean_legacy_rc: false,
            ..Default::default()
        };
        uninstall(opts_un).unwrap();

        let after_uninstall_bashrc = fs::read(&bashrc).unwrap();
        assert_eq!(after_uninstall_bashrc, bashrc_content, "uninstall modified .bashrc!");

        let after_uninstall_zshrc = fs::read(&zshrc).unwrap();
        assert_eq!(after_uninstall_zshrc, zshrc_content, "uninstall modified .zshrc!");
    }

    #[test]
    fn test_uninstall_legacy_rc_opt_in_cleanup() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("home_legacy");
        fs::create_dir_all(&fake_home).unwrap();

        let prefix = "# Pre-existing user settings\nexport PATH=/custom/bin:$PATH\n";
        let suffix = "\n# Post-existing user settings\nalias gs='git status'\n";
        let full_legacy = format!("{prefix}{ALIAS_BLOCK}{suffix}");

        let bashrc = fake_home.join(".bashrc");
        fs::write(&bashrc, full_legacy.as_bytes()).unwrap();

        let target_dir = fake_home.join(".local/bin");
        fs::create_dir_all(&target_dir).unwrap();

        let opts = Options {
            home_dir: fake_home.clone(),
            target_dir,
            clean_legacy_rc: true,
            ..Default::default()
        };

        uninstall(opts).unwrap();

        let cleaned = fs::read(&bashrc).unwrap();
        let expected = format!("{prefix}{suffix}");
        assert_eq!(cleaned, expected.as_bytes(), "expected exact byte-preserving cleanup");
    }

    #[test]
    fn test_install_native_preservation_before_shim_publication() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("fakehome");
        let target_dir = fake_home.join(".local/bin");
        let libexec_dir = fake_home.join(".local/libexec/agy-pool");
        fs::create_dir_all(&target_dir).unwrap();

        let _h = EnvRestore::new("HOME");
        let _p = EnvRestore::new("PATH");
        unsafe {
            std::env::set_var("HOME", &fake_home);
            std::env::set_var("PATH", &target_dir);
        }

        let initial_target_agy = target_dir.join("agy");
        let native_content = "#!/bin/sh\necho 'real native agy v1.2.3'\n";
        fs::write(&initial_target_agy, native_content).unwrap();
        fs::set_permissions(&initial_target_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let src_bin = temp_dir.join("built-agy-pool");
        fs::write(&src_bin, "#!/bin/sh\necho agy-pool\n").unwrap();
        fs::set_permissions(&src_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let opts = Options {
            source_binary: src_bin,
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            ..Default::default()
        };

        install(opts).unwrap();

        // 1. Managed native binary exists and contains exact original content
        let managed_native = libexec_dir.join("agy-native");
        let preserved_content = fs::read_to_string(&managed_native).unwrap();
        assert_eq!(preserved_content, native_content);

        // 2. Target agy is now the managed shim
        let shim_bytes = fs::read(&initial_target_agy).unwrap();
        let shim_str = String::from_utf8_lossy(&shim_bytes);
        assert!(shim_str.contains(MANAGED_SHIM_MARKER));
        assert!(shim_str.contains("agy-pool run"));

        // 3. Managed native path is resolvable by diagnostics resolver
        let (resolved, src) = crate::diagnostics::resolve_native_agy_binary().unwrap();
        assert_eq!(resolved, managed_native);
        assert_eq!(src, crate::diagnostics::SOURCE_MANAGED);

        // 4. agy shim is rejected as native candidate
        assert!(crate::diagnostics::validate_native_candidate(&initial_target_agy).is_err());
    }

    #[test]
    fn test_install_reinstall_and_official_upgrade_refresh() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("fakehome");
        let target_dir = fake_home.join(".local/bin");
        let libexec_dir = fake_home.join(".local/libexec/agy-pool");
        fs::create_dir_all(&target_dir).unwrap();

        let _h = EnvRestore::new("HOME");
        let _p = EnvRestore::new("PATH");
        unsafe {
            std::env::set_var("HOME", &fake_home);
            std::env::set_var("PATH", &target_dir);
        }

        let target_agy = target_dir.join("agy");
        let managed_native = libexec_dir.join("agy-native");

        // Phase 1: First install preserves native agy v1
        let v1_content = "#!/bin/sh\necho 'native agy v1'\n";
        fs::write(&target_agy, v1_content).unwrap();
        fs::set_permissions(&target_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let src_bin = temp_dir.join("built-agy-pool");
        fs::write(&src_bin, "#!/bin/sh\necho agy-pool\n").unwrap();
        fs::set_permissions(&src_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let opts = Options {
            source_binary: src_bin,
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            ..Default::default()
        };

        install(opts).unwrap();

        // Phase 2: Reinstall when agy is already shim retains existing managed native
        let opts2 = Options {
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            ..Default::default()
        };
        install(opts2).unwrap();
        let content = fs::read_to_string(&managed_native).unwrap();
        assert_eq!(content, v1_content);

        // Phase 3: Official agy installer overwrites target_dir/agy with new native ELF v2
        let v2_content = "#!/bin/sh\necho 'native agy v2 - upgraded by Google'\n";
        fs::write(&target_agy, v2_content).unwrap();
        fs::set_permissions(&target_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let opts3 = Options {
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            ..Default::default()
        };
        install(opts3).unwrap();

        let refreshed = fs::read_to_string(&managed_native).unwrap();
        assert_eq!(refreshed, v2_content);

        let restored_shim = fs::read_to_string(&target_agy).unwrap();
        assert!(restored_shim.contains(MANAGED_SHIM_MARKER));
    }

    #[test]
    fn test_uninstall_removes_managed_native_without_deleting_external_binary() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("fakehome");
        let target_dir = fake_home.join(".local/bin");
        let libexec_dir = fake_home.join(".local/libexec/agy-pool");
        fs::create_dir_all(&target_dir).unwrap();

        let external_dir = temp_dir.join("external_bin");
        fs::create_dir_all(&external_dir).unwrap();
        let external_agy = external_dir.join("agy");
        let external_content = "#!/bin/sh\necho 'external native agy'\n";
        fs::write(&external_agy, external_content).unwrap();
        fs::set_permissions(&external_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let opts = Options {
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            native_agy_path: external_agy.clone(),
            ..Default::default()
        };

        install(opts).unwrap();

        let managed_native = libexec_dir.join("agy-native");
        assert!(managed_native.exists());

        let opts_un = Options {
            target_dir: target_dir.clone(),
            home_dir: fake_home.clone(),
            native_agy_path: external_agy.clone(),
            ..Default::default()
        };
        uninstall(opts_un).unwrap();

        assert!(!managed_native.exists());
        let ext_data = fs::read_to_string(&external_agy).unwrap();
        assert_eq!(ext_data, external_content);
    }

    #[test]
    fn test_integration_agy_managed_shim_and_raw_bypass() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        // 1. Allocate ephemeral port
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let test_port = listener.local_addr().unwrap().port();
        drop(listener);

        let user_home = temp_dir.join("int_user_home");
        let target_bin = user_home.join(".local/bin");
        let libexec_dir = user_home.join(".local/libexec/agy-pool");
        let cfg_dir = user_home.join(".config/agy-pool");
        let data_dir = user_home.join(".local/share/agy-pool");

        fs::create_dir_all(&target_bin).unwrap();
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        let _h = EnvRestore::new("HOME");
        let _p = EnvRestore::new("PATH");
        unsafe {
            std::env::set_var("HOME", &user_home);
            let mut new_path = target_bin.to_str().unwrap().to_string();
            if let Ok(p) = std::env::var("PATH") {
                new_path.push(':');
                new_path.push_str(&p);
            }
            std::env::set_var("PATH", new_path);
        }

        let cfg_file = cfg_dir.join("config.json");
        let cfg_content = format!(
            r#"{{
  "version": 1,
  "server": {{
    "listen": "127.0.0.1",
    "port": {test_port}
  }},
  "scheduler": {{
    "strategy": "max_quota"
  }},
  "native_agy": {{
    "binary": null
  }},
  "logging": {{
    "max_size_bytes": 5242880,
    "backup_count": 1
  }}
}}
"#
        );
        fs::write(&cfg_file, cfg_content).unwrap();

        let env_log = temp_dir.join("env.log");
        let args_log = temp_dir.join("args.log");

        let initial_native_agy = target_bin.join("agy");
        let fake_native_script = format!(
            r#"#!/bin/sh
printf "%s" "$CLOUD_CODE_URL" > "{}"
: > "{}"
for arg in "$@"; do
  printf "%s\n" "$arg" >> "{}"
done
exit 0
"#,
            env_log.display(),
            args_log.display(),
            args_log.display()
        );
        fs::write(&initial_native_agy, fake_native_script).unwrap();
        fs::set_permissions(&initial_native_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let source_pool_bin = temp_dir.join("source-agy-pool");
        let staged_pool_script = r#"#!/bin/sh
cmd="$1"
shift
if [ "$cmd" = "run" ]; then
  cfgFile=""
  while [ $# -gt 0 ]; do
    case "$1" in
      -c)
        cfgFile="$2"
        shift 2
        ;;
      --)
        shift
        break
        ;;
      *)
        shift
        ;;
    esac
  done
  port=""
  if [ -n "$cfgFile" ] && [ -f "$cfgFile" ]; then
    port=$(grep -o '"port":[ ]*[0-9]*' "$cfgFile" | head -n1 | tr -dc '0-9')
  fi
  if [ -z "$port" ]; then
    port="8899"
  fi
  export CLOUD_CODE_URL="http://127.0.0.1:${port}"
  managed="$(dirname "$0")/../libexec/agy-pool/agy-native"
  exec "$managed" "$@"
elif [ "$cmd" = "raw" ]; then
  while [ $# -gt 0 ]; do
    if [ "$1" = "--" ]; then
      shift
      break
    fi
    shift
  done
  unset CLOUD_CODE_URL
  managed="$(dirname "$0")/../libexec/agy-pool/agy-native"
  exec "$managed" "$@"
else
  echo "Unknown command: $cmd" >&2
  exit 1
fi
"#;
        fs::write(&source_pool_bin, staged_pool_script).unwrap();
        fs::set_permissions(&source_pool_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let opts = Options {
            source_binary: source_pool_bin,
            target_dir: target_bin.clone(),
            home_dir: user_home.clone(),
            config_dir: cfg_dir,
            data_dir,
            ..Default::default()
        };

        install(opts).unwrap();

        let installed_agy = target_bin.join("agy");
        let installed_pool = target_bin.join("agy-pool");
        let installed_raw = target_bin.join("agy-raw");
        let installed_orig = target_bin.join("agy-orig");
        let managed_native = libexec_dir.join("agy-native");

        for p in [
            &installed_agy,
            &installed_pool,
            &installed_raw,
            &installed_orig,
            &managed_native,
        ] {
            let fi = fs::symlink_metadata(p).unwrap();
            if !fi.file_type().is_symlink() {
                assert_ne!(fi.permissions().mode() & 0o111, 0);
            }
        }

        let shim_bytes = fs::read(&installed_agy).unwrap();
        assert!(String::from_utf8_lossy(&shim_bytes).contains(MANAGED_SHIM_MARKER));

        // 6. Invoke managed 'agy' shim
        let test_args = [
            "--mode",
            "accept-edits",
            "--dangerously-skip-permissions",
            "-p",
            "hello test",
        ];
        let mut cmd = std::process::Command::new(&installed_agy);
        cmd.args(test_args);
        cmd.env("HOME", &user_home);
        let mut path_val = target_bin.to_str().unwrap().to_string();
        if let Ok(p) = std::env::var("PATH") {
            path_val.push(':');
            path_val.push_str(&p);
        }
        cmd.env("PATH", path_val);
        let output = cmd.output().expect("failed to run installed agy shim");
        assert!(
            output.status.success(),
            "running agy failed: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );

        let expected_url = format!("http://127.0.0.1:{test_port}");
        let actual_url = fs::read_to_string(&env_log).unwrap();
        assert_eq!(actual_url, expected_url);

        let args_data = fs::read_to_string(&args_log).unwrap();
        let received_args: Vec<&str> = args_data.trim().split('\n').collect();
        assert_eq!(received_args, test_args);

        // 7. Invoke 'agy-raw' shim
        fs::write(&env_log, "stale-url").unwrap();
        let mut raw_cmd = std::process::Command::new(&installed_raw);
        raw_cmd.args(["--raw-check", "arg1"]);
        raw_cmd.env("HOME", &user_home);
        raw_cmd.env("CLOUD_CODE_URL", "http://should-be-stripped");
        let mut path_val = target_bin.to_str().unwrap().to_string();
        if let Ok(p) = std::env::var("PATH") {
            path_val.push(':');
            path_val.push_str(&p);
        }
        raw_cmd.env("PATH", path_val);
        let raw_output = raw_cmd.output().expect("failed to run agy-raw shim");
        assert!(raw_output.status.success());
        let actual_raw_url = fs::read_to_string(&env_log).unwrap();
        assert!(actual_raw_url.is_empty(), "expected CLOUD_CODE_URL absent");

        // 8. Invoke 'agy-orig' symlink
        fs::write(&env_log, "stale-url").unwrap();
        let mut orig_cmd = std::process::Command::new(&installed_orig);
        orig_cmd.args(["--orig-check"]);
        orig_cmd.env("HOME", &user_home);
        orig_cmd.env("CLOUD_CODE_URL", "http://should-be-stripped");
        let mut path_val = target_bin.to_str().unwrap().to_string();
        if let Ok(p) = std::env::var("PATH") {
            path_val.push(':');
            path_val.push_str(&p);
        }
        orig_cmd.env("PATH", path_val);
        let orig_output = orig_cmd.output().expect("failed to run agy-orig shim");
        assert!(orig_output.status.success());
        let actual_orig_url = fs::read_to_string(&env_log).unwrap();
        assert!(actual_orig_url.is_empty(), "expected CLOUD_CODE_URL absent");
    }

    #[test]
    fn test_installer_isolation_user_mode_hermetic_roots() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let test_home = temp_dir.join("user_home");
        fs::create_dir_all(test_home.join(".local/bin")).unwrap();
        fs::create_dir_all(test_home.join(".config/agy-pool")).unwrap();
        fs::create_dir_all(test_home.join(".local/share/agy-pool")).unwrap();

        set_test_installer_roots(Some(InstallerRoots {
            user_home: test_home.clone(),
            bin_dir: test_home.join(".local/bin"),
            config_dir: test_home.join(".config/agy-pool"),
            data_dir: test_home.join(".local/share/agy-pool"),
            libexec_dir: test_home.join(".local/libexec/agy-pool"),
            system_root: temp_dir.join("sys_root"),
        }));

        let mock_agy = temp_dir.join("native_agy");
        fs::write(&mock_agy, "#!/bin/sh\necho mock native\n").unwrap();
        fs::set_permissions(&mock_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let roots = get_installer_roots();
        assert!(roots.bin_dir.starts_with(temp_dir));
        assert!(roots.config_dir.starts_with(temp_dir));
        assert!(roots.data_dir.starts_with(temp_dir));

        let real_home = crate::config::real_host_user_home();
        let real_entry = real_home.join(".local/bin/agy-pool");
        let real_existed = real_entry.exists();

        install(Options {
            native_agy_path: mock_agy,
            skip_shell_rc: true,
            ..Default::default()
        })
        .unwrap();

        let installed_agy = roots.bin_dir.join("agy");
        assert!(installed_agy.exists());
        let installed_cfg = roots.config_dir.join("config.json");
        assert!(installed_cfg.exists());

        uninstall(Options::default()).unwrap();
        assert!(!installed_agy.exists());

        if real_existed {
            assert!(real_entry.exists(), "CRITICAL: real host entrypoint was deleted!");
        }
    }

    #[test]
    fn test_installer_isolation_system_mode_synthetic_root() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let sys_root = temp_dir.join("sys_root");
        let test_home = temp_dir.join("user_home");
        fs::create_dir_all(sys_root.join("usr/local/bin")).unwrap();
        fs::create_dir_all(sys_root.join("etc/agy-pool")).unwrap();
        fs::create_dir_all(sys_root.join("var/lib/agy-pool")).unwrap();

        set_test_installer_roots(Some(InstallerRoots {
            user_home: test_home.clone(),
            bin_dir: test_home.join(".local/bin"),
            config_dir: test_home.join(".config/agy-pool"),
            data_dir: test_home.join(".local/share/agy-pool"),
            libexec_dir: test_home.join(".local/libexec/agy-pool"),
            system_root: sys_root.clone(),
        }));

        let mock_agy = temp_dir.join("native_agy");
        fs::write(&mock_agy, "#!/bin/sh\necho mock native\n").unwrap();
        fs::set_permissions(&mock_agy, fs::Permissions::from_mode(0o755)).unwrap();

        install(Options {
            is_system: true,
            native_agy_path: mock_agy,
            skip_shell_rc: true,
            ..Default::default()
        })
        .unwrap();

        let sys_shim = sys_root.join("usr/local/bin/agy");
        assert!(sys_shim.exists());
        let sys_cfg = sys_root.join("etc/agy-pool/config.json");
        assert!(sys_cfg.exists());

        uninstall(Options {
            is_system: true,
            ..Default::default()
        })
        .unwrap();

        assert!(!sys_shim.exists());
    }

    #[test]
    fn test_installer_isolation_cleanup_never_resolves_to_real_user_home() {
        let _env = crate::cli::test_support::cli_test_env();

        set_test_installer_roots(None);

        let roots = get_installer_roots();
        let real_home = crate::config::real_host_user_home();

        assert!(
            roots.user_home != real_home && !roots.user_home.starts_with(&real_home),
            "in test mode, get_installer_roots().user_home resolved to real user home: {:?}",
            roots.user_home
        );
        assert!(
            roots.bin_dir != real_home.join(".local/bin"),
            "in test mode, get_installer_roots().bin_dir resolved to real user bin: {:?}",
            roots.bin_dir
        );
    }

    #[test]
    fn test_uninstall_ownership_enforcement() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let target_dir = temp_dir.join("bin");
        fs::create_dir_all(&target_dir).unwrap();

        let unowned_agy = target_dir.join("agy");
        fs::write(&unowned_agy, "#!/bin/sh\necho 'unowned original agy'\n").unwrap();
        fs::set_permissions(&unowned_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let unowned_raw = target_dir.join("agy-raw");
        fs::write(&unowned_raw, "#!/bin/sh\necho 'unowned raw script'\n").unwrap();
        fs::set_permissions(&unowned_raw, fs::Permissions::from_mode(0o755)).unwrap();

        let unrelated_file = temp_dir.join("unrelated_target");
        fs::write(&unrelated_file, "important content").unwrap();
        let unowned_orig = target_dir.join("agy-orig");
        std::os::unix::fs::symlink(&unrelated_file, &unowned_orig).unwrap();

        let mut buf = Vec::new();
        let opts = Options {
            target_dir: target_dir.clone(),
            home_dir: temp_dir.to_path_buf(),
            stdout: Some(&mut buf),
            ..Default::default()
        };

        uninstall(opts).unwrap();

        assert!(unowned_agy.exists(), "CRITICAL DEFECT: unowned 'agy' was deleted!");
        assert!(unowned_raw.exists(), "CRITICAL DEFECT: unowned 'agy-raw' was deleted!");
        assert!(
            fs::symlink_metadata(&unowned_orig).is_ok(),
            "CRITICAL DEFECT: unowned 'agy-orig' was deleted!"
        );
        assert!(
            unrelated_file.exists(),
            "CRITICAL DEFECT: target of symlink was deleted!"
        );

        let out_str = String::from_utf8_lossy(&buf);
        assert!(
            out_str.contains("missing agy-pool ownership marker"),
            "expected missing ownership marker report, got: {out_str}"
        );
    }

    #[test]
    fn test_uninstall_toctou_hardening_symlink_swapped() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let target_dir = temp_dir.join("bin");
        fs::create_dir_all(&target_dir).unwrap();

        let victim_file = temp_dir.join("native_agy_victim");
        let victim_content = b"#!/bin/sh\necho 'victim native agy'\n";
        fs::write(&victim_file, victim_content).unwrap();
        fs::set_permissions(&victim_file, fs::Permissions::from_mode(0o755)).unwrap();

        let swapped_shim = target_dir.join("agy");
        std::os::unix::fs::symlink(&victim_file, &swapped_shim).unwrap();

        let mut buf = Vec::new();
        safe_remove_owned_entry(&target_dir, "agy", Some(&mut buf)).unwrap();

        assert!(victim_file.exists(), "SECURITY VIOLATION: victim file was deleted!");
        let cur_victim = fs::read(&victim_file).unwrap();
        assert_eq!(
            cur_victim, victim_content,
            "SECURITY VIOLATION: victim file was altered!"
        );
    }

    #[test]
    fn test_uninstall_toctou_hardening_hard_link_swapped() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let target_dir = temp_dir.join("bin");
        fs::create_dir_all(&target_dir).unwrap();

        let victim_file = temp_dir.join("native_agy_victim");
        let victim_content = b"#!/bin/sh\necho 'victim native agy'\n";
        fs::write(&victim_file, victim_content).unwrap();
        fs::set_permissions(&victim_file, fs::Permissions::from_mode(0o755)).unwrap();

        let hard_link_shim = target_dir.join("agy");
        if fs::hard_link(&victim_file, &hard_link_shim).is_err() {
            return;
        }

        let mut buf = Vec::new();
        safe_remove_owned_entry(&target_dir, "agy", Some(&mut buf)).unwrap();

        assert!(hard_link_shim.exists(), "CRITICAL DEFECT: hard link was unlinked!");
        assert!(victim_file.exists(), "CRITICAL DEFECT: victim file was deleted!");
    }

    #[test]
    fn test_native_agy_upgrade_and_movement() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let loc1_dir = temp_dir.join("loc1");
        let loc2_dir = temp_dir.join("loc2");
        fs::create_dir_all(&loc1_dir).unwrap();
        fs::create_dir_all(&loc2_dir).unwrap();

        let _h = EnvRestore::new("HOME");
        let _p = EnvRestore::new("PATH");
        let _b = EnvRestore::new("AGY_BIN");
        let _pr = EnvRestore::new("PREFIX");

        unsafe {
            std::env::set_var("HOME", temp_dir);
            std::env::set_var("PATH", &loc1_dir);
            std::env::set_var("AGY_BIN", "");
            std::env::set_var("PREFIX", "");
        }

        // Scenario A: Native agy replaced with newer executable at same path
        let agy_path = loc1_dir.join("agy");
        fs::write(&agy_path, "#!/bin/sh\necho '1.0.0'\n").unwrap();
        fs::set_permissions(&agy_path, fs::Permissions::from_mode(0o755)).unwrap();

        let (resolved, _) = crate::diagnostics::resolve_native_agy_binary().unwrap();
        assert_eq!(resolved, agy_path);

        fs::write(&agy_path, "#!/bin/sh\necho '2.0.0'\n").unwrap();
        let (resolved2, _) = crate::diagnostics::resolve_native_agy_binary().unwrap();
        assert_eq!(resolved2, agy_path);

        // Scenario B: Native agy removed from old path and appears at later PATH entry
        fs::remove_file(&agy_path).unwrap();
        let agy_path2 = loc2_dir.join("agy");
        fs::write(&agy_path2, "#!/bin/sh\necho '2.1.0'\n").unwrap();
        fs::set_permissions(&agy_path2, fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = format!("{}:{}", loc1_dir.display(), loc2_dir.display());
        unsafe { std::env::set_var("PATH", path_env) };

        let (resolved3, src) = crate::diagnostics::resolve_native_agy_binary().unwrap();
        assert_eq!(resolved3, agy_path2);
        assert_eq!(src, crate::diagnostics::SOURCE_PATH_ENV);

        // Scenario C: AGY_BIN points to new executable
        let loc3_dir = temp_dir.join("loc3");
        fs::create_dir_all(&loc3_dir).unwrap();
        let agy_path3 = loc3_dir.join("custom-agy");
        fs::write(&agy_path3, "#!/bin/sh\necho '3.0.0'\n").unwrap();
        fs::set_permissions(&agy_path3, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("AGY_BIN", &agy_path3) };

        let (resolved4, src) = crate::diagnostics::resolve_native_agy_binary().unwrap();
        assert_eq!(resolved4, agy_path3);
        assert_eq!(src, crate::diagnostics::SOURCE_ENV_AGY_BIN);
    }

    #[test]
    fn test_token_path_no_fictitious_path_regression() {
        let _env = crate::cli::test_support::cli_test_env();

        let token_path = crate::config::agy_token_file();
        assert!(
            token_path.ends_with("antigravity-oauth-token"),
            "expected token path to end with antigravity-oauth-token, got {:?}",
            token_path
        );
        let prohibited_name = format!("agy_{}", "token_compat");
        assert!(
            !token_path.to_string_lossy().contains(&prohibited_name),
            "fictitious path found in agy_token_file: {:?}",
            token_path
        );

        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

        fn walk_dir(dir: &Path, prohibited: &str) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if path.is_dir() {
                        if name_str == ".git" || name_str == "bin" || name_str == "target" {
                            continue;
                        }
                        walk_dir(&path, prohibited);
                    } else if path.is_file() {
                        if name_str.ends_with("uninstall_safety_test.go") || name_str == "mod.rs" {
                            continue;
                        }
                        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                        if ext == "go" || ext == "sh" || ext == "py" || ext == "rs" {
                            if let Ok(data) = fs::read(&path) {
                                let s = String::from_utf8_lossy(&data);
                                assert!(
                                    !s.contains(prohibited),
                                    "file {:?} contains prohibited fictitious token path {:?}",
                                    path,
                                    prohibited
                                );
                            }
                        }
                    }
                }
            }
        }

        walk_dir(repo_root, &prohibited_name);
    }

    #[test]
    fn test_install_no_go_state_in_gemini() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let fake_home = temp_dir.join("home_clean");
        fs::create_dir_all(&fake_home).unwrap();

        let target_dir = fake_home.join(".local/bin");
        let fake_native_agy = temp_dir.join("fake_native_agy");
        fs::write(&fake_native_agy, "#!/bin/sh\necho native\n").unwrap();
        fs::set_permissions(&fake_native_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let opts = Options {
            home_dir: fake_home.clone(),
            target_dir,
            native_agy_path: fake_native_agy,
            ..Default::default()
        };

        install(opts).unwrap();

        let fake_gemini = fake_home.join(".gemini");
        if let Ok(entries) = fs::read_dir(&fake_gemini) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(
                    !name.ends_with(".db")
                        && !name.ends_with(".pid")
                        && !name.ends_with(".log")
                        && !name.ends_with(".lock")
                        && name != "config.json",
                    "PROHIBITED STATE in ~/.gemini: found {}",
                    name
                );
            }
        }
    }

    #[test]
    fn test_upgrade_model_a_in_place_managed_native_self_update() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let user_home = temp_dir.join("user_home_a");
        let target_bin = user_home.join(".local/bin");
        let libexec_dir = user_home.join(".local/libexec/agy-pool");
        fs::create_dir_all(&target_bin).unwrap();

        let _h = EnvRestore::new("HOME");
        let _p = EnvRestore::new("PATH");
        unsafe {
            std::env::set_var("HOME", &user_home);
            std::env::set_var("PATH", &target_bin);
        }

        // Step 1: Initial install with v1 native binary
        let v1_content = "#!/bin/sh\necho '1.2.3'\n";
        let initial_agy = target_bin.join("agy");
        fs::write(&initial_agy, v1_content).unwrap();
        fs::set_permissions(&initial_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let opts = Options {
            target_dir: target_bin.clone(),
            home_dir: user_home.clone(),
            ..Default::default()
        };
        install(opts).unwrap();

        let managed_native = libexec_dir.join("agy-native");
        let shim_path = target_bin.join("agy");

        let initial_shim_hash = file_hash(&shim_path);

        // Step 2: In-place self-update of managed native binary to v2
        let v2_content = "#!/bin/sh\necho '2.0.0'\n";
        fs::write(&managed_native, v2_content).unwrap();

        let after_shim_hash = file_hash(&shim_path);
        assert_eq!(
            initial_shim_hash, after_shim_hash,
            "managed shim was modified during in-place native self-update!"
        );

        let (resolved, src) = crate::diagnostics::resolve_native_agy_binary().unwrap();
        assert_eq!(resolved, managed_native);
        assert_eq!(src, crate::diagnostics::SOURCE_MANAGED);

        let data = fs::read_to_string(&resolved).unwrap();
        assert_eq!(data, v2_content);

        // Doctor reports healthy layout without needing repair
        let shim_path_clone = shim_path.clone();
        crate::diagnostics::set_agy_entrypoint_finder(Some(Arc::new(move || {
            shim_path_clone.to_str().unwrap().to_string()
        })));
        crate::diagnostics::set_tls_prober(Some(Arc::new(|_, _| Ok(()))));

        let mut buf = Vec::new();
        let ok = crate::diagnostics::run_doctor(&mut buf);
        let doc_str = String::from_utf8_lossy(&buf);
        assert!(
            ok,
            "expected doctor to succeed on in-place updated native agy, got false. Output:\n{doc_str}"
        );
        assert!(
            doc_str.contains("managed shim ->"),
            "expected healthy managed shim message, got:\n{doc_str}"
        );
    }

    #[test]
    fn test_upgrade_model_b_official_updater_overwrites_shim_and_repair_flow() {
        let env = crate::cli::test_support::cli_test_env();
        let temp_dir = env.dir.path();

        let user_home = temp_dir.join("user_home_b");
        let target_bin = user_home.join(".local/bin");
        let libexec_dir = user_home.join(".local/libexec/agy-pool");
        let cfg_dir = user_home.join(".config/agy-pool");
        let data_dir = user_home.join(".local/share/agy-pool");

        fs::create_dir_all(&target_bin).unwrap();
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        let _h = EnvRestore::new("HOME");
        let _p = EnvRestore::new("PATH");
        unsafe {
            std::env::set_var("HOME", &user_home);
            std::env::set_var("PATH", &target_bin);
        }

        // Step 1: Initial install with v1 native binary
        let v1_content = "#!/bin/sh\necho 'native agy v1.2.3'\n";
        let target_agy = target_bin.join("agy");
        fs::write(&target_agy, v1_content).unwrap();
        fs::set_permissions(&target_agy, fs::Permissions::from_mode(0o755)).unwrap();

        let custom_config = cfg_dir.join("config.json");
        let custom_config_content =
            r#"{"version":1,"server":{"listen":"127.0.0.1","port":9444},"scheduler":{"strategy":"max_quota"}}"#;
        fs::write(&custom_config, custom_config_content).unwrap();

        let state_db = data_dir.join("state.db");
        fs::write(&state_db, b"SQLite format 3\x00--dummy-state-db-data--").unwrap();

        let opts = Options {
            target_dir: target_bin.clone(),
            home_dir: user_home.clone(),
            config_dir: cfg_dir.clone(),
            data_dir: data_dir.clone(),
            ..Default::default()
        };
        install(opts).unwrap();

        let managed_native = libexec_dir.join("agy-native");
        let config_hash_before = file_hash(&custom_config);
        let state_db_hash_before = file_hash(&state_db);

        // Step 2: Official updater overwrites target_bin/agy with new native ELF v2
        let v2_content = "#!/bin/sh\necho 'official native agy v2.0.0 by Google'\n";
        fs::write(&target_agy, v2_content).unwrap();
        fs::set_permissions(&target_agy, fs::Permissions::from_mode(0o755)).unwrap();

        // Step 3: agy-pool doctor detects and reports broken bypass state
        let target_agy_clone = target_agy.clone();
        crate::diagnostics::set_agy_entrypoint_finder(Some(Arc::new(move || {
            target_agy_clone.to_str().unwrap().to_string()
        })));

        let mut doc_buf = Vec::new();
        let doctor_healthy = crate::diagnostics::run_doctor(&mut doc_buf);
        assert!(!doctor_healthy, "expected doctor to report error");
        let doc_output = String::from_utf8_lossy(&doc_buf);
        assert!(doc_output.contains("is a native binary bypassing agy-pool"));
        assert!(doc_output.contains("agy-pool repair"));

        // Step 4: Run Repair
        let opts_rep = Options {
            target_dir: target_bin.clone(),
            home_dir: user_home.clone(),
            config_dir: cfg_dir.clone(),
            data_dir: data_dir.clone(),
            ..Default::default()
        };
        repair(opts_rep).unwrap();

        // 1) Verify managed native now contains v2 content
        let preserved_v2 = fs::read_to_string(&managed_native).unwrap();
        assert_eq!(preserved_v2, v2_content);

        // 2) Verify targetDir/agy is restored to managed shim
        let restored_shim = fs::read_to_string(&target_agy).unwrap();
        assert!(restored_shim.contains(MANAGED_SHIM_MARKER));
        assert!(restored_shim.contains("agy-pool run"));

        // 3) Verify resolver now resolves to managed native v2
        let (resolved_agy, src) = crate::diagnostics::resolve_native_agy_binary().unwrap();
        assert_eq!(resolved_agy, managed_native);
        assert_eq!(src, crate::diagnostics::SOURCE_MANAGED);

        // 4) Verify config.json and state.db remain completely unchanged
        let config_hash_after = file_hash(&custom_config);
        let state_db_hash_after = file_hash(&state_db);
        assert_eq!(config_hash_before, config_hash_after);
        assert_eq!(state_db_hash_before, state_db_hash_after);

        // 5) Verify doctor reports healthy after repair
        doc_buf.clear();
        assert!(crate::diagnostics::run_doctor(&mut doc_buf));

        // 6) Verify repair is idempotent
        let opts_rep2 = Options {
            target_dir: target_bin.clone(),
            home_dir: user_home.clone(),
            config_dir: cfg_dir.clone(),
            data_dir: data_dir.clone(),
            ..Default::default()
        };
        repair(opts_rep2).unwrap();
        let content_after = fs::read_to_string(&managed_native).unwrap();
        assert_eq!(content_after, v2_content);

        // Step 5: Repeated official upgrade: updater overwrites with native ELF v3
        let v3_content = "#!/bin/sh\necho 'official native agy v3.1.0 by Google'\n";
        fs::write(&target_agy, v3_content).unwrap();
        fs::set_permissions(&target_agy, fs::Permissions::from_mode(0o755)).unwrap();

        doc_buf.clear();
        assert!(!crate::diagnostics::run_doctor(&mut doc_buf));

        let opts_rep3 = Options {
            target_dir: target_bin.clone(),
            home_dir: user_home.clone(),
            config_dir: cfg_dir,
            data_dir,
            ..Default::default()
        };
        repair(opts_rep3).unwrap();

        let preserved_v3 = fs::read_to_string(&managed_native).unwrap();
        assert_eq!(preserved_v3, v3_content);
    }
}
