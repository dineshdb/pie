//! Ask an out-of-band approver (ACP client, TUI dialog, …) before a tool
//! changes the user's machine. Two kinds of tool need this and for opposite
//! reasons: `Write`/`Edit` run inside pie, where the shell sandbox cannot see
//! them; `Bash` runs in the sandbox, but the sandbox only draws a boundary
//! (the workspace is writable by design) — it cannot tell `cargo test` from
//! `rm -rf .`. Gating only the file tools left `printf 'x' > f.txt` as a
//! silent way around an approver that thought it was guarding edits.
//!
//! Grants are per tool name, per session: answering "always" for `Bash`
//! allows every later shell command in that session.

use agentsdk::core::agent::PreToolAction;
use agentsdk::core::plugin::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::{mpsc, oneshot};

/// Tools that may not run until the approver says so.
const GATED_TOOLS: [&str; 3] = ["Write", "Edit", "Bash"];

/// Longest tool title handed to the approver; a `Bash` heredoc can carry a
/// whole file, and that belongs in the tool call, not the dialog's title.
const MAX_TITLE: usize = 120;

/// One pending approval: the engine tool-call id (pairs the permission
/// request with the tool-call notification), the tool about to run, a
/// human-readable title (the command, or the target path), and where the
/// decision lands.
#[derive(Debug)]
pub struct GateAsk {
    pub id: String,
    pub tool: String,
    pub title: String,
    pub response_tx: oneshot::Sender<bool>,
}

/// Session-scoped "always allow" answers, keyed by tool name. Shared with
/// the approver, which writes the grants this plugin reads.
pub type ToolGrants = Arc<Mutex<HashSet<String>>>;

/// Gate [`GATED_TOOLS`] behind an external approver.
pub struct ToolGatePlugin {
    ask_tx: mpsc::UnboundedSender<GateAsk>,
    always_allowed: ToolGrants,
}

impl ToolGatePlugin {
    pub fn new(ask_tx: mpsc::UnboundedSender<GateAsk>, always_allowed: ToolGrants) -> Self {
        Self {
            ask_tx,
            always_allowed,
        }
    }

    /// What the approver is being asked to allow, in one line: the shell
    /// command for `Bash`, the target path for the file tools.
    fn ask_title(tool: &str, args: &Value) -> String {
        let detail = match tool {
            "Bash" => args.get("command").and_then(Value::as_str),
            _ => args.get("path").and_then(Value::as_str),
        }
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

        let title = format!("{tool} {detail}");
        match title.char_indices().nth(MAX_TITLE) {
            None => title,
            Some((cut, _)) => format!("{}…", &title[..cut]),
        }
    }

    /// Deny the *tool call*, not the turn: `Abort` returns this text to the
    /// model as the tool's result so it can acknowledge and continue.
    /// (`Stop` would error the whole run — a client rejection must not
    /// surface as an API error.)
    fn denied(tool: &str) -> PreToolAction {
        PreToolAction::Abort(format!("{tool} denied: the user rejected the request."))
    }
}

