use std::collections::HashMap;

use crate::{
    config::CliConfig,
    hook::{ExecutionStrategy, Hook, HookContext, HookEvent, HookOutcome},
};
use anyhow::Result;
use futures::future::BoxFuture;
use std::fmt::Write;

#[derive(Debug, Default)]
pub struct KnownCommandsPromptHook;

impl KnownCommandsPromptHook {
    pub fn new() -> Self {
        Self
    }
}

impl Hook for KnownCommandsPromptHook {
    fn name(&self) -> &'static str {
        "known_commands"
    }

    fn event(&self) -> HookEvent {
        HookEvent::PrePrompt
    }

    fn strategy(&self) -> ExecutionStrategy {
        ExecutionStrategy::Parallel
    }

    fn on<'a>(&'a self, _context: &'a HookContext) -> BoxFuture<'a, Result<HookOutcome>> {
        Box::pin(async move {
            let help = if let Some(config) = crate::config::CONFIG.get() {
                format_known_commands(&config.known_commands)
            } else {
                String::new()
            };

            Ok(HookOutcome::Transformed {
                name: self.name().to_string(),
                data: serde_json::json!({
                    "system": help
                }),
            })
        })
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
