//! Wire tests for the ACP server-side loop (`serve_acp`), driven against
//! a fake [`Engine`] over in-memory duplex streams: session lifecycle,
//! event translation, the permission round trip, cancel, busy refusal,
//! and error-code mapping. No pie anywhere — the point is that any
//! engine gets served the same wire behavior `pie acp` shows.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::unused_async,
    clippy::unused_async_trait_impl
)]

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::SessionUpdate;
use pie_acp::{
    ModeInfo, Modes, OpenError, OpenSession, OpenedSession, ReplayAuthor, ReplayEntry, ServerInfo,
    SessionSource, serve_acp,
};
use pie_core::bridge::{Ask, Engine, Event, TurnEnd, TurnIO};
use serde_json::{Value, json};
use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream, duplex};
use tokio::sync::oneshot;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the fake engine ────────────────────────────────────────────────

/// What the fake engine does on the next turn.
#[derive(Clone)]
enum Script {
    /// Emit the event vocabulary (deltas, both tool-call halves) —
    /// proving what the server forwards.
    Vocabulary,
    /// Ask permission for one gated `Bash` call, then echo the answer.
    GatedTool,
    /// Hang until cancelled.
    Hang,
    /// Fail immediately.
    Fail,
}

#[derive(Clone)]
struct FakeEngine {
    script: Arc<Mutex<Script>>,
    prompts: Arc<Mutex<Vec<String>>>,
    answers: Arc<Mutex<Vec<bool>>>,
    cancelled: Arc<AtomicBool>,
}

impl FakeEngine {
    fn new(script: Script) -> (Self, Arc<Mutex<Script>>) {
        let script = Arc::new(Mutex::new(script));
        (
            Self {
                script: Arc::clone(&script),
                prompts: Arc::new(Mutex::new(Vec::new())),
                answers: Arc::new(Mutex::new(Vec::new())),
                cancelled: Arc::new(AtomicBool::new(false)),
            },
            script,
        )
    }
}

impl Engine for FakeEngine {
    async fn run_turn(&self, prompt: String, mut io: TurnIO) -> TurnEnd {
        lock(&self.prompts).push(prompt);
        let script = lock(&self.script).clone();
        match script {
            Script::Vocabulary => {
                let events = io.events;
                let _ = events.send(Event::Delta("working ".into()));
                let _ = events.send(Event::Delta("hard".into()));
                let _ = events.send(Event::ToolCall {
                    id: "call-1".into(),
                    name: "Bash".into(),
                    display: "Bash cargo test".into(),
                    output: String::new(),
                    failed: false,
                });
                let _ = events.send(Event::ToolCall {
                    id: "call-1".into(),
                    name: String::new(),
                    display: String::new(),
                    output: "12 passed".into(),
                    failed: false,
                });
                let _ = events.send(Event::ToolCall {
                    id: "call-2".into(),
                    name: "Write".into(),
                    display: "Write /etc/hosts".into(),
                    output: String::new(),
                    failed: false,
                });
                let _ = events.send(Event::ToolCall {
                    id: "call-2".into(),
                    name: String::new(),
                    display: String::new(),
                    output: "nope".into(),
                    failed: true,
                });
                let _ = events.send(Event::Error("soft failure".into()));
                TurnEnd::Completed
            }
            Script::GatedTool => {
                let (response_tx, response_rx) = oneshot::channel();
                let _ = io.asks.send(Ask {
                    call_id: "call-9".into(),
                    tool: "Bash".into(),
                    title: "Bash rm -rf /tmp/x".into(),
                    response_tx,
                });
                let allowed = response_rx.await.unwrap_or(false);
                lock(&self.answers).push(allowed);
                let _ = io.events.send(Event::Delta(if allowed {
                    "granted".into()
                } else {
                    "denied".into()
                }));
                TurnEnd::Completed
            }
            Script::Fail => TurnEnd::Failed("engine exploded".into()),
            Script::Hang => tokio::select! {
                _ = io.cancel.changed() => {
                    self.cancelled.store(true, Ordering::SeqCst);
                    TurnEnd::Cancelled
                }
                () = pending::<()>() => unreachable!(),
            },
        }
    }
}

// ── the fake source ────────────────────────────────────────────────

struct FakeSource {
    engine: FakeEngine,
    /// Session ids minted so far.
    next: Mutex<usize>,
}

