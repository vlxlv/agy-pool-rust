use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::quota::{CapacityState, compute_capacity_state, freshness_rank};
use crate::storage::{Account, Pool, StorageError};

static SMART_RR_CURSOR: AtomicU64 = AtomicU64::new(0);

/// Advance the smart round-robin cursor by 1 and return the new value.
pub fn advance_smart_rr_cursor() -> u64 {
    SMART_RR_CURSOR.fetch_add(1, Ordering::SeqCst) + 1
}

/// Return the current smart round-robin cursor.
pub fn smart_rr_cursor() -> u64 {
    SMART_RR_CURSOR.load(Ordering::SeqCst)
}

/// Return the current smart round-robin cursor (Go alias).
pub fn get_smart_rr_cursor() -> u64 {
    smart_rr_cursor()
}

/// Reset the smart round-robin cursor to 0 (used for test isolation).
pub fn reset_smart_rr_cursor() {
    SMART_RR_CURSOR.store(0, Ordering::SeqCst);
}

/// FiveHourReserveFloor is the threshold below or equal to which an account's 5-hour quota is in reserve.
pub const FIVE_HOUR_RESERVE_FLOOR: f64 = 0.15;

/// WeeklyReserveFloor is the base threshold below or equal to which an account's weekly quota is in reserve.
pub const WEEKLY_RESERVE_FLOOR: f64 = 0.10;

/// DynamicWeeklyReserveFloor returns the time-decaying weekly reserve floor.
/// As weekly reset approaches, the reserve floor decays proportionally with the remaining time in the week,
/// ensuring perishable quota is not stranded before weekly reset.
pub fn dynamic_weekly_reserve_floor(r7: f64) -> f64 {
    if r7 <= 0.0 || r7 >= 1.0 {
        return WEEKLY_RESERVE_FLOOR;
    }
    let floor = WEEKLY_RESERVE_FLOOR * r7;
    if floor < crate::config::DEPLETED_THRESHOLD {
        return crate::config::DEPLETED_THRESHOLD;
    }
    floor
}

/// IsReserveCandidate reports whether a candidate account has one or more known quota windows
/// at or below the reserve thresholds, while not being depleted.
pub fn is_reserve_candidate(acc: &Account, now: f64) -> bool {
    let cap_state = compute_capacity_state(acc, now);
    is_reserve_capacity(&cap_state)
}

/// IsReserveCapacity reports whether a computed CapacityState qualifies as reserve.
/// Legacy fractions have no window identity; they retain existing capacity ranking
/// and depletion behavior without applying a window-specific reserve floor.
pub fn is_reserve_capacity(cap_state: &CapacityState) -> bool {
    if cap_state.is_depleted {
        return false;
    }
    if cap_state.q5_known && cap_state.q5.is_some_and(|q| q <= FIVE_HOUR_RESERVE_FLOOR) {
        return true;
    }
    let weekly_floor = dynamic_weekly_reserve_floor(cap_state.r7);
    if cap_state.q7_known && cap_state.q7.is_some_and(|q| q <= weekly_floor) {
        return true;
    }
    false
}

/// WeeklyPaceSlack is how far weekly quota may run ahead of elapsed weekly time (q7 - r7) before
/// routing holds the account back. Accounts with >= 70% weekly quota left are never affected.
pub const WEEKLY_PACE_SLACK: f64 = 0.30;

/// OverWeeklyPace reports whether an account is spending its week faster than time passes, so routing
/// can move it behind on-pace accounts and concurrency cannot drain one weekly quota early in the cycle.
/// Ordering and affinity use it; reserve labels and audit still use IsReserveCandidate.
pub fn over_weekly_pace(acc: &Account, now: f64) -> bool {
    let c = compute_capacity_state(acc, now);
    over_weekly_pace_capacity(&c)
}

/// OverWeeklyPaceCapacity reports whether a CapacityState indicates over-pace weekly spending.
pub fn over_weekly_pace_capacity(c: &CapacityState) -> bool {
    c.q7_known && c.pace7.is_some() && c.reset7_time.is_some() && c.pace7.unwrap() < -WEEKLY_PACE_SLACK - 1e-9
}

/// IsRestricted reports whether an account is blocked from generation routing.
pub fn is_restricted(acc: &Account) -> bool {
    acc.status == crate::storage::STATUS_VALIDATION_REQUIRED
        || acc.status == crate::storage::STATUS_VERIFICATION_PENDING
        || acc.status == crate::storage::STATUS_AUTH_ERROR
        || acc.status == crate::storage::STATUS_DISABLED
}

/// EffectiveWorstPace returns the scheduling pace for candidate ranking.
/// When an account's weekly quota is abundant (>= 0.80), the weekly window is not
/// a constraint bottleneck; we use 5H pace to avoid cold-start linear decay starvation
/// at the beginning of a 7-day cycle. Between 0.50 and 0.80 weekly quota, it blends
/// smoothly from Pace5 into WorstPace to avoid a cliff discontinuity at 0.80.
pub fn effective_worst_pace(cap_state: &CapacityState) -> f64 {
    if cap_state.q5_known
        && let Some(q5) = cap_state.q5
        && let Some(p5) = cap_state.pace5
    {
        let _ = q5;
        if cap_state.q7_known
            && let Some(q7) = cap_state.q7
        {
            if q7 >= 0.80 {
                return p5;
            }
            if q7 > 0.50 && cap_state.worst_pace < p5 {
                let w = (0.80 - q7) / 0.30;
                return (1.0 - w) * p5 + w * cap_state.worst_pace;
            }
        }
    }
    cap_state.worst_pace
}

// Smart scheduling constants
pub const SMART_HARVEST_5H_WINDOW: Duration = Duration::from_secs(90 * 60);
pub const SMART_HARVEST_MIN_HEADROOM: f64 = 0.25;
pub const SMART_HARVEST_MIN_WEEKLY_HEADROOM: f64 = 0.30;

const SMART_IN_FLIGHT_PER_TIER: usize = 4;
const MAX_QUOTA_IN_FLIGHT_PENALTY: f64 = 0.03;
const PACE_EPSILON: f64 = 0.005;

const BUCKET_15M_NS: i64 = 15 * 60 * 1_000_000_000;
const BUCKET_1H_NS: i64 = 3600 * 1_000_000_000;
const WINDOW_5H_NS: i64 = 90 * 60 * 1_000_000_000;

// smartHeadroomTier maps an account's bottleneck headroom to discrete tiers (20% bands).
// This guarantees strict weak ordering (transitivity) in sorting while enabling
// active concurrent multi-account rotation within each health band.
fn smart_headroom_tier(headroom: f64) -> i32 {
    if headroom >= 0.80 {
        4 // Prime: 80% - 100%
    } else if headroom >= 0.60 {
        3 // Healthy: 60% - 80%
    } else if headroom >= 0.40 {
        2 // Moderate: 40% - 60%
    } else if headroom >= 0.20 {
        1 // Low/Conservation: 20% - 40%
    } else {
        0 // Near Reserve: < 20%
    }
}

// smartHarvestFloor is the headroom an expiring window needs to be harvested. It decays linearly
// from SmartHarvestMinHeadroom at the start of the harvest window to the depleted threshold at reset,
// so small leftovers are still spent right before they expire.
fn smart_harvest_floor(rem: Duration, window: Duration) -> f64 {
    let floor = SMART_HARVEST_MIN_HEADROOM * rem.as_secs_f64() / window.as_secs_f64();
    if floor < crate::config::DEPLETED_THRESHOLD {
        crate::config::DEPLETED_THRESHOLD
    } else {
        floor
    }
}

