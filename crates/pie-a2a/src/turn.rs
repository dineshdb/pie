//! Turn setup shared by every door into the engine (MCP tasks, A2A tasks).
//!
//! Delegated runs always serve at depth 1 — they are themselves subagents,
//! so the engine withholds `main_agent_only` servers (pie itself) and no
//! door can nest through itself. The workspace is carried in
//! `AgentConfig.cwd`; the process cwd is never touched.

use crate::AppContext;
use pie_core::agent::{AgentConfig, PieAgent};
use std::path::Path;
use std::sync::Arc;

/// Why a turn could not be set up. Door-specific error types map the
/// message onto their own wire format.
pub type PrepareError = String;

/// Build the agent for one delegated turn in `session`'s workspace.
///
/// # Errors
///
/// Fails when the daemon's global config was never set, or the provider
/// client cannot be built.
pub fn prepare_turn(
    ctx: &AppContext,
    session: &pie_core::session::Session,
    agent_name: Option<&str>,
) -> Result<PieAgent, PrepareError> {
    // The engine reads pricing, `[mcp.*]`, and debug flags from the global
    // config; refuse to run if the daemon never set it.
    let Some(_config) = pie_core::config::CONFIG.get() else {
        return Err("server config not initialized".into());
    };
    let cwd = Path::new(&session.cwd);
    let registry = ctx.registries.get(cwd);
    let sandbox = Arc::new(pie_core::sandbox_grant::granted_sandbox(
        &ctx.sandbox,
        &[cwd.to_path_buf()],
    ));
    let agent_config = AgentConfig {
        retry: ctx.retry.clone(),
        agent_name: agent_name.map(str::to_owned),
        cwd: Some(cwd.to_path_buf()),
        // Depth 1: this run is itself a subagent — served through the pie
        // daemon — so agents cannot nest through the same door.
        depth: 1,
        ..AgentConfig::default()
    };
    let model = ctx.provider.build_client();
    Ok(PieAgent::new(
        model,
        registry,
        sandbox,
        session.clone(),
        agent_config,
    ))
}

/// Validate an optional `agent` persona against the workspace registry: an
/// unknown name would otherwise silently run without its persona.
pub fn resolve_agent(
    ctx: &AppContext,
    cwd: &Path,
    agent_name: Option<&str>,
) -> Result<Option<String>, PrepareError> {
    let Some(name) = agent_name else {
        return Ok(None);
    };
    let registry = ctx.registries.get(cwd);
    if registry.agents.iter().any(|a| a.name == name) {
        Ok(Some(name.to_owned()))
    } else {
        let mut known: Vec<&str> = registry.agents.iter().map(|a| a.name.as_str()).collect();
        known.sort_unstable();
        Err(format!(
            "unknown agent '{name}' (available in {}: {known:?})",
            cwd.display()
        ))
    }
}
