//! The TUI as an A2A consumer: drives an agent through an in-process
//! [`FrontDoor`] from the a2acp gateway — the same JSON-RPC requests and
//! streaming event sequences an HTTP client would see. The frontend
//! never knows (or cares) that the agent is in process; the client is the
//! only handle it has.
//!
//! ## Request mapping (TUI action → A2A method)
//!
//! | TUI action                  | A2A                                                              |
//! |-----------------------------|------------------------------------------------------------------|
//! | submit / prompt             | `SendStreamingMessage` (client-minted `taskId` + `contextId`, `metadata.agent`, `metadata.cwd` on a fresh conversation; the selection extension's opt-in + payload when a selection is pending) |
//! | cancel (Esc/Ctrl-C)         | `CancelTask` on the running task                                 |
//! | permission answer           | `SendMessage` on the parked task with `metadata.permissionOptionId` |
//! | `/new`                      | nothing on the wire — drop the `contextId`, so the next prompt starts a fresh conversation |
//!
//! ## Event mapping (stream frame → [`StreamEvent`])
//!
//! | A2A frame                                       | `StreamEvent`                          |
//! |--------------------------------------------------|----------------------------------------|
//! | `artifactUpdate` (`append: true`)                | `Delta` (the chunk text)               |
//! | `artifactUpdate` (final, `append: false`)        | the completed turn's text for `Done`   |
//! | `statusUpdate` `TASK_STATE_WORKING` with a `toolCall` data part | `ToolCall` (pre-half: the title; post-half: the output, paired by the call's real id) |
//! | `statusUpdate` `TASK_STATE_WORKING`, text only   | `ToolCall` (a flattened status line)   |
//! | `statusUpdate` `TASK_STATE_INPUT_REQUIRED`       | `PermissionAsk` (the data-part convention from a2acp's docs/A2A.md) |
//! | final `statusUpdate` `COMPLETED`/`FAILED`/`CANCELED` | `Done` / `Error` / `Error("Cancelled")`; the turn's `usage` data part feeds the session total (no event — the status bar reads the refreshed cache) |
//!
//! Session usage is never accumulated here: after a turn settles the
//! conversation's task history is fetched and the per-turn `usage` data
//! parts summed ([`Client::refresh_usage`]) — a stateless client that
//! survives daemon restarts and `/new`.
//!
//! Deliberate gaps over the bridge (TODO(a2acp)): fine-grained tool-call
//! output beyond the flattened status line has no A2A shape, and
//! pie-specific wants — `!shell` escapes — are answered by the caller
//! with a "not available over the bridge" notice instead of fake
//! success. Model and mode selection go through the selection extension
//! (below) — no side channels.

use crate::realm::{AskId, SessionId, StreamEvent};
use a2acp::FrontDoor;
use a2acp::a2a::FrontReply;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// The TUI's gateway connection: the in-process front door (the TUI
/// assembled its own gateway) or the HTTP door onto a running pie
/// daemon. Both speak the same protocol; every request the client makes
/// goes through here.
#[derive(Debug, Clone)]
pub enum Door {
    /// Same-process gateway (`FrontDoor` from `Gateway::connect`).
    InProcess(FrontDoor),
    /// Same-machine daemon over HTTP (`a2acp::HttpDoor`).
    Http(a2acp::HttpDoor),
}

impl Door {
    /// One JSON-RPC call against the gateway.
    ///
    /// # Errors
    ///
    /// Transport errors surface only on the HTTP leg; the in-process
    /// door cannot fail.
    pub async fn call(&self, body: Value) -> anyhow::Result<FrontReply> {
        match self {
            Door::InProcess(door) => Ok(door.call(body).await),
            Door::Http(door) => door.call(body).await,
        }
    }

    /// The agent card (the directory).
    pub async fn card(&self) -> Value {
        match self {
            Door::InProcess(door) => door.card(),
            Door::Http(door) => door.card().await.unwrap_or(Value::Null),
        }
    }

    /// Warm start: open the agent's session before any message.
    /// `None` = the daemon refused; the lazy first-prompt open remains.
    pub async fn warm(&self, agent: &str, cwd: &std::path::Path) -> Option<String> {
        match self {
            Door::InProcess(door) => door.warm(agent, cwd).await,
            Door::Http(door) => door.warm(agent, cwd).await,
        }
    }

    /// The conversation's confirmed selection (the read-back).
    pub async fn selection(&self, context_id: &str) -> Option<a2acp::a2a::ConversationSelection> {
        match self {
            Door::InProcess(door) => door.selection(context_id),
            Door::Http(door) => door.selection(context_id).await,
        }
    }
}
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc;

/// URI of the **selection extension** — the spec-sanctioned A2A
/// `AgentExtension` this client opts into per message when a selection
/// is pending (mode and/or model), with the payload under the same key
/// in the message's metadata.
pub const SELECTION_EXTENSION_URI: &str = "https://qreta.io/a2acp/extensions/selection/v1";

/// URI of the **usage extension** — the key the agent's `usage` object
/// carries its supplements (request count, cost) under, inside the
/// object's `_meta`. The same convention names the `usage` data part on
/// the turn's final A2A status (docs/A2A.md).
pub const USAGE_EXTENSION_URI: &str = "https://qreta.io/a2acp/extensions/usage/v1";

/// A selection the next outgoing message carries: the mode and/or model
/// leg of the selection extension, either optional. Composed onto the
/// turn that applies it; the conversation on the agent side remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub mode: Option<String>,
    pub model: Option<String>,
}

impl Selection {
    /// Whether any leg is set — the gate for the extension opt-in (an
    /// empty selection must leave the wire untouched).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mode.is_none() && self.model.is_none()
    }

    /// The extension's typed payload for this selection.
    fn payload(&self) -> Value {
        let mut payload = serde_json::Map::new();
        if let Some(mode) = &self.mode {
            payload.insert("mode".into(), json!(mode));
        }
        if let Some(model) = &self.model {
            payload.insert("model".into(), json!(model));
        }
        Value::Object(payload)
    }
}

