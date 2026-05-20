use crate::agent::OutputMode;
use crate::config::CONFIG;
use crate::hook::{
    ExecutionStrategy, HookContext, HookContextData, HookEvent, HookOutcome, HookScope, PromptData,
    ToolData,
};
use crate::plugin::StaticPlugin;
use agentsdk::core::agent::{CompletionAction, PostToolAction, PreToolAction};
use agentsdk::core::history::History;
use agentsdk::core::messages::Message;
use agentsdk::core::plugin::{AgentPlugin, PluginContext};
use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::Value;
use std::collections::HashMap;

pub struct UserPlugins(pub Vec<StaticPlugin>);

/// Run prompt hooks outside the agent loop (`PostUserQuery`, `PrePrompt`, `PostPrompt`).
pub(crate) async fn run_prompt_hook(
    plugins: &[StaticPlugin],
    event: HookEvent,
    system: Option<&str>,
    query: Option<&str>,
    session_id: &str,
    output_mode: OutputMode,
) -> (Option<String>, Option<String>) {
    let data = HookContextData::Prompt(PromptData {
        system: system.map(String::from),
        query: query.map(String::from),
    });
    let ctx = make_ctx(event, session_id, output_mode, data);
    match dispatch(&ctx, plugins).await {
        Ok((_, HookContextData::Prompt(p))) => (p.system, p.query),
        _ => (None, None),
    }
}

fn make_ctx(
    event: HookEvent,
    session_id: &str,
    output_mode: OutputMode,
    data: HookContextData,
) -> HookContext {
    HookContext::new(
        event,
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        session_id.to_string(),
        output_mode,
        data,
    )
}

/// Core dispatch: run all hooks matching `event` from the given plugins.
async fn dispatch(
    context: &HookContext,
    plugins: &[StaticPlugin],
) -> Result<(Vec<HookOutcome>, HookContextData)> {
    let applicable_hooks: Vec<_> = plugins
        .iter()
        .flat_map(|p| &p.hooks)
        .filter(|h| h.event() == context.event && h.matches(context))
        .cloned()
        .collect();

    if applicable_hooks.is_empty() {
        return Ok((vec![], context.data.clone()));
    }

    let mut all_outcomes = Vec::new();
    let mut current_data = context.data.clone();

    let (validations, transforms): (Vec<_>, Vec<_>) = applicable_hooks
        .into_iter()
        .partition(|h| h.scope() == HookScope::Validation);

    if !validations.is_empty() {
        let results = join_all(validations.iter().map(|h| h.on(context))).await;
        for result in results {
            all_outcomes.push(result?);
        }
        if all_outcomes
            .iter()
            .any(|o| matches!(o, HookOutcome::Error { .. }))
        {
            return Ok((all_outcomes, current_data));
        }
    }

    let mut iter = transforms.iter().peekable();
    while let Some(hook) = iter.next() {
        if hook.strategy() == ExecutionStrategy::Sequential {
            let ctx = make_ctx(
                context.event,
                &context.session_id,
                context.output_mode,
                current_data.clone(),
            );
            let outcome = hook.on(&ctx).await?;
            if let HookOutcome::Transformed { data, .. } = &outcome {
                current_data.merge(data.clone());
            }
            all_outcomes.push(outcome);
        } else {
            let mut batch = vec![hook];
            while iter.peek().is_some_and(|h| h.strategy() == ExecutionStrategy::Parallel) {
                if let Some(next) = iter.next() {
                    batch.push(next);
                }
            }

            let ctx = make_ctx(
                context.event,
                &context.session_id,
                context.output_mode,
                current_data.clone(),
            );
            let results = join_all(batch.iter().map(|h| h.on(&ctx))).await;
            for result in results {
                let outcome = result?;
                if let HookOutcome::Transformed { data, .. } = &outcome {
                    current_data.merge(data.clone());
                }
                all_outcomes.push(outcome);
            }
        }

        if all_outcomes
            .iter()
            .any(|o| matches!(o, HookOutcome::Error { .. }))
        {
            break;
        }
    }

    Ok((all_outcomes, current_data))
}

pub struct UserPluginRunner {
    session_id: String,
    output_mode: OutputMode,
    tool_params: HashMap<String, Value>,
}

impl UserPluginRunner {
    pub fn new(session_id: String, output_mode: OutputMode) -> Self {
        Self {
            session_id,
            output_mode,
            tool_params: HashMap::new(),
        }
    }

