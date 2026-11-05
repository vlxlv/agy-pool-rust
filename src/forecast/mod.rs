pub mod burn_rate;
pub mod format;
pub mod runway;

pub use burn_rate::*;
pub use format::*;
pub use runway::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Account, GenerationEvent, Pool, QuotaState, QuotaWindow, Status};

    #[test]
    fn format_duration_short_test() {
        assert_eq!(format_duration_short(None), "N/A");

        let sec30 = 30.0;
        assert_eq!(format_duration_short(Some(sec30)), "30s");

        let sec45m = 2700.0;
        assert_eq!(format_duration_short(Some(sec45m)), "45m");

        let sec4h50m = 4.0 * 3600.0 + 50.0 * 60.0;
        assert_eq!(format_duration_short(Some(sec4h50m)), "4h 50m");

        let sec6d22h = 6.0 * 86400.0 + 22.0 * 3600.0;
        assert_eq!(format_duration_short(Some(sec6d22h)), "6d 22h");
    }

    #[test]
    fn get_burn_rate_test() {
        let now = 1_000_000.0;

        // 1. Idle pool
        let pool = Pool::default();
        let telemetry = get_burn_rate(&pool, Some(now));
        assert!(telemetry.is_idle);
        assert_eq!(telemetry.gens_per_hour, 0.0);
        assert_eq!(telemetry.window_used, "idle");

        // 2. 15m window (e.g. 3 events in last 10 minutes)
        let pool_15m = Pool {
            generation_events: vec![
                GenerationEvent {
                    ts: (now - 100.0) as i64,
                    id: "acc_1".to_string(),
                },
                GenerationEvent {
                    ts: (now - 200.0) as i64,
                    id: "acc_1".to_string(),
                },
                GenerationEvent {
                    ts: (now - 300.0) as i64,
                    id: "acc_2".to_string(),
                },
            ],
            ..Default::default()
        };
        let telemetry = get_burn_rate(&pool_15m, Some(now));
        assert_eq!(telemetry.gens_last_15m, 3);
        assert_eq!(telemetry.gens_per_hour, 12.0);
        assert_eq!(telemetry.window_used, "15m");

        // 3. Sustained / smoothed window (>= 10 in 1h)
        let pool_smooth = Pool {
            generation_events: (0..15)
                .map(|i| GenerationEvent {
                    ts: (now - (i as f64) * 100.0) as i64,
                    id: "acc_1".to_string(),
                })
                .collect(),
            ..Default::default()
        };
        let telemetry = get_burn_rate(&pool_smooth, Some(now));
        assert_eq!(telemetry.window_used, "smoothed");
        assert!(!telemetry.is_idle);
    }

    #[test]
    fn simulate_quota_runway_empty_or_restricted() {
        let now = 1_000_000.0;

        // Empty pool
        let empty = Pool::default();
        let res = simulate_quota_runway(&empty, None, Some(now)).expect("forecast");
        assert_eq!(res.status, "no_accounts");

        // All restricted
        let pool = Pool {
            accounts: vec![
                Account {
                    id: "acc_1".to_string(),
                    name: "A1".to_string(),
                    status: Status::VALIDATION_REQUIRED.to_string(),
                    ..Default::default()
                },
                Account {
                    id: "acc_2".to_string(),
                    name: "A2".to_string(),
                    status: Status::AUTH_ERROR.to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let res = simulate_quota_runway(&pool, None, Some(now)).expect("forecast");
        assert_eq!(res.status, "all_restricted");
        assert_eq!(res.healthy_count, 0);
        assert_eq!(res.restricted_count, 2);
    }

    #[test]
    fn simulate_quota_runway_healthy() {
        let now = crate::clock::now_unix();
        let frac = 0.8;
        let pool = Pool {
            accounts: vec![
                Account {
                    id: "acc_1".to_string(),
                    name: "Account 1".to_string(),
                    last_quota: Some(QuotaState {
                        gemini_5h: Some(QuotaWindow {
                            fraction: Some(frac),
                            ..Default::default()
                        }),
                        gemini_weekly: Some(QuotaWindow {
                            fraction: Some(frac),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Account {
                    id: "acc_2".to_string(),
                    name: "Account 2".to_string(),
                    last_quota: Some(QuotaState {
                        gemini_5h: Some(QuotaWindow {
                            fraction: Some(frac),
                            ..Default::default()
                        }),
                        gemini_weekly: Some(QuotaWindow {
                            fraction: Some(frac),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        // 1. Idle simulation
        let res = simulate_quota_runway(&pool, None, Some(now)).expect("forecast");
        assert_eq!(res.status, "ok");
        assert_eq!(res.healthy_count, 2);
        assert!(res.is_idle);
        assert!(res.benchmark_runway.is_sustainable_5h);
        assert!(res.benchmark_runway.is_sustainable_weekly);

        // 2. Custom heavy pace simulation (e.g. 500 gens/hr)
        let heavy_pace = 500.0;
        let res_heavy = simulate_quota_runway(&pool, Some(heavy_pace), Some(now)).expect("forecast");
        assert_eq!(res_heavy.status, "ok");
        assert!(res_heavy.is_custom_pace);
        assert!(!res_heavy.current_runway.is_sustainable_5h);
        assert!(res_heavy.current_runway.first_stall_sec.is_some());
        assert!(res_heavy.current_runway.first_stall_sec.unwrap() > 0.0);

        // 3. PrintRunwaySummary output check
        let mut buf = Vec::new();
        print_runway_summary(&mut buf, &pool, now, None);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("⚡ Quota Runway & Endurance Forecast:"));
        assert!(out.contains("2/2 Active"));
    }

    #[test]
    fn runway_summary_matches_go() {
        let pool_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/pool/valid_3_accounts.json");
        let pool_data = std::fs::read_to_string(pool_path).unwrap();
        let pool: crate::storage::Pool = serde_json::from_str(&pool_data).unwrap();

        let now = 1789560000.0;
        let mut buf = Vec::new();
        print_runway_summary(&mut buf, &pool, now, Some(100));
        let got = String::from_utf8(buf).unwrap();

        let expected = "--------------------------------------------------------------------\n \
\x1b[1m\x1b[36m⚡ Quota Runway & Endurance Forecast:\x1b[0m\n  \
• Health & Pace:   3/3 Active | \x1b[2mIdle (0 gens/hr)\x1b[0m\n  \
• Capacity Reload: +60.0%/hr natural recovery across pool\n  \
• Active Coding:   \x1b[32m⚡ Sustainable\x1b[0m (> 24h at standard ~20 gens/hr pace)\n  \
• Weekly Budget:   \x1b[32m⚡ Sustainable\x1b[0m (> 7 Days remaining)\n";

        assert_eq!(got, expected);
    }
}
