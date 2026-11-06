use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingAuditEvent {
    #[serde(with = "crate::proxy::time_serde")]
    pub timestamp: SystemTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub strategy: String,
    pub account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub classification: String,

    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub five_hour_known: bool,
    pub five_hour_remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour_reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour_reset_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour_pace: Option<f64>,

    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub weekly_known: bool,
    pub weekly_remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_reset_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_pace: Option<f64>,

    pub known_window_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_rank: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worst_pace: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pace: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_floor: Option<f64>,

    pub healthy_count: usize,
    pub reserve_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub healthy_candidates: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reserve_candidates: Vec<String>,

    pub attempt: usize,
    pub status: u16,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub failover_reason: String,
    pub verdict: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub verdict_details: String,
}