    async fn run_pre_completion(&self, plugins: &[StaticPlugin], text: String) -> CompletionAction {
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: Some(text.clone()),
        });
        let ctx = make_ctx(
            HookEvent::PreCompletion,
            &self.session_id,
            self.output_mode,
            data,
        );
        match dispatch(&ctx, plugins).await {
            Ok((outcomes, HookContextData::Prompt(p))) => {
                for outcome in &outcomes {
                    if let HookOutcome::Error { message, .. } = outcome {
                        return CompletionAction::Reject {
                            reason: message.clone(),
                        };
                    }
                }
                if let Some(transformed) = p.query
                    && transformed != text
                {
                    CompletionAction::Accept(Some(transformed))
                } else {
                    CompletionAction::Accept(None)
                }
            }
            Err(e) => {
                tracing::warn!("completion.pre hook error: {e}");
                CompletionAction::Accept(None)
            }
            _ => CompletionAction::Accept(None),
        }
    }

    async fn run_pre_tool_use(
        &self,
        plugins: &[StaticPlugin],
        tool_name: &str,
        args: &Value,
    ) -> PreToolAction {
        let data = HookContextData::Tool(ToolData {
            tool: Some(tool_name.to_string()),
            input: Some(args.clone()),
            output: None,
        });
        let ctx = make_ctx(
            HookEvent::PreToolUse,
            &self.session_id,
            self.output_mode,
            data,
        );
        match dispatch(&ctx, plugins).await {
            Ok((outcomes, HookContextData::Tool(t))) => {
                for outcome in &outcomes {
                    if let HookOutcome::Error { message, .. } = outcome {
                        return PreToolAction::Abort(message.clone());
                    }
                }
                PreToolAction::Continue(t.input)
            }
            Err(e) => {
                tracing::warn!("tool.pre hook error: {e}");
                PreToolAction::Continue(None)
            }
            _ => PreToolAction::Continue(None),
        }
    }

    async fn run_post_tool_use(
        &self,
        plugins: &[StaticPlugin],
        tool_name: &str,
        params: &Value,
        result: &Value,
    ) -> PostToolAction {
        let data = HookContextData::Tool(ToolData {
            tool: Some(tool_name.to_string()),
            input: Some(params.clone()),
            output: Some(result.clone()),
        });
        let ctx = make_ctx(
            HookEvent::PostToolUse,
            &self.session_id,
            self.output_mode,
            data,
        );
        match dispatch(&ctx, plugins).await {
            Ok((outcomes, HookContextData::Tool(t))) => {
                for outcome in &outcomes {
                    if !matches!(
                        outcome,
                        HookOutcome::Success | HookOutcome::Transformed { .. }
                    ) {
                        tracing::warn!("tool.post hook: {}", outcome.format());
                    }
                }
                PostToolAction::Continue(t.output)
            }
            Err(e) => {
                tracing::warn!("tool.post hook error: {e}");
                PostToolAction::Continue(None)
            }
            _ => PostToolAction::Continue(None),
        }
    }

    async fn run_post_completion(&self, plugins: &[StaticPlugin], final_text: Option<String>) {
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: final_text,
        });
        let ctx = make_ctx(
            HookEvent::PostCompletion,
            &self.session_id,
            self.output_mode,
            data,
        );
        let _ = dispatch(&ctx, plugins).await;
    }
}

#[async_trait]
impl AgentPlugin for UserPluginRunner {
    fn name(&self) -> &'static str {
        "user_plugins"
    }

    async fn init(&mut self, ctx: &mut PluginContext) {
        let plugins = CONFIG.get().map(|c| c.plugins.clone()).unwrap_or_default();
        ctx.insert(UserPlugins(plugins));
    }

    async fn on_completion(&mut self, ctx: &PluginContext, text: String) -> CompletionAction {
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return CompletionAction::Accept(None);
        };
        self.run_pre_completion(&plugins.0, text).await
    }

    async fn on_tool_pre_execute(
        &mut self,
        ctx: &PluginContext,
        id: &str,
        name: &str,
        args: &Value,
    ) -> PreToolAction {
        self.tool_params.insert(id.to_string(), args.clone());
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return PreToolAction::Continue(None);
        };
        self.run_pre_tool_use(&plugins.0, name, args).await
    }

    async fn on_tool_post_execute(
        &mut self,
        ctx: &PluginContext,
        id: &str,
        name: &str,
        result: &Value,
    ) -> PostToolAction {
        let params = self.tool_params.remove(id).unwrap_or(Value::Null);
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return PostToolAction::Continue(None);
        };
        self.run_post_tool_use(&plugins.0, name, &params, result)
            .await
    }

    async fn shutdown(&mut self, ctx: &mut PluginContext) {
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return;
        };
        let final_text = ctx.get::<History>().and_then(|h| {
            h.0.iter().rev().find_map(|msg| match msg {
                Message::AssistantMessage(a) => a.content.clone(),
                _ => None,
            })
        });
        self.run_post_completion(&plugins.0, final_text).await;
    }
}
