use agentsdk::core::agent::PreToolAction;
use agentsdk::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Instant;

const MAX_AGE_RECENT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_AGE_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_REPEATS: usize = 2;

struct CallEntry {
    name: String,
    args: Value,
    at: Instant,
}

pub struct DoomLoopPlugin {
    calls: Vec<CallEntry>,
}

impl DoomLoopPlugin {
    pub fn new() -> Self {
        Self { calls: Vec::new() }
    }
}

#[async_trait]
impl AgentPlugin for DoomLoopPlugin {
    fn name(&self) -> &'static str {
        "doom_loop"
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        arguments: &Value,
    ) -> PreToolAction {
        let now = Instant::now();
        let same: Vec<_> = self
            .calls
            .iter()
            .filter(|c| c.name == name && c.args == *arguments)
            .collect();

        if let Some(last) = same.last() {
            let recency = now.duration_since(last.at) < MAX_AGE_RECENT;
            let frequency = same.len() > MAX_REPEATS
                && same
                    .iter()
                    .filter(|c| now.duration_since(c.at) < MAX_AGE_WINDOW)
                    .count()
                    > MAX_REPEATS;

            if recency && frequency {
                tracing::debug!(tool = name, "aborting duplicate tool call");
                return PreToolAction::Abort("You already made that tool call.".to_string());
            }
        }

        self.calls.push(CallEntry {
            name: name.to_string(),
            args: arguments.clone(),
            at: now,
        });
        PreToolAction::Proceed(None)
    }
}