impl SessionSource for FakeSource {
    type Engine = FakeEngine;

    fn modes(&self) -> Modes {
        Modes {
            current: "build".into(),
            available: vec![
                ModeInfo {
                    id: "build".into(),
                    description: "build".into(),
                },
                ModeInfo {
                    id: "plan".into(),
                    description: "plan".into(),
                },
            ],
        }
    }

    async fn open(&self, open: OpenSession) -> Result<OpenedSession<FakeEngine>, OpenError> {
        let (id, replay) = match &open.resume {
            Some(id) if id == "known" => (
                id.clone(),
                vec![
                    ReplayEntry {
                        author: ReplayAuthor::User,
                        text: "what is 2+2".into(),
                    },
                    ReplayEntry {
                        author: ReplayAuthor::Assistant,
                        text: "4".into(),
                    },
                ],
            ),
            Some(id) => return Err(OpenError::Invalid(format!("unknown session '{id}'"))),
            None => {
                let mut next = lock(&self.next);
                *next += 1;
                (format!("s{}", *next), Vec::new())
            }
        };
        Ok(OpenedSession {
            id,
            engine: self.engine.clone(),
            replay,
        })
    }

    fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<String, String> {
        let known = ["build", "plan"];
        if !known.contains(&mode_id) {
            return Err(format!("unknown mode '{mode_id}'"));
        }
        if session_id.starts_with('s') || session_id == "known" {
            Ok(mode_id.to_string())
        } else {
            Err(format!("unknown session '{session_id}'"))
        }
    }
}

// ── the wire harness ───────────────────────────────────────────────

fn spawn_server(script: Script) -> (DuplexStream, FakeEngine, Arc<Mutex<Script>>) {
    let (engine, script_slot) = FakeEngine::new(script);
    let source = FakeSource {
        engine: engine.clone(),
        next: Mutex::new(0),
    };
    let (client, server) = duplex(256 * 1024);
    let (read, write) = tokio::io::split(server);
    let transport = acp::ByteStreams::new(write.compat_write(), read.compat());
    tokio::spawn(async move {
        let _ = serve_acp(
            source,
            ServerInfo {
                name: "fake".into(),
                version: "0.0.0-test".into(),
            },
            transport,
        )
        .await;
    });
    (client, engine, script_slot)
}

async fn send(client: &mut DuplexStream, value: &Value) {
    client
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
    client.flush().await.unwrap();
}

/// Read frames until the response with `id` arrives; the notifications
/// and server→client requests seen on the way are returned alongside it.
async fn recv_response(client: &mut DuplexStream, id: i64) -> (Value, Vec<Value>) {
    let mut seen = Vec::new();
    let deadline = tokio::time::timeout(Duration::from_secs(10), async {
        let mut lines = BufReader::new(client).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("method").is_none() && frame.get("id") == Some(&json!(id)) {
                return frame;
            }
            seen.push(frame);
        }
        panic!("connection closed before response {id}");
    })
    .await;
    (
        deadline.unwrap_or_else(|_| panic!("timeout waiting for response {id}")),
        seen,
    )
}

/// Open a session and return its wire id.
async fn new_session(client: &mut DuplexStream) -> String {
    let tmp = tempfile::tempdir().unwrap();
    send(
        client,
        &json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{
            "cwd": tmp.path().to_string_lossy(), "mcpServers": []
        }}),
    )
    .await;
    let (response, _) = recv_response(client, 1).await;
    response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The `session/update` payloads among the collected frames, parsed.
fn updates(frames: &[Value]) -> Vec<SessionUpdate> {
    frames
        .iter()
        .filter(|frame| frame["method"] == "session/update")
        .map(|frame| {
            serde_json::from_value(frame["params"]["update"].clone())
                .expect("parses as a SessionUpdate")
        })
        .collect()
}

// ── lifecycle ──────────────────────────────────────────────────────

#[tokio::test]
async fn initialize_responds_with_v1_capabilities_and_agent_info() {
    let (mut client, _, _) = spawn_server(Script::Hang);
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
    assert_eq!(response["result"]["agentInfo"]["name"], "fake");
    assert_eq!(response["result"]["agentInfo"]["version"], "0.0.0-test");
    assert_eq!(response["result"]["authMethods"], json!([]));
}

