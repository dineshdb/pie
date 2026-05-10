use crate::config::pie_home;
use crate::hook::{ExecutionStrategy, Hook, HookContext, HookEvent, HookOutcome};
use crate::utils::{find_upward_in_repo, load_file};
use anyhow::Result;
use futures::future::BoxFuture;

#[derive(Debug, Default)]
pub struct AgentsMdHook;

impl AgentsMdHook {
    pub fn new() -> Self {
        Self
    }
}

impl Hook for AgentsMdHook {
    fn name(&self) -> &'static str {
        "agents_md"
    }

    fn event(&self) -> HookEvent {
        HookEvent::PrePrompt
    }

    fn strategy(&self) -> ExecutionStrategy {
        ExecutionStrategy::Parallel
    }

    fn on<'a>(&'a self, _context: &'a HookContext) -> BoxFuture<'a, Result<HookOutcome>> {
        Box::pin(async move {
            let global = load_file(pie_home().join("AGENTS.md")).unwrap_or_default();
            let local = find_upward_in_repo("AGENTS.md").unwrap_or_default();

            if global.is_empty() && local.is_empty() {
                return Ok(HookOutcome::Success);
            }

            let mut parts = Vec::new();
            if !global.is_empty() {
                parts.push(format!("### Global Agents Configuration\n\n{global}"));
            }
            if !local.is_empty() {
                parts.push(format!("### Project Agents Configuration\n\n{local}"));
            }

            Ok(HookOutcome::Transformed {
                name: self.name().to_string(),
                data: serde_json::json!({
                    "system": parts.join("\n\n---\n\n")
                }),
            })
        })
    }
}
