use crate::storage::Pool;
use serde::{Deserialize, Serialize};

/// BurnRateTelemetry contains rolling window inference metrics and estimated burn rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BurnRateTelemetry {
    pub gens_last_15m: usize,
    pub gens_last_1h: usize,
    pub gens_last_4h: usize,
    pub burst_rate_15m: f64,
    pub sustained_rate_1h: f64,
    pub sustained_rate_4h: f64,
    pub gens_per_hour: f64,
    pub daily_pace: f64,
    pub is_idle: bool,
    pub window_used: String,
}

/// Calculates recent inference frequency and quota burn rate across rolling windows.
/// Applies multi-window blended moving average and burst damping for realistic forecasting.
pub fn get_burn_rate(pool: &Pool, now_opt: Option<f64>) -> BurnRateTelemetry {
    let now = match now_opt {
        Some(n) if n > 0.0 => n,
        _ => crate::clock::now_unix(),
    };

    let mut valid_ts = Vec::new();
    for e in &pool.generation_events {
        if e.ts > 0 {
            valid_ts.push(e.ts as f64);
        }
    }

    let mut n15 = 0;
    let mut n60 = 0;
    let mut n240 = 0;
    for &ts in &valid_ts {
        let diff = now - ts;
        if diff <= 900.0 {
            n15 += 1;
        }
        if diff <= 3600.0 {
            n60 += 1;
        }
        if diff <= 14400.0 {
            n240 += 1;
        }
    }

    let r15 = (n15 as f64) * 4.0;
    let r60 = n60 as f64;
    let r240 = (n240 as f64) / 4.0;

    let rate: f64;
    let window: &str;

    if n240 == 0 {
        rate = 0.0;
        window = "idle";
    } else if n60 >= 10 {
        // Sustained session: damp high-frequency micro-bursts (max 2x 1h rate or at least 30)
        let r_burst = r15.min((r60 * 2.0).max(30.0));
        rate = 0.50 * r60 + 0.30 * r_burst + 0.20 * r240;
        window = "smoothed";
    } else if n15 >= 2 {
        rate = r15;
        window = "15m";
    } else if n60 >= 1 {
        rate = r60;
        window = "1h";
    } else {
        rate = r240;
        window = "4h";
    }

    let rate = (rate * 10.0).round() / 10.0;
    let daily_pace = if rate > 0.0 {
        ((rate / 3.0) * 10.0).round() / 10.0
    } else {
        0.0
    };

    BurnRateTelemetry {
        gens_last_15m: n15,
        gens_last_1h: n60,
        gens_last_4h: n240,
        burst_rate_15m: (r15 * 10.0).round() / 10.0,
        sustained_rate_1h: (r60 * 10.0).round() / 10.0,
        sustained_rate_4h: (r240 * 10.0).round() / 10.0,
        gens_per_hour: rate,
        daily_pace,
        is_idle: rate == 0.0,
        window_used: window.to_string(),
    }
}
