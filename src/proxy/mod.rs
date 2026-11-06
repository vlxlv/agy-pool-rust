pub mod affinity;
pub mod confirmation;
pub mod failover;
pub mod model_cooldown;
pub mod smooth_rate;
pub mod usage;

pub use affinity::*;
pub use confirmation::*;
pub use failover::*;
pub use model_cooldown::*;
pub use smooth_rate::*;
pub use usage::*;

use hyper::http;

/// Attaches debug headers as Go's `attachDebugHeaders` in `proxy.go`.
pub fn attach_debug_headers(
    headers: &mut http::HeaderMap,
    account: Option<&crate::storage::Account>,
    strategy: &str,
    affinity_status: &str,
) {
    let Some(acc) = account else { return };
    let disp = crate::accounts::display_account_name(Some(acc), Some(false));
    if let Ok(val) = http::HeaderValue::from_str(&disp) {
        headers.insert("X-Agy-Pool-Account", val);
    }
    if let Ok(val) = http::HeaderValue::from_str(&acc.id) {
        headers.insert("X-Agy-Pool-Account-Id", val);
    }
    if !strategy.is_empty()
        && let Ok(val) = http::HeaderValue::from_str(strategy)
    {
        headers.insert("X-Agy-Pool-Strategy", val);
    }
    if !affinity_status.is_empty()
        && let Ok(val) = http::HeaderValue::from_str(affinity_status)
    {
        headers.insert("X-Agy-Pool-Affinity", val);
    }
}
