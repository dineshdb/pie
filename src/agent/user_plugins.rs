use crate::agent::OutputMode;
use crate::config::CONFIG;
use crate::hook::{
    ExecutionStrategy, HookContext, HookContextData, HookEvent, HookOutcome, HookScope, PromptData,
    ToolData,
};
use crate::plugin::ExternalPlugin;
use crate::session::{HistoryContent, Session, ToolCall};
use agentsdk::core::agent::{CompletionAction, PostToolAction, PreToolAction};
use agentsdk::core::messages::Message;
use agentsdk::core::plugin::{AgentPlugin, PluginContext};
use anyhow::Result;
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::Value;
use std::collections::HashMap;

pub struct UserPlugins(pub Vec<ExternalPlugin>);

/// Run prompt hooks outside the agent loop (`PostUserQuery`, `PrePrompt`, `PostPrompt`).
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
    plugins: &[ExternalPlugin],
) -> Result<(Vec<HookOutcome>, HookContextData)> {
    let applicable_hooks: Vec<_> = plugins
        .iter()
        .flat_map(|p| p.hooks.iter().map(move |h| (p.name.clone(), h.clone())))
        .filter(|(_, h)| h.event() == context.event && h.matches(context))
        .collect();

    if applicable_hooks.is_empty() {
        return Ok((vec![], context.data.clone()));
    }

    let mut current_data = context.data.clone();

    let (validations, transforms): (Vec<_>, Vec<_>) = applicable_hooks
        .into_iter()
        .partition(|(_, h)| h.scope() == HookScope::Validation);

    let mut all_outcomes = run_validations(context, &validations).await?;

    if all_outcomes
        .iter()
        .any(|o| matches!(o, HookOutcome::Error { .. }))
    {
        return Ok((all_outcomes, current_data));
    }

    run_transforms(context, transforms, &mut all_outcomes, &mut current_data).await?;

    Ok((all_outcomes, current_data))
}

