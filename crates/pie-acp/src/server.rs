//! The ACP server-side loop: serve any [`Engine`] over the Agent Client
//! Protocol. Everything protocol-shaped lives here — handlers, the
//! `session/request_permission` round trip, stop reasons, error codes —
//! while everything agent-shaped lives behind [`SessionSource`] in the
//! assembly (`PieSessions` builds per-session [`Engine`]s from
//! `pie-core`; tests use fakes).
//!
//! Threading model (see the SDK's ordering chapter): the dispatch loop
//! runs each handler to completion, so lifecycle handlers stay quick
//! while the prompt handler validates, spawns the turn via `cx.spawn`,
//! and returns — a multi-minute LLM turn must not block `session/cancel`
//! or the routing of `session/request_permission` responses.
//!
//! ## Mapping
//!
//! | bridge                              | ACP                                                    |
//! |------------------------------------|--------------------------------------------------------|
//! | `Prompt` (via the server loop)     | `session/prompt` (content blocks flattened to text)     |
//! | `Delta`                            | `session/update`: `agent_message_chunk`                |
//! | `Error` (mid-turn, non-fatal)      | `session/update`: `agent_message_chunk` "error: …"     |
//! | `ToolCall` pre-execution half      | `tool_call` (title = `display`, kind from the name, `pending`) |
//! | `ToolCall` post-execution half     | `tool_call_update` (`completed`/`failed` + output text) |
//! | turn end                           | `session/prompt` response: `Completed` → `end_turn`, `Cancelled` → `cancelled`, `Failed` → error `-32603` (never `-32000`: ACP reserves it for `AuthRequired`) |
//! | `Ask`                              | `session/request_permission` (agent → client); `allow_always` settles `true` and grants the tool for the session |
//! | `session/cancel`, `$/cancel_request` | the `TurnIO` cancel signal — drops the engine future  |
//!
//! Deliberate gaps: final text, usage totals, and stop reasons ride the
//! turn's end instead of the event channel (see `pie_core::bridge`), so
//! nothing here forwards them as updates.

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, CurrentModeUpdate,
    EmbeddedResourceResource, Implementation, InitializeRequest, InitializeResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionMode, SessionModeId,
    SessionModeState, SessionNotification, SessionUpdate, SetSessionModeRequest,
    SetSessionModeResponse, StopReason, TextContent, ToolCall, ToolCallContent, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Client, ConnectTo as _, ConnectionTo, Error, Responder};
use pie_core::bridge::{Ask, Engine, Event, TurnEnd, TurnIO};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, PoisonError};
use tokio::sync::{mpsc, watch};

fn lock<T>(lock: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the assembly seam ──────────────────────────────────────────────

/// What the server says about itself in `initialize`.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// One advertised operating mode: an id clients send back in
/// `session/set_mode`, plus the description shown in their UI.
#[derive(Debug, Clone)]
pub struct ModeInfo {
    pub id: String,
    pub description: String,
}

/// The mode catalog a [`SessionSource`] advertises on session responses.
#[derive(Debug, Clone)]
pub struct Modes {
    pub current: String,
    pub available: Vec<ModeInfo>,
}

/// Who wrote a [`ReplayEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayAuthor {
    User,
    Assistant,
}

/// One history entry replayed as message chunks before the
/// `session/load` response.
#[derive(Debug, Clone)]
pub struct ReplayEntry {
    pub author: ReplayAuthor,
    pub text: String,
}

/// A session being opened: fresh in `cwd`, or resuming `resume`'s
/// conversation there (the `session/load` id).
#[derive(Debug, Clone)]
pub struct OpenSession {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub resume: Option<String>,
}

/// Why opening a session failed. `Invalid` answers JSON-RPC `-32602`
/// (the client's fault: unknown session id), `Internal` answers
/// `-32603` (the assembly's fault: storage failures).
#[derive(Debug, Clone)]
pub enum OpenError {
    Invalid(String),
    Internal(String),
}

/// What [`SessionSource::open`] hands the server: the wire session id,
/// the engine driving that conversation, and the history to replay.
#[derive(Debug)]
pub struct OpenedSession<E: Engine> {
    pub id: String,
    pub engine: E,
    pub replay: Vec<ReplayEntry>,
}

