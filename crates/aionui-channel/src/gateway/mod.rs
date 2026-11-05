//! Authenticated node transport to the independent Feishu gateway.
mod client;
mod config;
mod types;

pub use client::GatewayClient;
pub use config::GatewayConfig;
pub use types::*;
