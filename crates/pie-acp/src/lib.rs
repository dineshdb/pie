//! ACP — the Agent Client Protocol (v1), so any ACP client (Zed, …) can run
//! pie as its coding agent. `pie acp` serves JSON-RPC 2.0 over stdio (NDJSON),
//! implemented on the upstream [`agent_client_protocol`] SDK.
//!
//! Inbound: `initialize`, `session/new`, `session/load`, `session/prompt`,
//! `session/set_mode`, plus the `session/cancel` notification. Outbound:
//! `session/update` notifications (message chunks, tool calls) and
//! `session/request_permission` before Write/Edit/Bash run.
//!
//! The SDK's dispatch loop runs each handler to completion before the next
//! inbound message, so the prompt handler validates, spawns the turn via
//! `cx.spawn`, and returns — otherwise a multi-minute LLM turn would block
//! cancellation and permission responses (see the SDK's ordering chapter).
//!
//! Protocol reference: <https://agentclientprotocol.com>.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod server;

use agent_client_protocol as acp;
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder, Stdio};
use pie_core::config::ResolvedConfig;
use pie_core::db::DbPool;
use pie_core::plugin::AgentMode;
use pie_core::registry::Registry;
use std::sync::Arc;

use acp::schema::v1::{
    AgentCapabilities, CancelNotification, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SetSessionModeRequest, SetSessionModeResponse,
};

/// Serve ACP over stdin/stdout until the client closes the connection.
///
/// # Errors
///
/// Errors if the pie config cannot be loaded (for the `[sandbox]` section,
/// which never reaches `handle_command`) or the connection fails on I/O.
pub async fn serve_stdio(
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
    config: &ResolvedConfig,
) -> anyhow::Result<()> {
    let ctx = server::AppContext {
        pool,
        registry,
        sandbox: pie_core::config::build_sandbox(&pie_core::config::load_config()?),
        provider: config.provider.clone(),
        retry: config.retry.clone(),
    };
    let shared = Arc::new(server::Shared::new(ctx));
    connect(&shared)
        .connect_to(Stdio::new())
        .await
        .map_err(|e| anyhow::anyhow!("acp connection failed: {e}"))
}

/// Build the agent side of the connection over any transport — `Stdio` in
/// production, in-memory byte streams in tests. The returned value is a
/// fully-wired `ConnectTo<Client>`: pair it with a transport to run the
/// connection until the peer disconnects.
pub(crate) fn connect(shared: &Arc<server::Shared>) -> impl ConnectTo<Client> {
    Agent
        .builder()
        .name("pie")
        // Negotiation: v1 is the only protocol pie speaks, and per the ACP
        // rules an agent answers with the latest version it supports.
        .on_receive_request(
            async |_req: InitializeRequest,
                   responder: Responder<InitializeResponse>,
                   _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(acp::schema::ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new().load_session(true))
                        .agent_info(Some(Implementation::new("pie", env!("CARGO_PKG_VERSION")))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let shared = Arc::clone(shared);
                async move |req: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    responder.respond(server::new_session(&shared, cx, req).await?)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let shared = Arc::clone(shared);
                async move |req: LoadSessionRequest,
                            responder: Responder<LoadSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    responder.respond(server::load_session(&shared, &cx, req).await?)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let shared = Arc::clone(shared);
                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    server::start_prompt_turn(&shared, &cx, &req, responder)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let shared = Arc::clone(shared);
                async move |req: SetSessionModeRequest,
                            responder: Responder<SetSessionModeResponse>,
                            cx: ConnectionTo<Client>| {
                    responder.respond(server::set_mode(&shared, &cx, &req)?)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let shared = Arc::clone(shared);
                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    server::cancel_turn(&shared, &notif.session_id);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

/// The operating modes pie advertises per session, from the mode system.
pub(crate) fn mode_state(current: AgentMode) -> acp::schema::v1::SessionModeState {
    acp::schema::v1::SessionModeState::new(
        current.short_name(),
        AgentMode::all()
            .iter()
            .map(|mode| {
                acp::schema::v1::SessionMode::new(mode.short_name(), mode.short_name())
                    .description(mode.to_string())
            })
            .collect(),
    )
}
