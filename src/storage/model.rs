use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const TELEMETRY_CAP: usize = 1000;

pub const STATUS_VALIDATION_REQUIRED: &str = "validation_required";
pub const STATUS_VERIFICATION_PENDING: &str = "verification_pending";
pub const STATUS_AUTH_ERROR: &str = "auth_error";
pub const STATUS_DISABLED: &str = "disabled";

pub struct Status;
impl Status {
    pub const VALIDATION_REQUIRED: &'static str = STATUS_VALIDATION_REQUIRED;
    pub const VERIFICATION_PENDING: &'static str = STATUS_VERIFICATION_PENDING;
    pub const AUTH_ERROR: &'static str = STATUS_AUTH_ERROR;
    pub const DISABLED: &'static str = STATUS_DISABLED;
}

pub(crate) fn is_none_or_null(opt: &Option<serde_json::Value>) -> bool {
    matches!(opt, None | Some(serde_json::Value::Null))
}

pub(crate) fn serialize_any(v: &Option<serde_json::Value>) -> Option<String> {
    match v {
        None | Some(serde_json::Value::Null) => None,
        Some(val) => serde_json::to_string(val).ok(),
    }
}

pub(crate) fn deserialize_any(s: Option<&str>) -> Option<serde_json::Value> {
    match s {
        None | Some("") => None,
        Some(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) => Some(v),
            Err(_) => Some(serde_json::Value::String(text.to_string())),
        },
    }
}

/// QuotaWindow represents a single quota window (e.g. 5-hour or weekly).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct QuotaWindow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "is_none_or_null")]
    pub reset_time: Option<serde_json::Value>,

    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// QuotaState represents the last recorded quota telemetry for an account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct QuotaState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_5h: Option<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_weekly: Option<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub third_party_5h: Option<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub third_party_weekly: Option<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "is_none_or_null")]
    pub reset_time: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,

    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Account represents a single user account record in the pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Account {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_expiry: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limited_until: Option<f64>,
    #[serde(default)]
    pub request_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gen_count: Option<i64>,
    #[serde(default)]
    pub error_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_quota: Option<QuotaState>,

    // Legacy fields for alpha compatibility
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_5h_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_weekly_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "is_none_or_null")]
    pub gemini_5h_reset: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "is_none_or_null")]
    pub gemini_weekly_reset: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_5h_reset_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini_weekly_reset_sec: Option<f64>,

    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Account {
    /// hits returns gen_count if present, falling back to request_count matching Python.
    pub fn hits(&self) -> i64 {
        self.gen_count.unwrap_or(self.request_count)
    }

    pub fn get_hits(&self) -> i64 {
        self.hits()
    }
}

/// GenerationEvent represents a timestamped inference event for quota telemetry and runway forecasting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationEvent {
    pub ts: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

fn default_pool_version() -> i64 {
    1
}

/// Pool represents the top-level agy-pool-accounts.json data structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pool {
    #[serde(default = "default_pool_version")]
    pub version: i64,
    #[serde(default)]
    pub strategy: String,
    #[serde(default)]
    pub active_account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_email: Option<bool>,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generation_events: Vec<GenerationEvent>,

    #[serde(default, skip_serializing)]
    pub round_robin_last_account_id: Option<serde_json::Value>,

    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Pool {
    /// RecordGenerationTelemetry appends a generation event and trims to a rolling 4-hour window capped at TelemetryCap.
    pub fn record_generation_telemetry(&mut self, account_id: &str, now: i64) {
        if now <= 0 {
            return;
        }
        self.generation_events.push(GenerationEvent {
            ts: now,
            id: account_id.to_string(),
        });
        let cutoff = now - 14400; // 4 hours
        let mut filtered: Vec<GenerationEvent> = self
            .generation_events
            .iter()
            .filter(|e| e.ts >= cutoff)
            .cloned()
            .collect();
        if filtered.len() > TELEMETRY_CAP {
            let start = filtered.len() - TELEMETRY_CAP;
            filtered = filtered[start..].to_vec();
        }
        self.generation_events = filtered;
    }

    /// NewEmptyPool returns a newly initialized empty pool matching Python _empty_pool().
    pub fn new_empty() -> Self {
        Self {
            version: 1,
            strategy: "smart".to_string(),
            active_account_id: None,
            show_email: None,
            accounts: Vec::new(),
            generation_events: Vec::new(),
            round_robin_last_account_id: None,
            extra: serde_json::Map::new(),
        }
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new_empty()
    }
}

// ============================================================================
// Target Go-Native Architecture State Split Models
// ============================================================================

/// TargetAccountAuthRecord defines durable identity/auth material in accounts.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetAccountAuthRecord {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_expiry: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

/// TargetAccountsFile represents the target accounts.json structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetAccountsFile {
    pub version: i64,
    pub accounts: Vec<TargetAccountAuthRecord>,
}

/// TargetAccountRuntimeRecord defines mutable non-cache runtime state in runtime.json.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetAccountRuntimeRecord {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limited_until: Option<f64>,
    pub request_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gen_count: Option<i64>,
    pub error_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

/// TargetRuntimeFile represents the target runtime.json structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetRuntimeFile {
    pub version: i64,
    pub active_account_id: Option<String>,
    pub accounts: HashMap<String, TargetAccountRuntimeRecord>,
}

/// TargetQuotaFile represents the target quota.json structure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetQuotaFile {
    pub version: i64,
    pub accounts: HashMap<String, QuotaState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

/// MigrationResult describes the result of migrating legacy Python state to Go-native state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MigrationResult {
    pub source_path: String,
    pub target_file: String,
    pub accounts_count: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active_account_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: String,
}
