//! The ACP connection handlers: session state, prompt turns, permissions.
//!
//! Threading model (see the SDK's ordering chapter): the SDK's dispatch loop
//! runs each handler to completion, so session lifecycle handlers are quick,
//! while `start_prompt_turn` validates, spawns the turn via `cx.spawn`, and
//! returns — a multi-minute LLM turn must not block `session/cancel` or the
//! routing of `session/request_permission` responses.
//!
//! Runs carry their working directory in `AgentConfig.cwd`; the engine never
//! touches the process cwd, so sessions in different workspaces run
//! concurrently in one process. The `cwd` (and any `additionalDirectories`)
//! is granted read+write in the session's sandbox copy — an ACP client
//! explicitly points the agent at that workspace, which is exactly the
//! trust pie's CLI gets from being started inside a project.
//! `deny_read`/`deny_write` still apply on top, so `~/.ssh` and `.env` stay
//! off limits even under a broad root.
//!
//! Known v1 gaps (deliberate):
//! - `session/set_mode` applies from the next prompt, and the model can
//!   still call `switch_mode` itself — a client pin does not lock the mode.
//! - Client-provided `mcpServers` are logged and ignored; pie connects the
//!   servers from its own `[mcp.*]` config.
//! - Skill `permissions:` prompts (frontmatter) are denied rather than
//!   forwarded; only the tool gate talks to the client.

use super::mode_state;
use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, CurrentModeUpdate, EmbeddedResourceResource, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionId, SessionModeId, SessionNotification, SessionUpdate,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo, Error, Responder};
use pie_core::agent::{AgentConfig, AgentEvent, PieAgent};
use pie_core::db::DbPool;
use pie_core::p1e_sandbox::SandboxConfig;
use pie_core::plugin::{AgentMode, GateAsk, ToolGrants};
use pie_core::registry::Registry;
use pie_core::sandbox_grant::granted_sandbox;
use pie_core::session::{Role, Session, SessionId as PieSessionId};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, PoisonError};
use tokio::sync::{mpsc, watch};

/// Everything a connection needs from the pie process.
pub struct AppContext {
    pub pool: Arc<DbPool>,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub provider: pie_core::config::ResolvedProvider,
    pub retry: pie_core::config::RetryConfig,
}

/// Connection-wide state shared by all handlers.
pub struct Shared {
    pub(crate) ctx: AppContext,
    sessions: StdMutex<HashMap<String, AcpSession>>,
}

/// Per-ACP-session state.
struct AcpSession {
    session: Session,
    cwd: PathBuf,
    sandbox: Arc<SandboxConfig>,
    mode: AgentMode,
    busy: Arc<AtomicBool>,
    /// Signalled by `session/cancel`; recreated per turn.
    cancel_tx: Arc<StdMutex<Option<watch::Sender<()>>>>,
    /// Tools the user allowed for the rest of the session ("allow always").
    grants: ToolGrants,
}

impl Shared {
    pub fn new(ctx: AppContext) -> Self {
        Self {
            ctx,
            sessions: StdMutex::new(HashMap::new()),
        }
    }