/// One mode the agent advertises on the card (the selection extension's
/// per-agent report — empty until the agent's first session on the
/// gateway reported its modes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeOption {
    pub id: String,
    pub description: String,
}

/// One selectable model: the id the selection extension carries and the
/// model it resolves to. `id` is `"default"` (the startup provider) or a
/// configured tier name; the agent resolves it the same way the roster
/// resolves an agent's `model:` — a tier name wins wholesale, a literal
/// model id rides the current provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub model: String,
}

/// The provider/model catalog pie passes the TUI as plain startup data:
/// the default entry first, then one per configured `[model.<name>]`
/// tier in name order. What the picker offers is what the agent accepts
/// (plus the catalog's model ids as literals).
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    pub entries: Vec<CatalogEntry>,
}

impl ModelCatalog {
    /// Whether `id` is selectable — an entry id or one of the catalog's
    /// model ids (accepted as a literal on the current provider).
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.id == id || entry.model == id)
    }
}

/// Mints the JSON-RPC envelope ids of the door calls. Wire ids (tasks,
/// contexts, messages) are minted by [`mint`] instead — they outlive the
/// process (the gateway persists them), so they must be unique across
/// restarts, which a reset-every-launch counter is not.
static ENVELOPE_IDS: AtomicU64 = AtomicU64::new(0);

