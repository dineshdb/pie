//! pie-tui — the interactive terminal frontend.
//!
//! A pure A2A consumer: turns and permission answers flow through the
//! A2A client ([`crate::client`]); this crate renders
//! [`StreamEvent`]s and sends requests. The crate is STATELESS: the
//! conversation transcript lives in the gateway daemon's agent
//! filesystem, and the pie-local database (`~/.pie/pie.db` — usage
//! bookkeeping and MCP OAuth grants) belongs to `pie acp`
//! ([`pie_acp::store`]), never to this frontend. pie-core appears only
//! as leaf value types (registry, config, session history). The wire
//! client ([`crate::a2a`]) speaks the A2A HTTP contract itself — JSON-RPC
//! plus SSE, no gateway library. Model and mode selection go through the
//! selection extension ([`crate::client::SELECTION_EXTENSION_URI`]) —
//! no side channels.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod a2a;
pub mod client;
mod command;
mod components;
mod notify;
mod realm;
mod realm_terminal;
mod state;
mod theme;
mod widgets;

pub use client::{CatalogEntry, ModeOption, ModelCatalog, SELECTION_EXTENSION_URI, Selection};
pub use realm::{AskId, SessionId, StreamEvent};
pub use realm_terminal::{ProviderView, TuiDeps, run_tui};
