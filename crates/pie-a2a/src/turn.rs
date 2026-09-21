//! Turn setup for the A2A door. Building the delegated agent is shared
//! engine-side logic and lives in `pie-core` (`delegated_agent`); what
//! stays here is A2A-specific validation against the daemon's registry
//! cache.
//!
//! Delegated runs always serve at depth 1 — they are themselves
//! subagents, so the engine withholds `main_agent_only` servers (pie
//! itself) and no door can nest through itself. The workspace is carried
//! in `AgentConfig.cwd`; the process cwd is never touched.

use crate::AppContext;
use pie_core::bridge::{PrepareError, delegated_agent};
use std::path::Path;

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
) -> Result<pie_core::agent::PieAgent, PrepareError> {
    delegated_agent(
        &ctx.provider,
        &ctx.retry,
        &ctx.sandbox,
        &ctx.registries,
        session,
        agent_name,
    )
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
