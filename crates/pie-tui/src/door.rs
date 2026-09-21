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
//! | submit / prompt             | `SendStreamingMessage` (client-minted `taskId` + `contextId`, `metadata.agent`, `metadata.cwd` on a fresh conversation) |
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
//! documented flattening), and pie-specific wants — `!shell` escapes,
//! model switching/listing, mode markers — are answered by the caller
//! with a "not available over the bridge" notice instead of fake
//! success.

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
    (
        Client {
            door,
            agent: agent.into(),
            cwd,
            events,
            context: StdMutex::new(None),
            task: StdMutex::new(None),
            asks: Arc::new(StdMutex::new(HashMap::new())),
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
    /// this is the first prompt after `/new` (or launch).
    pub fn prompt(&self, query: &str) {
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

        let mut metadata = json!({"agent": self.agent});
        if fresh {
            let cwd = json!(self.cwd.display().to_string());
            if let Some(object) = metadata.as_object_mut() {
                object.insert("cwd".into(), cwd);
            }
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": n,
            "method": "SendStreamingMessage",
            "params": {
                "message": {
                    "role": "ROLE_USER",
                    "parts": [{"text": query}],
                    "messageId": format!("m-{n}"),
                    "taskId": task_id.clone(),
                    "contextId": context,
                    "metadata": metadata,
                },
                "configuration": {"historyLength": 0},
            },
        });
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
