pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod auth;
pub mod clock;
pub mod config;
pub mod http;
pub mod observability;
pub mod storage;

#[cfg(test)]
pub mod testutil;
