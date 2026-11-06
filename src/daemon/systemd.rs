use super::DaemonError;

pub const SYSTEMD_UNIT_NAME: &str = "agy-pool.service";

#[cfg(test)]
pub static TEST_LIVE_PID_OVERRIDE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Reads the PID of the live gateway if the PID file exists on this host.
pub fn live_gateway_pid() -> Option<i32> {
    #[cfg(test)]
    {
        let override_pid = TEST_LIVE_PID_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
        if override_pid > 0 {
            return Some(override_pid);
        }
    }
    let p = crate::config::real_host_user_home().join(".local/share/agy-pool/agy-pool.pid");
    if let Ok(data) = std::fs::read_to_string(&p)
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&data)
        && let Some(pid) = val.get("pid").and_then(|v| v.as_i64())
        && pid > 0
    {
        return Some(pid as i32);
    }
    None
}

/// Sends a signal to a process.
/// In tests, safely refuses to send any signal to the live host gateway process.
pub fn send_signal(pid: i32, sig: i32) -> Result<(), std::io::Error> {
    #[cfg(test)]
    {
        let live = if TEST_LIVE_PID_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            Some(TEST_LIVE_PID_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed))
        } else {
            live_gateway_pid()
        };
        if let Some(live_pid) = live
            && pid == live_pid
            && live_pid > 0
        {
            panic!("SAFETY GUARD: refused to send signal to live gateway pid {}", pid);
        }
    }
    let ret = unsafe { libc::kill(pid, sig) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// systemd_scope reports how systemd supervises pid: "--user", "--system", or "" when it does not.
pub fn systemd_scope(pid: i32) -> &'static str {
    let path = format!("/proc/{}/cgroup", pid);
    if let Ok(b) = std::fs::read_to_string(path) {
        cgroup_scope(&b)
    } else {
        ""
    }
}

/// Parses cgroup content to determine systemd supervision scope.
pub fn cgroup_scope(cgroup: &str) -> &'static str {
    let suffix = format!("/{}", SYSTEMD_UNIT_NAME);
    for line in cgroup.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with(&suffix) {
            if trimmed.contains("/user@") {
                return "--user";
            }
            return "--system";
        }
    }
    ""
}

/// Runs a systemctl verb on the agy-pool unit in scope.
/// In test mode, this panics unconditionally as required by live-gateway safety rules.
pub fn systemctl(scope: &str, verb: &str) -> Result<(), DaemonError> {
    #[cfg(test)]
    {
        panic!(
            "SAFETY GUARD: systemctl is refused in tests (scope={}, verb={})",
            scope, verb
        );
    }
    #[cfg(not(test))]
    {
        let out = std::process::Command::new("systemctl")
            .args([scope, verb, SYSTEMD_UNIT_NAME])
            .output()
            .map_err(|e| DaemonError::Message(format!("failed to execute systemctl: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            return Err(DaemonError::Message(format!(
                "systemctl {} {} {}: status {}: {}{}",
                scope,
                verb,
                SYSTEMD_UNIT_NAME,
                out.status,
                stdout.trim(),
                stderr.trim()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cgroup_scope() {
        let cases = [
            (
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/agy-pool.service\n",
                "--user",
            ),
            ("0::/system.slice/agy-pool.service\n", "--system"),
            (
                "12:pids:/system.slice/agy-pool.service\n1:name=systemd:/system.slice/agy-pool.service\n",
                "--system",
            ),
            ("0::/user.slice/user-1000.slice/session-3.scope\n", ""),
            ("0::/system.slice/other-agy-pool.service.d\n", ""),
        ];

        for (cgroup, want) in cases {
            assert_eq!(
                cgroup_scope(cgroup),
                want,
                "cgroup_scope({:?}) should be {:?}",
                cgroup,
                want
            );
        }
    }

    #[test]
    #[should_panic(expected = "SAFETY GUARD: systemctl is refused in tests")]
    fn systemctl_refused_in_tests() {
        let _ = systemctl("--user", "status");
    }

    #[test]
    #[should_panic(expected = "SAFETY GUARD: refused to send signal to live gateway pid")]
    fn send_signal_refuses_live_gateway_pid() {
        let live_pid = live_gateway_pid().unwrap_or(9999999);
        TEST_LIVE_PID_OVERRIDE.store(live_pid, std::sync::atomic::Ordering::Relaxed);
        let _ = send_signal(live_pid, 0);
    }
}
