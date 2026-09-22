//! The TUI as an A2A consumer: drives an agent through an in-process
//! [`FrontDoor`] from the a2acp gateway — the same JSON-RPC requests and
//! streaming event sequences an HTTP client would see. The frontend
//! never knows (or cares) that the agent is in process; the door is the
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
//! | `statusUpdate` `TASK_STATE_WORKING` message      | `ToolCall` (a flattened status line)   |
//! | `statusUpdate` `TASK_STATE_INPUT_REQUIRED`       | `PermissionAsk` (the data-part convention from a2acp's docs/A2A.md) |
//! | final `statusUpdate` `COMPLETED`/`FAILED`/`CANCELED` | `Done` / `Error` / `Error("Cancelled")` |
//!
//! Deliberate gaps over the bridge (TODO(a2acp)): usage totals and
//! fine-grained tool-call output have no A2A shape (status lines are the
//! documented flattening), and pie-specific wants — `!shell` escapes —
//! are answered by the caller with a "not available over the bridge"
//! notice instead of fake success. Model and mode selection go through
//! the selection extension (below) — no side channels.

use crate::realm::{AskId, SessionId, StreamEvent};
use a2acp::FrontDoor;
use a2acp::a2a::FrontReply;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

/// URI of the **selection extension** — the spec-sanctioned A2A
/// `AgentExtension` this client opts into per message when a selection
/// is pending (mode and/or model), with the payload under the same key
/// in the message's metadata.
pub const SELECTION_EXTENSION_URI: &str = "https://qreta.io/a2acp/extensions/selection/v1";

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

/// Mints every wire id this client sends (tasks, contexts, messages).
/// Process-global on purpose: two clients on one door must never mint
/// colliding ids — a colliding `taskId` replays another conversation's
/// turn instead of starting one.
static WIRE_IDS: AtomicU64 = AtomicU64::new(0);

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
    door: FrontDoor,
    agent: String,
    cwd: PathBuf,
    events: mpsc::UnboundedSender<StreamEvent>,
    /// The conversation the next prompt continues; `None` starts a fresh
    /// one on the next prompt.
    context: StdMutex<Option<String>>,
    /// The turn in flight — the `CancelTask` target.
    task: StdMutex<Option<String>>,
    /// Parked permission asks keyed by their task id, shared with the
    /// stream pumps that park them.
    asks: Arc<StdMutex<HashMap<String, Vec<AskOption>>>>,
    /// The cached agent card — the mode list for the pickers, refreshed
    /// by [`Client::refresh_card`] (the card is live state: an agent
    /// appears in it only after its first session reported modes).
    card: StdMutex<Value>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("agent", &self.agent)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// Wire a [`Client`] to a front door: requests go through the returned
/// handle, and every projected turn arrives on the returned stream.
#[must_use]
pub fn open(
    door: FrontDoor,
    agent: impl Into<String>,
    cwd: PathBuf,
) -> (Client, mpsc::UnboundedReceiver<StreamEvent>) {
    let (events, stream) = mpsc::unbounded_channel();
    let card = StdMutex::new(door.card());
    (
        Client {
            door,
            agent: agent.into(),
            cwd,
            events,
            context: StdMutex::new(None),
            task: StdMutex::new(None),
            asks: Arc::new(StdMutex::new(HashMap::new())),
            card,
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
        WIRE_IDS.fetch_add(1, Ordering::Relaxed)
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
        let task_id = format!("t-{n}");
        // Bind the cloned context out of the guard before branching:
        // a scrutinee temporary would hold the lock through the arms,
        // and the fresh arm re-locks it.
        let existing = lock(&self.context).clone();
        let (context, fresh) = if let Some(context) = existing {
            (context, false)
        } else {
            let context = format!("c-{n}");
            *lock(&self.context) = Some(context.clone());
            (context, true)
        };
        *lock(&self.task) = Some(task_id.clone());

        let body = turn_body(
            n,
            &task_id,
            &context,
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
                FrontReply::Stream(frames) => {
                    pump(frames, events, asks, task_id).await;
                }
                FrontReply::Envelope(envelope) => {
                    let message = rpc_error(&envelope).unwrap_or_else(|| "turn failed".into());
                    let _ = events.send(StreamEvent::Error(message));
                }
            }
        });
    }

    /// The conversation's current selection, as the gateway holds it —
    /// the read-back for the pickers and the mode bar. `None` legs mean
    /// "not selected" (the agent's own default); before the first turn
    /// there is no conversation to ask.
    pub fn selection(&self) -> Selection {
        let Some(context) = lock(&self.context).clone() else {
            return Selection::default();
        };
        self.door
            .selection(&context)
            .map_or_else(Selection::default, |held| Selection {
                mode: held.mode,
                model: held.model,
            })
    }

    /// The modes the driven agent advertises on the card — empty until
    /// its first session on the gateway reported them (an honest empty:
    /// the picker says "available after the first message").
    pub fn modes(&self) -> Vec<ModeOption> {
        let card = lock(&self.card).clone();
        let Some(report) = card
            .pointer("/capabilities/extensions")
            .and_then(Value::as_array)
            .and_then(|extensions| {
                extensions.iter().find(|extension| {
                    extension.get("uri").and_then(Value::as_str) == Some(SELECTION_EXTENSION_URI)
                })
            })
            .and_then(|extension| extension.pointer("/params/agents"))
            .and_then(|agents| agents.get(&self.agent))
        else {
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

    /// The mode a fresh session of the driven agent starts in, per the
    /// card's report — what the mode bar shows before any selection.
    pub fn default_mode(&self) -> Option<String> {
        let card = lock(&self.card).clone();
        card.pointer("/capabilities/extensions")
            .and_then(Value::as_array)
            .and_then(|extensions| {
                extensions.iter().find(|extension| {
                    extension.get("uri").and_then(Value::as_str) == Some(SELECTION_EXTENSION_URI)
                })
            })
            .and_then(|extension| extension.pointer("/params/agents"))
            .and_then(|agents| agents.get(&self.agent))
            .and_then(|report| report.get("currentModeId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Re-read the card — live state: after the first turn the driven
    /// agent's session has reported its modes, so the mode list fills in.
    pub fn refresh_card(&self) {
        *lock(&self.card) = self.door.card();
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
            if let FrontReply::Envelope(envelope) = door.call(body).await
                && let Some(message) = rpc_error(&envelope)
            {
                // The ask survives a bad answer; surface why.
                let _ = events.send(StreamEvent::Error(message));
            }
        });
    }

    /// Start a fresh conversation on the next prompt (`/new`): purely
    /// client-side — drop the context id and reset the TUI's views onto
    /// a locally minted display id (the input-history key).
    pub fn new_session(&self) {
        let n = Self::next_id();
        *lock(&self.context) = None;
        *lock(&self.task) = None;
        let _ = self
            .events
            .send(StreamEvent::SessionSwitched(SessionId::new(format!(
                "new-{n}"
            ))));
    }
}

/// The `SendStreamingMessage` body for one turn: the client-minted ids,
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
        "messageId": format!("m-{n}"),
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
                // Tool activity flattens into status message text (the
                // documented fidelity gap): render it as a status line.
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
    // The stream closed without a final status (the door shut down).
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
}
