use crate::agent::OutputMode;
use crate::config::CONFIG;
use crate::hook::{HookContext, HookContextData, HookEvent, HookOutcome, ToolData};
use agentsdk::core::tools::{Tool, ToolDefinition, ToolExecute};
use serde_json::Value;
use std::sync::Arc;

/// Helper to wrap all tools with hooks.
pub fn wrap_tools_with_hooks(
    tools: Vec<Tool>,
    session_id: &str,
    output_mode: OutputMode,
) -> anyhow::Result<Vec<Tool>> {
    tools
        .into_iter()
        .map(|t| {
            let name = t.name().to_string();
            let description = t.description().to_string();
            let input_schema = t.input_schema().clone();
            let inner_tool = Arc::new(t);
            let sid = session_id.to_string();

            Ok(Tool::builder()
                .definition(
                    ToolDefinition::builder()
                        .name(&name)
                        .description(&description)
                        .input_schema(input_schema)
                        .build()?,
                )
                .execute(ToolExecute::from_async(move |ctx, params| {
                    let inner = inner_tool.clone();
                    let session_id = sid.clone();
                    async move {
                        let cwd = std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();

                        let (tool_name, params, mut warnings) = run_pre_tool_hooks(
                            &session_id,
                            &cwd,
                            inner.name(),
                            params,
                            output_mode,
                        )
                        .await?;

                        // Execute the actual tool
                        let mut result = inner.execute.call(ctx, params.clone()).await;

                        if let Ok(output) = &result {
                            let output_str = if let Value::String(s) = output {
                                s.clone()
                            } else {
                                output.to_string()
                            };

                            let (new_output, post_warnings) = run_post_tool_hooks(
                                &session_id,
                                &cwd,
                                &tool_name,
                                params,
                                &output_str,
                                output_mode,
                            )
                            .await;

                            if let Some(new_output) = new_output {
                                result = Ok(Value::String(new_output));
                            }
                            warnings.extend(post_warnings);
                        }

                        // Prepend warnings to output if any
                        if let Ok(ref mut output) = result
                            && !warnings.is_empty()
                        {
                            let warnings_str = warnings.join("\n");
                            let output_str = if let Value::String(s) = output {
                                s.clone()
                            } else {
                                output.to_string()
                            };
                            *output = Value::String(format!("{warnings_str}\n\n{output_str}"));
                        }

                        result.map_err(|e| e.clone())
                    }
                }))
                .build()?)
        })
        .collect()
}

async fn run_pre_tool_hooks(
    session_id: &str,
    cwd: &str,
    tool_name: &str,
    params: Value,
    output_mode: OutputMode,
) -> Result<(String, Value, Vec<String>), String> {
    let mut current_tool_name = tool_name.to_string();
    let mut current_params = params;
    let mut warnings = Vec::new();

    let Some(cfg) = CONFIG.get() else {
        return Ok((current_tool_name, current_params, warnings));
    };

    let hook_ctx = HookContext::new(
        HookEvent::PreToolUse,
        cwd.to_string(),
        session_id.to_string(),
        output_mode,
        HookContextData::Tool(ToolData {
            tool: Some(current_tool_name.clone()),
            input: Some(current_params.clone()),
            output: None,
        }),
    );

    let (outcomes, transformed_data) = cfg
        .plugins
        .run(HookEvent::PreToolUse, &hook_ctx)
        .await
        .map_err(|e| e.to_string())?;

    if let HookContextData::Tool(t) = transformed_data {
        if let Some(new_tool) = t.tool {
            current_tool_name = new_tool;
        }
        if let Some(new_input) = t.input {
            current_params = new_input;
        }
    }

    let mut errors = Vec::new();
    for outcome in outcomes {
        match outcome {
            HookOutcome::Error { .. } => errors.push(outcome.format()),
            HookOutcome::Warning { .. } => warnings.push(outcome.format()),
            _ => {}
        }
    }

    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }

    Ok((current_tool_name, current_params, warnings))
}

async fn run_post_tool_hooks(
    session_id: &str,
    cwd: &str,
    tool_name: &str,
    params: Value,
    output: &str,
    output_mode: OutputMode,
) -> (Option<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut new_output = None;

    let Some(cfg) = CONFIG.get() else {
        return (new_output, warnings);
    };

    let output_json =
        serde_json::from_str::<Value>(output).unwrap_or_else(|_| Value::String(output.to_string()));

    let hook_ctx = HookContext::new(
        HookEvent::PostToolUse,
        cwd.to_string(),
        session_id.to_string(),
        output_mode,
        HookContextData::Tool(ToolData {
            tool: Some(tool_name.to_string()),
            input: Some(params),
            output: Some(output_json),
        }),
    );

    match cfg.plugins.run(HookEvent::PostToolUse, &hook_ctx).await {
        Ok((outcomes, transformed_data)) => {
            if let HookContextData::Tool(t) = transformed_data
                && let Some(transformed_output) = t.output
            {
                new_output = Some(match transformed_output {
                    Value::String(s) => s,
                    other => serde_json::to_string(&other).unwrap_or_else(|_| output.to_string()),
                });
            }

            for outcome in outcomes {
                if !matches!(
                    outcome,
                    HookOutcome::Success | HookOutcome::Transformed { .. }
                ) {
                    warnings.push(outcome.format());
                }
            }
        }
        Err(e) => {
            tracing::warn!("tool.post hook failed: {}", e);
            warnings.push(format!(
                "[Hook Error] tool.post infrastructure failure: {e}"
            ));
        }
    }

    (new_output, warnings)
}
