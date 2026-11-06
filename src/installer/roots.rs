use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerRoots {
    pub user_home: PathBuf,
    pub bin_dir: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub libexec_dir: PathBuf,
    pub system_root: PathBuf,
}

static TEST_INSTALLER_ROOTS: RwLock<Option<InstallerRoots>> = RwLock::new(None);

pub fn set_test_installer_roots(roots: Option<InstallerRoots>) {
    let mut lock = TEST_INSTALLER_ROOTS.write().unwrap_or_else(|e| e.into_inner());
    *lock = roots;
}

pub fn get_installer_roots() -> InstallerRoots {
    let custom = {
        let lock = TEST_INSTALLER_ROOTS.read().unwrap_or_else(|e| e.into_inner());
        (*lock).clone()
    };

    #[allow(unused_mut)]
    let mut roots = if let Some(custom) = custom {
        custom
    } else if crate::config::is_test_mode() {
        let sandbox = crate::config::get_synthetic_sandbox_root()
            .unwrap_or_else(|| std::env::temp_dir().join("agy_test_sandbox"));
        let test_home = sandbox.join("test_home");
        InstallerRoots {
            user_home: test_home.clone(),
            bin_dir: test_home.join(".local/bin"),
            config_dir: test_home.join(".config/agy-pool"),
            data_dir: test_home.join(".local/share/agy-pool"),
            libexec_dir: test_home.join(".local/libexec/agy-pool"),
            system_root: sandbox.join("test_system"),
        }
    } else {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| crate::config::real_host_user_home());
        InstallerRoots {
            user_home: home.clone(),
            bin_dir: home.join(".local/bin"),
            config_dir: home.join(".config/agy-pool"),
            data_dir: home.join(".local/share/agy-pool"),
            libexec_dir: home.join(".local/libexec/agy-pool"),
            system_root: PathBuf::new(),
        }
    };

    #[cfg(test)]
    {
        if !roots.user_home.as_os_str().is_empty() {
            roots.user_home = crate::config::deny_live_path(roots.user_home);
        }
        if !roots.bin_dir.as_os_str().is_empty() {
            roots.bin_dir = crate::config::deny_live_path(roots.bin_dir);
        }
        if !roots.config_dir.as_os_str().is_empty() {
            roots.config_dir = crate::config::deny_live_path(roots.config_dir);
        }
        if !roots.data_dir.as_os_str().is_empty() {
            roots.data_dir = crate::config::deny_live_path(roots.data_dir);
        }
        if !roots.libexec_dir.as_os_str().is_empty() {
            roots.libexec_dir = crate::config::deny_live_path(roots.libexec_dir);
        }
        if !roots.system_root.as_os_str().is_empty() {
            roots.system_root = crate::config::deny_live_path(roots.system_root);
        }
    }

    roots
}
