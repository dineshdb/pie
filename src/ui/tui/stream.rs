use crate::agent::{AgentConfig, AgentEvent, OutputMode, PieAgent};
use crate::session::{Session, SessionId};
use crate::ui::tui::components::input::InputComponent;
use crate::ui::tui::realm::StreamEvent;
use p1e_sandbox::SandboxConfig;
use std::sync::Arc;
use tokio::sync::mpsc::{self, UnboundedSender};

/// Environment shared across stream invocations — held by [`InputComponent`].
#[derive(Clone)]
pub struct StreamContext {
    pub model: agentsdk::OpenAI,
    pub sandbox: Arc<SandboxConfig>,
    pub session_id: SessionId,
    pub pool: Arc<crate::db::DbPool>,
    pub max_steps: u32,
    pub registry: Arc<crate::registry::Registry>,
}

impl From<&InputComponent> for StreamContext {
    fn from(input: &InputComponent) -> Self {
        Self {
            model: input.model.clone(),
            sandbox: input.sandbox_settings.clone(),
            session_id: input.session_id.clone(),
            pool: input.session_pool.clone(),
            max_steps: input.max_steps,
            registry: input.registry.clone(),
        }
    }
}

pub async fn spawn_stream(
    ctx: StreamContext,
    query: String,
    event_tx: UnboundedSender<StreamEvent>,
    mut abort_rx: mpsc::UnboundedReceiver<()>,
) {
    let session = match Session::load(ctx.pool.clone(), ctx.session_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to load session: {e}");
            let _ = event_tx.send(StreamEvent::Error(e.to_string()));
            return;
        }
    };

    let config = AgentConfig {
        max_steps: ctx.max_steps,
        ..Default::default()
    };

    let mut agent = PieAgent::new(
        ctx.model,
        ctx.registry,
        ctx.sandbox,
        ctx.pool,
        session,
        config,
    );

    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let event_tx_clone = event_tx.clone();

    tokio::spawn(async move {
        while let Some(event) = agent_rx.recv().await {
            match event {
                AgentEvent::Delta(d) => {
                    let _ = event_tx_clone.send(StreamEvent::Delta(d));
                }
                AgentEvent::Done(d) => {
                    let _ = event_tx_clone.send(StreamEvent::Done(d));
                }
                AgentEvent::Error(e) => {
                    let _ = event_tx_clone.send(StreamEvent::Error(e));
                }
                AgentEvent::ToolCall {
                    name,
                    display,
                    output,
                } => {
                    let _ = event_tx_clone.send(StreamEvent::ToolCall {
                        name,
                        display,
                        output,
                    });
                }
                AgentEvent::PlanUpdate => {
                    let _ = event_tx_clone.send(StreamEvent::PlanUpdate);
                }
            }
        }
    });

    tokio::select! {
        res = agent.stream(&query, OutputMode::Interactive, agent_tx) => {
            if let Err(e) = res {
                let _ = event_tx.send(StreamEvent::Error(e.to_string()));
            }
        }
        _ = abort_rx.recv() => {
            tracing::info!("stream cancelled");
            let _ = event_tx.send(StreamEvent::Error("Cancelled".to_string()));
        }
    }
}
