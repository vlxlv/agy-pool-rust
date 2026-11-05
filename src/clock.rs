use std::sync::RwLock;
use std::time::SystemTime;

static OVERRIDE: RwLock<Option<fn() -> SystemTime>> = RwLock::new(None);

pub fn now() -> SystemTime {
    match *OVERRIDE.read().unwrap_or_else(|e| e.into_inner()) {
        Some(f) => f(),
        None => SystemTime::now(),
    }
}

pub fn now_unix() -> f64 {
    now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub struct ClockGuard(Option<fn() -> SystemTime>);

pub fn set_now_for_test(f: fn() -> SystemTime) -> ClockGuard {
    let mut w = OVERRIDE.write().unwrap_or_else(|e| e.into_inner());
    ClockGuard(w.replace(f))
}

impl Drop for ClockGuard {
    fn drop(&mut self) {
        *OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn override_and_restore() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fn fixed() -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        }
        {
            let _g = set_now_for_test(fixed);
            assert_eq!(now_unix(), 1_800_000_000.0);
        }
        assert!(now_unix() > 1_700_000_000.0 && now_unix() != 1_800_000_000.0);
    }
}
