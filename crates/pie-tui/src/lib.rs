//! pie-tui — the interactive terminal frontend.
//!
//! A pure A2A consumer: turns and permission answers flow through the
//! front door client ([`crate::door`]); this crate renders
//! [`StreamEvent`]s and sends requests. It never touches the DB pool,
//! never constructs agents, and calls no engine handlers — pie-core
//! appears only as leaf value types (registry, config, session history),
//! and a2acp only as the door. The frontend does not know or care that
//! the agent is in process.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod command;
mod components;
pub mod door;
mod notify;
mod realm;
mod realm_terminal;
mod state;
mod widgets;

pub use realm::{AskId, SessionId, StreamEvent};
pub use realm_terminal::{ProviderView, TuiDeps, run_tui};