async fn run_validations(
    context: &HookContext,
    validations: &[(String, crate::hook::CommandHook)],
) -> Result<Vec<HookOutcome>> {
    let mut outcomes = Vec::new();
    for (plugin_name, hook) in validations {
        let start = std::time::Instant::now();
        let outcome = hook.on(context).await?;
        let elapsed = start.elapsed();
        tracing::info!(
            plugin = %plugin_name,
            event = ?context.event,
            scope = "validation",
            elapsed_ms = elapsed.as_millis(),
            "hook execution"
        );
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

async fn run_transforms(
    context: &HookContext,
    transforms: Vec<(String, crate::hook::CommandHook)>,
    all_outcomes: &mut Vec<HookOutcome>,
    current_data: &mut HookContextData,
) -> Result<()> {
    let mut iter = transforms.into_iter().peekable();
    while let Some((plugin_name, hook)) = iter.next() {
        if hook.strategy() == ExecutionStrategy::Sequential {
            let ctx = make_ctx(
                context.event,
                &context.session_id,
                context.output_mode,
                current_data.clone(),
            );
            let start = std::time::Instant::now();
            let outcome = hook.on(&ctx).await?;
            let elapsed = start.elapsed();
            tracing::info!(
                plugin = %plugin_name,
                event = ?context.event,
                scope = "transform",
                strategy = "sequential",
                elapsed_ms = elapsed.as_millis(),
                "hook execution"
            );
            if let HookOutcome::Transformed { data, .. } = &outcome {
                current_data.merge(data.clone());
            }
            all_outcomes.push(outcome);
        } else {
            let mut batch = vec![(plugin_name, hook)];
            while let Some((_, next_hook)) = iter.peek()
                && next_hook.strategy() == ExecutionStrategy::Parallel
            {
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

            let start = std::time::Instant::now();
            let mut futures = Vec::new();
            for (p_name, h) in &batch {
                let ctx_clone = ctx.clone();
                futures.push(async move { (p_name, h.on(&ctx_clone).await) });
            }

            let results = join_all(futures).await;
            let elapsed = start.elapsed();

            for (p_name, result) in results {
                let outcome = result?;
                tracing::info!(
                    plugin = %p_name,
                    event = ?context.event,
                    scope = "transform",
                    strategy = "parallel",
                    elapsed_ms = elapsed.as_millis(),
                    "hook execution"
                );
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
    Ok(())
}

pub struct UserPluginRunner {
    session: Session,
    output_mode: OutputMode,
    tool_ids: HashMap<String, i64>,
}

impl UserPluginRunner {
    pub fn new(session: Session, output_mode: OutputMode) -> Self {
        Self {
            session,
            output_mode,
            tool_ids: HashMap::new(),
        }
    }

    async fn run_hook<R, F>(
        &self,
        plugins: &[ExternalPlugin],
        event: HookEvent,
        data: HookContextData,
        f: F,
    ) -> R
    where
        F: FnOnce(Vec<HookOutcome>, HookContextData) -> R,
    {
        let ctx = make_ctx(event, &self.session.id.to_string(), self.output_mode, data);
        match dispatch(&ctx, plugins).await {
            Ok((outcomes, data)) => f(outcomes, data),
            Err(e) => {
                tracing::warn!("{event:?} hook error: {e}");
                f(vec![], ctx.data)
            }
        }
    }

    async fn run_pre_completion(
        &self,
        plugins: &[ExternalPlugin],
        text: String,
    ) -> CompletionAction {
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: Some(text.clone()),
        });
        self.run_hook(plugins, HookEvent::PreCompletion, data, |outcomes, data| {
            for outcome in &outcomes {
                if let HookOutcome::Error { message, .. } = outcome {
                    return CompletionAction::Reject {
                        reason: message.clone(),
                    };
                }
            }
            if let HookContextData::Prompt(p) = data
                && let Some(transformed) = p.query
                && transformed != text
            {
                CompletionAction::Accept(Some(transformed))
            } else {
                CompletionAction::Accept(None)
            }
        })
        .await
    }

    async fn run_pre_tool_use(
        &self,
        plugins: &[ExternalPlugin],
        tool_name: &str,
        args: &Value,
    ) -> PreToolAction {
        let data = HookContextData::Tool(ToolData {
            tool: Some(tool_name.to_string()),
            input: Some(args.clone()),
            output: None,
        });
        self.run_hook(plugins, HookEvent::PreToolUse, data, |outcomes, data| {
            for outcome in &outcomes {
                if let HookOutcome::Error { message, .. } = outcome {
                    return PreToolAction::Abort(message.clone());
                }
            }
            if let HookContextData::Tool(t) = data {
                PreToolAction::Continue(t.input)
            } else {
                PreToolAction::Continue(None)
            }
        })
        .await
    }

    async fn run_post_tool_use(
        &self,
        plugins: &[ExternalPlugin],
        tool_name: &str,
        params: &Value,
        result: &Value,
    ) -> PostToolAction {
        let data = HookContextData::Tool(ToolData {
            tool: Some(tool_name.to_string()),
            input: Some(params.clone()),
            output: Some(result.clone()),
        });
        self.run_hook(plugins, HookEvent::PostToolUse, data, |outcomes, data| {
            for outcome in &outcomes {
                if !matches!(
                    outcome,
                    HookOutcome::Success | HookOutcome::Transformed { .. }
                ) {
                    tracing::warn!("tool.post hook: {}", outcome.format());
                }
            }
            if let HookContextData::Tool(t) = data {
                PostToolAction::Continue(t.output)
            } else {
                PostToolAction::Continue(None)
            }
        })
        .await
    }

    async fn run_post_completion(&self, plugins: &[ExternalPlugin], final_text: Option<String>) {
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: final_text,
        });
        self.run_hook(plugins, HookEvent::PostCompletion, data, |_, _| ())
            .await;
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
    async fn on_user_message(&mut self, ctx: &mut PluginContext, text: String) -> String {
        let start = std::time::Instant::now();
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return text;
        };
        let data = HookContextData::Prompt(PromptData {
            system: None,
            query: Some(text.clone()),
        });
        let h_ctx = make_ctx(
            HookEvent::PostUserQuery,
            &self.session.id.to_string(),
            self.output_mode,
            data,
        );
        let result = match dispatch(&h_ctx, &plugins.0).await {
            Ok((_, HookContextData::Prompt(p))) => p.query.unwrap_or(text),
            _ => text,
        };
        tracing::info!(
            elapsed_ms = start.elapsed().as_millis(),
            "on_user_message total"
        );
        result
    }

    async fn on_model_response_completed(&mut self, _ctx: &mut PluginContext, msg: &Message) {
        let start = std::time::Instant::now();
        if let Message::AssistantMessage(a) = msg {
            if let Some(content) = &a.content
                && !content.is_empty()
            {
                let _ = self.session.add_assistant(content).await;
            }
            if let Some(calls) = &a.tool_calls {
                for call in calls {
                    let tc = ToolCall {
                        call_id: call.id.clone(),
                        tool_name: call.function.name.clone(),
                        params: serde_json::from_str(&call.function.arguments)
                            .unwrap_or(Value::Null),
                        output: None,
                    };
                    if let Ok(id) = self.session.add_tool_call(&tc).await {
                        self.tool_ids.insert(call.id.clone(), id);
                    }
                }
            }
        }
        tracing::info!(
            elapsed_ms = start.elapsed().as_millis(),
            "on_model_response_completed"
        );
    }

    async fn on_tool_pre_execute(
        &mut self,
        ctx: &mut PluginContext,
        _id: &str,
        name: &str,
        args: &Value,
    ) -> PreToolAction {
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return PreToolAction::Continue(None);
        };
        self.run_pre_tool_use(&plugins.0, name, args).await
    }

    async fn on_tool_post_execute(
        &mut self,
        ctx: &mut PluginContext,
        id: &str,
        name: &str,
        result: &Value,
    ) -> PostToolAction {
        // Update database with tool output
        if let Some(db_id) = self.tool_ids.remove(id) {
            let _ = self
                .session
                .update_tool_output_by_id(db_id, result.to_string())
                .await;
        }

        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return PostToolAction::Continue(None);
        };
        self.run_post_tool_use(&plugins.0, name, &Value::Null, result)
            .await
    }

    async fn on_completion(&mut self, ctx: &mut PluginContext, text: String) -> CompletionAction {
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return CompletionAction::Accept(None);
        };
        self.run_pre_completion(&plugins.0, text).await
    }

    async fn shutdown(&mut self, ctx: &mut PluginContext) {
        let Some(plugins) = ctx.get::<UserPlugins>() else {
            return;
        };
        let final_text = self.session.history_entries().iter().rev().find_map(|e| {
            if let Ok(HistoryContent::Assistant(c)) = e.to_history_content() {
                Some(c)
            } else {
                None
            }
        });
        self.run_post_completion(&plugins.0, final_text).await;
    }
}