    fn lock_sessions(&self) -> impl Deref<Target = HashMap<String, AcpSession>> + '_ {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_sessions_mut(&self) -> impl DerefMut<Target = HashMap<String, AcpSession>> + '_ {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

use std::ops::{Deref, DerefMut};

fn invalid_params(message: impl Into<String>) -> Error {
    Error::new(-32602, message.into())
}

fn internal_error(message: impl Into<String>) -> Error {
    // -32603 (JSON-RPC "Internal error"). Deliberately NOT -32000: ACP
    // reserves that code for AuthRequired, and clients (Zed) render it as a
    // login prompt, hiding the actual failure behind an authentication error.
    Error::new(-32603, message.into())
}

fn conflict_error(message: impl Into<String>) -> Error {
    // -32001 is in JSON-RPC's server-defined range; ACP reserves -32000
    // (AuthRequired) and -32002 (ResourceNotFound) around it.
    Error::new(-32001, message.into())
}

// ── session lifecycle ──────────────────────────────────────────────

fn canonical_roots(
    cwd: &PathBuf,
    additional: &[PathBuf],
) -> Result<(PathBuf, Vec<PathBuf>), Error> {
    let cwd_path = std::fs::canonicalize(cwd).map_err(|e| {
        invalid_params(format!(
            "session cwd '{}' is not accessible: {e}",
            cwd.display()
        ))
    })?;
    let mut extra = Vec::with_capacity(additional.len());
    for dir in additional {
        let canonical = std::fs::canonicalize(dir).map_err(|e| {
            invalid_params(format!(
                "additional directory '{}' is not accessible: {e}",
                dir.display()
            ))
        })?;
        extra.push(canonical);
    }
    Ok((cwd_path, extra))
}

fn register_session(
    shared: &Shared,
    session_id: &str,
    session: Session,
    cwd: PathBuf,
    roots: &[PathBuf],
) {
    let acp_session = AcpSession {
        session,
        cwd,
        sandbox: Arc::new(granted_sandbox(&shared.ctx.sandbox, roots)),
        mode: AgentMode::default(),
        busy: Arc::new(AtomicBool::new(false)),
        cancel_tx: Arc::new(StdMutex::new(None)),
        grants: Arc::new(StdMutex::new(HashSet::new())),
    };
    shared
        .lock_sessions_mut()
        .insert(session_id.to_string(), acp_session);
}

pub(crate) async fn new_session(
    shared: &Shared,
    _cx: ConnectionTo<Client>,
    req: NewSessionRequest,
) -> Result<NewSessionResponse, Error> {
    if !req.mcp_servers.is_empty() {
        // TODO: wire client-provided MCP servers through McpPlugin per
        // session; today pie only connects servers from its own `[mcp.*]`.
        tracing::warn!(
            count = req.mcp_servers.len(),
            "acp: client requested MCP servers; pie uses its own [mcp.*] config only"
        );
    }
    let (cwd, extra) = canonical_roots(&req.cwd, &req.additional_directories)?;
    let mut roots = vec![cwd.clone()];
    roots.extend(extra);

    let session = Session::create(shared.ctx.pool.clone(), &cwd)
        .await
        .map_err(|e| internal_error(e.to_string()))?;

    let session_id = session.id.to_string();
    register_session(shared, &session_id, session, cwd, &roots);
    tracing::info!(session = %session_id, cwd = %req.cwd.display(), "acp: session created");

    Ok(NewSessionResponse::new(SessionId::from(session_id))
        .modes(Some(mode_state(AgentMode::default()))))
}

pub(crate) async fn load_session(
    shared: &Shared,
    cx: &ConnectionTo<Client>,
    req: LoadSessionRequest,
) -> Result<LoadSessionResponse, Error> {
    if !req.mcp_servers.is_empty() {
        tracing::warn!(
            count = req.mcp_servers.len(),
            "acp: client requested MCP servers; pie uses its own [mcp.*] config only"
        );
    }
    let (cwd, extra) = canonical_roots(&req.cwd, &req.additional_directories)?;
    let mut roots = vec![cwd.clone()];
    roots.extend(extra);

    let session = Session::load(
        shared.ctx.pool.clone(),
        PieSessionId::from(req.session_id.to_string()),
    )
    .await
    .map_err(|_| invalid_params(format!("unknown session '{}'", req.session_id)))?;

    // Replay before responding: the SDK orders outbound frames by send, so
    // the response must follow every replay notification.
    for entry in session.history_entries() {
        let update = match entry.role() {
            Role::User => SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(entry.content()),
            ))),
            Role::Assistant => SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(entry.content())),
            )),
            Role::System | Role::Tool => continue,
        };
        send_notification(cx, &req.session_id, update);
    }

    let session_id = req.session_id.to_string();
    register_session(shared, &session_id, session, cwd, &roots);
    tracing::info!(session = %session_id, cwd = %req.cwd.display(), "acp: session loaded");

    let mut response = LoadSessionResponse::new();
    response.modes = Some(mode_state(AgentMode::default()));
    Ok(response)
}