/// A fresh wire id (`t-…`, `c-…`, `m-…`): time-ordered, unique across
/// restarts. The gateway persists tasks and contexts; a re-minted id
/// addresses the OLD turn — it replays a dead conversation instead of
/// starting a new one.
fn mint(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

/// One option of a parked permission ask (`{id, name, kind}`, the flat
/// list the `INPUT_REQUIRED` data part carries).
#[derive(Debug, Clone)]
struct AskOption {
    id: String,
    name: String,
    kind: String,
}

/// The TUI's A2A client: sends the requests above, projects the frames
/// above. Every method is fire-and-forget — outcomes arrive on the event
/// stream handed out by [`open`].
pub struct Client {
    door: Door,
    agent: String,
    cwd: PathBuf,
    events: mpsc::UnboundedSender<StreamEvent>,
    /// The conversation the next prompt continues; `None` starts a fresh
    /// one on the next prompt.
    context: Arc<StdMutex<Option<String>>>,
    /// The turn in flight — the `CancelTask` target.
    task: StdMutex<Option<String>>,
    /// Parked permission asks keyed by their task id, shared with the
    /// stream pumps that park them.
    asks: Arc<StdMutex<HashMap<String, Vec<AskOption>>>>,
    /// The cached agent card — the mode list for the pickers, refreshed
    /// by [`Client::refresh_card`] and the startup warm (the card is
    /// live state: an agent appears in it only after a session reported
    /// its modes).
    card: Arc<StdMutex<Value>>,
    /// The conversation's confirmed selection, as the gateway last
    /// reported it — the mode bar's read-back, refreshed by
    /// [`Client::refresh_selection`] after each settled turn.
    selection: Arc<StdMutex<Selection>>,
    /// The conversation's session usage summary (tokens + spend), summed
    /// over the task history by [`Client::refresh_usage`] — the status
    /// bar's read-back. `None` before the first billed turn.
    usage: Arc<StdMutex<Option<String>>>,
    /// Whether the startup warm has fired — one warm per client; a
    /// second call is a no-op (each warm would mint a conversation).
    warming: AtomicBool,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("agent", &self.agent)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// Wire a [`Client`] to a gateway door: requests go through the
/// returned handle, and every projected turn arrives on the returned
/// stream. The door is in-process or HTTP (`Door`).
pub async fn open(
    door: Door,
    agent: impl Into<String>,
    cwd: PathBuf,
) -> (Client, mpsc::UnboundedReceiver<StreamEvent>) {
    let (events, stream) = mpsc::unbounded_channel();
    let card = Arc::new(StdMutex::new(door.card().await));
    (
        Client {
            door,
            agent: agent.into(),
            cwd,
            events,
            context: Arc::new(StdMutex::new(None)),
            task: StdMutex::new(None),
            asks: Arc::new(StdMutex::new(HashMap::new())),
            card,
            selection: Arc::new(StdMutex::new(Selection::default())),
            usage: Arc::new(StdMutex::new(None)),
            warming: AtomicBool::new(false),
        },
        stream,
    )
}

fn lock<T>(lock: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Client {
    fn next_id() -> u64 {
        ENVELOPE_IDS.fetch_add(1, Ordering::Relaxed)
    }

    fn error(&self, message: impl Into<String>) {
        let _ = self.events.send(StreamEvent::Error(message.into()));
    }

    /// Run one turn of the current conversation: `SendStreamingMessage`
    /// with a client-minted task id (so `cancel` can address the turn
    /// before the first frame arrives) and the conversation's context
    /// id, minting both the conversation and its `metadata.cwd` when
    /// this is the first prompt after `/new` (or launch). A pending
    /// selection rides the message: the extension opt-in plus the typed
    /// payload under the extension uri (strictly additive — an empty
    /// selection leaves the body exactly as it is today).
    pub fn prompt(&self, query: &str, selection: &Selection) {
        let n = Self::next_id();
        let task_id = mint("t");
        // Bind the cloned context out of the guard before branching:
        // a scrutinee temporary would hold the lock through the arms,
        // and the fresh arm re-locks it.
        let existing = lock(&self.context).clone();
        let (context, fresh) = if let Some(context) = existing {
            (context, false)
        } else {
            let context = mint("c");
            *lock(&self.context) = Some(context.clone());
            (context, true)
        };
        *lock(&self.task) = Some(task_id.clone());
        let message_id = mint("m");

        let body = turn_body(
            n,
            &task_id,
            &context,
            &message_id,
            &self.agent,
            fresh,
            &self.cwd,
            query,
            selection,
        );
        let door = self.door.clone();
        let events = self.events.clone();
        let asks = Arc::clone(&self.asks);
        tokio::spawn(async move {
            match door.call(body).await {
                Ok(FrontReply::Stream(frames)) => {
                    pump(frames, events, asks, task_id).await;
                }
                Ok(FrontReply::Envelope(envelope)) => {
                    let message = rpc_error(&envelope).unwrap_or_else(|| "turn failed".into());
                    let _ = events.send(StreamEvent::Error(message));
                }
                Err(e) => {
                    let _ = events.send(StreamEvent::Error(format!("gateway unreachable: {e}")));
                }
            }
        });
    }

    /// The conversation's confirmed selection, as the gateway holds it —
    /// the cached read-back. Fresh values arrive on the spawned
    /// [`Client::refresh_selection`] task and surface on the next call.
    #[must_use]
    pub fn selection(&self) -> Selection {
        lock(&self.selection).clone()
    }

    /// Sum the conversation's session usage from its task history —
    /// every settled turn's final status carries that turn's `usage`
    /// data part, and the client holds no state of its own, so the
    /// total is always re-derived: fetch the tasks, add the parts up.
    /// Fire-and-forget; a [`StreamEvent::Usage`] pokes the redraw when
    /// the fresh total lands in the cache.
    pub fn refresh_usage(&self) {
        let Some(context) = lock(&self.context).clone() else {
            return;
        };
        let door = self.door.clone();
        let events = self.events.clone();
        let usage = Arc::clone(&self.usage);
        tokio::spawn(async move {
            let mut tasks = Vec::new();
            let mut page_token: Option<String> = None;
            // One page is the whole conversation in practice; the loop
            // only exists so a long session still sums completely.
            for _ in 0..100 {
                let n = Client::next_id();
                let mut params = json!({ "contextId": context, "pageSize": 100 });
                if let Some(token) = &page_token
                    && let Some(object) = params.as_object_mut()
                {
                    object.insert("pageToken".into(), json!(token));
                }
                let reply = door
                    .call(json!({
                        "jsonrpc": "2.0", "id": n, "method": "ListTasks", "params": params,
                    }))
                    .await;
                let Ok(FrontReply::Envelope(envelope)) = reply else {
                    return;
                };
                let result = envelope.get("result").cloned().unwrap_or(Value::Null);
                if let Some(page) = result.get("tasks").and_then(Value::as_array) {
                    tasks.extend(page.iter().cloned());
                }
                page_token = result
                    .get("nextPageToken")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if page_token.is_none() {
                    break;
                }
            }
            *lock(&usage) = session_usage(&tasks);
            let _ = events.send(StreamEvent::Usage);
        });
    }

    /// The cached session usage summary — the status bar's read-back.
    #[must_use]
    pub fn usage(&self) -> Option<String> {
        lock(&self.usage).clone()
    }

    /// Fetch the conversation's confirmed selection from the gateway
    /// (fire-and-forget). On arrival the cache updates and a
    /// [`StreamEvent::Selection`] pokes the TUI to redraw the mode bar.
    pub fn refresh_selection(&self) {
        let Some(context) = lock(&self.context).clone() else {
            return;
        };
        let door = self.door.clone();
        let events = self.events.clone();
        let selection = Arc::clone(&self.selection);
        tokio::spawn(async move {
            if let Some(held) = door.selection(&context).await {
                *lock(&selection) = Selection {
                    mode: held.mode,
                    model: held.model,
                };
                let _ = events.send(StreamEvent::Selection);
            }
        });
    }

    /// The modes the driven agent advertises on the card — filled by the
    /// startup warm (the session opens before the first message) or, if
    /// that failed, after the first turn's session reported them. An
    /// honest empty in between.
    pub fn modes(&self) -> Vec<ModeOption> {
        let card = lock(&self.card).clone();
        advertised_modes(&card, &self.agent)
    }

    /// The mode a fresh session of the driven agent starts in, per the
    /// card's report — what the mode bar shows before any selection.
    pub fn default_mode(&self) -> Option<String> {
        let card = lock(&self.card).clone();
        default_mode(&card, &self.agent)
    }

    /// The models the driven agent advertises on the card — `None` until
    /// its first session reported a model-catalog config select. An
    /// agent without an advertised catalog (pie in process among them)
    /// takes free-form model selections that ride the prompt's
    /// `_meta.model` instead. Entries: `id` is the selectable value,
    /// `model` the display name.
    pub fn models(&self) -> Option<Vec<CatalogEntry>> {
        let card = lock(&self.card).clone();
        advertised_models(&card, &self.agent)
    }

    /// The model a fresh session of the driven agent starts with, per
    /// the card's report — the mode bar's fallback before any selection.
    pub fn default_model(&self) -> Option<String> {
        let card = lock(&self.card).clone();
        default_model(&card, &self.agent)
    }

    /// Re-read the card — live state: after the first turn the driven
    /// agent's session has reported its modes, so the mode list fills in.
    /// Re-read the card — live state: after the first turn the driven
    /// agent's session has reported its modes, so the mode list fills in.
    /// Returns the refresh task handle so tests (and callers that care)
    /// can await the fresh value; normal callers drop it.
    pub fn refresh_card(&self) -> tokio::task::JoinHandle<()> {
        let door = self.door.clone();
        let card = Arc::clone(&self.card);
        tokio::spawn(async move {
            let fresh = door.card().await;
            *lock(&card) = fresh;
        })
    }

    /// Warm the conversation's session before the first message: the
    /// gateway opens the agent's session now, and its `session/new`
    /// report fills the card — modes and models are selectable before
    /// anything is sent. Fire-and-forget: on success the conversation
    /// adopts the warm context (the first prompt runs on it, no second
    /// open), the cached card refreshes, and a [`StreamEvent::Warm`]
    /// pokes the TUI to redraw. Failure is silent and the fallback is
    /// the lazy path — the first prompt opens the session exactly as
    /// before. One warm per client: a second call is a no-op (each warm
    /// would mint its own conversation).
    pub fn warm(&self) {
        if lock(&self.context).is_some() || self.warming.swap(true, Ordering::Relaxed) {
            return;
        }
        let door = self.door.clone();
        let events = self.events.clone();
        let agent = self.agent.clone();
        let cwd = self.cwd.clone();
        let context = Arc::clone(&self.context);
        let card = Arc::clone(&self.card);
        tokio::spawn(async move {
            let Some(context_id) = door.warm(&agent, &cwd).await else {
                return;
            };
            // Adopt the warm conversation unless a prompt minted one
            // before the warm settled.
            {
                let mut conversation = lock(&context);
                if conversation.is_none() {
                    *conversation = Some(context_id);
                }
            }
            let fresh_card = door.card().await;
            *lock(&card) = fresh_card;
            let _ = events.send(StreamEvent::Warm);
        });
    }

    /// Resume an existing gateway conversation (`--resume`): adopt its
    /// context id so the first prompt continues it instead of minting a
    /// fresh one. Runs before any prompt — like the warm adoption, a set
    /// context makes `warm` a no-op, so no extra conversation is minted.
    pub fn resume(&self, context_id: String) {
        *lock(&self.context) = Some(context_id);
    }

    /// Cancel the in-flight turn; the stream delivers the terminal
    /// event when the agent confirms. A no-op when no turn is in flight.
    pub fn cancel(&self) {
        let Some(task) = lock(&self.task).clone() else {
            return;
        };
        let n = Self::next_id();
        let door = self.door.clone();
        tokio::spawn(async move {
            // The reply carries the task snapshot; the outcome arrives
            // on the turn's stream, so nothing is done with it here.
            let _ = door
                .call(json!({
                    "jsonrpc": "2.0", "id": n, "method": "CancelTask",
                    "params": {"id": task},
                }))
                .await;
        });
    }

    /// Answer a parked permission ask: `SendMessage` on the ask's task
    /// naming the chosen option in `metadata.permissionOptionId` (the
    /// `INPUT_REQUIRED` convention). The call settles when the turn ends;
    /// the turn's stream still delivers every following event.
    pub fn answer_permission(&self, id: &AskId, allow: bool) {
        let Some(options) = lock(&self.asks).remove(&id.0) else {
            return;
        };
        let Some(option) = pick_option(&options, allow) else {
            // No option of the wanted kind: leave the ask parked — the
            // gateway's ask timeout answers it on the agent's behalf.
            self.error("no matching permission option; the ask will time out");
            return;
        };
        let n = Self::next_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": n,
            "method": "SendMessage",
            "params": {
                "message": {
                    "role": "ROLE_USER",
                    "parts": [{"text": option.id}],
                    "taskId": id.0.clone(),
                    "metadata": {"permissionOptionId": option.id},
                },
            },
        });
        let door = self.door.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Ok(FrontReply::Envelope(envelope)) = door.call(body).await
                && let Some(message) = rpc_error(&envelope)
            {
                // The ask survives a bad answer; surface why.
                let _ = events.send(StreamEvent::Error(message));
            }
        });
    }

    /// Start a fresh conversation on the next prompt (`/new`): purely
    /// client-side — drop the context id and reset the TUI's views onto
    /// a locally minted display id (the input-history key). The usage
    /// total dies with the conversation it summed.
    pub fn new_session(&self) {
        let n = Self::next_id();
        *lock(&self.context) = None;
        *lock(&self.task) = None;
        *lock(&self.usage) = None;
        let _ = self
            .events
            .send(StreamEvent::SessionSwitched(SessionId::new(format!(
                "new-{n}"
            ))));
    }
}

