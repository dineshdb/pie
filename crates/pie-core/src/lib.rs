//! pie-core — the pie agent library.
//!
//! Everything that makes pie run, with no frontend: the agent engine and
//! plugins, configuration, SQLite sessions, the registry (skills/agents),
//! cron, and the CLI command handlers. Frontends live elsewhere — the `pie`
//! application crate (CLI + TUI) and `pie-acp` (Agent Client Protocol).
//!
//! Lints: the workspace policy applies; test code is allowed
//! `unwrap`/`expect`/`panic`/indexing via the crate-level `cfg_attr` below.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
// Debt: pie-core's API went public with the crate split, waking pedantic
// lints that never fired while these modules were private to the binary.
// Fix the API properly instead of extending these allowances.
#![allow(
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::new_without_default,
    missing_copy_implementations,
    missing_debug_implementations
)]

pub mod agent;
pub mod cmd;
pub mod config;
pub mod cron;
pub mod db;
pub mod error;
pub mod handler;
pub mod instructions;
pub mod plugin;
pub mod prompt;
pub mod registry;
pub mod sandbox_grant;
pub mod session;
pub mod tools;
pub mod turn_gate;
pub mod usage;
pub mod utils;

/// Re-export so frontends can name sandbox types without depending on
/// p1e-sandbox directly.
pub use p1e_sandbox;
