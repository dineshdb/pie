use crate::config::CliConfig;
use agentsdk::{AgentPlugin, Messages, PluginContext};
use async_trait::async_trait;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write;

#[derive(Debug, Default)]
pub struct KnownCommandsPlugin;

impl KnownCommandsPlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentPlugin for KnownCommandsPlugin {
    fn name(&self) -> &'static str {
        "known_commands"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &PluginContext,
        _history: &Messages,
    ) -> Option<Cow<'static, str>> {
        let help = if let Some(config) = crate::config::CONFIG.get() {
            format_known_commands(&config.known_commands)
        } else {
            String::new()
        };

        if help.is_empty() {
            return None;
        }

        Some(Cow::Owned(help))
    }
}

fn format_known_commands(commands: &HashMap<String, CliConfig>) -> String {
    let mut out = String::from("You can run known external commands via shell tool:\n");
    let mut sorted: Vec<_> = commands.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    for (name, cfg) in sorted {
        let _ = writeln!(
            out,
            "- {name}: {}",
            cfg.description.as_deref().unwrap_or(&cfg.command)
        );
    }
    out
}