/// The `SendStreamingMessage` body for one turn: the minted ids,
/// `metadata.agent` always, `metadata.cwd` on a fresh conversation, and
/// — only when a selection is pending — the extension opt-in plus the
/// typed payload under the extension uri. With no selection the body is
/// byte-identical to the pre-extension shape: the extension is strictly
/// additive on the wire.
#[allow(clippy::too_many_arguments)] // one wire body, all of it wire-shaped
fn turn_body(
    n: u64,
    task_id: &str,
    context: &str,
    message_id: &str,
    agent: &str,
    fresh: bool,
    cwd: &std::path::Path,
    query: &str,
    selection: &Selection,
) -> Value {
    let mut metadata = json!({"agent": agent});
    if fresh {
        let cwd = json!(cwd.display().to_string());
        if let Some(object) = metadata.as_object_mut() {
            object.insert("cwd".into(), cwd);
        }
    }
    let mut message = json!({
        "role": "ROLE_USER",
        "parts": [{"text": query}],
        "messageId": message_id,
        "taskId": task_id,
        "contextId": context,
        "metadata": metadata,
    });
    if !selection.is_empty() {
        if let Some(object) = message.as_object_mut() {
            object.insert("extensions".into(), json!([SELECTION_EXTENSION_URI]));
        }
        let payload = selection.payload();
        if let Some(metadata) = message.get_mut("metadata").and_then(Value::as_object_mut) {
            metadata.insert(SELECTION_EXTENSION_URI.into(), payload);
        }
    }
    json!({
        "jsonrpc": "2.0",
        "id": n,
        "method": "SendStreamingMessage",
        "params": {
            "message": message,
            "configuration": {"historyLength": 0},
        },
    })
}

/// The option the boolean answer maps onto: allow picks the first
/// allow-kind option, deny the first reject-kind one. No matching kind
/// answers nothing — a wrong-kind answer would lie about the user's
/// choice, and the ask timeout cancels it honestly.
fn pick_option(options: &[AskOption], allow: bool) -> Option<&AskOption> {
    let wanted = if allow { "allow" } else { "reject" };
    options
        .iter()
        .find(|option| option.kind.to_lowercase().contains(wanted))
}