pub(crate) fn set_mode(
    shared: &Shared,
    cx: &ConnectionTo<Client>,
    req: &SetSessionModeRequest,
) -> Result<SetSessionModeResponse, Error> {
    let mode: AgentMode = req
        .mode_id
        .to_string()
        .parse()
        .map_err(|_| invalid_params(format!("unknown mode '{}'", req.mode_id)))?;
    {
        let mut sessions = shared.lock_sessions_mut();
        let session = sessions
            .get_mut(req.session_id.0.as_ref())
            .ok_or_else(|| invalid_params(format!("unknown session '{}'", req.session_id)))?;
        // The running turn keeps its start-of-turn mode; `session/set_mode`
        // applies from the next prompt (mid-turn switching would need
        // shared state inside the engine).
        session.mode = mode;
    }

    send_notification(
        cx,
        &req.session_id,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(
            mode.short_name(),
        ))),
    );
    Ok(SetSessionModeResponse::new())
}

pub(crate) fn cancel_turn(shared: &Shared, session_id: &SessionId) {
    let sessions = shared.lock_sessions();
    if let Some(session) = sessions.get(session_id.0.as_ref())
        && let Some(tx) = session
            .cancel_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    {
        let _ = tx.send(());
        tracing::info!(session = %session_id, "acp: turn cancelled");
    }
}

// ── prompt turns ───────────────────────────────────────────────────

/// Flatten the prompt's content blocks to the text pie sends to the model.
/// Text and resource links are baseline; embedded text resources are
/// inlined; binary content is refused (pie advertises no image/audio
/// capabilities — a compliant client won't send it, a sloppy one gets told
/// why it failed).
fn prompt_text(blocks: &[ContentBlock]) -> Result<String, Error> {
    if blocks.is_empty() {
        return Err(invalid_params("empty prompt"));
    }
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("[file: {}] ({})", link.name, link.uri));
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    parts.push(format!(
                        "## Attached: {}\n```\n{}\n```",
                        text.uri, text.text
                    ));
                }
                EmbeddedResourceResource::BlobResourceContents(_) => {
                    return Err(invalid_params(
                        "embedded resource is binary; paste the relevant text instead",
                    ));
                }
                _ => return Err(invalid_params("unsupported embedded resource")),
            },
            _ => return Err(invalid_params("unsupported prompt content block")),
        }
    }
    Ok(parts.join("\n\n"))
}

pub(crate) fn start_prompt_turn(
    shared: &Arc<Shared>,
    cx: &ConnectionTo<Client>,
    req: &PromptRequest,
    responder: Responder<PromptResponse>,
) -> Result<(), Error> {
    let query = prompt_text(&req.prompt)?;
    let session_id = req.session_id.clone();

    let snapshot = {
        let sessions = shared.lock_sessions();
        let Some(session) = sessions.get(session_id.0.as_ref()) else {
            return Err(invalid_params(format!("unknown session '{session_id}'")));
        };
        if session.busy.load(Ordering::SeqCst) {
            return Err(conflict_error(
                "a turn is already in progress for this session",
            ));
        }
        session.busy.store(true, Ordering::SeqCst);
        (
            session.session.clone(),
            session.cwd.clone(),
            session.sandbox.clone(),
            session.mode,
            session.busy.clone(),
            session.cancel_tx.clone(),
            session.grants.clone(),
        )
    };
    let (pie_session, cwd, sandbox, mode, busy, cancel_slot, grants) = snapshot;

    let (cancel_tx, mut cancel_rx) = watch::channel(());
    *cancel_slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(cancel_tx);

    let cancellation = responder.cancellation();
    let task_cx = cx.clone();
    let shared = Arc::clone(shared);
    let task_busy = Arc::clone(&busy);
    let spawn_result = cx.spawn(async move {
        let outcome = run_turn(
            &shared,
            &task_cx,
            &session_id,
            pie_session,
            cwd,
            sandbox,
            mode,
            grants,
            query,
            &mut cancel_rx,
            &cancellation,
        )
        .await;

        task_busy.store(false, Ordering::SeqCst);
        cancel_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();

        let result = match outcome {
            Ok(response) => Ok(response),
            Err(_err) if cancellation.is_cancelled() => {
                // The engine failed *because* the turn was cancelled
                // (dropped LLM request, aborted tool). Report the semantic
                // stop reason, not an error.
                Ok(PromptResponse::new(StopReason::Cancelled))
            }
            Err(err) => Err(err),
        };
        match result {
            Ok(response) => {
                let _ = responder.respond(response);
            }
            Err(err) => {
                tracing::warn!(session = %session_id, "acp: turn failed: {err}");
                let _ = responder.respond_with_error(err);
            }
        }
        Ok(())
    });
    if let Err(e) = spawn_result {
        busy.store(false, Ordering::SeqCst);
        return Err(internal_error(format!("failed to start turn task: {e}")));
    }
    Ok(())
}

