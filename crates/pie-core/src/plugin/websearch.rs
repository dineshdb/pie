use agentsdk::PluginTools;
use agentsdk::core::plugin::{AgentPlugin, PluginContext, PluginToolCall};
use agentsdk::core::sandbox::Sandbox;
use agentsdk::core::tools::ToolDefinition;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write;
use std::time::Instant;
use url::Url;

const DUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct WebsearchPlugin {
    recent_queries: HashMap<String, Instant>,
}

impl WebsearchPlugin {
    pub fn new() -> Self {
        Self {
            recent_queries: HashMap::new(),
        }
    }
}

impl Default for WebsearchPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(PluginTools, Serialize, Deserialize)]
enum WebsearchTools {
    /// Gather recent information from the internet. Use only for queries that can't be solved by other local tools.
    /// Use specific variation of the query first. if you don't find relevant answers, go for more generic and broader variation.
    /// User doesn't just want to search. Read few links, relevant pages then synthesize the result.
    /// Using same query would always return same results.
    WebSearch(WebsearchInput),
}

#[async_trait]
impl AgentPlugin for WebsearchPlugin {
    fn name(&self) -> &'static str {
        "web"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        WebsearchTools::definitions()
    }

    async fn run_tool(
        &mut self,
        ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match WebsearchTools::from_call(call)? {
            WebsearchTools::WebSearch(input) => {
                let cutoff = Instant::now()
                    .checked_sub(DUP_WINDOW)
                    .unwrap_or(Instant::now());
                self.recent_queries.retain(|_, at| *at > cutoff);

                if let Some(at) = self.recent_queries.get(&input.query) {
                    return Ok(json!(format!(
                        "Duplicate call detected ({:.0}s ago). The same query won't give a different result. Try a different query or rephrase.",
                        at.elapsed().as_secs_f32()
                    )));
                }

                let limit = input.limit.unwrap_or(5);
                let limit = if limit == 0 { 5 } else { limit };
                let quoted_query = shell_words::quote(&input.query);
                let cmd = format!("ddgr --json -n {limit} {quoted_query}");

                let sandbox = ctx.get::<Sandbox>().ok_or("No sandbox registered")?;
                let out = sandbox.exec(&cmd).await.map_err(|e| e.to_string())?;

                self.recent_queries
                    .insert(input.query.clone(), Instant::now());

                if out.exit_code != 0 {
                    return Err(format!(
                        "ddgr failed with exit code {}: {}",
                        out.exit_code, out.stderr
                    ));
                }

                let results: Vec<DdgrResult> = serde_json::from_str(&out.stdout).map_err(|e| {
                    format!("Failed to parse ddgr output: {e}. Output: {}", out.stdout)
                })?;

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
        }
    }
}

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
