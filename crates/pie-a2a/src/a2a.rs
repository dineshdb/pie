//! A2A — the `Agent2Agent` protocol (v1.0), so agent hosts (Citadel) can
//! delegate work to pie: agent-to-agent, push-not-poll.
//!
//! Wire contract (JSON-RPC 2.0 over HTTP on `POST /a2a`; the Agent Card at
//! `GET /.well-known/agent-card.json` points here):
//!
//! - `SendStreamingMessage` — the primary path. One POST whose response is
//!   an SSE stream (`text/event-stream`): a `Task` frame first (`WORKING`),
//!   then `artifactUpdate` per token delta and `statusUpdate` per tool-call
//!   display line, ending with a `statusUpdate` whose `final` flag is set.
//!   Frames flow from the first millisecond, so no proxy idle timer ever
//!   fires.
//! - `SendMessage` — blocking fallback: the POST stays open until the turn
//!   ends and answers with the task snapshot (`configuration.
//!   returnImmediately: true` returns the `WORKING` snapshot at once).
//! - `SubscribeToTask` — reattach to a running task's live feed after a
//!   dropped connection. Events before the reattach are not replayed (the
//!   spec defers replay/resumability to v2). Subscribing to a resolved
//!   task answers `UnsupportedOperationError` — there is nothing to
//!   stream; read it with `GetTask`.
//! - `GetTask` / `CancelTask` / `ListTasks` — inspect, abort, enumerate.
//!   `GetTask` and `ListTasks` take `historyLength` (0 omits history,
//!   N returns the N most recent); `ListTasks` filters by `contextId`,
//!   `status` and `statusTimestampAfter`, pages with an opaque
//!   `pageToken` (newest activity first), and includes artifacts only
//!   when `includeArtifacts` is set.
//! - `GetExtendedAgentCard` — the authenticated card: the public card
//!   plus one skill per agent persona registered on the daemon
//!   (workspace `.pie/agents`, global `~/.pie/agents`).
//!
//! **History and artifacts** — a task is a conversation, so its `history`
//! is the pie session's user/agent transcript (user prompts as
//! `ROLE_USER`, answers as `ROLE_AGENT`; tool/system entries stay
//! internal), and each finished turn appends its answer as one artifact.
//! `messageId` on an inbound message is honored: a redelivery replays the
//! task it produced instead of starting another turn.
//!
//! **Push notifications** — the callback channel for clients that cannot
//! hold a stream. Pass `pushNotificationConfig` (url, optional `token`,
//! optional `authentication`) inside `SendMessageConfiguration`, or attach
//! one to a running task with `CreateTaskPushNotificationConfig`; the
//! daemon then POSTs a `StreamResponse` frame to the webhook on every
//! status *transition* — turn start, and the final snapshot with the full
//! answer artifact — so the client never needs a follow-up `GetTask`.
//! Delivery echoes the client's `token` in `x-a2a-notification-token`
//! (v1.0 names no header; this is our choice), adds an `Authorization:
//! Bearer` header when `authentication.scheme` is `bearer`, and retries
//! non-2xx deliveries up to three times with exponential backoff. Token
//! deltas are never pushed — that is what the stream is for.
//!
//! Task identity (see the spec's "life of a task"): a task is a whole
//! conversation, and a resolved task is immutable. One pie session is one
//! conversation, so the canonical `taskId` *is* the pie session id, and
//! `contextId` is always the session id — the logical group the task
//! belongs to. Between turns the task sits in `INPUT_REQUIRED` (the agent
//! awaits input); a follow-up message with the same `taskId` starts the
//! next turn. `TASK_STATE_COMPLETED` is never emitted: a delegation chat
//! stays open until it fails or is canceled.
//!
//! Terminal states are final. A `FAILED` or `CANCELED` task instance is
//! remembered (in memory, for the daemon's lifetime) and can never be
//! resumed — any reference to it answers `TaskNotFound`. Continuing work
//! after a failure is a NEW task in the same context: send a message
//! without a `taskId` but with `contextId` set to the session id; the
//! server opens a fresh task instance (`<sessionId>~<suffix>`, still
//! opaque to clients) on the same conversation.
//!
//! Durability: the HTTP transport is stateless (any request works against
//! any daemon instance; no session affinity), but task *state* is durable.
//! Every task is persisted in SQLite (see `store`) — its state, answer
//! artifact and usage survive restarts, resolved tasks stay retrievable
//! via `GetTask`, and push notification configs keep firing for later
//! turns. Only the running turn itself lives in memory; a daemon restart
//! reports interrupted turns as failed at startup, and clients resume by
//! sending the next message with the same `taskId` (or a fresh task with
//! the same `contextId` after a resolved task).
//!
//! Delegated runs serve at depth 1 (the recursion guard: `main_agent_only`
//! servers are withheld) with no interactive tool gate — the sandbox is the
//! boundary, same posture as the MCP door. One in-flight turn per session
//! is enforced by the shared [`pie_core::turn_gate::TurnGate`].
//!
//! Documented deviations from the v1.0 spec: only `A2A-Version: 1.0` is
//! accepted (an empty header is rejected rather than read as `0.3`);
//! `TASK_STATE_COMPLETED` is never produced (a delegation chat stays open
//! until it fails or is canceled); batch requests are rejected with
//! `-32600` because v1.0 defines no batch transport; cancelling a task
//! with no running turn answers `TaskNotCancelable` because an idle
//! conversation is left open by design.

use crate::AppContext;
use crate::store;
use bytes::Bytes;
use chrono::Utc;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::{HeaderMap, Request, Response};
use pie_core::session::{Role, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::StreamExt as _;

type HttpBody = BoxBody<Bytes, Infallible>;

const A2A_VERSION: &str = "1.0";
const CARD_PATH: &str = "/.well-known/agent-card.json";
const RPC_PATH: &str = "/a2a";
const SSE_KEEPALIVE: Duration = Duration::from_secs(30);

// ── JSON-RPC envelope ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A JSON-RPC error destined for the response envelope.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    fn parse(message: impl Into<String>) -> Self {
        Self::new(-32700, message)
    }
    fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }
    fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }
    fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }
    fn task_not_found(id: &str) -> Self {
        Self::new(-32001, format!("task '{id}' not found"))
    }
    fn not_cancelable(id: &str) -> Self {
        Self::new(-32002, format!("task '{id}' has no running turn to cancel"))
    }
    fn unsupported_operation(message: impl Into<String>) -> Self {
        Self::new(-32004, message)
    }
    fn version() -> Self {
        Self::new(
            -32009,
            format!("unsupported A2A-Version; this server speaks {A2A_VERSION}"),
        )
    }
}

fn rpc_result(id: &Value, result: &impl Serialize) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, error: &RpcError) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": error.code, "message": error.message },
    })
}

// ── inbound wire types ───────────────────────────────────────────────

/// A content part. Only text (and JSON data, stringified) makes sense for
/// a coding-agent prompt; raw/url parts fail parsing with a clear error.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Part {
    Text { text: String },
    Data { data: Value },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InboundMessage {
    parts: Vec<Part>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    context_id: Option<String>,
    /// Client-side unique id: a redelivery replays the task it produced
    /// instead of starting another turn (v1.0 §3.3.1).
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Map<String, Value>>,
    // role is accepted and ignored: a delegation prompt's role is fixed.
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SendConfiguration {
    #[serde(default)]
    return_immediately: bool,
    /// How many transcript messages to include as the response task's
    /// `history` (0 omits it; absent — none on send responses).
    #[serde(default)]
    history_length: Option<u32>,
    /// Register a webhook for this task's updates — the callback channel:
    /// the daemon POSTs a `StreamResponse` frame (same shape as the SSE
    /// frames) to `url` on every status transition, with the full Task —
    /// answer artifact included — on the final one.
    #[serde(default)]
    push_notification_config: Option<PushConfigIn>,
}

/// How the daemon authenticates itself to the webhook.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushAuth {
    scheme: String,
    #[serde(default)]
    credentials: Option<String>,
}

/// Inbound `TaskPushNotificationConfig`: where to POST task updates.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushConfigIn {
    url: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    authentication: Option<PushAuth>,
}

/// The validated, resolved delivery target.
#[derive(Debug, Clone)]
struct PushTarget {
    url: String,
    token: Option<String>,
    /// (scheme, credentials) — `bearer` becomes an Authorization header.
    auth: Option<(String, Option<String>)>,
}

