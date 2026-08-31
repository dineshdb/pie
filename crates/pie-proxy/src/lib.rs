//! pie-proxy — a hostname-filtering egress proxy.
//!
//! It answers one question for every outbound connection: *may this process
//! reach this host?* Filtering at the hostname level needs no TLS
//! interception — the target is plaintext in the `CONNECT` authority or the
//! absolute request URI — so encrypted traffic is relayed byte for byte.
//!
//! Two ways to use it:
//! - **standalone**: run the `pie-proxy` binary and point a process at it with
//!   `HTTPS_PROXY`/`HTTP_PROXY`.
//! - **with piebox**: run it on the host as the microVM's egress gateway, where
//!   a guest cannot bypass or reconfigure it.
//!
//! [`policy`] is deliberately independent of the server, so the rules can be
//! tested on their own and reused wherever an egress decision is needed.
//!
//! # What it does not protect against
//!
//! Because encrypted traffic is relayed rather than inspected, these are out
//! of reach without TLS interception, and callers should not assume otherwise:
//!
//! - **SNI/Host inside a tunnel.** Once a `CONNECT` to an allowed host is
//!   established, the client chooses the TLS SNI and HTTP `Host` sent through
//!   it. An allowlisted CDN address can therefore still serve a different
//!   origin. (On the plaintext path the `Host` header *is* rewritten to the
//!   checked authority.)
//! - **DNS rebinding.** The hostname is authorized, then resolved by the
//!   connector; a name whose DNS the other side controls can point anywhere.
//!   Restricting reachable ports and denying private ranges is the practical
//!   mitigation.
//! - **Content.** Nothing inspects payloads, so an allowed host can still be
//!   used to move data.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod policy;
pub mod proxy;

pub use policy::{Decision, DenyReason, Host, HostPattern, Mode, Policy, PolicyError, Target};
pub use proxy::serve;