/// One full agent run. Forwards engine events as `session/update`
/// notifications, and races the engine future against the cancel signal via
/// the SDK's request-cancellation marker — dropping it aborts the in-flight
/// LLM request. The run carries its own cwd; the process cwd is never ours.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    shared: &Shared,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    pie_session: Session,
    cwd: PathBuf,
    sandbox: Arc<SandboxConfig>,
    mode: AgentMode,
    grants: ToolGrants,
    query: String,
    cancel_rx: &mut watch::Receiver<()>,
    cancellation: &acp::RequestCancellation,
) -> Result<PromptResponse, Error> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (ask_tx, mut ask_rx) = mpsc::unbounded_channel::<GateAsk>();
    let approver_grants = Arc::clone(&grants);

    // Approver task: one ask in flight at a time (the engine is
    // sequential), mapping the client's answer onto the gate. It records
    // "allow always" into the same grant set the gate plugin holds.
    {
        let cx = cx.clone();
        let session_id = session_id.clone();
        let grants = Arc::clone(&approver_grants);
        tokio::spawn(async move {
            while let Some(ask) = ask_rx.recv().await {
                let allowed = request_tool_permission(&cx, &session_id, &ask, &grants).await;
                let _ = ask.response_tx.send(allowed);
            }
        });
    }

    let agent_config = AgentConfig {
        retry: shared.ctx.retry.clone(),
        mode: Some(mode),
        // The client owns the mode selector, so the model gets no
        // switch_mode tool and never sees what this mode forbids.
        mode_switching: false,
        cwd: Some(cwd),
        ..AgentConfig::default()
    };
    let model = shared.ctx.provider.build_client();
    let mut agent = PieAgent::new(
        model,
        shared.ctx.registry.clone(),
        sandbox,
        pie_session,
        agent_config,
    )
    .with_tool_gate((ask_tx, grants));

    // Forward engine events → session/update notifications, for as long as
    // event_tx lives (the turn future owns it; a cancelled turn drops it and
    // this task drains and exits).
    {
        let cx = cx.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                forward_event(&cx, &session_id, event);
            }
        });
    }

    let turn = async move {
        agent
            .stream(&query, event_tx)
            .await
            .map_err(|e| internal_error(format!("{e:#}")))
    };
    // Two cancel paths reach the turn: the SDK's `$/cancel_request` marker
    // (via run_until_cancelled) and ACP v1's session-scoped `session/cancel`
    // (via the watch channel). Either drops the engine future, aborting the
    // in-flight LLM request.
    tokio::select! {
        result = cancellation.run_until_cancelled(turn) => match result {
            Ok(_) => Ok(PromptResponse::new(StopReason::EndTurn)),
            Err(_) if cancellation.is_cancelled() => Ok(PromptResponse::new(StopReason::Cancelled)),
            Err(err) => Err(err),
        },
        _ = cancel_rx.changed() => Ok(PromptResponse::new(StopReason::Cancelled)),
    }
}