impl PushTarget {
    fn try_from_config(cfg: &PushConfigIn) -> Result<Self, RpcError> {
        if !(cfg.url.starts_with("http://") || cfg.url.starts_with("https://")) {
            return Err(RpcError::invalid_params(
                "push notification url must be http(s)",
            ));
        }
        Ok(Self {
            url: cfg.url.clone(),
            token: cfg.token.clone(),
            auth: cfg
                .authentication
                .as_ref()
                .map(|a| (a.scheme.clone(), a.credentials.clone())),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageSendParams {
    message: InboundMessage,
    #[serde(default)]
    configuration: SendConfiguration,
}

#[derive(Debug, Deserialize)]
struct TaskRef {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatePushParams {
    task_id: String,
    push_notification_config: PushConfigIn,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetTaskParams {
    id: String,
    /// How many transcript messages to return as the task's `history`
    /// (absent — all; 0 — none).
    #[serde(default)]
    history_length: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListTasksParams {
    #[serde(default)]
    context_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    history_length: Option<u32>,
    #[serde(default)]
    page_size: Option<u32>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    status_timestamp_after: Option<String>,
    #[serde(default)]
    include_artifacts: bool,
}

/// `ListTasks` pagination bounds. Artifacts are only fetched for a page
/// when asked for; history only when `historyLength` is set (the default
/// list response stays lean).
const LIST_PAGE_DEFAULT: u32 = 50;
const LIST_PAGE_MAX: u32 = 100;

fn push_config_reply(task_id: &str, target: &PushTarget) -> Value {
    let mut config = serde_json::Map::new();
    config.insert("url".into(), serde_json::json!(target.url));
    if let Some(token) = &target.token {
        config.insert("token".into(), serde_json::json!(token));
    }
    if let Some((scheme, credentials)) = &target.auth {
        let mut auth = serde_json::Map::new();
        auth.insert("scheme".into(), serde_json::json!(scheme));
        if let Some(credentials) = credentials {
            auth.insert("credentials".into(), serde_json::json!(credentials));
        }
        config.insert("authentication".into(), Value::Object(auth));
    }
    serde_json::json!({ "taskId": task_id, "pushNotificationConfig": Value::Object(config) })
}

// ── outbound wire types ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum TaskState {
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    /// Between turns: the conversation is open and awaiting input.
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
}

impl TaskState {
    /// `TASK_STATE_COMPLETED` is deliberately absent: a delegation chat
    /// stays open until it fails or is canceled (see the module docs).
    fn resolved(self) -> bool {
        matches!(self, Self::Failed | Self::Canceled)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Working => "TASK_STATE_WORKING",
            Self::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Canceled => "TASK_STATE_CANCELED",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "TASK_STATE_WORKING" => Some(Self::Working),
            "TASK_STATE_INPUT_REQUIRED" => Some(Self::InputRequired),
            "TASK_STATE_FAILED" => Some(Self::Failed),
            "TASK_STATE_CANCELED" => Some(Self::Canceled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct WireMessage {
    role: &'static str,
    parts: Vec<WirePart>,
    #[serde(rename = "messageId")]
    message_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum WirePart {
    Text { text: String },
}

impl WireMessage {
    fn for_role(role: &'static str, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![WirePart::Text { text: text.into() }],
            message_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn text(text: impl Into<String>) -> Self {
        Self::for_role("ROLE_AGENT", text)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskStatus {
    state: TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<WireMessage>,
    timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    #[serde(rename = "artifactId")]
    id: String,
    name: &'static str,
    parts: Vec<WirePart>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    append: bool,
    #[serde(rename = "lastChunk", skip_serializing_if = "std::ops::Not::not")]
    last_chunk: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    id: String,
    context_id: String,
    status: TaskStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    artifacts: Vec<Artifact>,
    /// The conversation transcript so far, when the caller asked for it
    /// (`historyLength`).
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<Vec<WireMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskStatusUpdateEvent {
    task_id: String,
    context_id: String,
    status: TaskStatus,
    #[serde(rename = "final")]
    is_final: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskArtifactUpdateEvent {
    task_id: String,
    context_id: String,
    artifact: Artifact,
}

/// One SSE result frame's payload. Externally tagged on purpose: v1.0
/// wraps each event as `{"task": …}` / `{"statusUpdate": …}` /
/// `{"artifactUpdate": …}`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum StreamResponse {
    Task(Task),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

// ── agent card ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentInterface {
    url: String,
    protocol_binding: &'static str,
    protocol_version: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentCard {
    name: &'static str,
    description: &'static str,
    version: &'static str,
    protocol_version: &'static str,
    supported_interfaces: Vec<AgentInterface>,
    capabilities: Value,
    default_input_modes: Vec<&'static str>,
    default_output_modes: Vec<&'static str>,
    skills: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    security_schemes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    security_requirements: Option<Value>,
}

fn agent_card(interface_url: String, auth_required: bool) -> AgentCard {
    const DELEGATION_GUIDE: &str = "pie is a coding agent running on this machine, with \
     sandboxed file, shell and git access. Delegate work here instead of answering from memory \
     whenever the user asks about, explores, or wants changes in a local repository or \
     workspace. Send the request as a text message with metadata.cwd set to the repo's \
     absolute path; the streamed task's final artifact is the agent's answer. Continue the \
     conversation by sending another message with the same taskId; after a failure, open a \
     new task with contextId set to the old one.";
    AgentCard {
        name: "pie",
        description: DELEGATION_GUIDE,
        version: env!("CARGO_PKG_VERSION"),
        protocol_version: A2A_VERSION,
        supported_interfaces: vec![AgentInterface {
            url: interface_url,
            protocol_binding: "JSONRPC",
            protocol_version: A2A_VERSION,
        }],
        capabilities: serde_json::json!({
            "streaming": true,
            "pushNotifications": true,
            "extendedAgentCard": true,
        }),
        default_input_modes: vec!["text/plain"],
        default_output_modes: vec!["text/plain"],
        skills: vec![serde_json::json!({
            "id": "pie-agent",
            "name": "pie",
            "description": "Delegate a coding/agent task on this machine to a pie session.",
            "tags": ["coding", "agent", "workspace"],
        })],
        security_schemes: auth_required
            .then(|| serde_json::json!({ "bearerAuth": { "type": "http", "scheme": "bearer" } })),
        security_requirements: auth_required.then(|| serde_json::json!([{ "bearerAuth": [] }])),
    }
}

// ── task registry (running turns + resolved markers) ─────────────────

/// An event a subscriber receives over SSE.
#[derive(Debug, Clone)]
enum Event {
    Status(TaskStatusUpdateEvent),
    Artifact(TaskArtifactUpdateEvent),
}

struct LiveTask {
    task_id: String,
    /// The conversation this task belongs to — always the pie session id.
    context_id: String,
    running: AtomicBool,
    /// Set once the turn ends: `InputRequired` after an answer, a
    /// resolved state after failure/cancellation.
    final_state: StdMutex<Option<TaskState>>,
    /// The answer text accumulated so far (token deltas append here).
    response: StdMutex<String>,
    /// The latest non-terminal status line (tool activity, errors).
    status_message: StdMutex<Option<String>>,
    /// Usage metadata, set once the turn completes.
    completion: StdMutex<Option<serde_json::Map<String, Value>>>,
    artifact_id: String,
    /// Which turn of the conversation this is (1 = first); the artifact a
    /// finished turn leaves behind is keyed by it.
    turn: u32,
    events: broadcast::Sender<Event>,
    cancel: StdMutex<Option<watch::Sender<()>>>,
    /// Bumped once per finished turn; blocking `SendMessage` callers wait
    /// on this instead of on task state (an idle task never terminates).
    turn_epoch: watch::Sender<u64>,
    /// Where push notifications go, when the client registered a webhook.
    push: StdMutex<Option<PushTarget>>,
}

impl LiveTask {
    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn state(&self) -> TaskState {
        if self.running() {
            TaskState::Working
        } else {
            self.final_state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .unwrap_or(TaskState::InputRequired)
        }
    }

    fn status(&self) -> TaskStatus {
        let message = self
            .status_message
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .map(WireMessage::text);
        TaskStatus {
            state: self.state(),
            message,
            timestamp: Utc::now().to_rfc3339(),
        }
    }

    /// The task snapshot a client sees. A finished turn's answer is the
    /// full artifact; a running turn streams deltas instead (the spec
    /// defers replay to v2), so live snapshots omit artifacts. Prior
    /// turns' artifacts live in the store — snapshots here only carry the
    /// in-memory turn.
    fn snapshot(&self) -> Task {
        self.snapshot_with(Vec::new())
    }

    fn snapshot_with(&self, history: Vec<WireMessage>) -> Task {
        let response = self.response.lock().unwrap_or_else(PoisonError::into_inner);
        let status_message = self
            .status_message
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let completion = self
            .completion
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .map(Value::Object);
        let artifacts = (self.state() != TaskState::Working && !response.is_empty())
            .then(|| Artifact {
                id: self.artifact_id.clone(),
                name: "response",
                parts: vec![WirePart::Text {
                    text: response.clone(),
                }],
                append: false,
                last_chunk: true,
            })
            .into_iter()
            .collect();
        build_task(
            &self.task_id,
            &self.context_id,
            self.state(),
            status_message.as_deref(),
            artifacts,
            completion.as_ref(),
            history,
        )
    }

    fn status_event(&self, is_final: bool) -> TaskStatusUpdateEvent {
        TaskStatusUpdateEvent {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
            status: self.status(),
            is_final,
        }
    }

    fn artifact_event(&self, text: &str, chunk: bool) -> TaskArtifactUpdateEvent {
        TaskArtifactUpdateEvent {
            task_id: self.task_id.clone(),
            context_id: self.context_id.clone(),
            artifact: Artifact {
                id: self.artifact_id.clone(),
                name: "response",
                parts: vec![WirePart::Text {
                    text: text.to_owned(),
                }],
                append: chunk,
                last_chunk: !chunk,
            },
        }
    }
}

/// The in-flight turns only. Task *state* is durable ([`TaskStore`]);
/// this map is just the running engine futures.
#[derive(Default)]
struct LiveTurns {
    running: StdMutex<HashMap<String, Arc<LiveTask>>>,
}

impl LiveTurns {
    fn insert(&self, live: Arc<LiveTask>) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(live.task_id.clone(), live);
    }

    fn get(&self, id: &str) -> Option<Arc<LiveTask>> {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    fn remove(&self, id: &str) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }
}

// ── the A2A handler ──────────────────────────────────────────────────

/// The A2A door into the engine: one pool, one registry cache, one turn
/// gate — the same [`AppContext`] the rest of the daemon runs on.
#[derive(Clone)]
pub(crate) struct A2a {
    ctx: Arc<AppContext>,
    turns: Arc<LiveTurns>,
    store: store::TaskStore,
    /// Whether the daemon requires a bearer token — surfaced on the card.
    auth_required: bool,
    /// Outbound deliveries for push notifications (webhook callbacks).
    http: reqwest::Client,
}

impl A2a {
    pub(crate) fn new(ctx: Arc<AppContext>, auth_required: bool) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            store: store::TaskStore::new(Arc::clone(&ctx.pool)),
            ctx,
            turns: Arc::new(LiveTurns::default()),
            auth_required,
            http,
        }
    }

    /// Register a resolved, reserved turn as live: dedupe the delivery,
    /// wire push notifications, publish the snapshot and spawn the driver.
    async fn spawn_turn(&self, seed: TurnSeed) -> Result<TurnStart, RpcError> {
        // Remember the delivery only once the task is known: from here on
        // a redelivery replays this task instead of racing a new turn.
        if let Some(message_id) = &seed.message_id {
            let fresh = self
                .store
                .remember_message(message_id, &seed.task_id)
                .await
                .map_err(RpcError::internal)?;
            if !fresh {
                return Ok(TurnStart::Replay(
                    self.current_snapshot(&seed.task_id).await?,
                ));
            }
        }

        let (events, _) = broadcast::channel(256);
        let (turn_epoch, _) = watch::channel(0u64);
        let (cancel_tx, cancel_rx) = watch::channel(());
        let live = Arc::new(LiveTask {
            task_id: seed.task_id.clone(),
            context_id: seed.context_id.clone(),
            running: AtomicBool::new(true),
            final_state: StdMutex::new(None),
            response: StdMutex::new(String::new()),
            status_message: StdMutex::new(None),
            completion: StdMutex::new(None),
            artifact_id: seed.artifact_id,
            turn: seed.turn,
            events,
            cancel: StdMutex::new(Some(cancel_tx)),
            turn_epoch,
            push: StdMutex::new(None),
        });
        self.wire_push(&seed.task_id, seed.push_config.as_ref(), &live)
            .await?;
        self.turns.insert(Arc::clone(&live));

        let initial = live.snapshot();
        tokio::spawn(drive_turn(
            Arc::clone(&self.ctx),
            Arc::clone(&self.turns),
            self.store.clone(),
            Arc::clone(&live),
            seed.session,
            seed.agent_name,
            seed.prompt,
            seed.guard,
            cancel_rx,
        ));

        Ok(TurnStart::New(TurnHandle {
            task: initial,
            live,
        }))
    }

    /// Serve one request on the A2A surface (Agent Card or RPC).
    pub(crate) async fn handle<B>(&self, req: Request<B>) -> Response<HttpBody>
    where
        B: hyper::body::Body<Data = Bytes> + Unpin + Send + 'static,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        match req.uri().path() {
            CARD_PATH => self.card_response(&req),
            RPC_PATH => self.rpc(req).await,
            _ => json_response(&rpc_error(
                &Value::Null,
                &RpcError::invalid_request(format!(
                    "unknown A2A path; the card is at {CARD_PATH} and RPC at {RPC_PATH}"
                )),
            )),
        }
    }

    fn card_response<B>(&self, req: &Request<B>) -> Response<HttpBody> {
        if req.method() != hyper::Method::GET {
            return json_response(&rpc_error(
                &Value::Null,
                &RpcError::invalid_request("the agent card is served by GET"),
            ));
        }
        // The interface URL is advisory and built from the Host the client
        // already used to reach us, so loopback and remote setups both get
        // a usable address without extra configuration.
        let host = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("127.0.0.1");
        let card = agent_card(format!("http://{host}{RPC_PATH}"), self.auth_required);
        json_response(&card)
    }

    async fn rpc<B>(&self, req: Request<B>) -> Response<HttpBody>
    where
        B: hyper::body::Body<Data = Bytes> + Unpin + Send + 'static,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        if req.method() != hyper::Method::POST {
            return json_response(&rpc_error(
                &Value::Null,
                &RpcError::invalid_request("A2A RPC is served by POST"),
            ));
        }
        if !accepts_version(req.headers()) {
            return json_response(&rpc_error(&Value::Null, &RpcError::version()));
        }
        // The interface URL is advisory and built from the Host the client
        // already used to reach us, so loopback and remote setups both get
        // a usable address without extra configuration.
        let host = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("127.0.0.1")
            .to_owned();
        let bytes = match BodyExt::collect(req.into_body()).await {
            Ok(collected) => collected.to_bytes(),
            Err(e) => {
                return json_response(&rpc_error(&Value::Null, &RpcError::parse(e.to_string())));
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(e) => {
                return json_response(&rpc_error(&Value::Null, &RpcError::parse(e.to_string())));
            }
        };
        if value.is_array() {
            return json_response(&rpc_error(
                &Value::Null,
                &RpcError::invalid_request("batch requests are not supported"),
            ));
        }
        let parsed: RpcRequest = match serde_json::from_value(value) {
            Ok(parsed) => parsed,
            Err(e) => {
                return json_response(&rpc_error(
                    &Value::Null,
                    &RpcError::invalid_request(e.to_string()),
                ));
            }
        };
        let RpcRequest { id, method, params } = parsed;
        let interface = format!("http://{host}{RPC_PATH}");
        match self.dispatch(&id, &method, params, &interface).await {
            Ok(Reply::Json(task)) => json_response(&rpc_result(&id, &task)),
            Ok(Reply::JsonValue(value)) => json_response(&rpc_result(&id, &value)),
            Ok(Reply::Stream(frames)) => sse_response(frames),
            Err(error) => {
                tracing::debug!(method, code = error.code, message = %error.message, "rpc error");
                json_response(&rpc_error(&id, &error))
            }
        }
    }

    async fn dispatch(
        &self,
        id: &Value,
        method: &str,
        params: Value,
        interface: &str,
    ) -> Result<Reply, RpcError> {
        match method {
            "SendMessage" => Ok(self.blocking_send(parse_params(params)?).await?),
            "SendStreamingMessage" => Ok(self.streaming_send(id, parse_params(params)?).await?),
            "SubscribeToTask" => {
                let params: TaskRef = parse_params(params)?;
                if let Some(live) = self.turns.get(&params.id) {
                    return Ok(Reply::Stream(turn_stream(id, &live, Vec::new())));
                }
                let task = self.persisted_snapshot(&params.id, false).await?;
                if task.status.state.resolved() {
                    // A resolved task has nothing to stream; read it with
                    // GetTask instead.
                    return Err(RpcError::unsupported_operation(
                        "task is resolved; subscribe to a running task or read it with GetTask",
                    ));
                }
                Ok(Reply::Stream(idle_stream(id.clone(), task)))
            }
            "GetTask" => {
                let params: GetTaskParams = parse_params(params)?;
                let task = match self.turns.get(&params.id) {
                    Some(live) => live.snapshot(),
                    None => self.persisted_snapshot(&params.id, true).await?,
                };
                Ok(Reply::Json(
                    self.with_history(task, Some(params.history_length.unwrap_or(u32::MAX)))
                        .await,
                ))
            }
            "ListTasks" => {
                let params: ListTasksParams = parse_params(params)?;
                Ok(Reply::JsonValue(self.list_tasks(params).await?))
            }
            "GetExtendedAgentCard" => {
                let mut card = agent_card(interface.to_owned(), self.auth_required);
                card.skills.extend(self.persona_skills());
                Ok(Reply::JsonValue(
                    serde_json::to_value(card).unwrap_or(Value::Null),
                ))
            }
            "CancelTask" => {
                let params: TaskRef = parse_params(params)?;
                Ok(Reply::Json(self.cancel(&params.id).await?))
            }
            "CreateTaskPushNotificationConfig" => {
                let params: CreatePushParams = parse_params(params)?;
                Ok(Reply::JsonValue(self.create_push_config(params).await?))
            }
            "GetTaskPushNotificationConfig" => {
                let params: TaskRef = parse_params(params)?;
                if let Some(live) = self.turns.get(&params.id)
                    && let Some(target) = live
                        .push
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone()
                {
                    return Ok(Reply::JsonValue(push_config_reply(&params.id, &target)));
                }
                let config = self
                    .store
                    .push_config(&params.id)
                    .await
                    .map_err(RpcError::internal)?
                    .ok_or_else(|| {
                        RpcError::unsupported_operation(
                            "no push notification configuration on this task",
                        )
                    })?;
                Ok(Reply::JsonValue(push_config_reply(
                    &params.id,
                    &PushTarget {
                        url: config.url,
                        token: config.token,
                        auth: config
                            .auth_scheme
                            .map(|scheme| (scheme, config.auth_credentials.clone())),
                    },
                )))
            }
            other => Err(RpcError::new(-32601, format!("unknown method '{other}'"))),
        }
    }

    /// `SendMessage`: either the `WORKING` snapshot at once
    /// (`returnImmediately`) or the final one, holding the POST open until
    /// THIS turn finishes. The epoch, not the task state, is the wait
    /// signal — an idle task never terminates.
    async fn blocking_send(&self, params: MessageSendParams) -> Result<Reply, RpcError> {
        let history_length = params.configuration.history_length;
        let return_immediately = params.configuration.return_immediately;
        match self.start(params).await? {
            TurnStart::Replay(task) => Ok(Reply::Json(task)),
            TurnStart::New(handle) => {
                if return_immediately {
                    return Ok(Reply::Json(
                        self.with_history(handle.task, history_length).await,
                    ));
                }
                let mut epoch_rx = handle.live.turn_epoch.subscribe();
                let baseline = *epoch_rx.borrow();
                while *epoch_rx.borrow() == baseline {
                    epoch_rx
                        .changed()
                        .await
                        .map_err(|e| RpcError::internal(e.to_string()))?;
                }
                Ok(Reply::Json(
                    self.with_history(handle.live.snapshot(), history_length)
                        .await,
                ))
            }
        }
    }

    /// `SendStreamingMessage`: the live SSE feed, or — for a redelivered
    /// `messageId` — the replayed task's snapshot as a two-frame stream.
    async fn streaming_send(
        &self,
        id: &Value,
        params: MessageSendParams,
    ) -> Result<Reply, RpcError> {
        let history_length = params.configuration.history_length;
        match self.start(params).await? {
            TurnStart::Replay(task) => Ok(Reply::Stream(idle_stream(id.clone(), task))),
            TurnStart::New(handle) => {
                let history = match history_length {
                    None => Vec::new(),
                    Some(len) => self.task_history(&handle.task.context_id, len).await,
                };
                Ok(Reply::Stream(turn_stream(id, &handle.live, history)))
            }
        }
    }

    /// Attach a webhook to a task (durable — it keeps firing for later
    /// turns) and start forwarding this turn's transitions if the task is
    /// live.
    async fn create_push_config(&self, params: CreatePushParams) -> Result<Value, RpcError> {
        let target = PushTarget::try_from_config(&params.push_notification_config)?;
        let row = self
            .store
            .load(&params.task_id)
            .await
            .map_err(RpcError::internal)?;
        if row.as_ref().is_some_and(|row| row.state.resolved()) {
            // Resolved tasks are immutable; a config on one would never
            // fire again.
            return Err(RpcError::unsupported_operation(
                "task is resolved; open a new task with its contextId",
            ));
        }
        if row.is_none() && !self.conversation_exists(&params.task_id).await {
            return Err(RpcError::task_not_found(&params.task_id));
        }
        self.store
            .set_push_config(
                &params.task_id,
                &store::PushConfigRow {
                    url: target.url.clone(),
                    token: target.token.clone(),
                    auth_scheme: target.auth.as_ref().map(|(scheme, _)| scheme.clone()),
                    auth_credentials: target
                        .auth
                        .as_ref()
                        .and_then(|(_, credentials)| credentials.clone()),
                },
            )
            .await
            .map_err(RpcError::internal)?;
        if let Some(live) = self.turns.get(&params.task_id) {
            let was_unregistered = live
                .push
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none();
            *live.push.lock().unwrap_or_else(PoisonError::into_inner) = Some(target.clone());
            if was_unregistered {
                spawn_push_forwarder(self.http.clone(), Arc::clone(&live));
            }
        }
        Ok(push_config_reply(&params.task_id, &target))
    }

    /// Whether a conversation (pie session) exists for this task id.
    async fn conversation_exists(&self, id: &str) -> bool {
        Session::load(
            self.ctx.pool.clone(),
            pie_core::session::SessionId::from(id.to_string()),
        )
        .await
        .is_ok()
    }

    /// Snapshot of a task with no running turn, from its persisted row
    /// (state, artifacts and usage as recorded at the end of its turns).
    async fn persisted_snapshot(
        &self,
        id: &str,
        include_artifacts: bool,
    ) -> Result<Task, RpcError> {
        let row = self
            .store
            .load(id)
            .await
            .map_err(RpcError::internal)?
            .ok_or_else(|| RpcError::task_not_found(id))?;
        snapshot_from_row(&self.store, &row, include_artifacts).await
    }

    /// The freshest view of a task: the live turn when one is running,
    /// the persisted row otherwise.
    async fn current_snapshot(&self, id: &str) -> Result<Task, RpcError> {
        match self.turns.get(id) {
            Some(live) => Ok(live.snapshot()),
            None => self.persisted_snapshot(id, true).await,
        }
    }

    /// Attach the conversation transcript to a task snapshot; `None`
    /// leaves the task untouched, 0 attaches none.
    async fn with_history(&self, mut task: Task, history_length: Option<u32>) -> Task {
        if let Some(len) = history_length {
            let history = self.task_history(&task.context_id, len).await;
            task.history = (!history.is_empty()).then_some(history);
        }
        task
    }

    /// The pie session transcript as A2A messages: user prompts are
    /// `ROLE_USER`, answers `ROLE_AGENT`; tool/system entries stay
    /// internal. `len` keeps the most recent N (0 — none).
    async fn task_history(&self, context_id: &str, len: u32) -> Vec<WireMessage> {
        let Ok(session) = Session::load(
            self.ctx.pool.clone(),
            pie_core::session::SessionId::from(context_id.to_owned()),
        )
        .await
        else {
            return Vec::new();
        };
        let mut messages: Vec<WireMessage> = session
            .history_entries()
            .iter()
            .filter_map(|entry| match entry.role() {
                Role::User => Some(WireMessage::for_role("ROLE_USER", entry.content())),
                Role::Assistant => Some(WireMessage::for_role("ROLE_AGENT", entry.content())),
                _ => None,
            })
            .collect();
        let len = len as usize;
        if messages.len() > len {
            messages.drain(..messages.len() - len);
        }
        messages
    }

    /// The extended card's extra skills: one per agent persona registered
    /// on the daemon (project `.pie/agents` of the daemon workspace plus
    /// the global `~/.pie/agents`).
    fn persona_skills(&self) -> Vec<Value> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        self.ctx
            .registries
            .get(&cwd)
            .agents
            .iter()
            .map(|agent| {
                serde_json::json!({
                    "id": format!("pie-agent-{}", agent.name),
                    "name": agent.name,
                    "description": agent.description,
                    "tags": ["persona", "workspace"],
                })
            })
            .collect()
    }

    /// `ListTasks`: the persisted tasks, newest activity first, filtered
    /// and cursor-paginated.
    async fn list_tasks(&self, params: ListTasksParams) -> Result<Value, RpcError> {
        let page_size = params
            .page_size
            .unwrap_or(LIST_PAGE_DEFAULT)
            .clamp(1, LIST_PAGE_MAX);
        // The cursor is `<updated_at>:<id>` of the last row of the
        // previous page.
        let (cursor_updated, cursor_id) = match params.page_token.as_deref() {
            None => (None, None),
            Some(token) => {
                let (updated, id) = token
                    .split_once(':')
                    .ok_or_else(|| RpcError::invalid_params("malformed pageToken"))?;
                let updated = updated
                    .parse::<i64>()
                    .map_err(|_| RpcError::invalid_params("malformed pageToken"))?;
                (Some(updated), Some(id.to_owned()))
            }
        };
        let state = params
            .status
            .as_deref()
            .map(|status| {
                TaskState::parse(status)
                    .ok_or_else(|| RpcError::invalid_params(format!("unknown status '{status}'")))
            })
            .transpose()?;
        let updated_after = params
            .status_timestamp_after
            .as_deref()
            .map(|ts| {
                chrono::DateTime::parse_from_rfc3339(ts)
                    .map(|parsed| parsed.timestamp_millis())
                    .map_err(|_| {
                        RpcError::invalid_params(format!("invalid statusTimestampAfter '{ts}'"))
                    })
            })
            .transpose()?;
        // One row past the page size tells whether a next page exists.
        let (mut rows, total) = self
            .store
            .list(store::ListFilter {
                context_id: params.context_id.as_deref(),
                state,
                updated_after,
                cursor_updated,
                cursor_id: cursor_id.as_deref(),
                limit: page_size + 1,
            })
            .await
            .map_err(RpcError::internal)?;
        let more = rows.len() > page_size as usize;
        rows.truncate(page_size as usize);
        let mut tasks = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut task = snapshot_from_row(&self.store, row, params.include_artifacts).await?;
            if let Some(len) = params.history_length {
                let history = self.task_history(&task.context_id, len).await;
                task.history = (!history.is_empty()).then_some(history);
            }
            tasks.push(task);
        }
        let mut result = serde_json::Map::new();
        result.insert("tasks".into(), serde_json::json!(tasks));
        result.insert("totalSize".into(), serde_json::json!(total));
        if let Some(cursor) = more.then(|| rows.last()).flatten() {
            result.insert(
                "nextPageToken".into(),
                serde_json::json!(format!("{}:{}", cursor.updated_at, cursor.id)),
            );
        }
        Ok(Value::Object(result))
    }

    /// Resolve the prompt, task id and session, claim the turn slot,
    /// register the running task and spawn the driver. A message whose
    /// `messageId` was seen before never starts another turn — its task
    /// is replayed instead.
    async fn start(&self, params: MessageSendParams) -> Result<TurnStart, RpcError> {
        let message_id = params
            .message
            .message_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if let Some(message_id) = &message_id
            && let Some(existing) = self
                .store
                .message_task(message_id)
                .await
                .map_err(RpcError::internal)?
        {
            return Ok(TurnStart::Replay(self.current_snapshot(&existing).await?));
        }
        let InboundMessage {
            parts,
            task_id,
            context_id,
            metadata,
            ..
        } = params.message;
        let prompt = prompt_text(&parts)?;
        let metadata = metadata.unwrap_or_default();
        let agent_name = metadata_string(&metadata, "agent");
        let title = metadata_string(&metadata, "title");

        let (task_id, context_id, session) = self
            .resolve_turn_target(
                task_id,
                context_id,
                agent_name.as_deref(),
                metadata,
                &prompt,
                title,
            )
            .await?;

        // One in-flight turn per conversation.
        let guard = self
            .ctx
            .turns
            .try_acquire(&context_id, &format!("a2a:{task_id}"))
            .map_err(|_| {
                RpcError::unsupported_operation(
                    "a turn is already in progress for this conversation",
                )
            })?;

        // (Re)open the durable row for this turn — WORKING, no answer yet.
        let artifact_id = uuid::Uuid::new_v4().to_string();
        let turn = self
            .store
            .record_turn_start(&task_id, &context_id, &artifact_id)
            .await
            .map_err(RpcError::internal)?;

        self.spawn_turn(TurnSeed {
            task_id,
            context_id,
            session,
            agent_name,
            prompt,
            guard,
            artifact_id,
            turn,
            message_id,
            push_config: params.configuration.push_notification_config,
        })
        .await
    }

    /// Validate and resolve everything a turn needs before the turn slot
    /// is claimed: the conversation, the agent, immutability of resolved
    /// tasks, and the session title.
    async fn resolve_turn_target(
        &self,
        task_id: Option<String>,
        context_id: Option<String>,
        agent_name: Option<&str>,
        metadata: serde_json::Map<String, Value>,
        prompt: &str,
        title: Option<String>,
    ) -> Result<(String, String, Session), RpcError> {
        let (task_id, context_id, session) =
            resolve_conversation(&self.ctx, task_id, context_id, agent_name, metadata).await?;
        let session_cwd = std::path::Path::new(&session.cwd).to_path_buf();
        crate::turn::resolve_agent(&self.ctx, &session_cwd, agent_name)
            .map_err(RpcError::invalid_params)?;

        // Resolved tasks are immutable: a follow-up on one can never
        // reopen it — the client opens a new task with its contextId.
        if let Some(row) = self
            .store
            .load(&task_id)
            .await
            .map_err(RpcError::internal)?
            && row.state.resolved()
        {
            return Err(RpcError::unsupported_operation(
                "task is resolved; open a new task with its contextId",
            ));
        }

        if let Some(title) = title.or_else(|| {
            session
                .history_entries()
                .is_empty()
                .then(|| short_title(prompt))
        }) {
            let _ = session.set_title(&title).await;
        }
        Ok((task_id, context_id, session))
    }

    /// Webhook for this turn: the inline config wins; otherwise the task's
    /// persisted config keeps notifying across turns.
    async fn wire_push(
        &self,
        task_id: &str,
        config: Option<&PushConfigIn>,
        live: &Arc<LiveTask>,
    ) -> Result<(), RpcError> {
        let target = match config {
            Some(cfg) => {
                let target = PushTarget::try_from_config(cfg)?;
                self.store
                    .set_push_config(
                        task_id,
                        &store::PushConfigRow {
                            url: target.url.clone(),
                            token: target.token.clone(),
                            auth_scheme: target.auth.as_ref().map(|(scheme, _)| scheme.clone()),
                            auth_credentials: target
                                .auth
                                .as_ref()
                                .and_then(|(_, credentials)| credentials.clone()),
                        },
                    )
                    .await
                    .map_err(RpcError::internal)?;
                Some(target)
            }
            None => self
                .store
                .push_config(task_id)
                .await
                .map_err(RpcError::internal)?
                .map(|config| PushTarget {
                    url: config.url,
                    token: config.token,
                    auth: config
                        .auth_scheme
                        .map(|scheme| (scheme, config.auth_credentials)),
                }),
        };
        if let Some(target) = target {
            *live.push.lock().unwrap_or_else(PoisonError::into_inner) = Some(target);
            spawn_push_forwarder(self.http.clone(), Arc::clone(live));
        }
        Ok(())
    }

    /// Signal cancellation and wait (bounded) for the driver to finalize.
    /// A task with no running turn (idle or orphaned) is cancelled
    /// directly; a resolved task answers `-32002` — resolved is final.
    async fn cancel(&self, id: &str) -> Result<Task, RpcError> {
        let Some(live) = self.turns.get(id) else {
            let Some(row) = self.store.load(id).await.map_err(RpcError::internal)? else {
                return Err(RpcError::task_not_found(id));
            };
            if row.state.resolved() {
                return Err(RpcError::not_cancelable(id));
            }
            let record = store::TurnRecord {
                state: TaskState::Canceled,
                status_message: Some("cancelled"),
                response: &row.response,
                artifact_id: &row.artifact_id,
                completion: row.completion.as_ref(),
            };
            self.store
                .record_turn_end(id, row.turn, record)
                .await
                .map_err(RpcError::internal)?;
            let row = self
                .store
                .load(id)
                .await
                .map_err(RpcError::internal)?
                .ok_or_else(|| RpcError::internal("cancelled task row vanished"))?;
            return snapshot_from_row(&self.store, &row, true).await;
        };
        if let Some(cancel) = live
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = cancel.send(());
        }
        let mut epoch_rx = live.turn_epoch.subscribe();
        let baseline = *epoch_rx.borrow();
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);
        while *epoch_rx.borrow() == baseline {
            tokio::select! {
                changed = epoch_rx.changed() => if changed.is_err() { break },
                () = &mut deadline => break,
            }
        }
        // The turn finalized (or was orphaned): the durable record is the
        // complete view.
        self.current_snapshot(id).await
    }
}

/// What `SendMessage`/`SendStreamingMessage` get back from [`A2a::start`]:
/// either a freshly started turn, or the replay of a message delivered
/// before (`messageId` idempotency).
enum TurnStart {
    New(TurnHandle),
    Replay(Task),
}

/// Everything [`A2a::spawn_turn`] needs once `start` has resolved the
/// conversation, claimed the turn slot and opened the durable row.
struct TurnSeed {
    task_id: String,
    context_id: String,
    session: Session,
    agent_name: Option<String>,
    prompt: String,
    guard: pie_core::turn_gate::TurnGuard,
    artifact_id: String,
    turn: u32,
    message_id: Option<String>,
    push_config: Option<PushConfigIn>,
}

/// A freshly started turn: the initial snapshot plus the live handle
/// every path needs.
struct TurnHandle {
    task: Task,
    live: Arc<LiveTask>,
}

enum Reply {
    Json(Task),
    JsonValue(Value),
    Stream(mpsc::Receiver<String>),
}

// ── turn driving ─────────────────────────────────────────────────────

/// Run one pie turn: the engine is driven by the shared a2acp turn
/// runner (`pie_core::bridge::run_turn`); this side projects the bridge events
/// onto the live task's feed and finalizes (terminal broadcast → drop
/// from the registry). The gate guard drops here, wherever the turn ends.
#[allow(clippy::too_many_arguments)]
async fn drive_turn(
    ctx: Arc<AppContext>,
    turns: Arc<LiveTurns>,
    store: store::TaskStore,
    live: Arc<LiveTask>,
    session: Session,
    agent_name: Option<String>,
    prompt: String,
    guard: pie_core::turn_gate::TurnGuard,
    cancel_rx: watch::Receiver<()>,
) {
    let agent = match crate::turn::prepare_turn(&ctx, &session, agent_name.as_deref()) {
        Ok(agent) => agent,
        Err(e) => {
            end_turn(&turns, &live, &store, TaskState::Failed, Some(e)).await;
            return;
        }
    };

    // Drain bridge events onto the live feed while the turn runs; the
    // drain ends only after `run_turn` returns and drops its sender, so
    // every delta and status line lands before finalization.
    let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel::<pie_core::bridge::Event>();
    let run = tokio::spawn(pie_core::bridge::run_turn(
        agent, prompt, cancel_rx, bridge_tx,
    ));
    let drain_live = Arc::clone(&live);
    let drain = tokio::spawn(async move {
        while let Some(event) = bridge_rx.recv().await {
            forward_event(&drain_live, event);
        }
    });

    let end = match run.await {
        Ok(end) => end,
        Err(e) => pie_core::bridge::PieTurnEnd::Failed(e.to_string()),
    };
    let _ = drain.await;

    match end {
        pie_core::bridge::PieTurnEnd::Completed {
            text,
            usage,
            cost_usd,
        } => {
            {
                let mut response = live.response.lock().unwrap_or_else(PoisonError::into_inner);
                // Token deltas already accumulated the answer; the final
                // text fills the artifact only when the engine skipped
                // deltas entirely.
                if response.is_empty() && !text.is_empty() {
                    response.push_str(&text);
                }
            }
            let mut completion = serde_json::Map::new();
            if let Ok(usage) = serde_json::to_value(usage) {
                completion.insert("usage".into(), usage);
            }
            completion.insert("cost_usd".into(), serde_json::json!(cost_usd));
            *live
                .completion
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(completion);
            end_turn(&turns, &live, &store, TaskState::InputRequired, None).await;
        }
        pie_core::bridge::PieTurnEnd::Cancelled => {
            end_turn(&turns, &live, &store, TaskState::Canceled, None).await;
        }
        pie_core::bridge::PieTurnEnd::Failed(message) => {
            end_turn(&turns, &live, &store, TaskState::Failed, Some(message)).await;
        }
    }
    // The turn slot is released only after the final broadcast.
    drop(guard);
}

/// Turn-final transition: mark the task idle (or resolved), persist the
/// task's durable record, push the full artifact for answers, broadcast
/// the final event, and drop the in-memory entry — the persisted row is
/// now the task's single source of truth.
async fn end_turn(
    turns: &LiveTurns,
    live: &Arc<LiveTask>,
    store: &store::TaskStore,
    state: TaskState,
    message: Option<String>,
) {
    live.running.store(false, Ordering::SeqCst);
    *live
        .final_state
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(state);
    // All locks are scoped out before the persist await: guards are not
    // Send, and the spawned future must stay Send.
    let status_message = match message {
        Some(message) => {
            *live
                .status_message
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(message.clone());
            Some(message)
        }
        None => live
            .status_message
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
    };
    if state == TaskState::InputRequired {
        let response = live
            .response
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if !response.is_empty() {
            let _ = live
                .events
                .send(Event::Artifact(live.artifact_event(&response, false)));
        }
    }
    let response = live
        .response
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let completion = live
        .completion
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .map(Value::Object);
    let record = store::TurnRecord {
        state,
        status_message: status_message.as_deref(),
        response: &response,
        artifact_id: &live.artifact_id,
        completion: completion.as_ref(),
    };
    if let Err(e) = store
        .record_turn_end(&live.task_id, live.turn, record)
        .await
    {
        // The in-memory feed already told every subscriber the turn is
        // over; but the durable record is what survives the daemon.
        tracing::error!(task = %live.task_id, error = %e, "failed to persist turn end");
    }
    let _ = live.events.send(Event::Status(live.status_event(true)));
    turns.remove(&live.task_id);
    let next = live.turn_epoch.borrow().wrapping_add(1);
    let _ = live.turn_epoch.send(next);
}

/// Map one bridge event onto the live feed.
fn forward_event(live: &LiveTask, event: pie_core::bridge::Event) {
    match event {
        pie_core::bridge::Event::Delta(text) => {
            live.response
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_str(&text);
            let _ = live
                .events
                .send(Event::Artifact(live.artifact_event(&text, true)));
        }
        // One pie turn is sequential, so at most one tool call is in
        // flight; non-empty `display` is the pre-execution announcement.
        pie_core::bridge::Event::ToolCall { display, .. } if !display.is_empty() => {
            *live
                .status_message
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(display);
            let _ = live.events.send(Event::Status(live.status_event(false)));
        }
        // Post-execution halves, usage bookkeeping, soft errors: internal
        // detail — the final status carries the outcome.
        _ => {}
    }
}

// ── prompt & metadata parsing ────────────────────────────────────────

/// Resolve the task id, context id and pie session a message targets.
///
/// Task identity: an explicit taskId continues that conversation (it must
/// exist and must not be resolved — resolved tasks are immutable); a
/// contextId opens a NEW task instance on that conversation; neither
/// starts a fresh one. The contextId is always the pie session id.
async fn resolve_conversation(
    ctx: &AppContext,
    task_id: Option<String>,
    context_id: Option<String>,
    agent_name: Option<&str>,
    metadata: serde_json::Map<String, Value>,
) -> Result<(String, String, Session), RpcError> {
    // Task identity: an explicit taskId continues that conversation; a
    // contextId opens a NEW task instance on that conversation; neither
    // starts a fresh one. The contextId is always the pie session id.
    let Some(task_id) = task_id else {
        let (task_id, session) =
            fresh_or_continuation(ctx, context_id, &metadata, agent_name).await?;
        let context_id = session.id.to_string();
        return Ok((task_id, context_id, session));
    };
    // A task instance id carries its conversation: canonical ids are the
    // session id, continuations are `<sessionId>~<suffix>`. Either
    // resolves to the session.
    let session_ref = match task_id.split_once('~') {
        Some((session_id, _)) => session_id,
        None => task_id.as_str(),
    };
    let session = Session::load(
        ctx.pool.clone(),
        pie_core::session::SessionId::from(session_ref.to_owned()),
    )
    .await
    .map_err(|_| RpcError::task_not_found(&task_id))?;
    let context_id = session.id.to_string();
    Ok((task_id, context_id, session))
}

/// No `taskId` on the message: a `contextId` opens a NEW task instance on
/// that conversation; neither id starts a fresh conversation.
async fn fresh_or_continuation(
    ctx: &AppContext,
    context_id: Option<String>,
    metadata: &serde_json::Map<String, Value>,
    agent_name: Option<&str>,
) -> Result<(String, Session), RpcError> {
    if let Some(context_id) = context_id {
        let session = Session::load(
            ctx.pool.clone(),
            pie_core::session::SessionId::from(context_id.clone()),
        )
        .await
        .map_err(|_| RpcError::task_not_found(&context_id))?;
        let session_id = session.id.to_string();
        return Ok((fresh_instance(&session_id), session));
    }
    let cwd = match metadata_string(metadata, "cwd") {
        Some(cwd) => std::fs::canonicalize(&cwd)
            .map_err(|e| RpcError::invalid_params(format!("cwd '{cwd}' is not accessible: {e}")))?,
        // No workspace named: the daemon's own directory, exactly what the
        // CLI would have used.
        None => std::env::current_dir()
            .map_err(|e| RpcError::internal(format!("cannot determine cwd: {e}")))?,
    };
    crate::turn::resolve_agent(ctx, &cwd, agent_name).map_err(RpcError::invalid_params)?;
    let session = Session::create(ctx.pool.clone(), &cwd)
        .await
        .map_err(|e| RpcError::internal(format!("session creation failed: {e}")))?;
    let context_id = session.id.to_string();
    Ok((context_id, session))
}

/// First line, clipped — good enough for a session title.
fn short_title(message: &str) -> String {
    const MAX: usize = 80;
    let line = message.lines().next().unwrap_or(message).trim();
    if line.chars().count() <= MAX {
        line.to_owned()
    } else {
        let mut clipped: String = line.chars().take(MAX).collect();
        clipped.push('…');
        clipped
    }
}

fn prompt_text(parts: &[Part]) -> Result<String, RpcError> {
    if parts.is_empty() {
        return Err(RpcError::invalid_params("empty message"));
    }
    let mut texts = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Part::Text { text } => texts.push(text.clone()),
            Part::Data { data } => texts.push(data.to_string()),
        }
    }
    Ok(texts.join("\n\n"))
}

fn metadata_string(metadata: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    metadata.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn parse_params<P: for<'de> Deserialize<'de>>(params: Value) -> Result<P, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn accepts_version(headers: &HeaderMap) -> bool {
    headers
        .get("a2a-version")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|version| version == A2A_VERSION)
}

/// A fresh task instance on an existing conversation: opaque to clients,
/// but always derivable back to its session (pie session ids never
/// contain `~`).
fn fresh_instance(context_id: &str) -> String {
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    format!("{context_id}~{suffix}")
}

/// The idle snapshot of a task with no running turn: read from its
/// persisted row (state, answer artifacts and usage as recorded at the
/// end of the last turns).
async fn snapshot_from_row(
    store: &store::TaskStore,
    row: &store::TaskRow,
    include_artifacts: bool,
) -> Result<Task, RpcError> {
    let artifacts = if include_artifacts {
        store
            .artifacts(&row.id)
            .await
            .map_err(RpcError::internal)?
            .into_iter()
            .map(|artifact| Artifact {
                id: artifact.artifact_id,
                name: "response",
                parts: vec![WirePart::Text {
                    text: artifact.text,
                }],
                append: false,
                last_chunk: true,
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(build_task(
        &row.id,
        &row.context_id,
        row.state,
        row.status_message.as_deref(),
        artifacts,
        row.completion.as_ref(),
        Vec::new(),
    ))
}

/// Assemble the wire Task from its parts — the one place snapshots are
/// built from, whether the source is a live turn or a persisted row.
fn build_task(
    id: &str,
    context_id: &str,
    state: TaskState,
    status_message: Option<&str>,
    artifacts: Vec<Artifact>,
    completion: Option<&Value>,
    history: Vec<WireMessage>,
) -> Task {
    Task {
        id: id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state,
            message: status_message.map(WireMessage::text),
            timestamp: Utc::now().to_rfc3339(),
        },
        artifacts,
        history: (!history.is_empty()).then_some(history),
        metadata: completion.and_then(|value| value.as_object().cloned()),
    }
}

// ── HTTP plumbing ────────────────────────────────────────────────────

fn json_response<T: Serialize>(body: &T) -> Response<HttpBody> {
    let bytes = Bytes::from(serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec()));
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Full::new(bytes).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// SSE framing: one `data:` line per JSON-RPC result frame, blank line
/// terminator.
fn sse_frame(id: &Value, payload: &StreamResponse) -> String {
    let result = serde_json::to_value(payload).unwrap_or(Value::Null);
    format!(
        "data: {}\n\n",
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    )
}

fn sse_response(frames: mpsc::Receiver<String>) -> Response<HttpBody> {
    let stream = tokio_stream::wrappers::ReceiverStream::new(frames)
        .map(|frame| Ok::<_, Infallible>(hyper::body::Frame::data(Bytes::from(frame))));
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        // nginx: never buffer this response (the mcp-gateway already runs
        // this header through its proxy rules).
        .header("x-accel-buffering", "no")
        .body(StreamBody::new(stream).boxed())
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()).boxed()))
}

/// Build the SSE body for a live task: subscribe to the feed first, then
/// snapshot (so no event is lost), emit the snapshot frame, and forward
/// events until the final status update. Keepalive comments flow between
/// events so proxies never time the connection out mid-turn. `history`
/// rides on the initial frame when the caller asked for it.
fn turn_stream(
    id: &Value,
    live: &Arc<LiveTask>,
    history: Vec<WireMessage>,
) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel::<String>(64);
    let id = id.clone();
    let live = Arc::clone(live);

    tokio::spawn(async move {
        let mut events = live.events.subscribe();
        let mut keepalive =
            tokio::time::interval_at(tokio::time::Instant::now() + SSE_KEEPALIVE, SSE_KEEPALIVE);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        if tx
            .send(sse_frame(
                &id,
                &StreamResponse::Task(live.snapshot_with(history)),
            ))
            .await
            .is_err()
        {
            return;
        }
        if !live.running() {
            // Finished between lookup and stream start: replay the final
            // status and close.
            let _ = tx
                .send(sse_frame(
                    &id,
                    &StreamResponse::StatusUpdate(live.status_event(true)),
                ))
                .await;
            return;
        }
        loop {
            tokio::select! {
                _ = keepalive.tick() => {
                    if tx.send(": ping\n\n".to_owned()).await.is_err() {
                        return;
                    }
                }
                event = events.recv() => match event {
                    Ok(Event::Status(status)) => {
                        let done = status.is_final;
                        if tx.send(sse_frame(&id, &StreamResponse::StatusUpdate(status))).await.is_err() {
                            return;
                        }
                        if done {
                            return;
                        }
                    }
                    Ok(Event::Artifact(artifact)) => {
                        if tx.send(sse_frame(&id, &StreamResponse::ArtifactUpdate(artifact))).await.is_err() {
                            return;
                        }
                    }
                    // Closed: the sender only drops after the terminal
                    // broadcast, so the turn is over. Lagged: this client
                    // missed events it can re-subscribe for — a corrupted
                    // answer is worse than a short one.
                    Err(_) => return,
                }
            }
        }
    });
    rx
}

/// Spawn the push-notification forwarder for a task with a registered
/// webhook. It POSTs a `StreamResponse::Task` snapshot — same shape as the
/// SSE frames — on every status *transition* (turn start, turn end); the
/// final snapshot carries the full answer artifact, so the client needs no
/// follow-up `GetTask`. Token deltas are never pushed (that is what the
/// SSE stream is for). Retries: up to 3 attempts with exponential backoff.
fn spawn_push_forwarder(http: reqwest::Client, live: Arc<LiveTask>) {
    tokio::spawn(async move {
        let Some(target) = live
            .push
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let mut events = live.events.subscribe();
        let mut last_state = live.state();
        loop {
            // Sender drops only after the final broadcast; a lagged client
            // re-subscribes via a new delivery registration.
            let Ok(event) = events.recv().await else {
                return;
            };
            let Event::Status(status) = event else {
                continue; // artifact deltas are not pushed
            };
            let state = status.status.state;
            if state == last_state && !status.is_final {
                continue; // tool-call status line: no state change
            }
            last_state = state;
            let is_final = status.is_final;
            // Mid-turn delivery failures are logged inside `deliver`; the
            // final snapshot is still worth delivering after one.
            let _ = deliver(&http, &target, &StreamResponse::Task(live.snapshot())).await;
            if is_final {
                return;
            }
        }
    });
}

/// POST one frame to the webhook. The client's `token` is echoed in
/// `x-a2a-notification-token` (v1.0 names no header; this is our choice),
/// and a `bearer` authentication adds an Authorization header.
async fn deliver(
    http: &reqwest::Client,
    target: &PushTarget,
    payload: &StreamResponse,
) -> Result<(), String> {
    let body = serde_json::to_string(payload).map_err(|e| e.to_string())?;
    let mut backoff = PUSH_RETRY_BACKOFF;
    for attempt in 0..PUSH_RETRIES {
        let mut request = http
            .post(&target.url)
            .header("content-type", "application/json");
        if let Some(token) = &target.token {
            request = request.header("x-a2a-notification-token", token);
        }
        if let Some((scheme, credentials)) = &target.auth
            && scheme.eq_ignore_ascii_case("bearer")
            && let Some(credentials) = credentials
        {
            request = request.header("authorization", format!("Bearer {credentials}"));
        }
        match request.body(body.clone()).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                tracing::warn!(status = %response.status(), attempt, "push notification rejected");
            }
            Err(e) => tracing::warn!(error = %e, attempt, "push notification delivery failed"),
        }
        if attempt + 1 < PUSH_RETRIES {
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
    }
    Err("webhook delivery failed after retries".into())
}