#[tokio::test]
async fn session_new_advertises_modes_and_rejects_a_bad_cwd() {
    let (mut client, _, _) = spawn_server(Script::Hang);
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{
            "cwd": "/nonexistent/path/for/test"
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 2).await;
    assert_eq!(response["error"]["code"], -32602, "{response}");

    let tmp = tempfile::tempdir().unwrap();
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{
            "cwd": tmp.path().to_string_lossy(), "mcpServers": []
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 3).await;
    assert_eq!(response["result"]["sessionId"], "s1");
    assert_eq!(response["result"]["modes"]["currentModeId"], "build");
    assert_eq!(
        response["result"]["modes"]["availableModes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn session_load_replays_history_then_responds_and_rejects_unknown_ids() {
    let (mut client, _, _) = spawn_server(Script::Hang);
    let tmp = tempfile::tempdir().unwrap();
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":4,"method":"session/load","params":{
            "sessionId": "known", "cwd": tmp.path().to_string_lossy(), "mcpServers": []
        }}),
    )
    .await;
    let (response, frames) = recv_response(&mut client, 4).await;
    assert_eq!(response["result"]["modes"]["currentModeId"], "build");

    // The replay chunks ride ahead of the response, user before agent.
    let replay: Vec<String> = frames
        .iter()
        .filter(|frame| frame["method"] == "session/update")
        .map(|frame| {
            let update = &frame["params"]["update"];
            format!(
                "{}:{}",
                update["sessionUpdate"].as_str().unwrap(),
                update["content"]["text"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        replay,
        ["user_message_chunk:what is 2+2", "agent_message_chunk:4"]
    );

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":5,"method":"session/load","params":{
            "sessionId": "ghost", "cwd": tmp.path().to_string_lossy(), "mcpServers": []
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 5).await;
    assert_eq!(response["error"]["code"], -32602);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown session 'ghost'"),
        "{response}"
    );
}

#[tokio::test]
async fn set_mode_notifies_and_unknown_modes_or_sessions_fail() {
    let (mut client, _, _) = spawn_server(Script::Hang);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":6,"method":"session/set_mode","params":{
            "sessionId": session, "modeId": "plan"
        }}),
    )
    .await;
    let (response, frames) = recv_response(&mut client, 6).await;
    assert_eq!(response["result"], json!({}));
    let mode = frames
        .iter()
        .find(|frame| frame["method"] == "session/update")
        .expect("mode update notification");
    assert_eq!(
        mode["params"]["update"]["sessionUpdate"],
        "current_mode_update"
    );
    assert_eq!(mode["params"]["update"]["currentModeId"], "plan");

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":7,"method":"session/set_mode","params":{
            "sessionId": session, "modeId": "yolo"
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 7).await;
    assert_eq!(response["error"]["code"], -32602);

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":8,"method":"session/set_mode","params":{
            "sessionId": "ghost", "modeId": "plan"
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 8).await;
    assert_eq!(response["error"]["code"], -32602);
}

// ── prompt turns ───────────────────────────────────────────────────

#[tokio::test]
async fn prompt_translates_events_and_ends_the_turn() {
    let (mut client, engine, _) = spawn_server(Script::Vocabulary);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{
            "sessionId": session,
            "prompt": [{"type": "text", "text": "do things"}]
        }}),
    )
    .await;
    let (response, frames) = recv_response(&mut client, 9).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(*lock(&engine.prompts), vec!["do things".to_string()]);

    let updates = updates(&frames);
    // Exactly the mapping table's forwards: deltas as chunks, both tool
    // halves, soft errors as chunks — and nothing else (final text and
    // stop reasons ride the turn's end, not the event channel).
    assert_eq!(updates.len(), 7, "{frames:?}");
    assert!(matches!(
        &updates[0],
        SessionUpdate::AgentMessageChunk(c) if chunk_text(c) == Some("working ")
    ));
    assert!(matches!(
        &updates[1],
        SessionUpdate::AgentMessageChunk(c) if chunk_text(c) == Some("hard")
    ));
    assert!(matches!(
        &updates[2],
        SessionUpdate::ToolCall(call)
            if call.tool_call_id.to_string() == "call-1"
                && call.title == "Bash cargo test"
    ));
    assert!(matches!(
        &updates[3],
        SessionUpdate::ToolCallUpdate(update)
            if update.tool_call_id.to_string() == "call-1"
                && update.fields.status == Some(acp::schema::v1::ToolCallStatus::Completed)
    ));
    // A failed tool call closes out as failed with its output.
    assert!(matches!(
        &updates[5],
        SessionUpdate::ToolCallUpdate(update)
            if update.tool_call_id.to_string() == "call-2"
                && update.fields.status == Some(acp::schema::v1::ToolCallStatus::Failed)
    ));
    // A soft error forwards as a chunk.
    assert!(matches!(
        &updates[6],
        SessionUpdate::AgentMessageChunk(c) if chunk_text(c) == Some("error: soft failure")
    ));
}

