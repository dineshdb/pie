//! pie as an in-process agent of an `a2acp` gateway: every session the
//! gateway's pool opens is served by pie's own ACP server loop
//! ([`crate::server::serve_acp`]) over the channel the pool hands out.
//! To the gateway this pie is indistinguishable from a spawned
//! `pie acp` process — same wire, same semantics, no subprocess.

use crate::server::serve_acp;
use crate::{PieSessions, server_info};
use agent_client_protocol as acp;
use pie_core::config::{ResolvedProvider, RetryConfig};
use pie_core::db::DbPool;
use pie_core::p1e_sandbox::SandboxConfig;
use pie_core::registry::Registry;
use pie_core::session::SessionId as PieSessionId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, PoisonError};

fn lock<T>(lock: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything the hosted pie agent needs to open sessions.
pub struct HostDeps {
    pub pool: Arc<DbPool>,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub provider: ResolvedProvider,
    pub retry: RetryConfig,
    pub agent_name: Option<String>,
    /// The conversation the very first opened session resumes — the
    /// interactive TUI's startup session, so the first turn continues
    /// the conversation `pie` resolved at launch. Every later open
    /// creates a fresh session (`pie acp` never sets this).
    pub resume: Option<PieSessionId>,
}

impl std::fmt::Debug for HostDeps {
    // Manual `Debug`: pool/registry handles have no useful
    // representation, and provider config must not leak its api key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostDeps").finish_non_exhaustive()
    }
}

/// pie behind an a2acp [`a2acp::InProcessAgent`] door: one `connect` is
/// one session's connection, served by a fresh [`PieSessions`] assembly
/// (a new connection ≙ a respawned agent, so per-connection state like
/// pinned modes starts clean).
pub struct PieHost {
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    provider: ResolvedProvider,
    retry: RetryConfig,
    agent_name: Option<String>,
    /// Taken by the first connection's first session open.
    resume_first: StdMutex<Option<PieSessionId>>,
}

impl std::fmt::Debug for PieHost {
    // Manual `Debug`: pool/registry handles have no useful
    // representation, and provider config must not leak its api key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PieHost").finish_non_exhaustive()
    }
}

impl PieHost {
    #[must_use]
    pub fn new(deps: HostDeps) -> Self {
        Self {
            pool: deps.pool,
            registry: deps.registry,
            sandbox: deps.sandbox,
            provider: deps.provider,
            retry: deps.retry,
            agent_name: deps.agent_name,
            resume_first: StdMutex::new(deps.resume),
        }
    }

    /// The per-connection assembly: fresh `PieSessions`, seeded with the
    /// startup session for the first connection only.
    fn sessions(&self) -> PieSessions {
        let mut sessions = PieSessions::new(
            Arc::clone(&self.pool),
            Arc::clone(&self.registry),
            Arc::clone(&self.sandbox),
            self.provider.clone(),
            self.retry.clone(),
        );
        sessions.agent_name.clone_from(&self.agent_name);
        if let Some(id) = lock(&self.resume_first).take() {
            sessions.set_resume_first(id);
        }
        sessions
    }
}

impl a2acp::InProcessAgent for PieHost {
    fn connect(
        &self,
        transport: acp::Channel,
    ) -> Pin<Box<dyn Future<Output = Result<(), acp::Error>> + Send>> {
        let sessions = self.sessions();
        Box::pin(async move { serve_acp(sessions, server_info(), transport).await })
    }
}