#[async_trait]
impl AgentPlugin for ToolGatePlugin {
    fn name(&self) -> &'static str {
        "tool-gate"
    }

    /// No hook timeout: the ask blocks on a human answering the client's
    /// permission dialog, which may legitimately take minutes. This plugin
    /// fails closed on its own — a dead approver channel denies (see
    /// `on_tool_pre_execute`), so waiting costs nothing but latency.
    fn pre_execute_timeout(&self) -> Option<std::time::Duration> {
        None
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        tool_name: &str,
        args: &Value,
    ) -> PreToolAction {
        if !GATED_TOOLS.contains(&tool_name) {
            return PreToolAction::Proceed(None);
        }

        if self
            .always_allowed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(tool_name)
        {
            return PreToolAction::Proceed(None);
        }

        let title = Self::ask_title(tool_name, args);
        tracing::info!(tool = tool_name, %title, "requesting tool permission");

        let (response_tx, response_rx) = oneshot::channel();
        let ask = GateAsk {
            id: id.to_string(),
            tool: tool_name.to_string(),
            title,
            response_tx,
        };
        if self.ask_tx.send(ask).is_err() {
            // No approver attached (approver went away): deny — a gate that
            // cannot ask must not silently become a pass-through.
            return Self::denied(tool_name);
        }

        match response_rx.await {
            Ok(true) => PreToolAction::Proceed(None),
            _ => Self::denied(tool_name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gate() -> (ToolGatePlugin, mpsc::UnboundedReceiver<GateAsk>, ToolGrants) {
        let (tx, rx) = mpsc::unbounded_channel();
        let grants: ToolGrants = Arc::new(Mutex::new(HashSet::new()));
        (ToolGatePlugin::new(tx, Arc::clone(&grants)), rx, grants)
    }

    fn ctx() -> PluginContext {
        let mut world = agentsdk::hecs::World::new();
        let entity = world.spawn(());
        PluginContext::new(world, entity)
    }

    #[test]
    fn title_names_the_command_for_bash_and_the_path_for_edits() {
        assert_eq!(
            ToolGatePlugin::ask_title("Bash", &json!({"command": "cargo  test\n --lib"})),
            "Bash cargo test --lib"
        );
        assert_eq!(
            ToolGatePlugin::ask_title("Write", &json!({"path": "/a/b.rs"})),
            "Write /a/b.rs"
        );
        assert_eq!(ToolGatePlugin::ask_title("Write", &json!({})), "Write ");
    }

    #[test]
    fn long_titles_are_truncated_on_a_char_boundary() {
        let title = ToolGatePlugin::ask_title("Bash", &json!({"command": "é".repeat(400)}));
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), MAX_TITLE + 1);
    }

    #[tokio::test]
    async fn ungated_tools_run_without_asking() {
        let (mut gate, mut asks, _) = gate();
        let action = gate
            .on_tool_pre_execute(&mut ctx(), "1", "Read", &json!({"path": "a"}))
            .await;
        assert!(matches!(action, PreToolAction::Proceed(None)));
        assert!(asks.try_recv().is_err(), "Read must not reach the approver");
    }

    #[tokio::test]
    async fn shell_is_gated_like_the_file_tools() {
        for tool in GATED_TOOLS {
            let (mut gate, mut asks, _) = gate();
            let answer = tokio::spawn(async move {
                let ask = asks.recv().await.expect("approver was asked");
                let tool = ask.tool.clone();
                ask.response_tx.send(true).expect("gate still waiting");
                tool
            });
            let action = gate
                .on_tool_pre_execute(
                    &mut ctx(),
                    "1",
                    tool,
                    &json!({"command": "rm -rf x", "path": "x"}),
                )
                .await;
            assert!(matches!(action, PreToolAction::Proceed(None)));
            assert_eq!(answer.await.unwrap(), tool);
        }
    }

    #[tokio::test]
    async fn a_rejected_call_is_aborted_not_stopped() {
        let (mut gate, mut asks, _) = gate();
        tokio::spawn(async move {
            let ask = asks.recv().await.expect("approver was asked");
            ask.response_tx.send(false).expect("gate still waiting");
        });
        let action = gate
            .on_tool_pre_execute(&mut ctx(), "1", "Bash", &json!({"command": "rm -rf /"}))
            .await;
        // Abort feeds the model a tool result; Stop would kill the whole run.
        assert!(matches!(action, PreToolAction::Abort(msg) if msg.contains("Bash denied")));
    }

    #[tokio::test]
    async fn an_always_grant_skips_the_ask_for_that_tool_only() {
        let (mut gate, mut asks, grants) = gate();
        grants.lock().unwrap().insert("Bash".to_string());

        let action = gate
            .on_tool_pre_execute(&mut ctx(), "1", "Bash", &json!({"command": "ls"}))
            .await;
        assert!(matches!(action, PreToolAction::Proceed(None)));
        assert!(asks.try_recv().is_err(), "granted tool must not ask again");

        // The grant is per tool: Write still has to ask.
        tokio::spawn(async move {
            let ask = asks.recv().await.expect("approver was asked");
            ask.response_tx.send(false).expect("gate still waiting");
        });
        let action = gate
            .on_tool_pre_execute(&mut ctx(), "2", "Write", &json!({"path": "a"}))
            .await;
        assert!(matches!(action, PreToolAction::Abort(_)));
    }

    #[tokio::test]
    async fn a_gate_with_no_approver_denies() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // the approver went away mid-session
        let mut gate = ToolGatePlugin::new(tx, Arc::new(Mutex::new(HashSet::new())));
        let action = gate
            .on_tool_pre_execute(&mut ctx(), "1", "Bash", &json!({"command": "ls"}))
            .await;
        assert!(
            matches!(action, PreToolAction::Abort(_)),
            "must fail closed"
        );
    }
}
