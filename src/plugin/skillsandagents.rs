use crate::hook::{ExecutionStrategy, Hook, HookContext, HookEvent, HookOutcome};
use crate::registry::{CompletionKind, Registry};
use anyhow::Result;
use futures::future::BoxFuture;

#[derive(Debug, Default)]
pub struct SkillsAndAgentsHook;

impl SkillsAndAgentsHook {
    pub fn new() -> Self {
        Self
    }
}

impl Hook for SkillsAndAgentsHook {
    fn name(&self) -> &'static str {
        "skills_and_agents"
    }

    fn event(&self) -> HookEvent {
        HookEvent::PrePrompt
    }

    fn strategy(&self) -> ExecutionStrategy {
        ExecutionStrategy::Parallel
    }

    fn on<'a>(&'a self, _context: &'a HookContext) -> BoxFuture<'a, Result<HookOutcome>> {
        Box::pin(async move {
            let mut skills = Vec::new();

            if let Some(registry) = Registry::get() {
                for item in &registry.completions {
                    match item.kind {
                        CompletionKind::Builtin => {}
                        CompletionKind::Skill => {
                            skills.push(format!("- [s] {}: {}", item.label, item.description));
                        }
                        CompletionKind::Agent => {
                            skills.push(format!("- [a] {}: {}", item.label, item.description));
                        }
                    }
                }
            }

            Ok(HookOutcome::Transformed {
                name: self.name().to_string(),
                data: serde_json::json!({
                    "system": format!("{SKILLS_AND_AGENTS}\n{}", skills.join("\n"))
                }),
            })
        })
    }
}

const SKILLS_AND_AGENTS: &str = r"
## Skills and Agents
Skills are extra knowledge you can load on-demand using `load_skills` tool.

Agents are specialized personas you can delegate to using `subagent` tool.
Agents have their own context and provide only the reponse you want, keeping your context lean.

Skills and Agents can't be invoked directly as tools.
Available skills and agents:
";