fn chunk_text(chunk: &acp::schema::v1::ContentChunk) -> Option<&str> {
    match &chunk.content {
        acp::schema::v1::ContentBlock::Text(text) => Some(&text.text),
        _ => None,
    }
}

#[tokio::test]
async fn prompt_content_blocks_are_flattened_to_text() {
    let (mut client, engine, _) = spawn_server(Script::Fail);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":10,"method":"session/prompt","params":{
            "sessionId": session,
            "prompt": [
                {"type": "text", "text": "look at"},
                {"type": "resource_link", "name": "a.rs", "uri": "file:///tmp/a.rs"},
                {"type": "resource", "resource": {
                    "uri": "file:///tmp/b.rs",
                    "text": "fn main() {}"
                }}
            ]
        }}),
    )
    .await;
    let _ = recv_response(&mut client, 10).await;
    let flattened = {
        let prompts = lock(&engine.prompts);
        assert_eq!(prompts.len(), 1);
        prompts[0].clone()
    };
    assert!(flattened.contains("look at"));
    assert!(flattened.contains("[file: a.rs] (file:///tmp/a.rs)"));
    assert!(flattened.contains("## Attached: file:///tmp/b.rs"));
    assert!(flattened.contains("fn main() {}"));

    // Binary content is refused before the engine sees a turn.
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{
            "sessionId": session,
            "prompt": [
                {"type": "resource", "resource": {
                    "uri": "file:///tmp/x.png", "blob": "aGVsbG8=", "mimeType": "image/png"
                }}
            ]
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 11).await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert_eq!(lock(&engine.prompts).len(), 1);

    // An empty prompt never starts a turn.
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":12,"method":"session/prompt","params":{
            "sessionId": session, "prompt": []
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 12).await;
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(lock(&engine.prompts).len(), 1);
}

#[tokio::test]
async fn a_failed_turn_is_an_internal_error_never_auth_required() {
    let (mut client, _, _) = spawn_server(Script::Fail);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":13,"method":"session/prompt","params":{
            "sessionId": session,
            "prompt": [{"type": "text", "text": "hi"}]
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 13).await;
    let code = response["error"]["code"].as_i64().expect("error frame");
    // -32000 is ACP-reserved for AuthRequired and clients render it as a
    // login prompt; internal failures must use JSON-RPC's -32603.
    assert_ne!(code, -32000, "{response}");
    assert_eq!(code, -32603, "{response}");
}

#[tokio::test]
async fn a_prompt_for_an_unknown_session_is_invalid_params() {
    let (mut client, _, _) = spawn_server(Script::Hang);
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":14,"method":"session/prompt","params":{
            "sessionId": "ghost0",
            "prompt": [{"type": "text", "text": "hi"}]
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 14).await;
    assert_eq!(response["error"]["code"], -32602);
}

#[tokio::test]
async fn a_second_prompt_while_one_runs_is_a_conflict() {
    let (mut client, _, _) = spawn_server(Script::Hang);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":15,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "one"}]
        }}),
    )
    .await;
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":16,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "two"}]
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 16).await;
    assert_eq!(response["error"]["code"], -32001, "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("in progress")
    );

    // Cancel the hanging turn so the task ends cleanly.
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"session/cancel","params":{
            "sessionId": session
        }}),
    )
    .await;
}

#[tokio::test]
async fn cancel_ends_the_running_turn_and_releases_the_slot() {
    let (mut client, _, script) = spawn_server(Script::Hang);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":17,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "hang"}]
        }}),
    )
    .await;
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"session/cancel","params":{
            "sessionId": session
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 17).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");

    // The turn slot is released: a fresh prompt on the same session runs.
    *lock(&script) = Script::Vocabulary;
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":18,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "again"}]
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 18).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
}

// ── the permission round trip ──────────────────────────────────────

