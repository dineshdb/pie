use agentsdk::{AgentPlugin, PluginContext, PreToolAction};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Default)]
pub struct DeveloperPlugin {
    tool_params: HashMap<String, Value>,
}

impl DeveloperPlugin {
    pub fn new() -> Self {
        Self {
            tool_params: HashMap::new(),
        }
    }

    fn run_py(
        bin_name: &str,
        subcmd: &str,
        input: &Value,
        session_id: &str,
    ) -> std::io::Result<(i32, String, String)> {
        let input_str = serde_json::to_string(input)?;

        let db_path = crate::config::pie_home().join("pie.db");

        let output = Command::new(bin_name)
            .arg(subcmd)
            .env("PIE_INPUT", input_str)
            .env("PIE_SESSION_ID", session_id)
            .env("PIE_DATABASE_PATH", db_path)
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let code = output.status.code().unwrap_or(-1);

        Ok((code, stdout, stderr))
    }
}

#[async_trait]
impl AgentPlugin for DeveloperPlugin {
    fn name(&self) -> &'static str {
        "developer"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &mut PluginContext,
    ) -> Option<Cow<'static, str>> {
        let mut combined = String::new();

        // ground
        if let Ok((0, stdout, _)) = Self::run_py("pie-context", "ground", &json!({}), "default")
            && let Ok(val) = serde_json::from_str::<Value>(&stdout)
            && let Some(sys) = val.get("system").and_then(|s| s.as_str())
        {
            combined.push_str(sys);
        }

        // step-alignment
        if let Ok((0, stdout, _)) =
            Self::run_py("pie-context", "step-alignment", &json!({}), "default")
            && let Ok(val) = serde_json::from_str::<Value>(&stdout)
            && let Some(sys) = val.get("system").and_then(|s| s.as_str())
        {
            combined.push_str(sys);
        }

        if combined.is_empty() {
            None
        } else {
            Some(Cow::Owned(combined))
        }
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        tool_name: &str,
        args: &Value,
    ) -> PreToolAction {
        self.tool_params.insert(id.to_string(), args.clone());

        let input = json!({
            "tool": tool_name,
            "input": args
        });

        // test-first
        if tool_name == "fs__write" || tool_name == "fs__replace" {
            match Self::run_py("pie-guard", "test-first", &input, "default") {
                Ok((code, _, stderr)) if code != 0 => {
                    tracing::warn!("test-first-check failed: {}", stderr);
                }
                _ => {}
            }
        }

        PreToolAction::Proceed(None)
    }
}
