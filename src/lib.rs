pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod accounts;
pub mod auth;
pub mod clock;
pub mod config;
pub mod conversation;
pub mod forecast;
pub mod http;
pub mod observability;
pub mod quota;
pub mod scheduler;
pub mod storage;

#[cfg(test)]
pub mod testutil;