/// SmartHarvestCandidate checks if an account qualifies for harvest mode: an upcoming 5H reset
/// within 90m whose expiring window still has at least smartHarvestFloor headroom, while
/// weekly quota maintains at least 30% headroom and is not over weekly pace. Weekly quota is never
/// harvested as it is a hard stock ceiling for the entire week. Returns (earliestResetTime, true).
pub fn smart_harvest_candidate(cap_state: &CapacityState, now_time: SystemTime) -> (Option<SystemTime>, bool) {
    let legacy = !cap_state.q5_known && !cap_state.q7_known;
    if legacy && cap_state.raw_floor < SMART_HARVEST_MIN_HEADROOM {
        return (None, false);
    }
    let at_least =
        |known: bool, q: Option<f64>, min: f64| -> bool { legacy || !known || q.is_none() || q.unwrap() >= min };

    if let Some(reset5) = cap_state.reset5_time
        && let Ok(rem5) = reset5.duration_since(now_time)
        && rem5 > Duration::ZERO
        && rem5 <= SMART_HARVEST_5H_WINDOW
        && !over_weekly_pace_capacity(cap_state)
        && at_least(
            cap_state.q5_known,
            cap_state.q5,
            smart_harvest_floor(rem5, SMART_HARVEST_5H_WINDOW),
        )
        && at_least(cap_state.q7_known, cap_state.q7, SMART_HARVEST_MIN_WEEKLY_HEADROOM)
    {
        return (Some(reset5), true);
    }

    (None, false)
}

/// NextUpcomingReset returns the earliest upcoming reset time for the account.
pub fn next_upcoming_reset(cap_state: &CapacityState) -> Option<SystemTime> {
    let mut earliest = cap_state.reset5_time;
    if let Some(r7) = cap_state.reset7_time {
        match earliest {
            None => earliest = Some(r7),
            Some(e) => {
                if r7 < e {
                    earliest = Some(r7);
                }
            }
        }
    }
    earliest
}

// smartEffectiveHeadroom returns the quota fraction used to assign an account's cruising health tier.
// When weekly quota is abundant (>= 80%) or on/ahead of linear weekly pace (Pace7 >= 0), the weekly
// window is not a bottleneck, so the regenerable 5H window determines the health tier rather than
// letting mid-week Q7 < 80% strand a full 5H window.
fn smart_effective_headroom(cap_state: &CapacityState) -> f64 {
    if cap_state.q5_known
        && let Some(q5) = cap_state.q5
        && cap_state.q7_known
        && let Some(q7) = cap_state.q7
    {
        let _ = q7;
        if cap_state.q7.is_some_and(|w| w >= 0.80) || cap_state.pace7.is_some_and(|p| p >= -1e-9) {
            return q5;
        }
    }
    cap_state.raw_floor
}

// resetBucket quantizes a reset timestamp relative to nowTime: 15-minute buckets within the 90m
// 5H harvest range so imminent resets are never merged with hour-away resets across a UTC clock seam,
// and 1-hour buckets (offset by 100 to preserve monotonicity across the 90m boundary) beyond 90m.
fn reset_bucket(reset_time: SystemTime, now_time: SystemTime) -> i64 {
    let rem_ns = match reset_time.duration_since(now_time) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    };
    if rem_ns <= WINDOW_5H_NS {
        rem_ns / BUCKET_15M_NS
    } else {
        100 + (rem_ns / BUCKET_1H_NS)
    }
}

// compareResetTiming compares two accounts' upcoming reset schedules (earliest reset first, then
// secondary window reset if earliest buckets tie). Returns <0 if a resets sooner, >0 if b resets sooner.
fn compare_reset_timing(a: &CapacityState, b: &CapacityState, now_time: SystemTime) -> i32 {
    let next_a = next_upcoming_reset(a);
    let next_b = next_upcoming_reset(b);
    if next_a.is_none() && next_b.is_none() {
        return 0;
    }
    if next_a.is_none() {
        return 1;
    }
    if next_b.is_none() {
        return -1;
    }
    let ba = reset_bucket(next_a.unwrap(), now_time);
    let bb = reset_bucket(next_b.unwrap(), now_time);
    if ba != bb {
        if ba < bb {
            return -1;
        }
        return 1;
    }

    let later_window = |c: &CapacityState| -> Option<SystemTime> {
        if let (Some(r5), Some(r7)) = (c.reset5_time, c.reset7_time) {
            if r5 > r7 { Some(r5) } else { Some(r7) }
        } else {
            None
        }
    };

    let la = later_window(a);
    let lb = later_window(b);
    if let (Some(la_time), Some(lb_time)) = (la, lb) {
        let lba = reset_bucket(la_time, now_time);
        let lbb = reset_bucket(lb_time, now_time);
        if lba != lbb {
            if lba < lbb {
                return -1;
            }
            return 1;
        }
    }
    0
}

struct SortKey {
    cap: CapacityState,
    fresh: i32,
    flight: i64,
    hits: i64,
    harvest: bool,
    harvest_b: i64,
    score: i32,
    pace: f64,
    floor: f64,
    total_pace: f64,
}

// bucket quantizes v so epsilon comparisons stay transitive; infinities pass through unchanged.
fn bucket(v: f64, eps: f64) -> f64 {
    (v / eps).floor()
}

