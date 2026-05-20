use crate::registry::{CompletionKind, Registry};
use agentsdk::{AgentPlugin, Messages, PluginContext};
use async_trait::async_trait;
use std::borrow::Cow;

#[derive(Debug, Default)]
pub struct SkillsPlugin;

impl SkillsPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentPlugin for SkillsPlugin {
    fn name(&self) -> &'static str {
        "skills"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
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

        if skills.is_empty() {
            return None;
        }

        let content = format!("{SKILLS_AND_AGENTS}\n{}", skills.join("\n"));
        Some(Cow::Owned(content))
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
