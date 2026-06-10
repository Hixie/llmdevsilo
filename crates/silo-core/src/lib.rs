//! Shared contracts for the llmdevsilo harness.
//!
//! Every other crate in the workspace depends on this one and only
//! communicates with its peers through the types and traits defined here.
//! The harness crate wires concrete implementations of the [`traits`]
//! together; the implementation crates (`silo-llm`, `silo-sandbox`,
//! `silo-proxy`, `silo-frontend`, `silo-workspace`) provide them.

pub mod clock;
pub mod config;
pub mod conversation;
pub mod cost;
pub mod error;
pub mod event;
pub mod helper;
pub mod journal;
pub mod paths;
pub mod protocol;
pub mod replay;
pub mod risk;
pub mod sandbox;
pub mod secrets;
pub mod tool;
pub mod traits;

/// Protocol version spoken between the harness and interactive clients.
pub const PROTOCOL_VERSION: u32 = 1;

/// Generates a short random identifier (12 hex characters).
pub fn short_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    id[..12].to_string()
}