/// Order candidate accounts according to the specified strategy.
///
/// Returns indices into `accounts` in scheduled priority order.
/// Following the Phase 2 plan's rule: `storage::Pool` does not hold `Arc<Account>`,
/// so `order_candidates` returns `Vec<usize>` indices into `accounts` rather than
/// cloning or using `Arc`.
pub fn order_candidates(accounts: &[Account], strategy: &str, _pool: &Pool, now: f64) -> Vec<usize> {
    if accounts.is_empty() {
        return Vec::new();
    }

    let now_time = if now >= 0.0 {
        UNIX_EPOCH + Duration::from_secs(now as u64)
    } else {
        UNIX_EPOCH
    };

    let mut eligible = Vec::new();
    let mut depleted = Vec::new();
    let mut cooldown = Vec::new();
    let mut keys: HashMap<usize, SortKey> = HashMap::with_capacity(accounts.len());

    for (i, acc) in accounts.iter().enumerate() {
        if is_restricted(acc) {
            continue;
        }

        let cap_state = compute_capacity_state(acc, now);
        let flight = crate::observability::get_in_flight_generation(&acc.id);
        let flight_penalty = (flight as f64) * MAX_QUOTA_IN_FLIGHT_PENALTY;

        let (harvest_t, harvest) = smart_harvest_candidate(&cap_state, now_time);
        let harvest_valid = harvest && flight < (SMART_IN_FLIGHT_PER_TIER as i64);
        let harvest_b = if harvest_valid {
            reset_bucket(harvest_t.unwrap(), now_time)
        } else {
            0
        };

        let score = smart_headroom_tier(smart_effective_headroom(&cap_state)) * (SMART_IN_FLIGHT_PER_TIER as i32)
            - (flight as i32);

        let is_dep = cap_state.is_depleted;
        let pace = bucket(effective_worst_pace(&cap_state) - flight_penalty, PACE_EPSILON);
        let floor = bucket(cap_state.raw_floor - flight_penalty, PACE_EPSILON);
        let total_pace = bucket(cap_state.total_pace - flight_penalty, PACE_EPSILON);

        let k = SortKey {
            cap: cap_state,
            fresh: freshness_rank(acc, now),
            flight,
            hits: acc.hits(),
            harvest: harvest_valid,
            harvest_b,
            score,
            pace,
            floor,
            total_pace,
        };

        if acc.rate_limited_until.is_some_and(|r| r > now) {
            cooldown.push(i);
        } else if is_dep {
            depleted.push(i);
        } else {
            eligible.push(i);
        }

        keys.insert(i, k);
    }

    let mut healthy = Vec::new();
    let mut reserve = Vec::new();
    for &idx in &eligible {
        let k = &keys[&idx];
        if is_reserve_capacity(&k.cap) || over_weekly_pace(&accounts[idx], now) {
            reserve.push(idx);
        } else {
            healthy.push(idx);
        }
    }

    let less_max_quota = |a: &SortKey, b: &SortKey, id_a: &str, id_b: &str| -> bool {
        if a.cap.known_window_count != b.cap.known_window_count {
            return a.cap.known_window_count > b.cap.known_window_count;
        }
        if a.fresh != b.fresh {
            return a.fresh > b.fresh;
        }
        if a.pace > b.pace {
            return true;
        } else if a.pace < b.pace {
            return false;
        }
        if a.floor > b.floor {
            return true;
        } else if a.floor < b.floor {
            return false;
        }
        if a.total_pace > b.total_pace {
            return true;
        } else if a.total_pace < b.total_pace {
            return false;
        }
        if a.flight != b.flight {
            return a.flight < b.flight;
        }
        if a.hits != b.hits {
            return a.hits < b.hits;
        }
        id_a < id_b
    };

    let less_smart = |a: &SortKey, b: &SortKey, id_a: &str, id_b: &str| -> bool {
        let known_a = a.cap.known_window_count > 0;
        let known_b = b.cap.known_window_count > 0;
        if known_a != known_b {
            return known_a;
        }

        // 1. Freshness rank (prefer freshly probed accounts when unknown)
        if a.fresh != b.fresh {
            return a.fresh > b.fresh;
        }

        // 2. Harvest Mode: Prioritize accounts with surplus headroom
        // that are about to reset (5H <= 90m, WK <= 36h) to capture expiring quota.
        if a.harvest != b.harvest {
            return a.harvest;
        }
        if a.harvest && b.harvest {
            if a.harvest_b != b.harvest_b {
                return a.harvest_b < b.harvest_b;
            }
            if a.score != b.score {
                return a.score > b.score;
            }
            if a.flight != b.flight {
                return a.flight < b.flight;
            }
            if a.hits != b.hits {
                return a.hits < b.hits;
            }
            return id_a < id_b;
        }

        // 3. Cruising Mode: Headroom Health Tier (Discrete 20% bands), lowered by in-flight load
        if a.score != b.score {
            return a.score > b.score;
        }

        if a.flight != b.flight {
            return a.flight < b.flight;
        }

        // 4. Renewal Timing: within the same health tier and in-flight load, prefer sooner reset.
        let cmp = compare_reset_timing(&a.cap, &b.cap, now_time);
        if cmp != 0 {
            return cmp < 0;
        }

        // 5. Hits tie-breaker:
        if a.hits != b.hits {
            return a.hits < b.hits;
        }

        id_a < id_b
    };

    let less_depleted = |a: &SortKey, b: &SortKey, _id_a: &str, _id_b: &str| -> bool {
        if a.flight != b.flight {
            return a.flight < b.flight;
        }
        if a.cap.raw_floor > b.cap.raw_floor {
            return true;
        } else if a.cap.raw_floor < b.cap.raw_floor {
            return false;
        }
        a.hits < b.hits
    };

    type LessFn<'a> = &'a dyn Fn(&SortKey, &SortKey, &str, &str) -> bool;
    let less: LessFn<'_> = if strategy == crate::config::STRATEGY_MAX_QUOTA {
        &less_max_quota
    } else {
        &less_smart
    };

    let do_sort = |list: &mut [usize], less_fn: &dyn Fn(&SortKey, &SortKey, &str, &str) -> bool| {
        list.sort_by(|&i, &j| {
            let a = &keys[&i];
            let b = &keys[&j];
            let id_a = &accounts[i].id;
            let id_b = &accounts[j].id;
            if less_fn(a, b, id_a, id_b) {
                std::cmp::Ordering::Less
            } else if less_fn(b, a, id_b, id_a) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
    };

    do_sort(&mut healthy, less);
    do_sort(&mut reserve, less);
    do_sort(&mut depleted, &less_depleted);

    let mut result = Vec::with_capacity(healthy.len() + reserve.len() + depleted.len() + cooldown.len());
    result.extend(healthy);
    result.extend(reserve);
    result.extend(depleted);
    result.extend(cooldown);
    result
}

/// GetSchedulerSnapshot reads an authoritative point-in-time snapshot from state.db.
pub fn get_scheduler_snapshot() -> Result<Pool, StorageError> {
    crate::storage::load_pool()
}

/// TopTierCount computes how many leading candidates share the top health tier.
pub fn top_tier_count(candidates: &[Account], now: f64) -> usize {
    if candidates.is_empty() {
        return 0;
    }
    let now_time = if now >= 0.0 {
        UNIX_EPOCH + Duration::from_secs(now as u64)
    } else {
        UNIX_EPOCH
    };

    let score = |acc: &Account| -> Option<i32> {
        if is_restricted(acc) || acc.rate_limited_until.is_some_and(|r| r > now) || over_weekly_pace(acc, now) {
            return None;
        }
        let c = compute_capacity_state(acc, now);
        if c.is_depleted || is_reserve_capacity(&c) {
            return None;
        }
        let in_flight = crate::observability::get_in_flight_generation(&acc.id);
        Some(smart_headroom_tier(smart_effective_headroom(&c)) * (SMART_IN_FLIGHT_PER_TIER as i32) - in_flight as i32)
    };

    let Some(top) = score(&candidates[0]) else {
        return 0;
    };

    let cap0 = compute_capacity_state(&candidates[0], now);
    let (_, harvest) = smart_harvest_candidate(&cap0, now_time);
    if harvest {
        return 1;
    }

    let mut top_count = 1;
    for acc in &candidates[1..] {
        match score(acc) {
            Some(s) if s == top => top_count += 1,
            _ => break,
        }
    }
    top_count
}

/// RotateTopTier takes ordered candidates and rotates accounts within the top health tier by cursor.
/// It is applied to new sessions to spread them round-robin across equally healthy accounts. The group
/// uses smart's cruising key (health tier lowered by in-flight load) and stops at the first account smart
/// would not treat as a healthy peer: cooling down, depleted, reserve, or over weekly pace. Nothing is
/// rotated while the first account is harvesting an expiring 5H window, so that quota still gets used.
pub fn rotate_top_tier(candidates: &[Account], cursor: u64, now: f64) -> Vec<Account> {
    if candidates.len() <= 1 {
        return candidates.to_vec();
    }
    let top_count = top_tier_count(candidates, now);
    if top_count <= 1 {
        return candidates.to_vec();
    }

    let shift = (cursor % (top_count as u64)) as usize;
    if shift == 0 {
        return candidates.to_vec();
    }

    let mut rotated = Vec::with_capacity(candidates.len());
    rotated.extend_from_slice(&candidates[shift..top_count]);
    rotated.extend_from_slice(&candidates[..shift]);
    rotated.extend_from_slice(&candidates[top_count..]);
    rotated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Pool, QuotaState, QuotaWindow};

    /// Ordering reads process-wide in-flight counts keyed by account id; proxy tests hold slots for
    /// the same ids (acc_1, acc_2, ...) under TEST_LOCK, so ordering tests must hold it too.
    fn lock_in_flight() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::observability::reset_in_flight();
        guard
    }

    fn make_account_with_windows(id: &str, q5: Option<f64>, qw: Option<f64>, now: f64) -> Account {
        let mut lq = QuotaState {
            updated_at: Some((now - 10.0) as i64),
            ..Default::default()
        };
        if let Some(f5) = q5 {
            lq.gemini_5h = Some(QuotaWindow {
                fraction: Some(f5),
                reset_time: Some(serde_json::Value::String("2026-09-17T20:00:00Z".to_string())),
                ..Default::default()
            });
        }
        if let Some(fw) = qw {
            lq.gemini_weekly = Some(QuotaWindow {
                fraction: Some(fw),
                reset_time: Some(serde_json::Value::String("2026-09-24T00:00:00Z".to_string())),
                ..Default::default()
            });
        }
        Account {
            id: id.to_string(),
            status: "active".to_string(),
            last_quota: Some(lq),
            ..Default::default()
        }
    }

    #[derive(serde::Deserialize)]
    struct FixtureOrderingCase {
        test_id: String,
        strategy: String,
        now: f64,
        pool: Pool,
        candidates: Vec<Account>,
        expected_ordered_ids: Vec<String>,
    }

    #[test]
    fn scheduler_ordering_golden_cases() {
        let _l = lock_in_flight();
        let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/scheduler/ordering_cases.json");
        let data = std::fs::read_to_string(fixture_path).expect("read ordering_cases.json");
        let cases: Vec<FixtureOrderingCase> = serde_json::from_str(&data).expect("parse ordering_cases.json");

        for tc in cases {
            let ordered_indices = order_candidates(&tc.candidates, &tc.strategy, &tc.pool, tc.now);
            let actual_ids: Vec<String> = ordered_indices.iter().map(|&i| tc.candidates[i].id.clone()).collect();
            assert_eq!(
                actual_ids, tc.expected_ordered_ids,
                "ordering mismatch in golden test case {}",
                tc.test_id
            );
        }
    }

    #[test]
    fn scheduler_snapshot_consistent() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        crate::config::configure_state_dir(tmp.path()).unwrap();

        let now = 1789560000.0;
        let mut p = Pool {
            version: 1,
            strategy: "max_quota".to_string(),
            ..Default::default()
        };
        p.accounts.push(Account {
            id: "acc_1".to_string(),
            email: "snap@example.com".to_string(),
            request_count: 15,
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.75),
                updated_at: Some((now - 30.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        });
        crate::storage::save_pool(&p).unwrap();

        let snap = get_scheduler_snapshot().unwrap();
        assert_eq!(snap.accounts.len(), 1);
        let acc = &snap.accounts[0];
        assert_eq!(acc.id, "acc_1");
        assert_eq!(acc.request_count, 15);
        assert_eq!(acc.last_quota.as_ref().and_then(|q| q.remaining_fraction), Some(0.75));

        crate::config::reset_data_dir();
    }

    #[test]
    fn max_quota_reserve_headroom_healthy_beats_reserve() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        let main_acc = make_account_with_windows("Main", Some(0.10), Some(0.50), now);
        let backup_acc = make_account_with_windows("Backup", Some(0.80), Some(0.50), now);

        let candidates = vec![main_acc, backup_acc];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(ordered.len(), 2);
        assert_eq!(candidates[ordered[0]].id, "Backup");
        assert_eq!(candidates[ordered[1]].id, "Main");
    }

    #[test]
    fn max_quota_reserve_headroom_pace_ranking_preserved_among_healthy() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        let acc1 = make_account_with_windows("acc_1", Some(0.80), Some(0.90), now);
        let acc2 = make_account_with_windows("acc_2", Some(0.70), Some(0.85), now);

        let candidates = vec![acc2, acc1];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(ordered.len(), 2);
        assert_eq!(candidates[ordered[0]].id, "acc_1");
        assert_eq!(candidates[ordered[1]].id, "acc_2");
    }

    #[test]
    fn max_quota_reserve_headroom_reserve_fallback_works() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        let r1 = make_account_with_windows("res_1", Some(0.12), Some(0.80), now);
        let r2 = make_account_with_windows("res_2", Some(0.08), Some(0.80), now);

        let candidates = vec![r2, r1];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(ordered.len(), 2);
        assert_eq!(candidates[ordered[0]].id, "res_1");
        assert_eq!(candidates[ordered[1]].id, "res_2");
    }

    #[test]
    fn max_quota_reserve_headroom_depleted_never_wins_over_reserve() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        let dep_acc = make_account_with_windows("acc_depleted", Some(0.004), Some(0.50), now);
        let res_acc = make_account_with_windows("acc_reserve", Some(0.10), Some(0.50), now);

        let candidates = vec![dep_acc, res_acc];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(ordered.len(), 2);
        assert_eq!(candidates[ordered[0]].id, "acc_reserve");
        assert_eq!(candidates[ordered[1]].id, "acc_depleted");
    }

    #[test]
    fn max_quota_reserve_headroom_weekly_protection() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        let acc_weekly_low = make_account_with_windows("weekly_low", Some(0.80), Some(0.08), now);
        let acc_healthy = make_account_with_windows("healthy", Some(0.60), Some(0.50), now);

        assert!(is_reserve_candidate(&acc_weekly_low, now));
        assert!(!is_reserve_candidate(&acc_healthy, now));

        let candidates = vec![acc_weekly_low, acc_healthy];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "healthy");
        assert_eq!(candidates[ordered[1]].id, "weekly_low");
    }

    #[test]
    fn max_quota_dynamic_weekly_reserve_floor_near_reset() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        let near_reset_time = now + 22.0 * 3600.0;

        let make_near_reset_acc = |id: &str, q5: f64, qw: f64| -> Account {
            let lq = QuotaState {
                updated_at: Some((now - 10.0) as i64),
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(q5),
                    reset_time: Some(serde_json::Value::String("2026-09-16T17:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(qw),
                    reset_time: Some(serde_json::Value::Number(
                        serde_json::Number::from_f64(near_reset_time).unwrap(),
                    )),
                    ..Default::default()
                }),
                ..Default::default()
            };
            Account {
                id: id.to_string(),
                status: "active".to_string(),
                last_quota: Some(lq),
                ..Default::default()
            }
        };

        let backup_acc = make_near_reset_acc("backup", 1.0, 0.09);
        assert!(!is_reserve_candidate(&backup_acc, now));

        let critical_acc = make_near_reset_acc("critical", 1.0, 0.01);
        assert!(is_reserve_candidate(&critical_acc, now));

        let other_healthy = make_near_reset_acc("work", 0.80, 0.20);
        let candidates = vec![backup_acc, other_healthy];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(ordered.len(), 2);
        for &idx in &ordered {
            assert!(!is_reserve_candidate(&candidates[idx], now));
        }
    }

    #[test]
    fn max_quota_reserve_headroom_five_hour_protection() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        let acc_5h_low = make_account_with_windows("5h_low", Some(0.12), Some(0.70), now);
        let acc_healthy = make_account_with_windows("healthy", Some(0.60), Some(0.70), now);

        assert!(is_reserve_candidate(&acc_5h_low, now));
        assert!(!is_reserve_candidate(&acc_healthy, now));

        let candidates = vec![acc_5h_low, acc_healthy];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "healthy");
        assert_eq!(candidates[ordered[1]].id, "5h_low");
    }

    #[test]
    fn max_quota_reserve_headroom_boundaries() {
        let now = 1789560000.0;

        let acc_exact_15 = make_account_with_windows("exact_15", Some(0.15), Some(0.70), now);
        assert!(is_reserve_candidate(&acc_exact_15, now));

        let acc_above_15 = make_account_with_windows("above_15", Some(0.150001), Some(0.70), now);
        assert!(!is_reserve_candidate(&acc_above_15, now));

        let acc_exact_10 = make_account_with_windows("exact_10", Some(0.70), Some(0.10), now);
        assert!(is_reserve_candidate(&acc_exact_10, now));

        let acc_above_10 = make_account_with_windows("above_10", Some(0.70), Some(0.100001), now);
        assert!(!is_reserve_candidate(&acc_above_10, now));
    }

    #[test]
    fn max_quota_reserve_headroom_unknown_missing_window() {
        let now = 1789560000.0;

        let acc_only_5h = make_account_with_windows("only_5h", Some(0.80), None, now);
        assert!(!is_reserve_candidate(&acc_only_5h, now));

        let acc_only_weekly = make_account_with_windows("only_weekly", None, Some(0.80), now);
        assert!(!is_reserve_candidate(&acc_only_weekly, now));

        let acc_no_windows = Account {
            id: "no_windows".to_string(),
            status: "active".to_string(),
            ..Default::default()
        };
        assert!(!is_reserve_candidate(&acc_no_windows, now));
    }

    #[test]
    fn restricted_accounts_excluded_from_all_strategies() {
        let _l = lock_in_flight();
        let now = 1000.0;
        let ready = Account {
            id: "ready".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.9),
                ..Default::default()
            }),
            ..Default::default()
        };
        let reserve = Account {
            id: "reserve".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let depleted = Account {
            id: "depleted".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cooldown = Account {
            id: "cooldown".to_string(),
            rate_limited_until: Some(now + 100.0),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.9),
                ..Default::default()
            }),
            ..Default::default()
        };
        let validation = Account {
            id: "validation".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(1.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let auth_error = Account {
            id: "auth".to_string(),
            status: crate::storage::STATUS_AUTH_ERROR.to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(1.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let disabled = Account {
            id: "disabled".to_string(),
            status: crate::storage::STATUS_DISABLED.to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(1.0),
                ..Default::default()
            }),
            ..Default::default()
        };

        let all = vec![
            ready.clone(),
            reserve.clone(),
            depleted.clone(),
            cooldown.clone(),
            validation,
            auth_error,
            disabled,
        ];
        let eligible = vec![ready, reserve, depleted, cooldown];
        let pool = Pool::default();

        for strategy in [crate::config::STRATEGY_MAX_QUOTA, crate::config::STRATEGY_SMART] {
            let ordered_indices = order_candidates(&all, strategy, &pool, now);
            for &idx in &ordered_indices {
                assert!(!is_restricted(&all[idx]));
            }

            let want_indices = order_candidates(&eligible, strategy, &pool, now);
            assert_eq!(ordered_indices.len(), want_indices.len());
            for (oi, wi) in ordered_indices.iter().zip(want_indices.iter()) {
                assert_eq!(all[*oi].id, eligible[*wi].id);
            }
        }
    }

    #[test]
    fn reserve_headroom_healthy_preferred_over_reserve() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let mut main_acc = make_account_with_windows("Main", Some(0.10), Some(0.50), now);
        main_acc.request_count = 2;

        let mut backup_acc = make_account_with_windows("Backup", Some(0.80), Some(0.50), now);
        backup_acc.request_count = 20;

        let candidates = vec![main_acc, backup_acc];
        let ordered_mq = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates[ordered_mq[0]].id, "Backup");

        let ordered_smart = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered_smart[0]].id, "Backup");
    }

    #[test]
    fn max_quota_reserve_headroom_real_observed_snapshot() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let main_acc = make_account_with_windows("Main", Some(0.006), Some(0.492), now);
        let backup_acc = make_account_with_windows("Backup", Some(1.0), Some(0.671), now);
        let work_acc = make_account_with_windows("Work", Some(0.999), Some(0.671), now);

        assert!(is_reserve_candidate(&main_acc, now));
        assert!(!is_reserve_candidate(&backup_acc, now));
        assert!(!is_reserve_candidate(&work_acc, now));

        let candidates = vec![main_acc, backup_acc, work_acc];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(ordered.len(), 3);
        assert_ne!(candidates[ordered[0]].id, "Main");
        assert_ne!(candidates[ordered[1]].id, "Main");
        assert_eq!(candidates[ordered[2]].id, "Main");

        assert_eq!(candidates[ordered[0]].id, "Backup");
        assert_eq!(candidates[ordered[1]].id, "Work");
    }

    #[test]
    fn max_quota_reserve_headroom_legacy_fraction() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();
        for source in ["remaining_fraction", "quota"] {
            for &fraction in &[0.005, 0.006, 0.08, 0.10, 0.12, 0.15, 0.50] {
                let mut legacy = Account {
                    id: "legacy".to_string(),
                    last_quota: Some(QuotaState {
                        updated_at: Some((now - 10.0) as i64),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                if source == "remaining_fraction" {
                    legacy.last_quota.as_mut().unwrap().remaining_fraction = Some(fraction);
                } else {
                    legacy.quota = Some(fraction);
                }
                assert!(!is_reserve_candidate(&legacy, now));
                let capacity = compute_capacity_state(&legacy, now);
                let depleted = fraction <= crate::config::DEPLETED_THRESHOLD;
                assert_eq!(capacity.known_window_count, 1);
                assert_eq!(capacity.is_depleted, depleted);

                let reserve = make_account_with_windows("reserve", Some(0.12), Some(0.80), now);
                let candidates = vec![reserve, legacy];
                let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
                let want_first_id = if depleted { "reserve" } else { "legacy" };
                assert_eq!(candidates[ordered[0]].id, want_first_id);
            }
        }

        let five_hour = make_account_with_windows("5h", Some(0.12), None, now);
        let weekly = make_account_with_windows("weekly", None, Some(0.12), now);
        assert!(is_reserve_candidate(&five_hour, now));
        assert!(!is_reserve_candidate(&weekly, now));
    }

    #[test]
    fn max_quota_abundant_weekly_quota_no_cold_start_starvation() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let fresh_acc = Account {
            id: "fresh_acc_4".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.99),
                    reset_time: Some(serde_json::Value::String("2026-09-16T20:50:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.99),
                    reset_time: Some(serde_json::Value::String("2026-09-23T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mid_cycle_acc = Account {
            id: "mid_cycle_acc_2".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.97),
                    reset_time: Some(serde_json::Value::String("2026-09-16T20:45:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.63),
                    reset_time: Some(serde_json::Value::String("2026-09-20T11:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![mid_cycle_acc, fresh_acc];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(ordered.len(), 2);
        assert_eq!(candidates[ordered[0]].id, "fresh_acc_4");
    }

    #[test]
    fn smart_strategy_renewal_order() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_a = Account {
            id: "acc_soon".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-17T18:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_b = Account {
            id: "acc_later".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-21T18:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_b, acc_a];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_soon");
        assert_eq!(candidates[ordered[1]].id, "acc_later");
    }

    #[test]
    fn smart_strategy_hour_truncation() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc1 = Account {
            id: "acc_hits5".to_string(),
            status: "active".to_string(),
            request_count: 5,
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.70),
                    reset_time: Some(serde_json::Value::String("2026-09-18T10:15:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc_hits2".to_string(),
            status: "active".to_string(),
            request_count: 2,
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.70),
                    reset_time: Some(serde_json::Value::String("2026-09-18T10:45:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc1, acc2];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_hits2");
        assert_eq!(candidates[ordered[1]].id, "acc_hits5");
    }

    #[test]
    fn smart_strategy_hierarchical_windows() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc1 = Account {
            id: "acc_5h_later".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-17T15:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.70),
                    reset_time: Some(serde_json::Value::String("2026-09-18T10:10:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc_5h_sooner".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-17T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.70),
                    reset_time: Some(serde_json::Value::String("2026-09-18T10:50:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc1, acc2];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_5h_sooner");
        assert_eq!(candidates[ordered[1]].id, "acc_5h_later");
    }

    #[test]
    fn smart_strategy_five_hour_only_vs_weekly() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_5h_only = Account {
            id: "acc_5h_only".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-16T15:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_weekly = Account {
            id: "acc_weekly".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-19T10:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_weekly, acc_5h_only];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_5h_only");
        assert_eq!(candidates[ordered[1]].id, "acc_weekly");
    }

    #[test]
    fn smart_strategy_reserve_deprioritization() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_reserve = Account {
            id: "acc_reserve".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.10),
                    reset_time: Some(serde_json::Value::String("2026-09-16T13:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.50),
                    reset_time: Some(serde_json::Value::String("2026-09-16T13:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_healthy = Account {
            id: "acc_healthy".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-18T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_reserve, acc_healthy];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_healthy");
        assert_eq!(candidates[ordered[1]].id, "acc_reserve");
    }

    #[test]
    fn smart_strategy_cooldown_sink() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_healthy = Account {
            id: "healthy".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-18T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_depleted = Account {
            id: "depleted".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.001),
                    reset_time: Some(serde_json::Value::String("2026-09-17T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_cooldown = Account {
            id: "cooldown".to_string(),
            status: "active".to_string(),
            rate_limited_until: Some(now + 120.0),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.95),
                    reset_time: Some(serde_json::Value::String("2026-09-16T13:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_cooldown, acc_depleted, acc_healthy];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(ordered.len(), 3);
        assert_eq!(candidates[ordered[0]].id, "healthy");
        assert_eq!(candidates[ordered[1]].id, "depleted");
        assert_eq!(candidates[ordered[2]].id, "cooldown");
    }

    #[test]
    fn smart_strategy_user_observed_single_account_drain_prevented() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let backup = Account {
            id: "Backup".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.42),
                    reset_time: Some(serde_json::Value::String("2026-09-16T12:34:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.23),
                    reset_time: Some(serde_json::Value::String("2026-09-18T13:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let main_acc = Account {
            id: "Main".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.97),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:54:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.99),
                    reset_time: Some(serde_json::Value::String("2026-09-23T11:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![backup, main_acc];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "Main");
    }

    #[test]
    fn smart_strategy_expiring_quota_harvesting() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_expiring = Account {
            id: "acc_expiring".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-16T12:25:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_cruising = Account {
            id: "acc_cruising".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.95),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:30:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.95),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_cruising, acc_expiring];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_expiring");
    }

    #[test]
    fn smart_strategy_low_quota_account_protected_from_harvest() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_low = Account {
            id: "acc_low".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.20),
                    reset_time: Some(serde_json::Value::String("2026-09-16T13:20:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_healthy = Account {
            id: "acc_healthy".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:30:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_low, acc_healthy];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_healthy");
    }

    #[test]
    fn smart_strategy_harvest_surplus_above_25_percent() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_harvest = Account {
            id: "acc_harvest_27pct".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.27),
                    reset_time: Some(serde_json::Value::String("2026-09-16T12:25:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_cruising = Account {
            id: "acc_cruising".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:30:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_cruising, acc_harvest];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_harvest_27pct");
    }

    #[test]
    fn smart_strategy_balanced_cruising_load_distribution() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc1 = Account {
            id: "acc1".to_string(),
            status: "active".to_string(),
            request_count: 100,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.97),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:50:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.99),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc2".to_string(),
            status: "active".to_string(),
            request_count: 10,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.96),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:50:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.99),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc1, acc2];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc2");
    }

    #[test]
    fn smart_strategy_strict_weak_ordering_transitivity() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let accs = vec![
            Account {
                id: "acc_top_10hits".to_string(),
                request_count: 10,
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.95),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.95),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_top_50hits".to_string(),
                request_count: 50,
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.90),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.95),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_mid_5hits".to_string(),
                request_count: 5,
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.65),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.70),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_mid_20hits".to_string(),
                request_count: 20,
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.70),
                        reset_time: Some(serde_json::Value::String("2026-09-16T15:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.70),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_low_1hit".to_string(),
                request_count: 1,
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.35),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.35),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_harvest_30m".to_string(),
                request_count: 100,
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.85),
                        reset_time: Some(serde_json::Value::String("2026-09-16T12:30:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.90),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];

        let ordered = order_candidates(&accs, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(ordered.len(), accs.len());
        assert_eq!(accs[ordered[0]].id, "acc_harvest_30m");

        let mut indices = HashMap::new();
        for (i, &idx) in ordered.iter().enumerate() {
            indices.insert(accs[idx].id.clone(), i);
        }

        assert!(indices["acc_top_10hits"] < indices["acc_top_50hits"]);
        assert!(indices["acc_top_50hits"] < indices["acc_mid_5hits"]);
        assert!(indices["acc_mid_20hits"] < indices["acc_low_1hit"]);
    }

    #[test]
    fn in_flight_concurrency_max_quota_and_smart() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::observability::reset_in_flight();

        let now = 1700000000.0;
        let pool = Pool::default();

        let acc_busy = Account {
            id: "acc_busy".to_string(),
            name: "BusyAccount".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(1.0),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(1.0),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_idle = Account {
            id: "acc_idle".to_string(),
            name: "IdleAccount".to_string(),
            request_count: 10,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.7),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.7),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_idle.clone(), acc_busy.clone()];
        let ordered_mq = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates[ordered_mq[0]].id, "acc_busy");

        let ordered_smart = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered_smart[0]].id, "acc_busy");

        let done = crate::observability::start_in_flight_generation("acc_busy");

        let candidates2 = vec![acc_busy.clone(), acc_idle];
        let ordered_mq_if = order_candidates(&candidates2, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates2[ordered_mq_if[0]].id, "acc_busy");

        let ordered_smart_if = order_candidates(&candidates2, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates2[ordered_smart_if[0]].id, "acc_busy");

        let acc_same_tier_idle = Account {
            id: "acc_same_tier_idle".to_string(),
            name: "SameTierIdleAccount".to_string(),
            request_count: 5,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(1.0),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(1.0),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates3 = vec![acc_busy, acc_same_tier_idle];
        let ordered_smart_same = order_candidates(&candidates3, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates3[ordered_smart_same[0]].id, "acc_same_tier_idle");

        let ordered_mq_same = order_candidates(&candidates3, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates3[ordered_mq_same[0]].id, "acc_same_tier_idle");

        drop(done);
        crate::observability::reset_in_flight();
    }

    #[test]
    fn smart_harvest_candidate_decaying_floor() {
        let now_time = UNIX_EPOCH + Duration::from_secs(1789560000);
        let weekly_far = now_time + Duration::from_secs(5 * 24 * 3600);

        struct Case {
            name: &'static str,
            q5: f64,
            q7: f64,
            reset5: Option<SystemTime>,
            want: bool,
        }

        let cases = vec![
            Case {
                name: "5H leftover right before reset is harvested",
                q5: 0.15,
                q7: 0.90,
                reset5: Some(now_time + Duration::from_secs(10 * 60)),
                want: true,
            },
            Case {
                name: "5H leftover too early in window is kept",
                q5: 0.15,
                q7: 0.90,
                reset5: Some(now_time + Duration::from_secs(80 * 60)),
                want: false,
            },
            Case {
                name: "5H harvest never drains a low weekly window",
                q5: 0.90,
                q7: 0.15,
                reset5: Some(now_time + Duration::from_secs(10 * 60)),
                want: false,
            },
            Case {
                name: "5H harvest blocked when weekly headroom below 30%",
                q5: 0.90,
                q7: 0.25,
                reset5: Some(now_time + Duration::from_secs(10 * 60)),
                want: false,
            },
            Case {
                name: "5H reset outside harvest window",
                q5: 0.90,
                q7: 0.90,
                reset5: Some(now_time + Duration::from_secs(3 * 3600)),
                want: false,
            },
        ];

        for tc in cases {
            let cap_state = CapacityState {
                q5: Some(tc.q5),
                q7: Some(tc.q7),
                r5: 0.0,
                r7: 0.0,
                pace5: None,
                pace7: None,
                worst_pace: 0.0,
                total_pace: 0.0,
                raw_floor: tc.q5.min(tc.q7),
                q5_known: true,
                q7_known: true,
                known_window_count: 2,
                legacy_fraction_known: false,
                is_depleted: false,
                reset5_time: tc.reset5,
                reset7_time: Some(weekly_far),
            };
            let (_, got) = smart_harvest_candidate(&cap_state, now_time);
            assert_eq!(got, tc.want, "{}: harvest candidate mismatch", tc.name);
        }
    }

    #[test]
    fn smart_harvest_candidate_blocked_over_weekly_pace() {
        let now_time = UNIX_EPOCH + Duration::from_secs(1789560000);
        let reset5_soon = now_time + Duration::from_secs(10 * 60);
        let reset7_far = now_time + Duration::from_secs(6 * 24 * 3600);

        let q5 = 0.90;
        let q7 = 0.45;
        let p7 = q7 - (6.0 * 24.0 * 3600.0) / (7.0 * 24.0 * 3600.0);
        let cap_state = CapacityState {
            q5: Some(q5),
            q7: Some(q7),
            r5: 0.0,
            r7: 0.0,
            pace5: None,
            pace7: Some(p7),
            worst_pace: 0.0,
            total_pace: 0.0,
            raw_floor: 0.45,
            q5_known: true,
            q7_known: true,
            known_window_count: 2,
            legacy_fraction_known: false,
            is_depleted: false,
            reset5_time: Some(reset5_soon),
            reset7_time: Some(reset7_far),
        };

        let (_, harvest) = smart_harvest_candidate(&cap_state, now_time);
        assert!(!harvest);
    }

    #[test]
    fn smart_strategy_over_weekly_pace_held_back() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_fast = Account {
            id: "acc_fast".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(1.0),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.45),
                    reset_time: Some(serde_json::Value::String("2026-09-23T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_on_pace = Account {
            id: "acc_on_pace".to_string(),
            status: "active".to_string(),
            request_count: 50,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.5),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.5),
                    reset_time: Some(serde_json::Value::String("2026-09-19T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        for strategy in [crate::config::STRATEGY_SMART, crate::config::STRATEGY_MAX_QUOTA] {
            let candidates = vec![acc_fast.clone(), acc_on_pace.clone()];
            let ordered = order_candidates(&candidates, strategy, &pool, now);
            assert_eq!(candidates[ordered[0]].id, "acc_on_pace");
        }
        assert!(!is_reserve_candidate(&acc_fast, now));
    }

    #[test]
    fn order_candidates_in_flight_tier_precedence() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::observability::reset_in_flight();

        let now = 1700000000.0;
        let pool = Pool::default();

        // (a) busy high-tier outranks idle low-tier
        {
            let acc_high_busy = Account {
                id: "acc_high_busy".to_string(),
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.90),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.90),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let acc_low_idle = Account {
                id: "acc_low_idle".to_string(),
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.70),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.70),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let done = crate::observability::start_in_flight_generation("acc_high_busy");

            let candidates = vec![acc_low_idle.clone(), acc_high_busy.clone()];
            let ordered_smart = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
            assert_eq!(candidates[ordered_smart[0]].id, "acc_high_busy");

            let ordered_mq = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
            assert_eq!(candidates[ordered_mq[0]].id, "acc_high_busy");

            drop(done);
            crate::observability::reset_in_flight();
        }

        // (b) idle same-tier outranks busy same-tier cruising
        {
            let acc_same_busy = Account {
                id: "acc_same_busy".to_string(),
                status: "active".to_string(),
                request_count: 0,
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.85),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.85),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let acc_same_idle = Account {
                id: "acc_same_idle".to_string(),
                status: "active".to_string(),
                request_count: 20,
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.85),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.85),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let done = crate::observability::start_in_flight_generation("acc_same_busy");

            let candidates = vec![acc_same_busy.clone(), acc_same_idle.clone()];
            let ordered_smart = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
            assert_eq!(candidates[ordered_smart[0]].id, "acc_same_idle");

            let ordered_mq = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
            assert_eq!(candidates[ordered_mq[0]].id, "acc_same_idle");

            drop(done);
            crate::observability::reset_in_flight();
        }

        // (c) idle same-tier outranks busy same-tier harvest
        {
            let now_time = UNIX_EPOCH + Duration::from_secs(1789560000);
            let now_f = 1789560000.0;
            let reset5 = "2026-09-16T12:30:00Z";
            let reset7 = "2026-09-21T12:00:00Z";

            let acc_harvest_busy = Account {
                id: "acc_harvest_busy".to_string(),
                status: "active".to_string(),
                request_count: 0,
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.50),
                        reset_time: Some(serde_json::Value::String(reset5.to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.50),
                        reset_time: Some(serde_json::Value::String(reset7.to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now_f - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let acc_harvest_idle = Account {
                id: "acc_harvest_idle".to_string(),
                status: "active".to_string(),
                request_count: 20,
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.50),
                        reset_time: Some(serde_json::Value::String(reset5.to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.50),
                        reset_time: Some(serde_json::Value::String(reset7.to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now_f - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let _ = now_time;
            let done = crate::observability::start_in_flight_generation("acc_harvest_busy");

            let candidates = vec![acc_harvest_busy, acc_harvest_idle];
            let ordered_smart = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now_f);
            assert_eq!(candidates[ordered_smart[0]].id, "acc_harvest_idle");

            drop(done);
            crate::observability::reset_in_flight();
        }
    }

    #[test]
    fn order_candidates_smart_unknown_quota_sorts_after_known_quota() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_known = Account {
            id: "acc_known".to_string(),
            status: "active".to_string(),
            request_count: 100,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.75),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.75),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 50.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let acc_unknown_fresh = Account {
            id: "acc_unknown_fresh".to_string(),
            status: "active".to_string(),
            request_count: 0,
            last_quota: Some(QuotaState {
                updated_at: Some((now - 1.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let acc_unknown_nil = Account {
            id: "acc_unknown_nil".to_string(),
            status: "active".to_string(),
            request_count: 0,
            ..Default::default()
        };

        let candidates1 = vec![acc_unknown_fresh, acc_known.clone()];
        let ordered1 = order_candidates(&candidates1, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates1[ordered1[0]].id, "acc_known");

        let candidates2 = vec![acc_unknown_nil, acc_known];
        let ordered2 = order_candidates(&candidates2, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates2[ordered2[0]].id, "acc_known");
    }

    #[test]
    fn order_candidates_smart_in_flight_spills_to_next_tier() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::observability::reset_in_flight();

        let now = 1700000000.0;
        let pool = Pool::default();
        let make_acc = |id: &str, frac: f64| -> Account {
            Account {
                id: id.to_string(),
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(frac),
                        reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(frac),
                        reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some((now - 10.0) as i64),
                    ..Default::default()
                }),
                ..Default::default()
            }
        };

        let top = make_acc("acc_top", 0.85);
        let next = make_acc("acc_next", 0.70);

        let mut guards = Vec::new();
        for i in 1..=SMART_IN_FLIGHT_PER_TIER {
            guards.push(crate::observability::start_in_flight_generation("acc_top"));
            let want = if i == SMART_IN_FLIGHT_PER_TIER {
                "acc_next"
            } else {
                "acc_top"
            };
            let candidates = vec![next.clone(), top.clone()];
            let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
            assert_eq!(candidates[ordered[0]].id, want);
        }

        drop(guards);
        crate::observability::reset_in_flight();
    }

    #[test]
    fn smart_strategy_mid_week_5h_not_stranded_by_weekly_tier() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_mid_week = Account {
            id: "acc_mid_week_full_5h".to_string(),
            status: "active".to_string(),
            request_count: 10,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.95),
                    reset_time: Some(serde_json::Value::String("2026-09-16T14:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.65),
                    reset_time: Some(serde_json::Value::String("2026-09-20T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_fresh_week = Account {
            id: "acc_fresh_week_later_5h".to_string(),
            status: "active".to_string(),
            request_count: 10,
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.85),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:30:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.85),
                    reset_time: Some(serde_json::Value::String("2026-09-23T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_fresh_week, acc_mid_week];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_mid_week_full_5h");
    }

    #[test]
    fn rotate_top_tier_round_robin_cycle() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let frac90 = 0.90;
        let frac50 = 0.50;

        let acc1 = Account {
            id: "acc_1".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac90),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac90),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc_2".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac90),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac90),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc3 = Account {
            id: "acc_3".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac90),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac90),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc4 = Account {
            id: "acc_4".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac50),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac50),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc1, acc2, acc3, acc4];

        let rot0 = rotate_top_tier(&candidates, 0, now);
        assert_eq!(
            [
                rot0[0].id.as_str(),
                rot0[1].id.as_str(),
                rot0[2].id.as_str(),
                rot0[3].id.as_str()
            ],
            ["acc_1", "acc_2", "acc_3", "acc_4"]
        );

        let rot1 = rotate_top_tier(&candidates, 1, now);
        assert_eq!(
            [
                rot1[0].id.as_str(),
                rot1[1].id.as_str(),
                rot1[2].id.as_str(),
                rot1[3].id.as_str()
            ],
            ["acc_2", "acc_3", "acc_1", "acc_4"]
        );

        let rot2 = rotate_top_tier(&candidates, 2, now);
        assert_eq!(
            [
                rot2[0].id.as_str(),
                rot2[1].id.as_str(),
                rot2[2].id.as_str(),
                rot2[3].id.as_str()
            ],
            ["acc_3", "acc_1", "acc_2", "acc_4"]
        );

        let rot3 = rotate_top_tier(&candidates, 3, now);
        assert_eq!(
            [
                rot3[0].id.as_str(),
                rot3[1].id.as_str(),
                rot3[2].id.as_str(),
                rot3[3].id.as_str()
            ],
            ["acc_1", "acc_2", "acc_3", "acc_4"]
        );
    }

    #[test]
    fn smart_strategy_harvest_15_min_bucket_avoids_clock_seam() {
        let _l = lock_in_flight();
        let now = 1789560120.0;
        let pool = Pool::default();

        let acc_imminent = Account {
            id: "acc_imminent_3m".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.45),
                    reset_time: Some(serde_json::Value::String("2026-09-16T12:05:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_later_harvest = Account {
            id: "acc_later_56m".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.85),
                    reset_time: Some(serde_json::Value::String("2026-09-16T12:58:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.90),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_later_harvest, acc_imminent];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_SMART, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_imminent_3m");
    }

    #[test]
    fn max_quota_smooth_weekly_transition_no_cliff_at_80_pct() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_79_weekly = Account {
            id: "acc_q7_79_q5_100".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(1.00),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.79),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_80_weekly_low_5h = Account {
            id: "acc_q7_80_q5_60".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.60),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_80_weekly_low_5h, acc_79_weekly];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_q7_79_q5_100");
    }

    #[test]
    fn max_quota_in_flight_spills_across_close_pace_buckets() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::observability::reset_in_flight();

        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_95 = Account {
            id: "acc_95".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.95),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.95),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_93 = Account {
            id: "acc_93".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.93),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.93),
                    reset_time: Some(serde_json::Value::String("2026-09-23T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_93.clone(), acc_95.clone()];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_95");

        let done = crate::observability::start_in_flight_generation("acc_95");

        let candidates2 = vec![acc_95, acc_93];
        let ordered2 = order_candidates(&candidates2, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates2[ordered2[0]].id, "acc_93");

        drop(done);
        crate::observability::reset_in_flight();
    }

    #[test]
    fn max_quota_total_pace_tie_breaker() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        let acc_lower_weekly = Account {
            id: "acc_wk_82".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.82),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc_higher_weekly = Account {
            id: "acc_wk_98".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.80),
                    reset_time: Some(serde_json::Value::String("2026-09-16T16:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.98),
                    reset_time: Some(serde_json::Value::String("2026-09-22T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        let candidates = vec![acc_lower_weekly, acc_higher_weekly];
        let ordered = order_candidates(&candidates, crate::config::STRATEGY_MAX_QUOTA, &pool, now);
        assert_eq!(candidates[ordered[0]].id, "acc_wk_98");
    }

    #[test]
    fn rotate_top_tier_skips_cooldown_harvest_and_busy_accounts() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let fresh = (now - 10.0) as i64;
        let healthy = |id: &str, reset5: &str| -> Account {
            Account {
                id: id.to_string(),
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.90),
                        reset_time: Some(serde_json::Value::String(reset5.to_string())),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.90),
                        reset_time: Some(serde_json::Value::String("2026-09-22T00:00:00Z".to_string())),
                        ..Default::default()
                    }),
                    updated_at: Some(fresh),
                    ..Default::default()
                }),
                ..Default::default()
            }
        };
        let ids = |accs: &[Account]| -> String { accs.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(",") };
        let later = "2026-09-16T16:00:00Z";

        // 1. An account on 429 cooldown is never rotated to the front.
        let mut cooling = healthy("cooling", later);
        cooling.rate_limited_until = Some(now + 3600.0);
        let cands = vec![healthy("a", later), healthy("b", later), cooling];
        for cursor in 0..6 {
            let rot = rotate_top_tier(&cands, cursor, now);
            assert_ne!(rot[0].id, "cooling");
        }

        // 2. A first account harvesting an expiring 5H window keeps the front.
        let harvest = healthy("harvest", "2026-09-16T12:30:00Z");
        let pool = Pool::default();
        let cands_in = vec![healthy("x", later), harvest];
        let ordered_indices = order_candidates(&cands_in, crate::config::STRATEGY_SMART, &pool, now);
        let cands_ordered: Vec<Account> = ordered_indices.iter().map(|&i| cands_in[i].clone()).collect();
        assert_eq!(cands_ordered[0].id, "harvest");
        for cursor in 0..4 {
            let rot = rotate_top_tier(&cands_ordered, cursor, now);
            assert_eq!(ids(&rot), "harvest,x");
        }

        // 3. An account busy with in-flight requests is not a peer of idle ones.
        crate::observability::reset_in_flight();
        let done = crate::observability::start_in_flight_generation("busy");
        let cands3 = vec![healthy("idle", later), healthy("busy", later)];
        let rot_busy = rotate_top_tier(&cands3, 1, now);
        assert_eq!(ids(&rot_busy), "idle,busy");

        drop(done);
        crate::observability::reset_in_flight();
    }

    #[test]
    fn ordering_ties_match_go() {
        let _l = lock_in_flight();
        let now = 1789560000.0;
        let pool = Pool::default();

        // Pool order puts the unknown-quota account first and a reserve account between the tied ones,
        // so only a stable sort with Go's exact less() reproduces Go's order.
        let unknown = Account {
            id: "unknown".to_string(),
            status: "active".to_string(),
            ..Default::default()
        };
        let candidates = vec![
            unknown,
            make_account_with_windows("low", Some(0.30), Some(0.60), now),
            make_account_with_windows("tie_a", Some(0.80), Some(0.80), now),
            make_account_with_windows("tie_b", Some(0.80), Some(0.80), now),
            make_account_with_windows("reserve", Some(0.10), Some(0.50), now),
            make_account_with_windows("tie_c", Some(0.80), Some(0.80), now),
            make_account_with_windows("high", Some(1.00), Some(0.95), now),
        ];

        // Expected orders printed by Go's OrderCandidates for the identical input (with &storage.Pool{}).
        let expected = vec!["high", "tie_a", "tie_b", "tie_c", "unknown", "low", "reserve"];
        for strategy in [crate::config::STRATEGY_SMART, crate::config::STRATEGY_MAX_QUOTA] {
            let ordered = order_candidates(&candidates, strategy, &pool, now);
            let ids: Vec<&str> = ordered.iter().map(|&i| candidates[i].id.as_str()).collect();
            assert_eq!(ids, expected, "strategy {strategy}");
        }
    }
}
