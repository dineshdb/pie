//! The HTTP transport: one listener, path-dispatched to the A2A surface.
//! Served by a small hyper 1 listener — no web framework.
//!
//! - `GET /.well-known/agent-card.json` — the A2A Agent Card. Unauthenticated
//!   by design: discovery declares the auth, it carries no secrets.
//! - `POST /a2a` (and friends) — the A2A RPC surface.
//! - everything else — 404.
//!
//! The `Authorization: Bearer` check applies to every route except the
//! card, and the A2A routes enforce a Host-header allowlist (loopback
//! always, plus `[server] allowed_hosts`) — a DNS-rebinding guard, since a
//! browser-crafted cross-site request cannot set the bearer header but
//! also must not learn anything.

use crate::AppContext;
use crate::a2a::A2a;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::{Request, Response};
use pie_core::config::ServerConfig;
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

type HttpBody = BoxBody<Bytes, Infallible>;

const CARD_PATH: &str = "/.well-known/agent-card.json";
const A2A_PATH: &str = "/a2a";

fn unauthorized() -> Response<HttpBody> {
    Response::builder()
        .status(401)
        .header("WWW-Authenticate", "Bearer")
        .body(Full::new(Bytes::from_static(b"unauthorized\n")).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

fn forbidden() -> Response<HttpBody> {
    Response::builder()
        .status(403)
        .body(Full::new(Bytes::from_static(b"host not allowed\n")).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// hyper service: bearer auth in front of everything but the Agent Card,
/// A2A on its paths, 404 elsewhere.
#[derive(Clone)]
pub(crate) struct ServerHttp {
    a2a: A2a,
    api_key: Option<String>,
    allowed_hosts: Arc<Vec<String>>,
}

impl ServerHttp {
    pub fn new(ctx: Arc<AppContext>, server: &ServerConfig) -> Self {
        // Host-header allowlist: with an empty list only loopback hosts
        // are accepted, rejecting the very hostnames a remote setup is
        // meant to serve. Configured hosts extend the loopback defaults —
        // they never replace them.
        let mut allowed_hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        allowed_hosts.extend(server.allowed_hosts.iter().cloned());
        Self {
            a2a: A2a::new(ctx, server.api_key.is_some()),
            api_key: server.api_key.as_ref().map(|k| k.expose_secret().clone()),
            allowed_hosts: Arc::new(allowed_hosts),
        }
    }

    /// A DNS-rebinding guard: a browser-crafted cross-site request cannot
    /// set the bearer header, but it also must not learn anything — so the
    /// A2A routes are rejected by host before auth even matters.
    fn host_allowed(&self, req: &Request<Incoming>) -> bool {
        let authority = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|value| value.to_str().ok());
        host_in_allowlist(authority, &self.allowed_hosts)
    }
}

/// Host-header allowlist: loopback always allowed, `[server] allowed_hosts`
/// extends it. The port is stripped; a missing Host header is a rejection.
fn host_in_allowlist(authority: Option<&str>, allowed_hosts: &[String]) -> bool {
    let Some(authority) = authority else {
        return false;
    };
    // Strip the port; bracketed IPv6 loses its brackets too.
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || allowed_hosts.iter().any(|allowed| allowed == host)
}

impl hyper::service::Service<Request<Incoming>> for ServerHttp {
    type Response = Response<HttpBody>;
    type Error = Infallible;
    type Future =
        std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        match req.uri().path() {
            // Discovery: no bearer (the card declares the auth), host-gated.
            CARD_PATH => {
                if !self.host_allowed(&req) {
                    return Box::pin(async { Ok(forbidden()) });
                }
                let a2a = self.a2a.clone();
                Box::pin(async move { Ok(a2a.handle(req).await) })
            }
            path if path == A2A_PATH => {
                if let Some(expected) = &self.api_key
                    && !bearer_matches(req.headers(), expected)
                {
                    tracing::warn!(path = %path, "rejected request: missing or invalid bearer token");
                    return Box::pin(async { Ok(unauthorized()) });
                }
                if !self.host_allowed(&req) {
                    return Box::pin(async { Ok(forbidden()) });
                }
                let a2a = self.a2a.clone();
                Box::pin(async move { Ok(a2a.handle(req).await) })
            }
            // Unknown paths: not found, without leaking what exists.
            _ => Box::pin(async {
                Ok(Response::builder()
                    .status(404)
                    .body(Full::new(Bytes::from_static(b"not found\n")).boxed())
                    .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed())))
            }),
        }
    }
}

fn bearer_matches(headers: &hyper::HeaderMap, expected: &str) -> bool {
    headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| constant_time_eq(token, expected))
}

/// Length-checked comparison; a plain `==` over strings leaks the key
/// length one byte at a time to a timing oracle. Not cryptographic-grade
/// (allocation timing still leaks), but removes the obvious signal.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_equal_strings() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secrets"));
        assert!(!constant_time_eq("secret", "public"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn host_allowlist_accepts_loopback_and_configured_hosts() {
        let empty: Vec<String> = vec!["localhost".into(), "127.0.0.1".into()];
        assert!(host_in_allowlist(Some("localhost:8629"), &empty));
        assert!(host_in_allowlist(Some("127.0.0.1:8629"), &empty));
        assert!(host_in_allowlist(Some("[::1]:8629"), &empty));
        assert!(!host_in_allowlist(Some("evil.example:80"), &empty));
        assert!(!host_in_allowlist(None, &empty));

        let extended = vec!["localhost".into(), "127.0.0.1".into(), "pie.lvh.me".into()];
        assert!(host_in_allowlist(Some("pie.lvh.me:80"), &extended));
        assert!(!host_in_allowlist(Some("other.lvh.me:80"), &extended));
    }
}
