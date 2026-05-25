use crate::session::{Session, ToolCall};
use agentsdk::core::messages::Message;
use agentsdk::core::plugin::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

pub struct PersistencePlugin {
    session: Session,
    pending_tool_calls: HashMap<String, ToolCall>,
}

impl PersistencePlugin {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            pending_tool_calls: HashMap::new(),
        }
    }
}

#[async_trait]
impl AgentPlugin for PersistencePlugin {
    fn name(&self) -> &'static str {
        "persistence"
    }

    fn on_model_response_completed(&mut self, _ctx: &mut PluginContext, msg: &Message) {
        let Message::AssistantMessage(a) = msg else {
            return;
        };

        if let Some(content) = &a.content
            && !content.is_empty()
        {
            let mut session = self.session.clone();
            let content = content.clone();
            tokio::spawn(async move {
                let _ = session.add_assistant(&content).await;
            });
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
            self.pending_tool_calls.insert(call.id.clone(), tc);
        }
    }

    async fn on_tool_post_execute(
        &mut self,
        _ctx: &mut PluginContext,
        id: &str,
        _name: &str,
        result: &Value,
    ) -> agentsdk::core::agent::PostToolAction {
        if let Some(mut tc) = self.pending_tool_calls.remove(id) {
            tc.output = Some(Ok(result.clone()));
            let _ = self.session.add_tool_call(&tc).await;
        }
        agentsdk::core::agent::PostToolAction::Proceed(None)
    }
}
