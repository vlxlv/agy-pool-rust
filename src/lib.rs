/// Release builds inject the tag version (like Go's `-X config.Version=`); `update` compares against it.
pub const VERSION: &str = match option_env!("AGY_POOL_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

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