fn send_notification(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) {
    if let Err(e) = cx.send_notification(SessionNotification::new(session_id.clone(), update)) {
        tracing::warn!("acp: send session/update failed: {e}");
    }
}

fn forward_event(cx: &ConnectionTo<Client>, session_id: &SessionId, event: AgentEvent) {
    let update = match event {
        AgentEvent::Delta(text) => SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )),
        AgentEvent::UserMessage(_)
        | AgentEvent::Usage { .. }
        | AgentEvent::Done(_)
        // Skill-permission prompts (frontmatter `permissions:`) have no ACP
        // counterpart yet; without a prompt channel they are denied, which
        // is the safe direction. TODO: map onto session/request_permission.
        | AgentEvent::PermissionRequest(_) => return,
        AgentEvent::Error(message) => SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(format!("error: {message}"))),
        )),
        AgentEvent::ToolCall {
            id,
            name,
            display,
            output,
            failed,
        } => {
            if display.is_empty() {
                // Post-execution half: close the call out.
                let status = if failed {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id,
                    ToolCallUpdateFields::new()
                        .status(Some(status))
                        .content(Some(vec![ToolCallContent::Content(
                            acp::schema::v1::Content::new(ContentBlock::Text(TextContent::new(
                                output,
                            ))),
                        )])),
                ))
            } else {
                // Pre-execution half: announce the call.
                SessionUpdate::ToolCall(
                    ToolCall::new(id, display)
                        .kind(tool_kind(&name))
                        .status(ToolCallStatus::Pending),
                )
            }
        }
    };
    send_notification(cx, session_id, update);
}

/// Send `session/request_permission` for one gated tool call and interpret
/// the client's answer. Returns whether the call may proceed.
async fn request_tool_permission(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    ask: &GateAsk,
    grants: &StdMutex<HashSet<String>>,
) -> bool {
    // The gate ask carries the engine's tool-call id, so the client can
    // line the prompt up with the `tool_call` notification it saw.
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(
            ToolCallId::new(ask.id.clone()),
            ToolCallUpdateFields::new()
                .title(Some(ask.title.clone()))
                .kind(Some(tool_kind(&ask.tool)))
                .status(Some(ToolCallStatus::Pending)),
        ),
        permission_options(&ask.tool),
    );

    let response = match cx.send_request(request).block_task().await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!("acp: permission request failed: {e}");
            return false;
        }
    };
    let outcome = match response.outcome {
        RequestPermissionOutcome::Selected(selected) => selected.option_id.to_string(),
        RequestPermissionOutcome::Cancelled => {
            tracing::info!(tool = %ask.tool, "acp: tool permission cancelled");
            return false;
        }
        _ => {
            tracing::info!(tool = %ask.tool, "acp: tool permission denied (unknown outcome)");
            return false;
        }
    };
    let allowed = match outcome.as_str() {
        "allow_once" => Some(false),
        "allow_always" => Some(true),
        _ => None,
    };
    let Some(always) = allowed else {
        tracing::info!(tool = %ask.tool, outcome = %outcome, "acp: tool permission denied");
        return false;
    };
    if always {
        grants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ask.tool.clone());
    }
    tracing::info!(tool = %ask.tool, always, "acp: tool permission granted");
    true
}

/// Map a pie tool name to the ACP kind clients use for icons.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Read" | "Ls" | "Glob" | "Grep" => ToolKind::Read,
        "Write" | "Edit" => ToolKind::Edit,
        "Bash" => ToolKind::Execute,
        "WebSearch" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// "Always" grants the whole tool for the rest of the session, so the label
