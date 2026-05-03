use crate::config::CONFIG;
use crate::hook::{HookContext, HookContextData, HookEvent, HookOutcome};
use agentsdk::core::tools::{Tool, ToolExecute};
use serde_json::Value;
use std::sync::Arc;

/// Helper to wrap all tools with hooks.
#[allow(clippy::expect_used)]
pub fn wrap_tools_with_hooks(tools: Vec<Tool>, session_id: &str) -> Vec<Tool> {
    tools
        .into_iter()
        .map(|t| {
            let name = t.name.clone();
            let description = t.description.clone();
            let input_schema = t.input_schema.clone();
            let inner_tool = Arc::new(t);
            let sid = session_id.to_string();

            Tool::builder()
                .name(&name)
                .description(&description)
                .input_schema(input_schema)
                .execute(ToolExecute::from_async(move |ctx, params| {
                    let inner = inner_tool.clone();
                    let session_id = sid.clone();
                    async move {
                        let cwd = std::env::current_dir()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();

                        let (tool_name, params, mut warnings) =
                            run_pre_tool_hooks(&session_id, &cwd, &inner.name, params).await?;

                        // Execute the actual tool
                        let mut result = inner
                            .execute
                            .call(ctx, params.clone())
                            .await
                            .map_err(|e| e.to_string());

                        if let Ok(output) = &result {
                            let (new_output, post_warnings) =
                                run_post_tool_hooks(&session_id, &cwd, &tool_name, params, output)
                                    .await;

                            if let Some(new_output) = new_output {
                                result = Ok(new_output);
                            }
                            warnings.extend(post_warnings);
                        }

                        // Prepend warnings to output if any
                        if let Ok(ref mut output) = result
                            && !warnings.is_empty()
                        {
                            let warnings_str = warnings.join("\n");
                            *output = format!("{warnings_str}\n\n{output}");
                        }

                        result
                    }
                }))
                .build()
                .expect("failed to rebuild tool with hooks")
        })
        .collect()
}

async fn run_pre_tool_hooks(
    session_id: &str,
    cwd: &str,
    tool_name: &str,
    params: Value,
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
        HookContextData::Tool {
            tool: current_tool_name.clone(),
            input: current_params.clone(),
            output: None,
        },
    );

    let (outcomes, transformed_data) = cfg
        .hooks
        .run(HookEvent::PreToolUse, &hook_ctx)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(new_tool) = transformed_data.get("tool").and_then(|v| v.as_str()) {
        current_tool_name = new_tool.to_string();
    }
    if let Some(new_input) = transformed_data.get("input").cloned() {
        current_params = new_input;
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
        HookContextData::Tool {
            tool: tool_name.to_string(),
            input: params,
            output: Some(output_json),
        },
    );

    match cfg.hooks.run(HookEvent::PostToolUse, &hook_ctx).await {
        Ok((outcomes, transformed_data)) => {
            if let Some(transformed_output) = transformed_data.get("output") {
                new_output = Some(match transformed_output {
                    Value::String(s) => s.clone(),
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
