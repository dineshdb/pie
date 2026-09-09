//! The `pie server` daemon: pie exposed to other agents over the
//! [`Agent2Agent`](https://a2a-protocol.org) protocol (v1.0, JSON-RPC over
//! streamable HTTP).
//!
//! `SendStreamingMessage` streams a pie turn over SSE; tasks are
//! in-flight-only (no persisted task store — the durable record is the
//! session transcript, so idle tasks are derived from the DB on demand and
//! a restart costs nothing). See [`a2a`] for the wire contract, task
//! identity and the immutability rules.
//!
//! Runs carry their working directory in `AgentConfig.cwd` and serve at
//! depth 1 (the recursion guard: `main_agent_only` servers are withheld
//! from delegated runs), so sessions in different workspaces execute in
//! parallel; one in-flight turn per session is enforced by the shared
//! [`pie_core::turn_gate::TurnGate`].

mod a2a;
mod http;
mod store;
mod turn;

use pie_core::config::{ResolvedConfig, ServerConfig};
use pie_core::db::DbPool;
use pie_core::p1e_sandbox::SandboxConfig;
use pie_core::turn_gate::TurnGate;
use std::path::Path;
use std::sync::Arc;

/// Everything the daemon shares across connections and turns.
pub struct AppContext {
    /// Manual `Debug`: pool handles and registry contents have no useful
    /// representation, and provider config must not leak its api key.
    pub pool: Arc<DbPool>,
    /// Per-workspace agent/skill registries (project `.pie/` discovery).
    pub registries: pie_core::registry::RegistryCache,
    /// One in-flight turn per session, across every door.
    pub turns: TurnGate,
    pub sandbox: Arc<SandboxConfig>,
    pub provider: pie_core::config::ResolvedProvider,
    pub retry: pie_core::config::RetryConfig,
}

impl AppContext {
    /// The sandbox for a turn running in `cwd`: the daemon's config with
    /// the turn workspace granted read+write.
    pub fn sandbox_for(&self, cwd: &Path) -> Arc<SandboxConfig> {
        Arc::new(pie_core::sandbox_grant::granted_sandbox(
            &self.sandbox,
            &[cwd.to_path_buf()],
        ))
    }
}

impl std::fmt::Debug for AppContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppContext").finish_non_exhaustive()
    }
}

/// Start the `pie server` daemon. The CLI frontend owns process-wide
/// initialization (config resolution, the global `CONFIG`, the DB pool) and
/// hands everything in; `bind_override` (the CLI `--bind` flag) replaces
/// `[server] bind` from the config.
///
/// # Errors
///
/// Returns an error if the listener bind fails — including the deliberate
/// refusal to bind a non-loopback address without an `[server] api_key`.
pub async fn serve(
    bind_override: Option<String>,
    pool: Arc<DbPool>,
    sandbox: Arc<SandboxConfig>,
    server: ServerConfig,
    config: &ResolvedConfig,
) -> anyhow::Result<()> {
    let mut server = server;
    if let Some(bind) = bind_override {
        server.bind = bind;
    }
    if !server.is_loopback_bind() && server.api_key.is_none() {
        anyhow::bail!(
            "refusing to bind non-loopback '{}' without an api_key: \
             set [server] api_key (or a [secrets] reference) in pie.toml",
            server.bind
        );
    }

    let ctx = Arc::new(AppContext {
        pool,
        registries: pie_core::registry::RegistryCache::default(),
        turns: TurnGate::default(),
        sandbox,
        provider: config.provider.clone(),
        retry: config.retry.clone(),
    });

    // A restart interrupts in-flight turns; report them as failed so
    // clients observe a terminal state instead of an eternal WORKING.
    let store = store::TaskStore::new(Arc::clone(&ctx.pool));
    match store.fail_stale_working().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(stale = n, "marked interrupted a2a turns as failed"),
        Err(e) => tracing::warn!(error = %e, "could not sweep stale a2a turns"),
    }

    let http = http::ServerHttp::new(ctx, &server);

    let listener = tokio::net::TcpListener::bind(&server.bind).await?;
    println!("pie server listening on http://{} (A2A)", server.bind);
    tracing::info!(bind = %server.bind, "pie server listening (A2A)");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                break;
            }
            accepted = listener.accept() => {
                let (stream, _peer) = accepted?;
                let http = http.clone();
                tokio::spawn(async move {
                    let io = hyper_util::rt::TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, http)
                        .await;
                });
            }
        }
    }
    Ok(())
}