/// says which tool rather than a bare "Allow always" — approving every future
/// `Bash` is a different promise from approving every future `Write`.
fn permission_options(tool: &str) -> Vec<PermissionOption> {
    vec![
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow_always",
            format!("Always allow {tool} this session"),
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new("reject_once", "Reject", PermissionOptionKind::RejectOnce),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{ByteStreams, ConnectTo as _};
    use pie_core::config::ResolvedProvider;
    use redact::Secret;
    use serde_json::{Value, json};
    use tokio::io::AsyncWriteExt;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream, duplex};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn test_provider() -> ResolvedProvider {
        ResolvedProvider {
            name: "test".into(),
            model: "test-model".into(),
            anthropic_url: None,
            openai_url: "http://127.0.0.1:9/v1".parse().unwrap(),
            api_key: Secret::new("k".into()),
            temperature: None,
        }
    }

    async fn test_ctx() -> AppContext {
        AppContext {
            pool: Arc::new(pie_core::db::create_test_pool().await.unwrap()),
            registry: Arc::new(Registry {
                agents: Vec::new(),
                skills: Vec::new(),
                completions: Vec::new(),
            }),
            sandbox: Arc::new(SandboxConfig::default()),
            provider: test_provider(),
            retry: pie_core::config::RetryConfig::default(),
        }
    }

    /// Run the agent connection against the client side of a duplex pair.
    fn spawn_server(ctx: AppContext) -> DuplexStream {
        let (client, server) = duplex(64 * 1024);
        let (read, write) = tokio::io::split(server);
        let shared = Arc::new(Shared::new(ctx));
        tokio::spawn(async move {
            let transport = ByteStreams::new(write.compat_write(), read.compat());
            let _ = crate::connect(&shared).connect_to(transport).await;
        });
        client
    }

    async fn send(client: &mut DuplexStream, value: &Value) {
        client
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        client.flush().await.unwrap();
    }

    /// Read frames until the response with `id` arrives; the notifications
    /// seen on the way are returned alongside it.
    async fn recv_response(client: &mut DuplexStream, id: i64) -> (Value, Vec<Value>) {
        let mut notifications = Vec::new();
        let mut lines = BufReader::new(client).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("method").is_none() && frame.get("id") == Some(&json!(id)) {
                return (frame, notifications);
            }
            notifications.push(frame);
        }
        panic!("connection closed before response {id}");
    }

    #[tokio::test]
    async fn initialize_responds_with_v1_and_capabilities() {
        let mut client = spawn_server(test_ctx().await);
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": true}},
                "clientInfo": {"name": "test-client"}
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 1).await;
        assert_eq!(response["result"]["protocolVersion"], 1);
        assert_eq!(response["result"]["agentCapabilities"]["loadSession"], true);
        assert_eq!(response["result"]["agentInfo"]["name"], "pie");
        assert_eq!(response["result"]["authMethods"], json!([]));
    }

    #[tokio::test]
    async fn unknown_method_answers_method_not_found() {
        let mut client = spawn_server(test_ctx().await);
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{}}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 2).await;
        assert_eq!(response["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn malformed_line_answers_parse_error_with_null_id() {
        let mut client = spawn_server(test_ctx().await);
        client.write_all(b"not json\n").await.unwrap();
        client.flush().await.unwrap();

        // A malformed frame gets a -32700 whose id is null — the client
        // cannot have supplied one.
        let mut lines = BufReader::new(client).lines();
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("timeout waiting for parse-error frame"))
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["id"], Value::Null);
        assert_eq!(frame["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn session_new_reports_modes_and_bad_cwd_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut client = spawn_server(test_ctx().await);

        // A nonexistent cwd must fail with invalid params, not panic.
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{
                "cwd": "/nonexistent/path/for/pie/acp/test"
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 3).await;
        assert_eq!(response["error"]["code"], -32602);

        let cwd = tmp.path().to_string_lossy().to_string();
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":4,"method":"session/new","params":{
                "cwd": cwd, "mcpServers": []
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 4).await;
        assert!(response["result"]["sessionId"].is_string(), "{response}");
        let modes = &response["result"]["modes"];
        assert_eq!(modes["currentModeId"], "build");
        assert_eq!(modes["availableModes"].as_array().unwrap().len(), 6);
        // The session's cwd is metadata now: the process cwd is never
        // touched, so sessions in different workspaces can run concurrently.
        assert_ne!(std::env::current_dir().unwrap(), tmp.path());
    }

    #[tokio::test]
    async fn session_load_replays_history_and_set_mode_notifies() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let ctx = test_ctx().await;

        // Seed a session directly in the store with history to replay.
        let mut session = Session::create(ctx.pool.clone(), tmp.path()).await.unwrap();
        session.add_user("what is 2+2").await.unwrap();
        session.add_assistant("4").await.unwrap();
        let session_id = session.id.to_string();

        let mut client = spawn_server(ctx);
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":5,"method":"session/load","params":{
                "sessionId": session_id,
                "cwd": cwd, "mcpServers": []
            }}),
        )
        .await;
        let (response, notifications) = recv_response(&mut client, 5).await;
        // The response carries the mode state; the replay rode ahead of it.
        assert_eq!(
            response["result"]["modes"]["currentModeId"], "build",
            "{response}"
        );

        let updates: Vec<&Value> = notifications
            .iter()
            .filter(|n| n["method"] == "session/update")
            .collect();
        assert_eq!(updates.len(), 2, "{notifications:?}");
        assert_eq!(
            updates[0]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            updates[0]["params"]["update"]["content"]["text"],
            "what is 2+2"
        );
        assert_eq!(
            updates[1]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(updates[1]["params"]["update"]["content"]["text"], "4");

        // Unknown session id → invalid params.
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":6,"method":"session/load","params":{
                "sessionId": "nope00", "cwd": cwd, "mcpServers": []
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 6).await;
        assert_eq!(response["error"]["code"], -32602);

        // set_mode: valid → notification + empty result; invalid → -32602.
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":7,"method":"session/set_mode","params":{
                "sessionId": session_id, "modeId": "plan"
            }}),
        )
        .await;
        let (response, notifications) = recv_response(&mut client, 7).await;
        assert_eq!(response["result"], json!({}));
        let mode_update = notifications
            .iter()
            .find(|n| n["params"]["update"]["sessionUpdate"] == "current_mode_update")
            .expect("mode update notification");
        assert_eq!(mode_update["params"]["update"]["currentModeId"], "plan");

        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":8,"method":"session/set_mode","params":{
                "sessionId": session_id, "modeId": "yolo"
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 8).await;
        assert_eq!(response["error"]["code"], -32602);

        // prompt on an unknown session fails before touching the LLM.
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{
                "sessionId": "ghost0",
                "prompt": [{"type": "text", "text": "hi"}]
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 9).await;
        assert_eq!(response["error"]["code"], -32602);
    }

    /// A failed turn must never carry the ACP-reserved AuthRequired code.
    /// -32000 makes clients (Zed) render any failure as "Authentication
    /// required" and hide the real cause behind a login prompt.
    #[tokio::test]
    async fn failed_turn_is_not_mislabeled_as_auth_required() {
        let tmp = tempfile::tempdir().unwrap();

        // The test provider points at a dead port; zero the retries so the
        // prompt fails immediately instead of backing off.
        let mut ctx = test_ctx().await;
        ctx.retry = pie_core::config::RetryConfig {
            api_error: pie_core::config::ApiErrorConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
            rate_limit: pie_core::config::RateLimitConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
        };
        let mut client = spawn_server(ctx);

        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":10,"method":"session/new","params":{
                "cwd": tmp.path().to_string_lossy(), "mcpServers": []
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 10).await;
        let session_id = response["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "hi"}]
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 11).await;
        let code = response["error"]["code"].as_i64().expect("error frame");
        assert_ne!(code, -32000, "auth-reserved code leaked: {response}");
        assert_eq!(code, -32603, "{response}");
    }
}
