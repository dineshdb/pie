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

    fn on_model_response_completed(&mut self, _ctx: &mut PluginContext, msg: &Message) {
        let rt = tokio::runtime::Handle::current();
        let Message::AssistantMessage(a) = msg else {
            return;
        };

        if let Some(content) = &a.content
            && !content.is_empty()
        {
            let mut session = self.session.clone();
            let _ = rt.block_on(session.add_assistant(content));
        }

        let Some(calls) = &a.tool_calls else {
            return;
        };

        for call in calls {
            let tc = ToolCall {
                call_id: call.id.clone(),
                tool_name: call.function.name.clone(),
                params: serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null),
                output: None,
            };
            let mut session = self.session.clone();
            if let Ok(id) = rt.block_on(session.add_tool_call(&tc)) {
                self.tool_ids.insert(call.id.clone(), id);
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
        agentsdk::core::agent::PostToolAction::Proceed(None)
    }
}
