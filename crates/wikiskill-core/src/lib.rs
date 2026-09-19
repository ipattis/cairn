//! `wikiskill-core` — the loop, gate, verifier runner, curator agents, model clients,
//! Jev client, vault and git layers described in the WikiSkill Harness rationale.
//!
//! Everything that is *our* logic lives here. The only external executor is OpenCode,
//! reached through the [`executor::Executor`] trait so a V2 server API (or pi, the
//! runner-up) can be swapped in without touching the loop.

pub mod agent;
pub mod config;
pub mod eval;
pub mod executor;
pub mod gate;
pub mod git;
pub mod iteration;
pub mod jev;
pub mod model;
pub mod redact;
pub mod sandbox;
pub mod state;
pub mod trace;
pub mod vault;
pub mod verifier;

pub use config::Config;
pub use vault::Vault;

/// Crate-wide result type.
pub type Result<T> = anyhow::Result<T>;
