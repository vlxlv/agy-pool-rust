pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod accounts;
pub mod auth;
pub mod cli;
pub mod clock;
pub mod config;
pub mod conversation;
pub mod daemon;
pub mod diagnostics;
pub mod forecast;
pub mod http;
pub mod installer;
pub mod observability;
pub mod proxy;
pub mod quota;
pub mod scheduler;
pub mod storage;
pub mod upstream;

#[cfg(test)]
pub mod testutil;
