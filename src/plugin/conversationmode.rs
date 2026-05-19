use crate::{
    hook::{ExecutionStrategy, Hook, HookContext, HookEvent, HookOutcome},
    plugin::Plugin,
};
use anyhow::Result;
use futures::future::BoxFuture;
use std::sync::{Arc, LazyLock};

static CONVERSATION_MODE_HOOKS: LazyLock<[Arc<dyn Hook>; 1]> =
    LazyLock::new(|| [Arc::new(ConversationModeHook)]);

#[derive(Debug)]
pub struct ConversationModePlugin;

impl ConversationModePlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for ConversationModePlugin {
    fn name(&self) -> &'static str {
        "ConversationMode"
    }

    fn hooks(&self) -> &[Arc<dyn Hook>] {
        CONVERSATION_MODE_HOOKS.as_slice()
    }
}

#[derive(Debug)]
pub struct ConversationModeHook;

impl Hook for ConversationModeHook {
    fn name(&self) -> &'static str {
        "ConversationMode"
    }

    fn event(&self) -> HookEvent {
        HookEvent::PrePrompt
    }

    fn strategy(&self) -> ExecutionStrategy {
        ExecutionStrategy::Parallel
    }

    fn on<'a>(&'a self, context: &'a HookContext) -> BoxFuture<'a, Result<HookOutcome>> {
        let output_mode = context.output_mode;
        Box::pin(async move {
            Ok(HookOutcome::system_transform(
                self.name(),
                output_mode.prompt(),
            ))
        })
    }
}
