use agentsdk::core::tools::{Tool, ToolDefinition, ToolExecute};
use p1e_sandbox::SandboxConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use url::Url;

#[derive(JsonSchema, Deserialize, Serialize)]
struct WebsearchInput {
    /// The search query.
    query: String,
    /// Optional: Maximum number of results (default: 5).
    limit: Option<usize>,
}

#[derive(serde::Deserialize)]
struct DdgrResult {
    #[serde(rename = "abstract")]
    description: String,
    title: String,
    url: Url,
}

/// Search the web using `DuckDuckGo` (ddgr) and return results in Markdown format. Use for finding information not in the local context.
pub fn websearch(sandbox: Arc<SandboxConfig>) -> anyhow::Result<Tool> {
    let schema = schemars::schema_for!(WebsearchInput);

    Ok(Tool::builder()
        .definition(
            ToolDefinition::builder()
                .name("websearch")
                .description("Search the web using DuckDuckGo (ddgr) and return results in Markdown format. Use for finding information not in the local context")
                .input_schema(schema)
                .build()?,
        )
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let sandbox = sandbox.clone();
            async move {
                let input: WebsearchInput =
                    serde_json::from_value(params).map_err(|e| e.to_string())?;

                super::emit_tool_input("websearch", &json!(input));

                let limit = input.limit.unwrap_or(5);
                let limit = if limit == 0 { 5 } else { limit };
                let quoted_query = shell_words::quote(&input.query);
                let cmd = format!("ddgr --json -n {limit} {quoted_query}");

                tracing::debug!(cmd = %cmd, "websearch:");
                let output = p1e_sandbox::build_shell_command(&cmd, &sandbox)
                    .output();

                match output {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !out.status.success() {
                            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                            return Err(format!("ddgr failed with exit code {}: {}", out.status.code().unwrap_or(-1), stderr));
                        }

                        let results: Vec<DdgrResult> = serde_json::from_str(&stdout)
                            .map_err(|e| format!("Failed to parse ddgr output: {e}. Output: {stdout}"))?;

                        if results.is_empty() {
                            return Ok(json!("No results found."));
                        }

                        let mut md = format!("### Web Search Results for: {}\n\n", input.query);
                        for (i, result) in results.iter().enumerate() {
                            let _ = writeln!(md, "{}. [{}]({})", i + 1, result.title, result.url);
                            let _ = writeln!(md, "   {}\n", result.description);
                        }

                        Ok(json!(md))
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "websearch failed");
                        Err(format!("Failed to execute ddgr: {e}"))
                    }
                }
            }
        }))
        .build()?)
}
