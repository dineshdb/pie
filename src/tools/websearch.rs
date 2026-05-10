use agentsdk::core::tools::{Tool, ToolExecute};
use p1e_sandbox::SandboxConfig;
use std::fmt::Write;
use std::sync::Arc;
use url::Url;

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct WebsearchInput {
    /// The search query
    query: String,
    /// Maximum number of results (default: 5)
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    5
}

#[derive(serde::Deserialize)]
struct DdgrResult {
    #[serde(rename = "abstract")]
    description: String,
    title: String,
    url: Url,
}

/// Search the web using `DuckDuckGo` (ddgr) and return results in Markdown.
#[allow(clippy::unwrap_used)]
pub fn websearch(sandbox_settings: Arc<SandboxConfig>) -> Tool {
    Tool::builder()
        .name("websearch")
        .description("Search the web using DuckDuckGo (ddgr) and return results in Markdown format. Use for finding information not in the local context.")
        .input_schema(schemars::schema_for!(WebsearchInput))
        .execute(ToolExecute::from_async(move |_ctx, params| {
            let sandbox_settings = sandbox_settings.clone();
            async move {
                super::emit_tool_input("websearch", &params);

                let input: WebsearchInput = serde_json::from_value(params)
                    .map_err(|e| format!("Invalid input: {e}"))?;

                let limit = if input.limit == 0 { 5 } else { input.limit };
                let escaped_query = input.query
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('$', "\\$")
                    .replace('`', "\\`")
                    .replace('!', "\\!");
                let cmd = format!("ddgr --json -n {limit} \"{escaped_query}\"");

                tracing::debug!(cmd = %cmd, "websearch:");
                let output = p1e_sandbox::build_shell_command(&cmd, &sandbox_settings)
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
                            return Ok("No results found.".to_string());
                        }

                        let mut md = format!("### Web Search Results for: {}\n\n", input.query);
                        for (i, result) in results.iter().enumerate() {
                            let _ = writeln!(md, "{}. [{}]({})", i + 1, result.title, result.url);
                            let _ = writeln!(md, "   {}\n", result.description);
                        }

                        Ok(md)
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "websearch failed");
                        Err(format!("Failed to execute ddgr: {e}"))
                    }
                }
            }
        }))
        .build()
        .unwrap()
}