/// The assembly seam: how the server opens conversations and steers
/// their modes. Implementations live where the agent lives — `PieSessions`
/// builds pie sessions and `PieEngine`s; tests use fakes.
pub trait SessionSource: Send + Sync + 'static {
    type Engine: Engine;

    /// The modes advertised in `session/new`/`session/load` responses.
    fn modes(&self) -> Modes;

    /// Open one session (create, or resume `open.resume` when set).
    ///
    /// # Errors
    ///
    /// `Invalid` for a bad request (unknown resume id), `Internal` for
    /// assembly-side failures; the server maps them onto JSON-RPC
    /// `-32602`/`-32603` respectively.
    fn open(
        &self,
        open: OpenSession,
    ) -> impl Future<Output = Result<OpenedSession<Self::Engine>, OpenError>> + Send;

    /// Validate `mode_id` for the session and pin it from the next
    /// prompt. Returns the canonical mode id for the
    /// `current_mode_update` notification.
    ///
    /// # Errors
    ///
    /// Fails for an unknown session or an unknown mode (JSON-RPC
    /// `-32602`).
    fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<String, String>;
}

// ── connection state ───────────────────────────────────────────────

/// One session's server-side state: the engine driving it plus the
/// protocol bookkeeping (busy flag, cancel signal, "always allow"
/// grants) that must outlive individual turns.
struct SessionSlot<E: Engine> {
    engine: Arc<E>,
    busy: bool,
    /// Signalled by `session/cancel`; installed per turn.
    cancel_tx: Option<watch::Sender<()>>,
    /// Tools allowed for the rest of the session ("allow always"),
    /// recorded by the permission pump.
    always: Arc<StdMutex<HashSet<String>>>,
}

struct Connection<S: SessionSource> {
    source: Arc<S>,
    info: ServerInfo,
    sessions: StdMutex<HashMap<String, SessionSlot<S::Engine>>>,
}

// ── protocol error shapes ──────────────────────────────────────────

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

// ── the server entry ───────────────────────────────────────────────

/// Serve any [`Engine`] over ACP: `source` opens sessions (and their
/// engines), `info` identifies the agent in `initialize`, and the
/// transport is the SDK's `ConnectTo<Client>` — `Stdio` in production,
/// in-memory byte streams or an `acp::Channel` in tests and in-process
/// hosting. Runs until the client disconnects.
///
/// # Errors
///
/// Fails on transport I/O errors, like the SDK's `connect_to`.
pub async fn serve_acp<S, T>(source: S, info: ServerInfo, transport: T) -> Result<(), Error>
where
    S: SessionSource,
    T: acp::ConnectTo<acp::Agent> + 'static,
{
    let connection = Arc::new(Connection {
        source: Arc::new(source),
        info,
        sessions: StdMutex::new(HashMap::new()),
    });
    build_connection(&connection).connect_to(transport).await
}