/// The `error.message` of a JSON-RPC envelope, when it is one.
fn rpc_error(envelope: &Value) -> Option<String> {
    envelope
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The card's selection-extension report for one agent — what its last
/// session on the gateway reported about its selectable state. `None`
/// until that first session reported (a cold card advertises nothing).
fn agent_report<'a>(card: &'a Value, agent: &str) -> Option<&'a Value> {
    card.pointer("/capabilities/extensions")
        .and_then(Value::as_array)?
        .iter()
        .find(|extension| {
            extension.get("uri").and_then(Value::as_str) == Some(SELECTION_EXTENSION_URI)
        })
        .and_then(|extension| extension.pointer("/params/agents"))?
        .get(agent)
}

/// The modes one agent's report advertises.
fn advertised_modes(card: &Value, agent: &str) -> Vec<ModeOption> {
    let Some(report) = agent_report(card, agent) else {
        return Vec::new();
    };
    report
        .get("availableModes")
        .and_then(Value::as_array)
        .map(|modes| {
            modes
                .iter()
                .filter_map(|mode| {
                    Some(ModeOption {
                        id: mode.get("id").and_then(Value::as_str)?.to_owned(),
                        description: mode
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The mode a fresh session starts in, per one agent's report.
fn default_mode(card: &Value, agent: &str) -> Option<String> {
    agent_report(card, agent)?
        .get("currentModeId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The model a fresh session starts with, per one agent's report.
fn default_model(card: &Value, agent: &str) -> Option<String> {
    agent_report(card, agent)?
        .pointer("/models/currentModelId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The models one agent's report advertises (`None` when the agent
/// reported no model catalog). Selectable ids only: `id` is what a
/// selection carries, `model` the display name.
fn advertised_models(card: &Value, agent: &str) -> Option<Vec<CatalogEntry>> {
    let available = agent_report(card, agent)?
        .get("models")?
        .get("availableModels")?
        .as_array()?;
    let entries: Vec<CatalogEntry> = available
        .iter()
        .filter_map(|model| {
            Some(CatalogEntry {
                id: model.get("id").and_then(Value::as_str)?.to_owned(),
                model: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect();
    (!entries.is_empty()).then_some(entries)
}

/// Drive one turn's frame stream to its end, projecting every frame
/// onto the TUI's event vocabulary (the tables in the module docs).
async fn pump(
    mut frames: mpsc::Receiver<String>,
    events: mpsc::UnboundedSender<StreamEvent>,
    asks: Arc<StdMutex<HashMap<String, Vec<AskOption>>>>,
    task: String,
) {
    let mut answer = String::new();
    let mut whole_answer: Option<String> = None;
    let mut line = 0_u64;
    // Tool calls already announced this turn — an update for a seen id is
    // its completion half, not a second call.
    let mut announced: HashSet<String> = HashSet::new();
    while let Some(frame) = frames.recv().await {
        let Ok(value) = serde_json::from_str::<Value>(&frame) else {
            continue;
        };
        let Some(result) = value.get("result") else {
            continue;
        };
        if let Some(artifact) = result.get("artifactUpdate").and_then(|u| u.get("artifact")) {
            let text = artifact_text(artifact);
            if artifact
                .get("append")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                answer.push_str(&text);
                let _ = events.send(StreamEvent::Delta(text));
            } else {
                // The final frame carries the whole answer — the `Done`
                // text when deltas never streamed it.
                whole_answer = Some(text);
            }
        }
        let Some(status) = result.get("statusUpdate") else {
            continue;
        };
        let state = status
            .pointer("/status/state")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let is_final = status
            .get("final")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match state {
            "TASK_STATE_INPUT_REQUIRED" => {
                if let Some(ask) = permission_ask(status) {
                    lock(&asks).insert(task.clone(), ask.options);
                    let _ = events.send(StreamEvent::PermissionAsk {
                        id: AskId(task.clone()),
                        skill: ask.title,
                        permissions: ask.names,
                    });
                }
            }
            "TASK_STATE_WORKING" => {
                // The structured call rides the status message as a data
                // part (`toolCall` — a2acp's convention): pre-half on
                // first sight, output on the update, paired by the call's
                // real id. Text-only statuses still flatten into a line.
                match status_tool_call(status) {
                    Some(call) => emit_tool_call(&events, &mut announced, call),
                    None => {
                        if let Some(text) = status_text(status) {
                            line += 1;
                            let _ = events.send(StreamEvent::ToolCall {
                                id: format!("status-{line}"),
                                name: String::new(),
                                display: text,
                                output: String::new(),
                                failed: false,
                            });
                        }
                    }
                }
            }
            _ if is_final => {
                lock(&asks).remove(&task);
                let event = match state {
                    "TASK_STATE_COMPLETED" => {
                        StreamEvent::Done(whole_answer.unwrap_or(std::mem::take(&mut answer)))
                    }
                    "TASK_STATE_CANCELED" => StreamEvent::Error("Cancelled".into()),
                    _ => StreamEvent::Error(
                        status_text(status).unwrap_or_else(|| "turn failed".into()),
                    ),
                };
                let _ = events.send(event);
                return;
            }
            _ => {}
        }
    }
    // The stream closed without a final status (the front door shut down).
    let _ = events.send(StreamEvent::Error("the agent stream ended".into()));
}

/// The concatenated text parts of an artifact.
fn artifact_text(artifact: &Value) -> String {
    artifact
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// One turn's `usage` data part, summed into the session total: the
/// token counts sit on the standard (camelCase) fields; what the
/// standard has no slot for rides the object's `_meta` under the usage
/// extension uri.
#[derive(Default)]
struct UsageSum {
    total_tokens: u64,
    requests: u64,
    cost_usd: Option<f64>,
}

impl UsageSum {
    fn absorb(&mut self, usage: &Value) {
        self.total_tokens += usage
            .get("totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(supplements) = usage
            .get("_meta")
            .and_then(|meta| meta.get(USAGE_EXTENSION_URI))
        {
            self.requests += supplements
                .get("requests")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.cost_usd = match (
                self.cost_usd,
                supplements.get("costUsd").and_then(Value::as_f64),
            ) {
                (acc, None) => acc,
                (None, Some(cost)) => Some(cost),
                (Some(acc), Some(cost)) => Some(acc + cost),
            };
        }
    }

    /// The status-bar summary: `12.3k tokens · 8 turns · $0.0123`, cost
    /// only when some turn was priced.
    fn summary(&self) -> String {
        #[allow(clippy::cast_precision_loss)]
        let tokens = match self.total_tokens {
            0..=999 => self.total_tokens.to_string(),
            1_000..=999_999 => format!("{:.1}k", self.total_tokens as f64 / 1_000.0),
            _ => format!("{:.2}M", self.total_tokens as f64 / 1_000_000.0),
        };
        let mut parts = vec![format!("{tokens} tokens")];
        if self.requests > 0 {
            parts.push(format!("{} turns", self.requests));
        }
        if let Some(cost) = self.cost_usd {
            parts.push(format!("${cost:.4}"));
        }
        parts.join(" · ")
    }
}

/// The session usage summary over a conversation's task history: every
/// settled turn's final status carries its `usage` data part; the
/// session total is their sum. `None` when no turn was billed.
fn session_usage(tasks: &[Value]) -> Option<String> {
    let mut sum = UsageSum::default();
    for task in tasks {
        let Some(parts) = task
            .pointer("/status/message/parts")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for part in parts {
            if let Some(usage) = part.pointer("/data/usage") {
                sum.absorb(usage);
            }
        }
    }
    (sum.total_tokens > 0).then(|| sum.summary())
}

/// The status message's text part, when it has one.
fn status_text(status: &Value) -> Option<String> {
    let text = status
        .pointer("/status/message/parts")?
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

/// The structured tool call riding a working status's data part —
/// a2acp serializes the call (id, title, name, kind, status, content)
/// beside the human title under `toolCall` (docs/A2A.md).
fn status_tool_call(status: &Value) -> Option<&Value> {
    status
        .pointer("/status/message/parts")?
        .as_array()?
        .iter()
        .find_map(|part| part.get("data").and_then(|data| data.get("toolCall")))
}

/// Project one `toolCall` status onto `ToolCall` events: the first sight
/// of an id announces the call by its title; a later frame for the same
/// id carries its output (or nothing new). A call that arrives already
/// terminal announces and completes in one breath.
fn emit_tool_call(
    events: &mpsc::UnboundedSender<StreamEvent>,
    announced: &mut HashSet<String>,
    call: &Value,
) {
    let Some(id) = call.get("toolCallId").and_then(Value::as_str) else {
        return;
    };
    let output = tool_call_output(call);
    let failed = call.get("status").and_then(Value::as_str) == Some("failed");
    let finished = failed || call.get("status").and_then(Value::as_str) == Some("completed");

    if announced.insert(id.to_string()) {
        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
        let display = call.get("title").and_then(Value::as_str).unwrap_or(id);
        let _ = events.send(StreamEvent::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            display: display.to_string(),
            output: String::new(),
            failed: false,
        });
        if finished && !output.is_empty() {
            let _ = events.send(StreamEvent::ToolCall {
                id: id.to_string(),
                name: String::new(),
                display: String::new(),
                output,
                failed,
            });
        }
    } else if !output.is_empty() {
        let _ = events.send(StreamEvent::ToolCall {
            id: id.to_string(),
            name: String::new(),
            display: String::new(),
            output,
            failed,
        });
    }
}

/// The text a tool call's `content` carries, if any — the shape the ACP
/// bridge writes (`content[].content.text`), read leniently.
fn tool_call_output(call: &Value) -> String {
    call.get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.pointer("/content/text")
                        .or_else(|| item.get("text"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// The parked ask carried by an `INPUT_REQUIRED` status message: the raw
/// ACP ask's tool title plus the flat options list (the data-part
/// convention from a2acp's docs/A2A.md).
struct ParkedAsk {
    title: String,
    names: Vec<String>,
    options: Vec<AskOption>,
}

fn permission_ask(status: &Value) -> Option<ParkedAsk> {
    let data = status
        .pointer("/status/message/parts")?
        .as_array()?
        .iter()
        .find_map(|part| part.get("data"))?;
    let options: Vec<AskOption> = data
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|option| {
            Some(AskOption {
                id: option.get("id").and_then(Value::as_str)?.to_owned(),
                name: option
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                kind: option
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect();
    let names = options.iter().map(|option| option.name.clone()).collect();
    let title = data
        .pointer("/permissionRequest/toolCall/title")
        .and_then(Value::as_str)
        .unwrap_or("permission required")
        .to_owned();
    Some(ParkedAsk {
        title,
        names,
        options,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn body(selection: &Selection) -> Value {
        turn_body(
            7,
            "t-7",
            "c-7",
            "m-7",
            "pie",
            true,
            std::path::Path::new("/repo"),
            "hello",
            selection,
        )
    }

    fn message(body: &Value) -> &Value {
        &body["params"]["message"]
    }

    /// With no selection the body carries no extension opt-in and no
    /// metadata key under the uri — byte-identical to the shape the TUI
    /// sent before the extension existed.
    #[test]
    fn no_selection_leaves_the_wire_untouched() {
        let bare = body(&Selection::default());
        assert!(message(&bare).get("extensions").is_none());
        assert!(
            !message(&bare)["metadata"]
                .as_object()
                .unwrap()
                .contains_key(SELECTION_EXTENSION_URI)
        );
        assert_eq!(message(&bare)["metadata"]["agent"], "pie");
        assert_eq!(message(&bare)["metadata"]["cwd"], "/repo");
    }

    #[test]
    fn both_legs_ride_the_extension_payload() {
        let selected = body(&Selection {
            mode: Some("plan".into()),
            model: Some("deep".into()),
        });
        assert_eq!(
            message(&selected)["extensions"],
            json!([SELECTION_EXTENSION_URI]),
            "the opt-in lists the uri"
        );
        assert_eq!(
            message(&selected)["metadata"][SELECTION_EXTENSION_URI],
            json!({"mode": "plan", "model": "deep"}),
        );
    }

    #[test]
    fn either_leg_is_optional() {
        let mode_only = body(&Selection {
            mode: Some("review".into()),
            model: None,
        });
        let payload = &message(&mode_only)["metadata"][SELECTION_EXTENSION_URI];
        assert_eq!(payload["mode"], "review");
        assert!(payload.get("model").is_none());

        let with_model = body(&Selection {
            mode: None,
            model: Some("default".into()),
        });
        let payload = &message(&with_model)["metadata"][SELECTION_EXTENSION_URI];
        assert_eq!(payload["model"], "default");
        assert!(payload.get("mode").is_none());
    }

    /// A pure selection (the mode leg alone, no query to run) may carry
    /// a single empty text part — the extension's consumption note.
    #[test]
    fn a_pure_selection_may_carry_an_empty_text_part() {
        let pure = turn_body(
            9,
            "t-9",
            "c-9",
            "m-9",
            "pie",
            true,
            std::path::Path::new("/repo"),
            "",
            &Selection {
                mode: Some("plan".into()),
                model: None,
            },
        );
        assert_eq!(
            message(&pure)["parts"],
            json!([{ "text": "" }]),
            "one empty text part, nothing to run"
        );
        assert_eq!(
            message(&pure)["metadata"][SELECTION_EXTENSION_URI],
            json!({"mode": "plan"})
        );
    }

    #[test]
    fn a_continued_conversation_omits_the_cwd() {
        let continued = turn_body(
            11,
            "t-11",
            "c-7",
            "m-11",
            "pie",
            false,
            std::path::Path::new("/repo"),
            "again",
            &Selection::default(),
        );
        assert!(
            !message(&continued)["metadata"]
                .as_object()
                .unwrap()
                .contains_key("cwd")
        );
    }

    /// Minted wire ids are prefixed, uuid-shaped, and never repeat —
    /// a repeat re-addresses a persisted turn from a previous run (the
    /// gateway would replay it instead of starting a new one).
    #[test]
    fn minted_wire_ids_are_prefixed_and_unique() {
        let mut minted = HashSet::new();
        for _ in 0..100 {
            let id = mint("t");
            let hex = id.strip_prefix("t-").expect("prefix");
            assert_eq!(hex.len(), 32, "uuid simple form: {id}");
            assert!(
                hex.chars().all(|c| c.is_ascii_hexdigit()),
                "uuid simple form: {id}"
            );
            assert!(minted.insert(id), "minted id repeated");
        }
    }

    // ── card-driven pickers (the selection extension's report) ──────

    /// A card whose agent reported the config-options shape (opencode
    /// ≥ 1.18 through the gateway): mode + model selects advertised as
    /// `availableModes`/`models`.
    fn card_with(report: &Value) -> Value {
        json!({
            "capabilities": {"extensions": [{
                "uri": SELECTION_EXTENSION_URI,
                "params": {"agents": {"opencode": report}},
            }]},
        })
    }

    #[test]
    fn card_modes_models_and_defaults_parse_off_the_report() {
        let card = card_with(&json!({
            "currentModeId": "build",
            "availableModes": [
                {"id": "build", "name": "Build", "description": "the build agent"},
                {"id": "plan", "name": "Plan"},
            ],
            "models": {
                "currentModelId": "opencode/big-pickle",
                "availableModels": [
                    {"id": "opencode/big-pickle", "name": "OpenCode Zen/Big Pickle"},
                    {"id": "zai-coding-plan/glm-5.3", "name": "Z.AI Coding Plan/GLM-5.3"},
                ],
            },
        }));

        assert_eq!(
            advertised_modes(&card, "opencode"),
            vec![
                ModeOption {
                    id: "build".into(),
                    description: "the build agent".into(),
                },
                ModeOption {
                    id: "plan".into(),
                    description: String::new(),
                },
            ]
        );
        assert_eq!(default_mode(&card, "opencode").as_deref(), Some("build"));
        assert_eq!(
            advertised_models(&card, "opencode"),
            Some(vec![
                CatalogEntry {
                    id: "opencode/big-pickle".into(),
                    model: "OpenCode Zen/Big Pickle".into(),
                },
                CatalogEntry {
                    id: "zai-coding-plan/glm-5.3".into(),
                    model: "Z.AI Coding Plan/GLM-5.3".into(),
                },
            ])
        );
        assert_eq!(
            default_model(&card, "opencode").as_deref(),
            Some("opencode/big-pickle")
        );
    }

    /// An agent with no report (cold card) or a report without a model
    /// catalog (pie in process: modes only) advertises no models — the
    /// free-form `_meta.model` path stays.
    #[test]
    fn no_report_or_no_catalog_advertises_no_models() {
        let cold = json!({});
        assert!(advertised_modes(&cold, "opencode").is_empty());
        assert_eq!(advertised_models(&cold, "opencode"), None);

        let modes_only = card_with(&json!({
            "currentModeId": "build",
            "availableModes": [{"id": "build", "name": "Build"}],
        }));
        assert_eq!(advertised_models(&modes_only, "opencode"), None);
        assert_eq!(advertised_models(&modes_only, "other-agent"), None);

        let empty_catalog = card_with(&json!({
            "models": {"currentModelId": "m", "availableModels": []},
        }));
        assert_eq!(advertised_models(&empty_catalog, "opencode"), None);
    }

    // ── tool-call projection (the WORKING status's data part) ────────

    /// The gateway's convention: the human title as a text part, the
    /// structured call beside it under `toolCall`.
    fn working_status(call: &Value) -> Value {
        let title = call
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("working");
        json!({
            "status": {
                "state": "TASK_STATE_WORKING",
                "message": {"parts": [
                    {"text": format!("🔧 {title}")},
                    {"data": {"toolCall": call}},
                ]},
            }
        })
    }

    fn pump_tool_events(announced: &mut HashSet<String>, status: &Value) -> Vec<StreamEvent> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        if let Some(call) = status_tool_call(status) {
            emit_tool_call(&tx, announced, call);
        }
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn a_call_announces_by_title_and_completes_by_output() {
        let mut announced = HashSet::new();

        let start = working_status(&json!({
            "toolCallId": "call-1",
            "title": "Bash{command = ls}",
            "name": "Bash",
            "kind": "execute",
        }));
        let events = pump_tool_events(&mut announced, &start);
        assert_eq!(
            events,
            vec![StreamEvent::ToolCall {
                id: "call-1".into(),
                name: "Bash".into(),
                display: "Bash{command = ls}".into(),
                output: String::new(),
                failed: false,
            }]
        );

        let update = working_status(&json!({
            "toolCallId": "call-1",
            "title": "Bash{command = ls}",
            "name": "Bash",
            "kind": "execute",
            "status": "completed",
            "content": [{"content": {"text": r#"{"code":0,"stdout":"a.rs","stderr":""}"#}}],
        }));
        let events = pump_tool_events(&mut announced, &update);
        assert_eq!(
            events,
            vec![StreamEvent::ToolCall {
                id: "call-1".into(),
                name: String::new(),
                display: String::new(),
                output: r#"{"code":0,"stdout":"a.rs","stderr":""}"#.to_string(),
                failed: false,
            }],
            "the update carries the output under the same id"
        );
    }

    /// A call whose first status is already terminal (the start and the
    /// update coalesced into one frame) announces and completes at once.
    #[test]
    fn a_terminal_first_sight_announces_and_completes_in_one_breath() {
        let mut announced = HashSet::new();
        let status = working_status(&json!({
            "toolCallId": "call-1",
            "title": "Write{path = a.rs}",
            "name": "Write",
            "status": "completed",
            "content": [{"content": {"text": "{\"status\":\"saved\"}"}}],
        }));
        let events = pump_tool_events(&mut announced, &status);
        assert_eq!(events.len(), 2, "header first, then its output");
        assert_eq!(
            events[0],
            StreamEvent::ToolCall {
                id: "call-1".into(),
                name: "Write".into(),
                display: "Write{path = a.rs}".into(),
                output: String::new(),
                failed: false,
            }
        );
        assert_eq!(
            events[1],
            StreamEvent::ToolCall {
                id: "call-1".into(),
                name: String::new(),
                display: String::new(),
                output: "{\"status\":\"saved\"}".to_string(),
                failed: false,
            }
        );
    }

    /// A progress re-announcement for a seen id with nothing new emits
    /// nothing — the duplicate title line it used to produce is the bug.
    #[test]
    fn an_update_without_output_is_silence_not_a_second_line() {
        let start = working_status(&json!({
            "toolCallId": "call-1",
            "title": "Bash{command = ls}",
            "name": "Bash",
        }));
        let mut announced = HashSet::new();
        assert_eq!(pump_tool_events(&mut announced, &start).len(), 1);

        // The merged re-announcement: same call, still pending, no output.
        assert_eq!(pump_tool_events(&mut announced, &start).len(), 0);
    }

    #[test]
    fn a_failed_call_marks_the_result() {
        let mut announced = HashSet::new();
        let status = working_status(&json!({
            "toolCallId": "call-1",
            "title": "Read{path = a.rs}",
            "name": "Read",
            "status": "failed",
            "content": [{"content": {"text": "path not allowed"}}],
        }));
        let events = pump_tool_events(&mut announced, &status);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            StreamEvent::ToolCall {
                id: "call-1".into(),
                name: String::new(),
                display: String::new(),
                output: "path not allowed".to_string(),
                failed: true,
            }
        );
    }

    /// Text-only working statuses (no data part) still flatten into a
    /// status line — agents that never attach `toolCall` keep working.
    #[test]
    fn a_text_only_working_status_still_flattens() {
        let status = json!({
            "status": {
                "state": "TASK_STATE_WORKING",
                "message": {"parts": [{"text": "thinking…"}]},
            }
        });
        assert!(status_tool_call(&status).is_none());
    }

    // ── session usage (the sum over the task history) ────────────────

    /// A settled turn's task as the store serves it: the final status
    /// message carries that turn's `usage` data part.
    fn billed_task(total_tokens: u64, requests: u64, cost: f64) -> Value {
        json!({
            "status": {
                "state": "TASK_STATE_COMPLETED",
                "message": {"parts": [{"data": {"usage": {
                    "totalTokens": total_tokens,
                    "inputTokens": total_tokens - 100,
                    "outputTokens": 100,
                    "_meta": { USAGE_EXTENSION_URI: {
                        "requests": requests,
                        "costUsd": cost,
                    } },
                }}}]},
            }
        })
    }

    #[test]
    fn session_usage_sums_every_turns_usage_part() {
        let tasks = vec![
            billed_task(1_000, 2, 0.0025),
            billed_task(2_500, 3, 0.0012),
            // An unbilled turn (a failure, or an agent that reports
            // nothing) contributes nothing and never blocks the sum.
            json!({"status": {"state": "TASK_STATE_FAILED",
                "message": {"parts": [{"text": "error: boom"}]}}}),
        ];
        assert_eq!(
            session_usage(&tasks).as_deref(),
            Some("3.5k tokens · 5 turns · $0.0037"),
        );
    }

    #[test]
    fn an_unbilled_history_has_no_session_usage() {
        assert_eq!(session_usage(&[]), None);
        let unbilled = json!({"status": {"state": "TASK_STATE_COMPLETED",
            "message": {"parts": [{"text": "done"}]}}});
        assert_eq!(session_usage(&[unbilled]), None);
    }
}