/// Read frames until the server asks `session/request_permission`.
async fn recv_permission_request(client: &mut DuplexStream) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut lines = BufReader::new(client).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame["method"] == "session/request_permission" {
                return frame;
            }
        }
        panic!("connection closed waiting for the permission request");
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting for the permission request"))
}

/// Answer the server's pending `session/request_permission`.
async fn answer_permission(client: &mut DuplexStream, request: &Value, option: &str) {
    send(
        client,
        &json!({"jsonrpc":"2.0","id": request["id"], "result": {
            "outcome": {"outcome": "selected", "optionId": option}
        }}),
    )
    .await;
}

#[tokio::test]
async fn gated_tools_round_trip_through_request_permission() {
    let (mut client, engine, _script) = spawn_server(Script::GatedTool);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":18,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "run it"}]
        }}),
    )
    .await;
    let request = recv_permission_request(&mut client).await;

    // The ask is correlated with the tool call and offers the full
    // option set, the "always" one naming the tool.
    assert_eq!(request["params"]["sessionId"], json!(session));
    assert_eq!(request["params"]["toolCall"]["toolCallId"], "call-9");
    assert_eq!(request["params"]["toolCall"]["title"], "Bash rm -rf /tmp/x");
    assert_eq!(request["params"]["toolCall"]["kind"], "execute");
    assert_eq!(request["params"]["toolCall"]["status"], "pending");
    let options = request["params"]["options"].as_array().unwrap();
    assert_eq!(options.len(), 3);
    assert_eq!(options[0]["optionId"], "allow_once");
    assert_eq!(options[1]["optionId"], "allow_always");
    assert_eq!(options[1]["name"], "Always allow Bash this session");
    assert_eq!(options[2]["optionId"], "reject_once");

    answer_permission(&mut client, &request, "allow_always").await;
    let (response, frames) = recv_response(&mut client, 18).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(*lock(&engine.answers), vec![true]);
    assert!(
        updates(&frames)
            .iter()
            .any(|update| matches!(update, SessionUpdate::AgentMessageChunk(c) if chunk_text(c) == Some("granted"))),
        "the engine proceeded after the grant: {frames:?}"
    );

    // A second gated Bash call must not ask again: "allow always"
    // granted the tool for the session.
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":19,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "again"}]
        }}),
    )
    .await;
    let (response, frames) = recv_response(&mut client, 19).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        !frames
            .iter()
            .any(|frame| frame["method"] == "session/request_permission"),
        "the session grant must suppress the second ask: {frames:?}"
    );
    assert_eq!(*lock(&engine.answers), vec![true, true]);
}

#[tokio::test]
async fn a_rejected_tool_call_denies_the_ask() {
    let (mut client, engine, _) = spawn_server(Script::GatedTool);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":20,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "run it"}]
        }}),
    )
    .await;
    let request = recv_permission_request(&mut client).await;

    answer_permission(&mut client, &request, "reject_once").await;
    let (response, frames) = recv_response(&mut client, 20).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(*lock(&engine.answers), vec![false]);
    assert!(
        updates(&frames)
            .iter()
            .any(|update| matches!(update, SessionUpdate::AgentMessageChunk(c) if chunk_text(c) == Some("denied"))),
        "the engine saw the denial: {frames:?}"
    );
}

#[tokio::test]
async fn cancelling_with_a_permission_pending_still_settles_the_turn() {
    let (mut client, engine, _) = spawn_server(Script::GatedTool);
    let session = new_session(&mut client).await;

    send(
        &mut client,
        &json!({"jsonrpc":"2.0","id":21,"method":"session/prompt","params":{
            "sessionId": session, "prompt": [{"type": "text", "text": "run it"}]
        }}),
    )
    .await;
    let request = recv_permission_request(&mut client).await;

    // Cancel while the ask is pending: the turn settles as cancelled
    // without waiting for the client's answer, and the pump's late
    // answer lands on a dropped oneshot (a denial) without disturbing
    // the wire.
    send(
        &mut client,
        &json!({"jsonrpc":"2.0","method":"session/cancel","params":{
            "sessionId": session
        }}),
    )
    .await;
    let (response, _) = recv_response(&mut client, 21).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");

    answer_permission(&mut client, &request, "allow_once").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        lock(&engine.answers).is_empty(),
        "the late answer must not reach the finished turn"
    );
}