/// Register the SDK agent-side handlers on one connection state: the
/// protocol half of the server, kept apart from the entry above. One
/// flat registration table — each method's handler stays next to its
/// siblings on purpose.
#[allow(clippy::too_many_lines)]
fn build_connection<S: SessionSource>(
    connection: &Arc<Connection<S>>,
) -> impl acp::ConnectTo<Client> + use<S> {
    acp::Agent
        .builder()
        .name(connection.info.name.clone())
        // Negotiation: v1 is the only protocol the server speaks, and per
        // the ACP rules an agent answers with the latest version it supports.
        .on_receive_request(
            {
                let connection = Arc::clone(connection);
                async move |_req: InitializeRequest,
                            responder: Responder<InitializeResponse>,
                            _cx: ConnectionTo<Client>| {
                    responder.respond(
                        InitializeResponse::new(acp::schema::ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().load_session(true))
                            .agent_info(Some(Implementation::new(
                                connection.info.name.clone(),
                                connection.info.version.clone(),
                            ))),
                    )
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let connection = Arc::clone(connection);
                async move |req: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            _cx: ConnectionTo<Client>| {
                    warn_mcp_servers(&req.mcp_servers);
                    let (id, _) =
                        open_session(&connection, &req.cwd, &req.additional_directories, None)
                            .await?;
                    responder.respond(
                        NewSessionResponse::new(SessionId::from(id))
                            .modes(Some(mode_state(&connection.source.modes()))),
                    )
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let connection = Arc::clone(connection);
                async move |req: LoadSessionRequest,
                            responder: Responder<LoadSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    warn_mcp_servers(&req.mcp_servers);
                    let (_, replay) = open_session(
                        &connection,
                        &req.cwd,
                        &req.additional_directories,
                        Some(req.session_id.to_string()),
                    )
                    .await?;

                    // Replay before responding: the SDK orders outbound
                    // frames by send, so the response must follow every
                    // replay notification.
                    for entry in replay {
                        let chunk =
                            ContentChunk::new(ContentBlock::Text(TextContent::new(entry.text)));
                        let update = match entry.author {
                            ReplayAuthor::User => SessionUpdate::UserMessageChunk(chunk),
                            ReplayAuthor::Assistant => SessionUpdate::AgentMessageChunk(chunk),
                        };
                        send_notification(&cx, &req.session_id, update);
                    }

                    let mut response = LoadSessionResponse::new();
                    response.modes = Some(mode_state(&connection.source.modes()));
                    responder.respond(response)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let connection = Arc::clone(connection);
                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    start_prompt_turn(&connection, &cx, &req, responder)
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let connection = Arc::clone(connection);
                async move |req: SetSessionModeRequest,
                            responder: Responder<SetSessionModeResponse>,
                            cx: ConnectionTo<Client>| {
                    let canonical = connection
                        .source
                        .set_mode(req.session_id.0.as_ref(), &req.mode_id.to_string())
                        .map_err(invalid_params)?;
                    send_notification(
                        &cx,
                        &req.session_id,
                        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                            SessionModeId::from(canonical),
                        )),
                    );
                    responder.respond(SetSessionModeResponse::new())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let connection = Arc::clone(connection);
                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    cancel_turn(&connection, &notif.session_id);
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
}

// ── session lifecycle ──────────────────────────────────────────────

fn mode_state(modes: &Modes) -> SessionModeState {
    SessionModeState::new(
        SessionModeId::from(modes.current.clone()),
        modes
            .available
            .iter()
            .map(|mode| {
                SessionMode::new(mode.id.clone(), mode.id.clone())
                    .description(mode.description.clone())
            })
            .collect(),
    )
}

/// Client-provided MCP servers are logged and ignored: the assembly
/// connects the servers from its own configuration (pie: `[mcp.*]`).
fn warn_mcp_servers(mcp_servers: &[acp::schema::v1::McpServer]) {
    if !mcp_servers.is_empty() {
        tracing::warn!(
            count = mcp_servers.len(),
            "acp: client requested MCP servers; the agent uses its own configured servers only"
        );
    }
}

fn canonical_roots(cwd: &Path, additional: &[PathBuf]) -> Result<(PathBuf, Vec<PathBuf>), Error> {
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

/// Canonicalize the roots, ask the source for a session, register it,
/// and return its wire id alongside the history to replay.
async fn open_session(
    connection: &Connection<impl SessionSource>,
    cwd: &Path,
    additional: &[PathBuf],
    resume: Option<String>,
) -> Result<(String, Vec<ReplayEntry>), Error> {
    let (cwd, extra) = canonical_roots(cwd, additional)?;
    let opened = connection
        .source
        .open(OpenSession {
            cwd,
            additional_directories: extra,
            resume,
        })
        .await
        .map_err(|e| match e {
            OpenError::Invalid(message) => invalid_params(message),
            OpenError::Internal(message) => internal_error(message),
        })?;
    let id = opened.id;
    lock(&connection.sessions).insert(
        id.clone(),
        SessionSlot {
            engine: Arc::new(opened.engine),
            busy: false,
            cancel_tx: None,
            always: Arc::new(StdMutex::new(HashSet::new())),
        },
    );
    Ok((id, opened.replay))
}

fn cancel_turn<S: SessionSource>(connection: &Connection<S>, session_id: &SessionId) {
    let mut sessions = lock(&connection.sessions);
    if let Some(slot) = sessions.get_mut(session_id.0.as_ref())
        && let Some(tx) = slot.cancel_tx.take()
    {
        let _ = tx.send(());
        tracing::info!(session = %session_id, "acp: turn cancelled");
    }
}

// ── prompt turns ───────────────────────────────────────────────────

/// Flatten the prompt's content blocks to the text sent to the engine.
/// Text and resource links are baseline; embedded text resources are
/// inlined; binary content is refused (no image/audio capabilities are
/// advertised — a compliant client won't send it, a sloppy one gets told
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

/// Validate the prompt, claim the session's turn slot, and spawn the
/// turn off the dispatch loop (see the module's threading model).
fn start_prompt_turn<S: SessionSource>(
    connection: &Arc<Connection<S>>,
    cx: &ConnectionTo<Client>,
    req: &PromptRequest,
    responder: Responder<PromptResponse>,
) -> Result<(), Error> {
    let query = prompt_text(&req.prompt)?;
    let session_id = req.session_id.clone();

    let (engine, always, cancel_rx) = {
        let mut sessions = lock(&connection.sessions);
        let Some(slot) = sessions.get_mut(session_id.0.as_ref()) else {
            return Err(invalid_params(format!("unknown session '{session_id}'")));
        };
        if slot.busy {
            return Err(conflict_error(
                "a turn is already in progress for this session",
            ));
        }
        slot.busy = true;
        let (cancel_tx, cancel_rx) = watch::channel(());
        slot.cancel_tx = Some(cancel_tx);
        (
            Arc::clone(&slot.engine),
            Arc::clone(&slot.always),
            cancel_rx,
        )
    };

    let cancellation = responder.cancellation();
    let task_cx = cx.clone();
    let task_connection = Arc::clone(connection);
    let spawn_result = cx.spawn(drive_turn(
        task_connection,
        engine,
        query,
        task_cx,
        session_id,
        responder,
        cancellation,
        cancel_rx,
        always,
    ));
    if let Err(e) = spawn_result {
        release_turn(connection, &req.session_id);
        return Err(internal_error(format!("failed to start turn task: {e}")));
    }
    Ok(())
}

/// Give the session's turn slot back and drop its cancel sender.
fn release_turn<S: SessionSource>(connection: &Connection<S>, session_id: &SessionId) {
    let mut sessions = lock(&connection.sessions);
    if let Some(slot) = sessions.get_mut(session_id.0.as_ref()) {
        slot.busy = false;
        slot.cancel_tx = None;
    }
}

/// One full turn: run the engine future, forwarding its events as
/// `session/update` notifications, while the permission pump answers the
/// engine's [`Ask`]s through `session/request_permission`. Both cancel
/// paths — ACP's session-scoped `session/cancel` and the SDK's
/// `$/cancel_request` marker — drop the engine future, aborting the
/// in-flight LLM request; every event emitted first is still forwarded,
/// then the prompt response settles the turn.
#[allow(clippy::too_many_arguments)]
async fn drive_turn<S: SessionSource>(
    connection: Arc<Connection<S>>,
    engine: Arc<S::Engine>,
    query: String,
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    responder: Responder<PromptResponse>,
    cancellation: acp::RequestCancellation,
    mut cancel_rx: watch::Receiver<()>,
    always: Arc<StdMutex<HashSet<String>>>,
) -> Result<(), Error> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let (ask_tx, mut ask_rx) = mpsc::unbounded_channel::<Ask>();

    // Permission pump: one ask in flight at a time (the engine is
    // sequential). Runs off the driver loop so a pending ask — which may
    // take minutes to answer — cannot block cancellation; a turn that
    // ends underneath it leaves the pump to drain and exit.
    {
        let cx = cx.clone();
        let session_id = session_id.clone();
        tokio::spawn(async move {
            while let Some(ask) = ask_rx.recv().await {
                let allowed = permission_round_trip(&cx, &session_id, &ask, &always).await;
                // A dropped oneshot just means the turn already ended.
                let _ = ask.response_tx.send(allowed);
            }
        });
    }

    let io = TurnIO {
        events: event_tx,
        asks: ask_tx,
        cancel: cancel_rx.clone(),
    };
    let mut turn = Box::pin(engine.run_turn(query, io));
    let end = loop {
        tokio::select! {
            biased;
            _ = cancel_rx.changed() => break TurnEnd::Cancelled,
            () = cancellation.cancelled() => break TurnEnd::Cancelled,
            Some(event) = event_rx.recv() => forward_update(&cx, &session_id, event),
            end = &mut turn => break end,
        }
    };
    drop(turn);

    // The engine sends its final events before the future resolves, but
    // the biased select may have returned on another branch first — drain
    // what is still queued so no delta is lost and the response follows
    // every notification.
    while let Ok(event) = event_rx.try_recv() {
        forward_update(&cx, &session_id, event);
    }

    release_turn(&connection, &session_id);
    let result = match end {
        TurnEnd::Completed => Ok(PromptResponse::new(StopReason::EndTurn)),
        TurnEnd::Cancelled => Ok(PromptResponse::new(StopReason::Cancelled)),
        TurnEnd::Failed(_) if cancellation.is_cancelled() => {
            // The engine failed *because* the turn was cancelled (dropped
            // LLM request, aborted tool). Report the semantic stop
            // reason, not an error.
            Ok(PromptResponse::new(StopReason::Cancelled))
        }
        TurnEnd::Failed(message) => Err(internal_error(message)),
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
}

/// Send `session/request_permission` for one ask and interpret the
/// client's answer. An "allow always" for a gated tool also grants it
/// for the rest of the session, so later calls skip the round trip.
async fn permission_round_trip(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    ask: &Ask,
    always: &StdMutex<HashSet<String>>,
) -> bool {
    if lock(always).contains(&ask.tool) {
        return true;
    }

    // The ask carries the engine's tool-call id, so the client can line
    // the prompt up with the `tool_call` notification it saw.
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(
            ToolCallId::new(ask.call_id.clone()),
            ToolCallUpdateFields::new()
                .title(Some(ask.title.clone()))
                .kind(Some(tool_kind(&ask.tool)))
                .status(Some(ToolCallStatus::Pending)),
        ),
        tool_options(&ask.tool),
    );
    let outcome = match cx.send_request(request).block_task().await {
        Ok(response) => response.outcome,
        Err(e) => {
            tracing::warn!("acp: permission request failed: {e}");
            return false;
        }
    };
    let selected = match outcome {
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
    let granted_forever = match selected.as_str() {
        "allow_once" => Some(false),
        "allow_always" => Some(true),
        _ => None,
    };
    let Some(forever) = granted_forever else {
        tracing::info!(tool = %ask.tool, outcome = %selected, "acp: tool permission denied");
        return false;
    };
    if forever {
        lock(always).insert(ask.tool.clone());
    }
    tracing::info!(tool = %ask.tool, always = forever, "acp: tool permission granted");
    true
}

/// Map a tool name to the ACP kind clients use for icons.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Read" | "Ls" | "Glob" | "Grep" => ToolKind::Read,
        "Write" | "Edit" => ToolKind::Edit,
        "Bash" => ToolKind::Execute,
        "WebSearch" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// "Always" grants the whole tool for the rest of the session, so the
/// label says which tool rather than a bare "Allow always" — approving
/// every future `Bash` is a different promise from approving every
/// future `Write`.
fn tool_options(tool: &str) -> Vec<PermissionOption> {
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

// ── event forwarding ───────────────────────────────────────────────

fn send_notification(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) {
    if let Err(e) = cx.send_notification(SessionNotification::new(session_id.clone(), update)) {
        tracing::warn!("acp: send session/update failed: {e}");
    }
}

/// Translate one bridge event into a `session/update` notification —
/// the forwarding direction of the mapping table in this module's docs.
fn forward_update(cx: &ConnectionTo<Client>, session_id: &SessionId, event: Event) {
    let update = match event {
        Event::Delta(text) => SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )),
        Event::Error(message) => SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(format!("error: {message}"))),
        )),
        Event::ToolCall {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kinds_map_for_icons() {
        assert_eq!(tool_kind("Read"), ToolKind::Read);
        assert_eq!(tool_kind("Bash"), ToolKind::Execute);
        assert_eq!(tool_kind("Write"), ToolKind::Edit);
        assert_eq!(tool_kind("WebSearch"), ToolKind::Search);
        assert_eq!(tool_kind("mcp__thing"), ToolKind::Other);
    }

    #[test]
    fn always_option_names_the_tool() {
        let options = tool_options("Bash");
        assert_eq!(options.len(), 3);
        assert_eq!(options[1].name, "Always allow Bash this session");
    }
}