const PUSH_RETRIES: u32 = 3;
const PUSH_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// SSE body for an idle task: the snapshot plus its closing status.
fn idle_stream(id: Value, task: Task) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel::<String>(4);
    tokio::spawn(async move {
        let final_event = TaskStatusUpdateEvent {
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
            status: task.status.clone(),
            is_final: true,
        };
        if tx
            .send(sse_frame(&id, &StreamResponse::Task(task)))
            .await
            .is_err()
        {
            return;
        }
        let _ = tx
            .send(sse_frame(&id, &StreamResponse::StatusUpdate(final_event)))
            .await;
    });
    rx
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::needless_pass_by_value
    )]
    use super::*;
    use crate::AppContext;
    use pie_core::config::{
        ApiErrorConfig, GlobalAgentConfig, PieConfig, ProviderConfig, RateLimitConfig,
        ResolvedConfig, RetryConfig,
    };
    use redact::Secret;
    use std::time::{Duration, Instant};

    /// A provider pointing at a dead port with zero retries: turns fail
    /// fast, deterministically, with no network leaving the machine.
    fn test_config() -> ResolvedConfig {
        let mut provider = ProviderConfig::default();
        provider.endpoint.openai = Some("http://127.0.0.1:9/v1".into());
        provider.api_key = Some(Secret::new("k".into()));
        provider.model = Some("test-model".into());
        let pie = PieConfig {
            default_provider: Some("test".into()),
            provider: HashMap::from([("test".to_string(), provider)]),
            secrets: HashMap::new(),
            model: HashMap::new(),
            mcp: HashMap::new(),
            server: pie_core::config::ServerConfig::default(),
            pricing: HashMap::new(),
            agent: Some(GlobalAgentConfig {
                retry: RetryConfig {
                    rate_limit: RateLimitConfig {
                        max_errors: 0,
                        retry_delay_secs: 0,
                    },
                    api_error: ApiErrorConfig {
                        max_errors: 0,
                        retry_delay_secs: 0,
                    },
                },
            }),
            sandbox: None,
            output_format: None,
            log_level: None,
        };
        (pie_core::config::CliOverrides::default(), pie)
            .try_into()
            .expect("test config resolves")
    }

    /// The engine reads the process-global config; set it once per test
    /// binary. Dead-provider config: turns fail fast and deterministically.
    fn set_global_config() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = pie_core::config::CONFIG.set(test_config());
        });
    }

    async fn test_a2a(auth_required: bool) -> (A2a, Arc<AppContext>) {
        set_global_config();
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
        let global = pie_core::config::CONFIG.get().expect("config set");
        let ctx = Arc::new(AppContext {
            pool: Arc::new(pie_core::db::create_test_pool().await.unwrap()),
            registries: pie_core::registry::RegistryCache::default(),
            turns: pie_core::turn_gate::TurnGate::default(),
            sandbox: Arc::new(pie_core::p1e_sandbox::SandboxConfig::default()),
            provider: global.provider.clone(),
            retry: global.retry.clone(),
        });
        (A2a::new(ctx.clone(), auth_required), ctx)
    }

    fn rpc_request(method: &str, params: Value, version: Option<&str>) -> Request<Full<Bytes>> {
        let body =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut builder = Request::builder()
            .method("POST")
            .uri(RPC_PATH)
            .header("content-type", "application/json");
        if let Some(version) = version {
            builder = builder.header("a2a-version", version);
        }
        builder
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap()
    }

    fn send_params(
        text: &str,
        task_id: Option<&str>,
        context_id: Option<&str>,
        cwd: Option<&str>,
        return_immediately: bool,
    ) -> Value {
        let mut message = serde_json::json!({
            "role": "ROLE_USER",
            "parts": [{"text": text}],
            "messageId": uuid::Uuid::new_v4().to_string(),
        });
        if let Some(task_id) = task_id {
            message["taskId"] = serde_json::json!(task_id);
        }
        if let Some(context_id) = context_id {
            message["contextId"] = serde_json::json!(context_id);
        }
        let mut params = serde_json::json!({ "message": message });
        if let Some(cwd) = cwd {
            params["message"]["metadata"] = serde_json::json!({ "cwd": cwd });
        }
        params["configuration"] = serde_json::json!({ "returnImmediately": return_immediately });
        params
    }

    async fn body_json(response: Response<HttpBody>) -> Value {
        let bytes = BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        serde_json::from_slice(&bytes).expect("response is JSON")
    }

    async fn error_code(a2a: &A2a, request: Request<Full<Bytes>>) -> (i64, Value) {
        let value = body_json(a2a.handle(request).await).await;
        let code = value["error"]["code"].as_i64().unwrap_or(0);
        (code, value)
    }

    async fn result_value(a2a: &A2a, method: &str, params: Value) -> Value {
        let value = body_json(
            a2a.handle(rpc_request(method, params.clone(), Some("1.0")))
                .await,
        )
        .await;
        assert!(
            value.get("error").is_none(),
            "unexpected error from {method}({params}): {value}"
        );
        value["result"].clone()
    }

    /// Split an SSE body into its JSON-RPC result frames.
    async fn sse_frames(response: Response<HttpBody>) -> Vec<Value> {
        let bytes = BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split("\n\n")
            .filter_map(|block| {
                let data = block.lines().find_map(|line| line.strip_prefix("data: "))?;
                Some(serde_json::from_str(data).expect("SSE frame is JSON"))
            })
            .collect()
    }

    async fn seed_conversation(ctx: &AppContext, cwd: &std::path::Path) -> String {
        let mut session = Session::create(ctx.pool.clone(), cwd).await.unwrap();
        session.add_user("what is 2+2").await.unwrap();
        session.add_assistant("4").await.unwrap();
        session.id.to_string()
    }

    #[tokio::test]
    async fn agent_card_declares_streaming_and_auth() {
        let (a2a, _ctx) = test_a2a(false).await;
        let request = Request::builder()
            .method("GET")
            .uri(CARD_PATH)
            .header("host", "localhost:8629")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let card = body_json(a2a.handle(request).await).await;
        assert_eq!(card["name"], "pie");
        assert_eq!(card["protocolVersion"], "1.0");
        assert_eq!(card["capabilities"]["streaming"], true);
        assert_eq!(card["capabilities"]["pushNotifications"], true);
        assert_eq!(card["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
        assert_eq!(
            card["supportedInterfaces"][0]["url"],
            "http://localhost:8629/a2a"
        );
        assert!(
            card.get("securitySchemes").is_none(),
            "no auth configured, none declared"
        );

        let (a2a, _ctx) = test_a2a(true).await;
        let request = Request::builder()
            .method("GET")
            .uri(CARD_PATH)
            .body(Full::new(Bytes::new()))
            .unwrap();
        let card = body_json(a2a.handle(request).await).await;
        assert!(card["securitySchemes"].is_object(), "{card}");
    }

    #[tokio::test]
    async fn card_post_is_rejected() {
        let (a2a, _ctx) = test_a2a(false).await;
        let request = Request::builder()
            .method("POST")
            .uri(CARD_PATH)
            .body(Full::new(Bytes::new()))
            .unwrap();
        let (code, _) = error_code(&a2a, request).await;
        assert_eq!(code, -32600);
    }

    #[tokio::test]
    async fn rpc_rejects_wrong_or_missing_version_header() {
        let (a2a, _ctx) = test_a2a(false).await;
        let params = send_params("hi", None, None, None, false);
        for version in [None, Some("0.3"), Some("2.0")] {
            let (code, _) = error_code(&a2a, rpc_request("GetTask", params.clone(), version)).await;
            assert_eq!(code, -32009, "version {version:?}");
        }
    }

    #[tokio::test]
    async fn unknown_method_batch_and_bad_json_are_rejected() {
        let (a2a, _ctx) = test_a2a(false).await;
        let (code, _) = error_code(
            &a2a,
            rpc_request("Nope", serde_json::json!({}), Some("1.0")),
        )
        .await;
        assert_eq!(code, -32601);

        let batch = Request::builder()
            .method("POST")
            .uri(RPC_PATH)
            .header("a2a-version", "1.0")
            .body(Full::new(Bytes::from("[1,2]")))
            .unwrap();
        let (code, _) = error_code(&a2a, batch).await;
        assert_eq!(code, -32600);

        let bad = Request::builder()
            .method("POST")
            .uri(RPC_PATH)
            .header("a2a-version", "1.0")
            .body(Full::new(Bytes::from("not json")))
            .unwrap();
        let (code, _) = error_code(&a2a, bad).await;
        assert_eq!(code, -32700);
    }

    #[tokio::test]
    async fn blocking_send_fails_and_the_task_stays_retrievable() {
        let (a2a, _ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let task = result_value(
            &a2a,
            "SendMessage",
            send_params(
                "hello",
                None,
                None,
                Some(tmp.path().to_str().unwrap()),
                false,
            ),
        )
        .await;
        let task_id = task["id"].as_str().unwrap().to_string();
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
        assert_eq!(
            task["contextId"], task_id,
            "canonical task: context = session"
        );
        assert!(
            task["status"]["message"]["parts"][0]["text"]
                .as_str()
                .is_some_and(|t| !t.is_empty()),
            "the failure is reported as the status message"
        );

        // Resolved but retrievable: the durable record answers GetTask.
        let task = result_value(&a2a, "GetTask", serde_json::json!({"id": task_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");

        // Resolved tasks are immutable: a follow-up on the same id is
        // refused — continue via a new task in the same context.
        let (code, _) = error_code(
            &a2a,
            rpc_request(
                "SendMessage",
                send_params("again", Some(&task_id), None, None, false),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32004);
    }

    #[tokio::test]
    async fn return_immediately_reports_working_then_persists_the_outcome() {
        let (a2a, _ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let task = result_value(
            &a2a,
            "SendMessage",
            send_params(
                "hello",
                None,
                None,
                Some(tmp.path().to_str().unwrap()),
                true,
            ),
        )
        .await;
        let task_id = task["id"].as_str().unwrap().to_string();
        assert_eq!(task["status"]["state"], "TASK_STATE_WORKING");

        // The turn settles against the dead provider; the durable record
        // reports the failure.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let task = result_value(&a2a, "GetTask", serde_json::json!({"id": task_id})).await;
            if task["status"]["state"] != "TASK_STATE_WORKING" {
                assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
                break;
            }
            assert!(Instant::now() < deadline, "task never settled");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn streaming_send_frames_task_then_final_status() {
        let (a2a, _ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let response = a2a
            .handle(rpc_request(
                "SendStreamingMessage",
                send_params(
                    "hello",
                    None,
                    None,
                    Some(tmp.path().to_str().unwrap()),
                    false,
                ),
                Some("1.0"),
            ))
            .await;
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let frames = sse_frames(response).await;
        assert!(frames.len() >= 2, "task frame + final event: {frames:?}");

        let first = &frames[0]["result"]["task"];
        let task_id = first["id"].as_str().unwrap().to_string();
        let context_id = first["contextId"].as_str().unwrap().to_string();
        assert_eq!(context_id, task_id);
        for frame in &frames {
            let result = &frame["result"];
            let id = result
                .get("task")
                .map(|t| &t["id"])
                .or_else(|| result.get("statusUpdate").map(|s| &s["taskId"]))
                .or_else(|| result.get("artifactUpdate").map(|a| &a["taskId"]))
                .unwrap();
            assert_eq!(id, &Value::String(task_id.clone()), "frame {result}");
        }
        let last = frames.last().unwrap();
        assert_eq!(last["result"]["statusUpdate"]["final"], true);
        assert_eq!(
            last["result"]["statusUpdate"]["status"]["state"],
            "TASK_STATE_FAILED"
        );
    }

    #[tokio::test]
    async fn resolved_tasks_stay_retrievable_and_stream_their_snapshot() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;

        // A persisted task with a recorded turn (as if it had run).
        let store = store::TaskStore::new(ctx.pool.clone());
        store
            .record_turn_start(&session_id, &session_id, "a1")
            .await
            .unwrap();
        store
            .record_turn_end(
                &session_id,
                1,
                store::TurnRecord {
                    state: TaskState::InputRequired,
                    status_message: None,
                    response: "4",
                    artifact_id: "a1",
                    completion: Some(&serde_json::json!({"cost_usd": 0.0})),
                },
            )
            .await
            .unwrap();

        let task = result_value(&a2a, "GetTask", serde_json::json!({"id": session_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
        assert_eq!(task["contextId"], session_id);
        assert_eq!(task["artifacts"][0]["parts"][0]["text"], "4");
        assert_eq!(task["metadata"]["cost_usd"], 0.0);

        // Subscribing to a task with no running turn: snapshot + closing
        // status.
        let response = a2a
            .handle(rpc_request(
                "SubscribeToTask",
                serde_json::json!({"id": session_id}),
                Some("1.0"),
            ))
            .await;
        let frames = sse_frames(response).await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["result"]["task"]["id"], session_id);
        assert_eq!(frames[1]["result"]["statusUpdate"]["final"], true);
    }

    #[tokio::test]
    async fn follow_up_resumes_the_conversation_and_continuation_opens_a_new_instance() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;

        // Follow-up on the canonical task id: same task, next turn (which
        // fails against the dead provider and resolves the task).
        let task = result_value(
            &a2a,
            "SendMessage",
            send_params("and 3 more?", Some(&session_id), None, None, false),
        )
        .await;
        assert_eq!(task["id"], session_id);
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");

        // Continuation: contextId only — a new immutable instance on the
        // same conversation.
        let task = result_value(
            &a2a,
            "SendMessage",
            send_params("retry please", None, Some(&session_id), None, false),
        )
        .await;
        let instance_id = task["id"].as_str().unwrap().to_string();
        assert_ne!(instance_id, session_id);
        assert!(instance_id.starts_with(&format!("{session_id}~")));
        assert_eq!(task["contextId"], session_id);

        // Both instances are persisted: retrievable, and immutable —
        // reopening one is refused with UnsupportedOperation.
        for id in [&session_id, &instance_id] {
            let task = result_value(&a2a, "GetTask", serde_json::json!({"id": id})).await;
            assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
            let (code, _) = error_code(
                &a2a,
                rpc_request(
                    "SendMessage",
                    send_params("reopen?", Some(id), None, None, false),
                    Some("1.0"),
                ),
            )
            .await;
            assert_eq!(code, -32004);
        }

        // The conversation itself continues via another fresh instance.
        let task = result_value(
            &a2a,
            "SendMessage",
            send_params("and again", None, Some(&session_id), None, false),
        )
        .await;
        assert_ne!(task["id"].as_str().unwrap(), instance_id);
        assert_eq!(task["contextId"], session_id);
    }

    #[tokio::test]
    async fn second_turn_on_a_busy_conversation_is_unsupported() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::create(ctx.pool.clone(), tmp.path()).await.unwrap();
        let session_id = session.id.to_string();
        // Held under the MCP door's label: the gate is shared across doors.
        let _guard = ctx.turns.try_acquire(&session_id, "mcp:fake").unwrap();

        let (code, _) = error_code(
            &a2a,
            rpc_request(
                "SendMessage",
                send_params("hello", Some(&session_id), None, None, false),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32004);
    }

    #[tokio::test]
    async fn unknown_task_ids_are_not_created() {
        let (a2a, _ctx) = test_a2a(false).await;
        for params in [
            send_params("hi", Some("zzzzzz"), None, None, false),
            send_params("hi", None, Some("zzzzzz"), None, false),
        ] {
            let (code, _) = error_code(&a2a, rpc_request("SendMessage", params, Some("1.0"))).await;
            assert_eq!(code, -32001);
        }
    }

    #[tokio::test]
    async fn cancel_finalizes_a_running_turn_and_resolves_it() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::create(ctx.pool.clone(), tmp.path()).await.unwrap();
        let session_id = session.id.to_string();
        let store = store::TaskStore::new(ctx.pool.clone());
        store
            .record_turn_start(&session_id, &session_id, "a1")
            .await
            .unwrap();

        // A stand-in driver: blocks until the cancel signal, then finishes
        // the turn exactly like the real driver does.
        let (cancel_tx, mut cancel_rx) = watch::channel(());
        let live = Arc::new(LiveTask {
            task_id: session_id.clone(),
            context_id: session_id.clone(),
            running: AtomicBool::new(true),
            final_state: StdMutex::new(None),
            response: StdMutex::new("partial".into()),
            status_message: StdMutex::new(None),
            completion: StdMutex::new(None),
            artifact_id: "a1".into(),
            turn: 1,
            events: broadcast::channel(16).0,
            cancel: StdMutex::new(Some(cancel_tx)),
            turn_epoch: watch::channel(0).0,
            push: StdMutex::new(None),
        });
        a2a.turns.insert(Arc::clone(&live));
        let turns = Arc::clone(&a2a.turns);
        let driver_live = Arc::clone(&live);
        tokio::spawn(async move {
            cancel_rx.changed().await.ok();
            end_turn(&turns, &driver_live, &store, TaskState::Canceled, None).await;
        });

        let task = result_value(&a2a, "CancelTask", serde_json::json!({"id": session_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");

        // Resolved: cancel again is refused, but the record stays.
        let (code, _) = error_code(
            &a2a,
            rpc_request(
                "CancelTask",
                serde_json::json!({"id": session_id}),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32002);
        let task = result_value(&a2a, "GetTask", serde_json::json!({"id": session_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");
    }

    #[tokio::test]
    async fn cancel_resolves_an_idle_conversation_once() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;
        let store = store::TaskStore::new(ctx.pool.clone());
        store
            .record_turn_start(&session_id, &session_id, "a1")
            .await
            .unwrap();
        store
            .record_turn_end(
                &session_id,
                1,
                store::TurnRecord {
                    state: TaskState::InputRequired,
                    status_message: None,
                    response: "4",
                    artifact_id: "a1",
                    completion: None,
                },
            )
            .await
            .unwrap();

        // The task is idle (input required) but exists: cancelling it
        // resolves the conversation.
        let task = result_value(&a2a, "CancelTask", serde_json::json!({"id": session_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");

        // Resolved: cancel again is refused; the task stays retrievable;
        // a follow-up cannot reopen it.
        let (code, _) = error_code(
            &a2a,
            rpc_request(
                "CancelTask",
                serde_json::json!({"id": session_id}),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32002);
        let task = result_value(&a2a, "GetTask", serde_json::json!({"id": session_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");
        let (code, _) = error_code(
            &a2a,
            rpc_request(
                "SendMessage",
                send_params("hi", Some(&session_id), None, None, false),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32004);
    }

    #[tokio::test]
    async fn two_conversations_stream_independently() {
        let (a2a, _ctx) = test_a2a(false).await;
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let response_a = a2a
            .handle(rpc_request(
                "SendStreamingMessage",
                send_params(
                    "one",
                    None,
                    None,
                    Some(first.path().to_str().unwrap()),
                    false,
                ),
                Some("1.0"),
            ))
            .await;
        let response_b = a2a
            .handle(rpc_request(
                "SendStreamingMessage",
                send_params(
                    "two",
                    None,
                    None,
                    Some(second.path().to_str().unwrap()),
                    false,
                ),
                Some("1.0"),
            ))
            .await;
        let frames_a = sse_frames(response_a).await;
        let frames_b = sse_frames(response_b).await;

        let id_a = frames_a[0]["result"]["task"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let id_b = frames_b[0]["result"]["task"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(id_a, id_b);
        assert_ne!(
            frames_a[0]["result"]["task"]["contextId"],
            frames_b[0]["result"]["task"]["contextId"]
        );
        for (frames, id) in [(frames_a, &id_a), (frames_b, &id_b)] {
            let last = frames.last().unwrap();
            assert_eq!(last["result"]["statusUpdate"]["final"], true);
            assert_eq!(last["result"]["statusUpdate"]["taskId"], *id);
        }
    }

    #[tokio::test]
    async fn tasks_and_configs_survive_a_restart() {
        let (a2a, ctx) = test_a2a(false).await;
        let (url, _rx) = spawn_webhook().await;
        let tmp = tempfile::tempdir().unwrap();
        let mut params = send_params(
            "hello",
            None,
            None,
            Some(tmp.path().to_str().unwrap()),
            true,
        );
        params["configuration"]["pushNotificationConfig"] =
            serde_json::json!({"url": url, "token": "tok"});
        let task = result_value(&a2a, "SendMessage", params).await;
        let task_id = task["id"].as_str().unwrap().to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let task = result_value(&a2a, "GetTask", serde_json::json!({"id": task_id})).await;
            if task["status"]["state"] != "TASK_STATE_WORKING" {
                break;
            }
            assert!(Instant::now() < deadline, "task never settled");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // "Restart": a fresh daemon instance over the same database.
        let restarted = A2a::new(ctx.clone(), false);
        let task = result_value(&restarted, "GetTask", serde_json::json!({"id": task_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
        let config = result_value(
            &restarted,
            "GetTaskPushNotificationConfig",
            serde_json::json!({"id": task_id}),
        )
        .await;
        assert_eq!(config["pushNotificationConfig"]["url"], url);
        let (code, _) = error_code(
            &restarted,
            rpc_request(
                "SendMessage",
                send_params("hi", Some(&task_id), None, None, false),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32004);
    }

    #[tokio::test]
    async fn stale_working_tasks_fail_at_startup() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;
        let store = store::TaskStore::new(ctx.pool.clone());
        store
            .record_turn_start(&session_id, &session_id, "a1")
            .await
            .unwrap();

        // The startup sweep reports interrupted turns as failed.
        let failed = store.fail_stale_working().await.unwrap();
        assert_eq!(failed, 1);
        let task = result_value(&a2a, "GetTask", serde_json::json!({"id": session_id})).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
    }

    /// A local webhook: accepts POSTs forever, answers 200, and streams
    /// each captured (method, request head, body) to the receiver.
    async fn spawn_webhook() -> (String, mpsc::Receiver<(String, String, String)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut sock = sock;
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let header_end = loop {
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                        if n == 0 {
                            return;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let mut body = buf[header_end..].to_vec();
                    while body.len() < content_length {
                        let n = sock.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&tmp[..n]);
                    }
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = sock.flush().await;
                    let method = head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().next())
                        .unwrap_or_default()
                        .to_string();
                    tx.send((method, head, String::from_utf8_lossy(&body).to_string()))
                        .await
                        .ok();
                });
            }
        });
        (format!("http://{addr}/push"), rx)
    }

    #[tokio::test]
    async fn inline_push_config_delivers_the_final_task_snapshot() {
        let (a2a, _ctx) = test_a2a(false).await;
        let (url, mut rx) = spawn_webhook().await;
        let tmp = tempfile::tempdir().unwrap();

        let mut params = send_params(
            "hello",
            None,
            None,
            Some(tmp.path().to_str().unwrap()),
            false,
        );
        params["configuration"]["pushNotificationConfig"] = serde_json::json!({
            "url": url,
            "token": "secret42",
        });
        let handle = a2a
            .handle(rpc_request("SendStreamingMessage", params, Some("1.0")))
            .await;
        let _ = handle; // the stream may end before/after deliveries; the webhook is the contract

        // Deliveries: the turn-start snapshot, then the final snapshot
        // with the resolved state. The dead provider means FAILED.
        let deadline = Instant::now() + Duration::from_secs(5);
        let (method, head, body) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(method, "POST");
        assert!(
            head.to_ascii_lowercase()
                .contains("x-a2a-notification-token: secret42"),
            "{head}"
        );
        let first: Value = serde_json::from_str(&body).unwrap();
        let _task_id = first["task"]["id"].as_str().unwrap().to_string();
        // The turn may already have resolved before the first delivery
        // was captured; walk frames until the terminal state arrives.
        if first["task"]["status"]["state"] == "TASK_STATE_FAILED" {
            return;
        }

        loop {
            let (_, _, body) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let frame: Value = serde_json::from_str(&body).unwrap();
            let state = frame["task"]["status"]["state"].as_str();
            if state == Some("TASK_STATE_FAILED") {
                break;
            }
            assert!(Instant::now() < deadline, "final push never arrived");
        }
    }

    #[tokio::test]
    async fn push_delivery_echoes_the_client_token() {
        let (a2a, _ctx) = test_a2a(false).await;
        let (url, mut rx) = spawn_webhook().await;
        let tmp = tempfile::tempdir().unwrap();

        let mut params = send_params(
            "hello",
            None,
            None,
            Some(tmp.path().to_str().unwrap()),
            true,
        );
        params["configuration"]["pushNotificationConfig"] = serde_json::json!({
            "url": url,
            "token": "secret42",
        });
        let task = result_value(&a2a, "SendMessage", params).await;
        let task_id = task["id"].as_str().unwrap().to_string();

        let (_, head, _) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            head.to_ascii_lowercase()
                .contains("x-a2a-notification-token: secret42"),
            "{head}"
        );

        // The turn settles; the durable record then reports the failure.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let task = result_value(&a2a, "GetTask", serde_json::json!({"id": task_id})).await;
            if task["status"]["state"] != "TASK_STATE_WORKING" {
                assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
                break;
            }
            assert!(Instant::now() < deadline, "task never settled");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn create_and_get_push_config_attach_to_a_running_turn() {
        let (a2a, ctx) = test_a2a(false).await;
        let (url, mut rx) = spawn_webhook().await;
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::create(ctx.pool.clone(), tmp.path()).await.unwrap();
        let session_id = session.id.to_string();
        let store = store::TaskStore::new(ctx.pool.clone());
        store
            .record_turn_start(&session_id, &session_id, "a1")
            .await
            .unwrap();

        // A stand-in turn: never finishes until cancelled, exactly like a
        // real in-flight turn from the registry's perspective.
        let (cancel_tx, mut cancel_rx) = watch::channel(());
        let live = Arc::new(LiveTask {
            task_id: session_id.clone(),
            context_id: session_id.clone(),
            running: AtomicBool::new(true),
            final_state: StdMutex::new(None),
            response: StdMutex::new("partial".into()),
            status_message: StdMutex::new(None),
            completion: StdMutex::new(None),
            artifact_id: "a1".into(),
            turn: 1,
            events: broadcast::channel(16).0,
            cancel: StdMutex::new(Some(cancel_tx)),
            turn_epoch: watch::channel(0).0,
            push: StdMutex::new(None),
        });
        a2a.turns.insert(Arc::clone(&live));
        let turns = Arc::clone(&a2a.turns);
        let driver_live = Arc::clone(&live);
        tokio::spawn(async move {
            cancel_rx.changed().await.ok();
            end_turn(&turns, &driver_live, &store, TaskState::Canceled, None).await;
        });

        let created = result_value(
            &a2a,
            "CreateTaskPushNotificationConfig",
            serde_json::json!({
                "taskId": session_id,
                "pushNotificationConfig": { "url": url, "token": "tok" },
            }),
        )
        .await;
        assert_eq!(created["taskId"], session_id);
        assert_eq!(created["pushNotificationConfig"]["url"], url);

        let got = result_value(
            &a2a,
            "GetTaskPushNotificationConfig",
            serde_json::json!({"id": session_id}),
        )
        .await;
        assert_eq!(got["pushNotificationConfig"]["url"], url);

        // Cancelling the turn ends it; the webhook receives the final
        // CANCELED snapshot.
        let _ = result_value(&a2a, "CancelTask", serde_json::json!({"id": session_id})).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen_canceled = false;
        while !seen_canceled {
            assert!(Instant::now() < deadline, "final push never arrived");
            let (_, _, body) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let frame: Value = serde_json::from_str(&body).unwrap();
            if frame["task"]["status"]["state"] == "TASK_STATE_CANCELED" {
                seen_canceled = true;
            }
        }
    }

    #[tokio::test]
    async fn push_config_persists_on_an_idle_conversation() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;
        let (url, _rx) = spawn_webhook().await;

        // Configs are durable: attach one while no turn is running and it
        // is still there afterwards.
        let created = result_value(
            &a2a,
            "CreateTaskPushNotificationConfig",
            serde_json::json!({
                "taskId": session_id,
                "pushNotificationConfig": { "url": url, "token": "tok" },
            }),
        )
        .await;
        assert_eq!(created["taskId"], session_id);
        assert_eq!(created["pushNotificationConfig"]["url"], url);

        let got = result_value(
            &a2a,
            "GetTaskPushNotificationConfig",
            serde_json::json!({"id": session_id}),
        )
        .await;
        assert_eq!(got["pushNotificationConfig"]["url"], url);

        // An unknown task id is a hard 404.
        let (code, _) = error_code(
            &a2a,
            rpc_request(
                "CreateTaskPushNotificationConfig",
                serde_json::json!({
                    "taskId": "zzzzzz",
                    "pushNotificationConfig": { "url": url },
                }),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32001);
    }

    #[tokio::test]
    async fn push_config_rejects_non_http_urls() {
        let (a2a, _ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let mut params = send_params(
            "hello",
            None,
            None,
            Some(tmp.path().to_str().unwrap()),
            true,
        );
        params["configuration"]["pushNotificationConfig"] =
            serde_json::json!({ "url": "ftp://example.invalid/push" });
        let (code, _) = error_code(&a2a, rpc_request("SendMessage", params, Some("1.0"))).await;
        assert_eq!(code, -32602);
    }

    /// Three conversations with one recorded turn each; the second also
    /// gets a continuation instance (same context, new task id). Spread
    /// the `updated_at` stamps so the `ListTasks` ordering is deterministic.
    async fn seed_task_rows(ctx: &AppContext) -> Vec<(String, String, &'static str)> {
        let mut rows = Vec::new();
        for (n, cwd_age) in [(0i64, "a"), (1, "b"), (2, "c")] {
            let tmp = tempfile::tempdir().unwrap();
            let session_id = seed_conversation(ctx, tmp.path()).await;
            let store = store::TaskStore::new(ctx.pool.clone());
            let mut instances = vec![session_id.clone()];
            if n == 1 {
                instances.push(format!("{session_id}~deadbeef"));
            }
            for instance in &instances {
                store
                    .record_turn_start(instance, &session_id, &format!("art-{n}"))
                    .await
                    .unwrap();
                store
                    .record_turn_end(
                        instance,
                        1,
                        store::TurnRecord {
                            state: TaskState::InputRequired,
                            status_message: None,
                            response: &format!("answer {cwd_age}"),
                            artifact_id: &format!("art-{n}"),
                            completion: None,
                        },
                    )
                    .await
                    .unwrap();
                sqlx::query("UPDATE a2a_tasks SET updated_at = ? WHERE id = ?")
                    .bind(1_700_000_000_000i64 + n)
                    .bind(instance)
                    .execute(&*ctx.pool)
                    .await
                    .unwrap();
                rows.push((instance.clone(), session_id.clone(), cwd_age));
            }
        }
        rows
    }

    #[tokio::test]
    async fn list_tasks_paginates_newest_first() {
        let (a2a, ctx) = test_a2a(false).await;
        let rows = seed_task_rows(&ctx).await;

        let page = result_value(&a2a, "ListTasks", serde_json::json!({})).await;
        assert_eq!(page["totalSize"], 4);
        let ids: Vec<&str> = page["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_str().unwrap())
            .collect();
        // Newest activity first: the continuation instance shares its
        // conversation's stamps, so ties break on id — both rows of the
        // middle conversation must be present, and none of the tasks is
        // missing.
        assert_eq!(ids.len(), 4);
        assert!(ids.contains(&rows[2].0.as_str()));
        assert_eq!(page.get("nextPageToken"), None);

        // Two per page: the first page carries a cursor for the rest.
        let mut page = result_value(&a2a, "ListTasks", serde_json::json!({ "pageSize": 2 })).await;
        assert_eq!(page["tasks"].as_array().unwrap().len(), 2);
        let token = page["nextPageToken"].as_str().unwrap().to_owned();
        page = result_value(
            &a2a,
            "ListTasks",
            serde_json::json!({ "pageSize": 2, "pageToken": token }),
        )
        .await;
        let rest = page["tasks"].as_array().unwrap().len();
        assert_eq!(rest, 2);
        assert_eq!(page["totalSize"], 4);
        assert_eq!(page.get("nextPageToken"), None);

        // Malformed inputs answer invalid params, not a page.
        for params in [
            serde_json::json!({ "pageToken": "garbage" }),
            serde_json::json!({ "status": "TASK_STATE_COMPLETED" }),
            serde_json::json!({ "statusTimestampAfter": "yesterday" }),
        ] {
            let (code, _) = error_code(&a2a, rpc_request("ListTasks", params, Some("1.0"))).await;
            assert_eq!(code, -32602);
        }
    }

    #[tokio::test]
    async fn list_tasks_filters_by_context_status_and_time() {
        let (a2a, ctx) = test_a2a(false).await;
        let rows = seed_task_rows(&ctx).await;

        let (first, first_context, _) = &rows[0];
        let page = result_value(
            &a2a,
            "ListTasks",
            serde_json::json!({ "contextId": first_context }),
        )
        .await;
        assert_eq!(page["totalSize"], 1);
        assert_eq!(page["tasks"][0]["id"], *first);

        // The continuation instance belongs to the same conversation.
        let (_, middle_context, _) = &rows[1];
        let page = result_value(
            &a2a,
            "ListTasks",
            serde_json::json!({ "contextId": middle_context }),
        )
        .await;
        assert_eq!(page["totalSize"], 2);

        // One row into the future leaves nothing.
        let after = chrono::DateTime::from_timestamp(1_700_000_002, 0).unwrap();
        let page = result_value(
            &a2a,
            "ListTasks",
            serde_json::json!({
                "statusTimestampAfter": after.to_rfc3339(),
                "includeArtifacts": true,
            }),
        )
        .await;
        assert_eq!(page["totalSize"], 0);
        assert_eq!(page["tasks"].as_array().unwrap().len(), 0);

        // Artifacts ride along only when asked for.
        let page = result_value(
            &a2a,
            "ListTasks",
            serde_json::json!({ "contextId": first_context, "includeArtifacts": true }),
        )
        .await;
        assert_eq!(
            page["tasks"][0]["artifacts"][0]["parts"][0]["text"],
            "answer a"
        );
        let page = result_value(
            &a2a,
            "ListTasks",
            serde_json::json!({ "contextId": first_context }),
        )
        .await;
        assert!(page["tasks"][0].get("artifacts").is_none());
    }

    #[tokio::test]
    async fn get_task_serves_history_per_history_length() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;
        store::TaskStore::new(ctx.pool.clone())
            .record_turn_start(&session_id, &session_id, "a1")
            .await
            .unwrap();

        // Default: the whole transcript, oldest first.
        let task = result_value(&a2a, "GetTask", serde_json::json!({ "id": session_id })).await;
        assert_eq!(task["history"].as_array().unwrap().len(), 2);
        assert_eq!(task["history"][0]["role"], "ROLE_USER");
        assert_eq!(task["history"][0]["parts"][0]["text"], "what is 2+2");
        assert_eq!(task["history"][1]["role"], "ROLE_AGENT");

        // One message: the most recent.
        let task = result_value(
            &a2a,
            "GetTask",
            serde_json::json!({ "id": session_id, "historyLength": 1 }),
        )
        .await;
        let history = task["history"].as_array().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["parts"][0]["text"], "4");

        // Zero: no history at all.
        let task = result_value(
            &a2a,
            "GetTask",
            serde_json::json!({ "id": session_id, "historyLength": 0 }),
        )
        .await;
        assert!(task.get("history").is_none());
    }

    #[tokio::test]
    async fn duplicate_message_id_replays_the_same_task() {
        let (a2a, _ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let message_id = uuid::Uuid::new_v4().to_string();
        let send = |text: &str| {
            serde_json::json!({
                "message": {
                    "role": "ROLE_USER",
                    "parts": [{ "text": text }],
                    "messageId": message_id,
                    "metadata": { "cwd": tmp.path().to_str().unwrap() },
                },
                "configuration": { "returnImmediately": true },
            })
        };

        let first = result_value(&a2a, "SendMessage", send("first prompt")).await;
        let task_id = first["id"].as_str().unwrap().to_owned();

        // Same messageId, different text and no task reference: no new
        // turn, no new conversation — the original task replays.
        let replay = result_value(&a2a, "SendMessage", send("second prompt")).await;
        assert_eq!(replay["id"], task_id);

        // The streaming door dedupes identically: the replay stream is
        // the task's snapshot plus its closing status.
        let frames = sse_frames(
            a2a.handle(rpc_request(
                "SendStreamingMessage",
                send("third"),
                Some("1.0"),
            ))
            .await,
        )
        .await;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["result"]["task"]["id"], task_id);

        // And a different messageId is a brand-new conversation.
        let other = result_value(
            &a2a,
            "SendMessage",
            serde_json::json!({
                "message": {
                    "role": "ROLE_USER",
                    "parts": [{ "text": "fresh" }],
                    "messageId": uuid::Uuid::new_v4().to_string(),
                    "metadata": { "cwd": tmp.path().to_str().unwrap() },
                },
                "configuration": { "returnImmediately": true },
            }),
        )
        .await;
        assert_ne!(other["id"], task_id);
    }

    #[tokio::test]
    async fn extended_card_lists_agent_personas() {
        let (a2a, _ctx) = test_a2a(false).await;
        let card = result_value(&a2a, "GetExtendedAgentCard", serde_json::json!({})).await;
        assert_eq!(card["protocolVersion"], "1.0");
        let skills = card["skills"].as_array().unwrap();
        assert!(
            skills.iter().any(|skill| skill["id"] == "pie-agent"),
            "the base delegation skill is always present: {card}"
        );
        for skill in skills {
            if skill["id"] != "pie-agent" {
                assert_eq!(skill["tags"][0], "persona");
                assert!(!skill["description"].as_str().unwrap_or("").is_empty());
            }
        }

        // The public card announces that an extended one exists.
        let request = Request::builder()
            .method("GET")
            .uri(CARD_PATH)
            .header("host", "localhost:8629")
            .body(Full::new(Bytes::new()))
            .unwrap();
        let card = body_json(a2a.handle(request).await).await;
        assert_eq!(card["capabilities"]["extendedAgentCard"], true);
    }

    #[tokio::test]
    async fn subscribing_to_a_resolved_task_is_unsupported() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;
        let store = store::TaskStore::new(ctx.pool.clone());
        store
            .record_turn_start(&session_id, &session_id, "a1")
            .await
            .unwrap();
        store
            .record_turn_end(
                &session_id,
                1,
                store::TurnRecord {
                    state: TaskState::Failed,
                    status_message: Some("boom"),
                    response: "",
                    artifact_id: "a1",
                    completion: None,
                },
            )
            .await
            .unwrap();

        // GetTask still retrieves the resolved record…
        let task = result_value(&a2a, "GetTask", serde_json::json!({ "id": session_id })).await;
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
        // …but there is nothing to stream.
        let (code, _) = error_code(
            &a2a,
            rpc_request(
                "SubscribeToTask",
                serde_json::json!({ "id": session_id }),
                Some("1.0"),
            ),
        )
        .await;
        assert_eq!(code, -32004);
    }

    #[tokio::test]
    async fn artifacts_accumulate_one_per_turn() {
        let (a2a, ctx) = test_a2a(false).await;
        let tmp = tempfile::tempdir().unwrap();
        let session_id = seed_conversation(&ctx, tmp.path()).await;
        let store = store::TaskStore::new(ctx.pool.clone());

        store
            .record_turn_start(&session_id, &session_id, "art-1")
            .await
            .unwrap();
        store
            .record_turn_end(
                &session_id,
                1,
                store::TurnRecord {
                    state: TaskState::InputRequired,
                    status_message: None,
                    response: "first",
                    artifact_id: "art-1",
                    completion: None,
                },
            )
            .await
            .unwrap();
        // A failed turn with partial output keeps its artifact too.
        store
            .record_turn_start(&session_id, &session_id, "art-2")
            .await
            .unwrap();
        store
            .record_turn_end(
                &session_id,
                2,
                store::TurnRecord {
                    state: TaskState::Failed,
                    status_message: Some("boom"),
                    response: "second",
                    artifact_id: "art-2",
                    completion: None,
                },
            )
            .await
            .unwrap();
        // A turn that produced nothing leaves no artifact.
        store
            .record_turn_start(&session_id, &session_id, "art-3")
            .await
            .unwrap();
        store
            .record_turn_end(
                &session_id,
                3,
                store::TurnRecord {
                    state: TaskState::InputRequired,
                    status_message: None,
                    response: "",
                    artifact_id: "art-3",
                    completion: None,
                },
            )
            .await
            .unwrap();

        let task = result_value(&a2a, "GetTask", serde_json::json!({ "id": session_id })).await;
        let artifacts = task["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 2, "{task}");
        assert_eq!(artifacts[0]["artifactId"], "art-1");
        assert_eq!(artifacts[0]["parts"][0]["text"], "first");
        assert_eq!(artifacts[1]["artifactId"], "art-2");
        assert_eq!(artifacts[1]["parts"][0]["text"], "second");
    }
}
