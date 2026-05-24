use crate::session::{Session, ToolCall};
use agentsdk::core::messages::Message;
use agentsdk::core::plugin::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

pub struct PersistencePlugin {
    session: Session,
    tool_ids: HashMap<String, i64>,
}

impl PersistencePlugin {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            tool_ids: HashMap::new(),
        }
    }
}

#[async_trait]
impl AgentPlugin for PersistencePlugin {
    fn name(&self) -> &'static str {
        "persistence"
    }

    async fn on_model_response_completed(&mut self, _ctx: &mut PluginContext, msg: &Message) {
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
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        _name: &str,
        result: &Value,
    ) -> agentsdk::core::agent::PostToolAction {
        if let Some(db_id) = self.tool_ids.remove(id) {
            let _ = self
                .session
                .update_tool_output_by_id(db_id, result.to_string())
                .await;
        }
        agentsdk::core::agent::PostToolAction::Continue(None)
    }
}
