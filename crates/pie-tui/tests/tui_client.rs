//! The TUI-facing flow over the A2A wire contract: `pie_tui`'s client
//! drives a stub gateway daemon over real HTTP — JSON-RPC envelopes,
//! chunked SSE streams, the card endpoint, and the daemon's documented
//! wire extensions (`x_warm`, `x_selection`) — prompt → streamed events
//! → done, the `INPUT_REQUIRED` permission ask → answer, cancel,
//! `/new`, and the selection extension (mode + model riding a turn, the
//! client's read-back; both the legacy mode report and the
//! config-options shape opencode ≥ 1.18 answers with). This is the
//! exact path interactive `pie` runs: HTTP, a process boundary, and
//! nothing else.
//!
//! One test function on purpose: the stub's environment redirections
//! cannot be raced by parallel tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use pie_tui::SELECTION_EXTENSION_URI;
use pie_tui::StreamEvent;
use pie_tui::a2a::A2aClient;
use pie_tui::client::{self, Selection};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::sync::{Notify, mpsc};

/// Sessions the stub gateway opened (warm or lazy first prompt) — the
/// warm-start assertions' counter.
static SESSIONS: AtomicU64 = AtomicU64::new(0);

/// Warm-minted conversation ids — unique per warm call.
static WARMS: AtomicU64 = AtomicU64::new(0);

fn warm_counter() -> u64 {
    WARMS.fetch_add(1, Ordering::Relaxed)
}

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// What the stub gateway saw — the assertions' eyes on the agent side.
#[derive(Debug, Default, Clone)]
struct Recorder {
    /// The effective model of every prompt, in order (`None` = the
    /// conversation's default; the remembered leg rides every turn).
    prompt_models: Arc<Mutex<Vec<Option<String>>>>,
    /// Every accepted mode selection, in order (the `session/set_mode`
    /// forwarding leg).
    set_modes: Arc<Mutex<Vec<String>>>,
    /// Every accepted config-channel selection as (config id, value).
    set_config_options: Arc<Mutex<Vec<(String, String)>>>,
}

fn lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the scripted gateway (the wire contract, served over HTTP) ──────

/// What the stub gateway does with every prompt.
#[derive(Debug, Clone)]
enum Script {
    /// Stream the chunks as appended artifacts, then end the turn.
    Echo(Vec<&'static str>),
    /// Ask permission for one gated `Bash` call, then echo the answer.
    Ask,
    /// Stream one chunk, then hang until `CancelTask`.
    Hang,
}

/// What the agent's session reports about its selectable state: the
/// legacy `modes` shape, or the `configOptions` selects opencode
/// ≥ 1.18 answers with (no legacy modes at all). Relayed onto the card
/// after the agent's first session, exactly as the gateway does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    Modes,
    ConfigOptions,
}

impl Report {
    /// The selection-extension report JSON this shape puts on the card.
    fn card_report(self) -> Value {
        match self {
            Self::Modes => json!({
                "currentModeId": "build",
                "availableModes": [
                    {"id": "build", "description": "build"},
                    {"id": "plan", "description": "plan"},
                    {"id": "review", "description": "review"},
                ],
            }),
            Self::ConfigOptions => json!({
                "currentModeId": "build",
                "availableModes": [
                    {"id": "build", "description": "build"},
                    {"id": "plan", "description": "plan"},
                ],
                "models": {
                    "currentModelId": "citadel/glm",
                    "availableModels": [
                        {"id": "citadel/glm", "name": "Citadel GLM"},
                        {"id": "opencode/zen", "name": "OpenCode Zen"},
                    ],
                },
            }),
        }
    }

    fn modes(self) -> Vec<&'static str> {
        match self {
            Self::Modes => vec!["build", "plan", "review"],
            Self::ConfigOptions => vec!["build", "plan"],
        }
    }

    fn models(self) -> Vec<&'static str> {
        match self {
            Self::Modes => Vec::new(),
            Self::ConfigOptions => vec!["citadel/glm", "opencode/zen"],
        }
    }
}

/// One parked permission ask: the answer lands here, the stream wakes.
#[derive(Debug, Default)]
struct AskChannel {
    answered: Mutex<Option<String>>,
    notify: Notify,
}

impl AskChannel {
    fn answer(&self, option_id: String) {
        *lock(&self.answered) = Some(option_id);
        self.notify.notify_one();
    }

    async fn take_answer(&self) -> String {
        loop {
            self.notify.notified().await;
            if let Some(option) = lock(&self.answered).take() {
                return option;
            }
        }
    }
}

/// One live conversation on the gateway.
#[derive(Debug, Default)]
struct Context {
    /// The conversation's accepted selection (the read-back state).
    selection: Mutex<Selection>,
    /// Parked while the agent waits for a permission answer.
    ask: Mutex<Option<Arc<AskChannel>>>,
    /// Woken by `CancelTask`.
    cancel: Mutex<Option<Arc<Notify>>>,
}

/// The stub gateway: card + `/a2a` over real HTTP, the wire contract
/// and only so much gateway semantics as the flow needs (warm mints a
/// conversation, selections validate against the advertised report and
/// are remembered per conversation).
struct Gateway {
    agents: BTreeMap<String, (Script, Report)>,
    recorder: Recorder,
    state: Mutex<GatewayState>,
}

#[derive(Default)]
struct GatewayState {
    /// Agents with at least one session — their report is on the card.
    reported: HashSet<String>,
    contexts: HashMap<String, Arc<Context>>,
    /// The turn tasks of each conversation: `taskId` → `contextId` —
    /// `SendMessage` and `CancelTask` address tasks.
    tasks: HashMap<String, String>,
}

impl Gateway {
    fn new(agents: &[(&str, Script, Report)], recorder: Recorder) -> Arc<Self> {
        Arc::new(Self {
            agents: agents
                .iter()
                .map(|(name, script, report)| ((*name).to_string(), (script.clone(), *report)))
                .collect(),
            recorder,
            state: Mutex::new(GatewayState::default()),
        })
    }

    /// The card as the gateway relays it: every agent that has run a
    /// session advertises its report's selection state.
    fn card(&self) -> Value {
        let state = lock(&self.state);
        let mut agents = serde_json::Map::new();
        for (name, (_, report)) in &self.agents {
            if state.reported.contains(name) {
                agents.insert(name.clone(), report.card_report());
            }
        }
        if agents.is_empty() {
            return json!({"skills": []});
        }
        json!({
            "skills": self.agents.keys().map(|name| json!({"id": name})).collect::<Vec<_>>(),
            "capabilities": {"extensions": [{
                "uri": SELECTION_EXTENSION_URI,
                "params": {"agents": agents},
            }]},
        })
    }

    fn ctx(state: &mut GatewayState, context_id: &str) -> Arc<Context> {
        state
            .contexts
            .entry(context_id.to_string())
            .or_insert_with(|| Arc::new(Context::default()))
            .clone()
    }

    /// Dispatch one JSON-RPC request; `Ok(response)` is a JSON envelope,
    /// `Err(frames)` an SSE stream whose frames arrive as the turn runs
    /// (parked turns hold the stream open).
    fn dispatch(&self, request: &Value) -> Result<Value, Receiver<Value>> {
        let method = request["method"].as_str().unwrap_or_default().to_string();
        match method.as_str() {
            "x_warm" => {
                let agent = request["params"]["agent"].as_str().unwrap_or_default();
                let Some((_, _report)) = self.agents.get(agent) else {
                    return Ok(json!({
                        "jsonrpc": "2.0", "id": request["id"],
                        "error": {"code": -32602,
                                  "message": format!("unknown agent '{agent}'")},
                    }));
                };
                let context_id = format!("c-warm-{}", warm_counter());
                {
                    let mut state = lock(&self.state);
                    state.reported.insert(agent.to_string());
                    Self::ctx(&mut state, &context_id);
                }
                SESSIONS.fetch_add(1, Ordering::Relaxed);
                Ok(json!({
                    "jsonrpc": "2.0", "id": request["id"],
                    "result": {"contextId": context_id},
                }))
            }
            "SendStreamingMessage" => {
                let message = &request["params"]["message"];
                let agent = message["metadata"]["agent"].as_str().unwrap_or_default();
                let Some((script, report)) = self.agents.get(agent) else {
                    return Ok(json!({
                        "jsonrpc": "2.0", "id": request["id"],
                        "error": {"code": -32602,
                                  "message": format!("unknown agent '{agent}'")},
                    }));
                };
                let task_id = message["taskId"].as_str().unwrap_or_default().to_string();
                let context_id = message["contextId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let selection = Self::selection_of(message);
                let (ctx, fresh) = {
                    let mut state = lock(&self.state);
                    let existed = state.contexts.contains_key(&context_id);
                    let ctx = Self::ctx(&mut state, &context_id);
                    if !existed {
                        state.tasks.insert(task_id.clone(), context_id.clone());
                    }
                    (ctx, !existed)
                };
                if fresh {
                    SESSIONS.fetch_add(1, Ordering::Relaxed);
                    lock(&self.state).reported.insert(agent.to_string());
                }
                // Selection legs validate against the advertised report;
                // accepted legs are remembered on the conversation.
                let mut accepted = lock(&ctx.selection).clone();
                if let Some(mode) = &selection.mode {
                    if !report.modes().contains(&mode.as_str()) {
                        return Ok(json!({
                            "jsonrpc": "2.0", "id": request["id"],
                            "error": {"code": -32602, "message": format!(
                                "agent '{agent}' does not advertise mode '{mode}' (available: {})",
                                report.modes().join(", "))},
                        }));
                    }
                    lock(&self.recorder.set_modes).push(mode.clone());
                    if *report == Report::ConfigOptions {
                        lock(&self.recorder.set_config_options)
                            .push(("mode".to_string(), mode.clone()));
                    }
                    accepted.mode = Some(mode.clone());
                }
                if let Some(model) = &selection.model {
                    // A report WITHOUT a catalog takes free-form model
                    // selections (they ride the prompt); with one, the
                    // id must be advertised.
                    if !report.models().is_empty() && !report.models().contains(&model.as_str()) {
                        return Ok(json!({
                            "jsonrpc": "2.0", "id": request["id"],
                            "error": {"code": -32602, "message": format!(
                                "agent '{agent}' does not advertise model '{model}' (catalog: {})",
                                report.models().join(", "))},
                        }));
                    }
                    if *report == Report::ConfigOptions {
                        lock(&self.recorder.set_config_options)
                            .push(("model".to_string(), model.clone()));
                    }
                    accepted.model = Some(model.clone());
                }
                *lock(&ctx.selection) = accepted;
                // The remembered leg rides every prompt of the
                // conversation, selected or not.
                let effective_model = lock(&ctx.selection).model.clone();
                lock(&self.recorder.prompt_models).push(effective_model);

                let (tx, rx) = channel::<Value>(16);
                let script = script.clone();
                tokio::spawn(Self::run_turn(script, ctx, tx));
                Err(rx)
            }
            "SendMessage" => {
                let message = &request["params"]["message"];
                let task_id = message["taskId"].as_str().unwrap_or_default().to_string();
                let option = message["metadata"]["permissionOptionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let ctx = {
                    let state = lock(&self.state);
                    state
                        .tasks
                        .get(&task_id)
                        .and_then(|context_id| state.contexts.get(context_id))
                        .cloned()
                };
                if let Some(ask) = ctx.and_then(|ctx| lock(&ctx.ask).clone()) {
                    ask.answer(option);
                }
                Ok(json!({"jsonrpc": "2.0", "id": request["id"], "result": {}}))
            }
            "CancelTask" => {
                let task_id = request["params"]["id"].as_str().unwrap_or_default();
                let ctx = {
                    let state = lock(&self.state);
                    state
                        .tasks
                        .get(task_id)
                        .and_then(|context_id| state.contexts.get(context_id))
                        .cloned()
                };
                if let Some(cancel) = ctx.and_then(|ctx| lock(&ctx.cancel).clone()) {
                    cancel.notify_one();
                }
                Ok(json!({
                    "jsonrpc": "2.0", "id": request["id"],
                    "result": {"id": task_id},
                }))
            }
            "ListTasks" => Ok(json!({
                "jsonrpc": "2.0", "id": request["id"],
                "result": {"tasks": [], "nextPageToken": Value::Null},
            })),
            "x_selection" => {
                let context_id = request["params"]["contextId"].as_str().unwrap_or_default();
                let selection = {
                    let state = lock(&self.state);
                    state
                        .contexts
                        .get(context_id)
                        .map(|ctx| lock(&ctx.selection).clone())
                };
                let Some(selection) = selection else {
                    return Ok(json!({
                        "jsonrpc": "2.0", "id": request["id"],
                        "error": {"code": -32602, "message": "unknown context"},
                    }));
                };
                let mut payload = serde_json::Map::new();
                if let Some(mode) = &selection.mode {
                    payload.insert("mode".into(), json!(mode));
                }
                if let Some(model) = &selection.model {
                    payload.insert("model".into(), json!(model));
                }
                Ok(json!({
                    "jsonrpc": "2.0", "id": request["id"],
                    "result": {"selection": payload},
                }))
            }
            other => Ok(json!({
                "jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32601, "message": format!("unknown method '{other}'")},
            })),
        }
    }

    /// Run one turn's script onto the stream, frame by frame — the
    /// parking scripts await mid-stream (the permission answer and the
    /// cancel arrive on other connections while this one stays open).
    async fn run_turn(script: Script, ctx: Arc<Context>, tx: Sender<Value>) {
        match script {
            Script::Echo(chunks) => {
                let whole: String = chunks.concat();
                for chunk in chunks {
                    let _ = tx
                        .send(json!({"result": {"artifactUpdate": {
                            "artifact": {"parts": [{"text": chunk}], "append": true},
                        }}}))
                        .await;
                }
                let _ = tx
                    .send(json!({"result": {"artifactUpdate": {
                        "artifact": {"parts": [{"text": whole}], "append": false},
                    }}}))
                    .await;
                let _ = tx.send(Self::completed()).await;
            }
            Script::Ask => {
                let ask = Arc::new(AskChannel::default());
                *lock(&ctx.ask) = Some(Arc::clone(&ask));
                let parked = json!({
                    "result": {"statusUpdate": {
                        "final": false,
                        "status": {
                            "state": "TASK_STATE_INPUT_REQUIRED",
                            "message": {"parts": [{"data": {
                                "options": [
                                    {"id": "allow_once", "name": "Allow once", "kind": "allow"},
                                    {"id": "reject_once", "name": "Reject", "kind": "reject"},
                                ],
                                "permissionRequest": {"toolCall": {
                                    "title": "Bash rm -rf /tmp/x"}},
                            }}]},
                        },
                    }}
                });
                let _ = tx.send(parked).await;
                let option = ask.take_answer().await;
                let answer = if option == "allow_once" {
                    "granted"
                } else {
                    "denied"
                };
                let _ = tx
                    .send(json!({"result": {"artifactUpdate": {
                        "artifact": {"parts": [{"text": answer}], "append": false},
                    }}}))
                    .await;
                let _ = tx.send(Self::completed()).await;
            }
            Script::Hang => {
                let cancel = Arc::new(Notify::new());
                *lock(&ctx.cancel) = Some(Arc::clone(&cancel));
                let _ = tx
                    .send(json!({"result": {"artifactUpdate": {
                        "artifact": {"parts": [{"text": "hanging"}], "append": true},
                    }}}))
                    .await;
                cancel.notified().await;
                let _ = tx
                    .send(json!({"result": {"statusUpdate": {
                        "final": true,
                        "status": {"state": "TASK_STATE_CANCELED"},
                    }}}))
                    .await;
            }
        }
    }

    fn completed() -> Value {
        json!({"result": {"statusUpdate": {
            "final": true,
            "status": {"state": "TASK_STATE_COMPLETED", "message": {"parts": [
                {"data": {"usage": {"totalTokens": 10}}},
            ]}},
        }}})
    }

    /// The message's selection-extension payload, when the turn opts in.
    fn selection_of(message: &Value) -> Selection {
        let opted_in = message["extensions"].as_array().is_some_and(|extensions| {
            extensions
                .iter()
                .any(|uri| uri.as_str() == Some(SELECTION_EXTENSION_URI))
        });
        if !opted_in {
            return Selection::default();
        }
        let payload = &message["metadata"][SELECTION_EXTENSION_URI];
        Selection {
            mode: payload["mode"].as_str().map(str::to_string),
            model: payload["model"].as_str().map(str::to_string),
        }
    }
}

// ── the raw HTTP server ────────────────────────────────────────────

/// Serve the stub gateway until the listener dies. One task per
/// connection — parked turns hold an SSE connection open while the
/// permission answer and cancel arrive on fresh ones.
async fn serve(gateway: Arc<Gateway>, listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let gateway = Arc::clone(&gateway);
        tokio::spawn(async move {
            let _ = handle(gateway, stream).await;
        });
    }
}

async fn handle(gateway: Arc<Gateway>, mut stream: TcpStream) -> std::io::Result<()> {
    let request = read_request(&mut stream).await?;
    if request.path.starts_with("/.well-known/agent-card.json") {
        return write_json(&mut stream, &gateway.card()).await;
    }
    let Ok(request_body) = serde_json::from_str::<Value>(&request.body) else {
        return write_json(
            &mut stream,
            &json!({"error": {"code": -32700, "message": "parse error"}}),
        )
        .await;
    };
    match gateway.dispatch(&request_body) {
        Ok(envelope) => write_json(&mut stream, &envelope).await,
        Err(mut frames) => write_sse(&mut stream, &mut frames).await,
    }
}

struct RawRequest {
    #[allow(dead_code)]
    method: String,
    path: String,
    body: String,
}

/// One HTTP/1.1 request: head + content-length body, then close.
async fn read_request(stream: &mut TcpStream) -> std::io::Result<RawRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::Error::other("client closed mid-head"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(RawRequest {
        method,
        path,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

async fn write_json(stream: &mut TcpStream, value: &Value) -> std::io::Result<()> {
    let body = value.to_string();
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.flush().await
}

/// The SSE response: chunked transfer encoding, one `data:` line per
/// frame — the exact encoding the daemon writes and the client parses.
async fn write_sse(stream: &mut TcpStream, frames: &mut Receiver<Value>) -> std::io::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
              transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )
        .await?;
    while let Some(frame) = frames.recv().await {
        let event = format!("data: {frame}\n\n");
        let chunk = format!("{:x}\r\n{event}\r\n", event.len());
        stream.write_all(chunk.as_bytes()).await?;
        stream.flush().await?;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await
}

// ── the harness ────────────────────────────────────────────────────

/// Boot the stub gateway over real HTTP: `(client, recorder)`.
async fn gateway(agents: &[(&str, Script, Report)]) -> (A2aClient, Recorder) {
    let recorder = Recorder::default();
    let gateway = Gateway::new(agents, recorder.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the stub gateway binds");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(serve(gateway, listener));
    let client = A2aClient::new(base_url).expect("client builds");
    (client, recorder)
}

type Events = mpsc::UnboundedReceiver<StreamEvent>;

async fn client_for(door: &A2aClient, agent: &str) -> (client::Client, Events) {
    client::open(door.clone(), agent, std::env::temp_dir()).await
}

/// Collect events until `stop` matches (or the deadline panics).
async fn events_until(
    events: &mut Events,
    stop: impl Fn(&StreamEvent) -> bool,
) -> Vec<StreamEvent> {
    let mut seen = Vec::new();
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            match events.recv().await.expect("event stream alive") {
                event if stop(&event) => {
                    seen.push(event);
                    return;
                }
                event => seen.push(event),
            }
        }
    })
    .await
    .expect("the flow must finish");
    seen
}

// ── the flow ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_tui_flow_over_the_gateway() {
    let (door, recorder) = gateway(&[
        ("echo", Script::Echo(vec!["hello", " world"]), Report::Modes),
        ("ask", Script::Ask, Report::Modes),
        ("hang", Script::Hang, Report::Modes),
        (
            "config",
            Script::Echo(vec!["hello", " world"]),
            Report::ConfigOptions,
        ),
    ])
    .await;

    // ── warm start: the session opens BEFORE the first message ───────
    // The startup warm opens the agent's session in the background; when
    // it settles, the card advertises the reported modes with no prompt
    // sent, and the first prompt then runs on the warm session (no
    // second open).
    {
        let (client, mut events) = client_for(&door, "echo").await;
        assert!(
            client.modes().is_empty(),
            "cold card: nothing advertised before the warm settles"
        );
        let sessions_before = SESSIONS.load(Ordering::Relaxed);
        client.warm();
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Warm)).await;
        assert_eq!(seen.last(), Some(&StreamEvent::Warm));
        let modes = client.modes();
        let mode_ids: Vec<&str> = modes.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            mode_ids,
            vec!["build", "plan", "review"],
            "the card advertises the reported modes before any message"
        );
        assert_eq!(
            client.default_mode().as_deref(),
            Some("build"),
            "the mode bar's fallback reads the reported default"
        );
        assert_eq!(
            SESSIONS.load(Ordering::Relaxed),
            sessions_before + 1,
            "exactly one session opened"
        );

        // The first prompt rides the warm conversation: no second open.
        client.prompt("after warm", &Selection::default());
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("hello world".into())),
            "the warmed-up conversation answers: {seen:?}"
        );
        assert_eq!(
            SESSIONS.load(Ordering::Relaxed),
            sessions_before + 1,
            "the warm session serves the first prompt"
        );
    }

    // A double warm is a no-op — one client, one warm, one conversation
    // (a second would mint its own and leak its session to the idle
    // grace).
    {
        let (client, mut events) = client_for(&door, "echo").await;
        let sessions_before = SESSIONS.load(Ordering::Relaxed);
        client.warm();
        client.warm();
        events_until(&mut events, |ev| matches!(ev, StreamEvent::Warm)).await;
        assert_eq!(
            SESSIONS.load(Ordering::Relaxed),
            sessions_before + 1,
            "the second warm call changed nothing"
        );
    }

    // A failed warm is silent and stays lazy: an agent the gateway does
    // not know opens nothing and emits nothing; the prompt fails the way
    // it always did (the gateway refuses the unknown agent).
    {
        let (client, mut events) = client_for(&door, "missing").await;
        let sessions_before = SESSIONS.load(Ordering::Relaxed);
        client.warm();
        let settled = tokio::time::timeout(Duration::from_millis(300), events.recv()).await;
        assert!(settled.is_err(), "a failed warm stays silent: {settled:?}");
        assert_eq!(SESSIONS.load(Ordering::Relaxed), sessions_before);

        client.prompt("hello", &Selection::default());
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Error(_))).await;
        let StreamEvent::Error(message) = seen.last().unwrap() else {
            unreachable!("stopped on the error");
        };
        assert!(
            message.contains("unknown agent 'missing'"),
            "the lazy path fails exactly as before: {message}"
        );
    }

    // Prompt → deltas → done, with the final artifact as the done text.
    {
        let (client, mut events) = client_for(&door, "echo").await;
        client.prompt("hi", &Selection::default());
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        let deltas: Vec<&str> = seen
            .iter()
            .filter_map(|ev| match ev {
                StreamEvent::Delta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["hello", " world"]);
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("hello world".into())),
            "the final artifact carries the answer"
        );
    }

    // Permission ask → the turn parks INPUT_REQUIRED; the answer resumes
    // it and the turn completes with the granted path.
    {
        let (client, mut events) = client_for(&door, "ask").await;
        client.prompt("run it", &Selection::default());
        let seen = events_until(&mut events, |ev| {
            matches!(ev, StreamEvent::PermissionAsk { .. })
        })
        .await;
        let StreamEvent::PermissionAsk {
            id,
            skill,
            permissions,
        } = seen.last().unwrap()
        else {
            unreachable!("stopped on the ask");
        };
        assert_eq!(skill, "Bash rm -rf /tmp/x", "the ask shows the tool title");
        assert_eq!(
            permissions,
            &vec!["Allow once".to_string(), "Reject".to_string()],
            "the flat options list is rendered as choices"
        );

        client.answer_permission(id, true);
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("granted".into())),
            "the agent proceeded after the grant: {seen:?}"
        );
    }

    // A denied ask runs the turn on the denied path.
    {
        let (client, mut events) = client_for(&door, "ask").await;
        client.prompt("run it", &Selection::default());
        let seen = events_until(&mut events, |ev| {
            matches!(ev, StreamEvent::PermissionAsk { .. })
        })
        .await;
        let StreamEvent::PermissionAsk { id, .. } = seen.last().unwrap() else {
            unreachable!("stopped on the ask");
        };

        client.answer_permission(id, false);
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("denied".into())),
            "the agent saw the denial: {seen:?}"
        );
    }

    // Cancel: a running turn ends as the terminal Cancelled error, the
    // same event the pre-bridge TUI rendered.
    {
        let (client, mut events) = client_for(&door, "hang").await;
        client.prompt("hang", &Selection::default());
        // Wait for the turn to actually be running so CancelTask
        // addresses a live task.
        events_until(
            &mut events,
            |ev| matches!(ev, StreamEvent::Delta(t) if t == "hanging"),
        )
        .await;

        client.cancel();
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Error(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Error("Cancelled".into())),
            "cancel is the terminal event"
        );
    }

    // `/new` resets the conversation client-side; the next prompt starts
    // a fresh one and completes in it.
    {
        let (client, mut events) = client_for(&door, "echo").await;
        client.new_session();
        let seen = events_until(&mut events, |ev| {
            matches!(ev, StreamEvent::SessionSwitched(_))
        })
        .await;
        let StreamEvent::SessionSwitched(id) = seen.last().unwrap() else {
            unreachable!("stopped on the switch");
        };
        assert!(!id.0.is_empty(), "the switch carries a display id");

        client.prompt("after reset", &Selection::default());
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("hello world".into())),
            "the fresh conversation answers: {seen:?}"
        );
    }

    // Follow-up prompts continue one conversation (same contextId → the
    // agent keeps its memory): no session switch between turns.
    {
        let (client, mut events) = client_for(&door, "echo").await;
        client.prompt("first", &Selection::default());
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(seen.last(), Some(&StreamEvent::Done("hello world".into())));

        client.prompt("second", &Selection::default());
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("hello world".into())),
            "the follow-up answers on the same conversation"
        );
        assert!(
            !seen
                .iter()
                .any(|ev| matches!(ev, StreamEvent::SessionSwitched(_))),
            "no session switch between turns of one conversation: {seen:?}"
        );
    }

    // ── the selection extension: /mode + /model riding a turn ────────
    // A pending selection composes onto the outgoing message (the
    // extension opt-in plus the typed payload), the agent side honors
    // both legs (the mode leg forwards; the model rides every prompt of
    // the conversation), and the read-back reports what the conversation
    // now runs under.
    {
        let (client, mut events) = client_for(&door, "echo").await;
        // No selection yet: the read-back is the default and the first
        // prompt carries no model meta.
        assert_eq!(client.selection(), Selection::default());
        client.prompt("plain", &Selection::default());
        events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            lock(&recorder.prompt_models).last(),
            Some(&None),
            "an unselected conversation sends no model meta"
        );

        // Mode + model on one turn — exactly what a TUI user picking
        // both between messages produces. The mode forwards; the model
        // is free-form here (echo advertises no catalog).
        client.prompt(
            "selected",
            &Selection {
                mode: Some("plan".into()),
                model: Some("deep".into()),
            },
        );
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("hello world".into())),
            "the selected turn runs to completion"
        );
        assert_eq!(
            lock(&recorder.set_modes).last(),
            Some(&"plan".to_string()),
            "the mode leg forwarded"
        );
        assert_eq!(
            lock(&recorder.prompt_models).last(),
            Some(&Some("deep".to_string())),
            "the model leg rode the prompt"
        );
        // The read-back is async now: refresh, wait for the poke, read.
        client.refresh_selection();
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Selection)).await;
        assert!(!seen.is_empty(), "the selection refresh pokes the TUI");
        assert_eq!(
            client.selection(),
            Selection {
                mode: Some("plan".into()),
                model: Some("deep".into()),
            },
            "the read-back reports the conversation's selection"
        );

        // The conversation remembers: the NEXT turn carries the model
        // meta again (no re-selection needed) and stays in its mode.
        client.prompt("follow-up", &Selection::default());
        events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            lock(&recorder.prompt_models).last(),
            Some(&Some("deep".to_string())),
            "the remembered model rides every later prompt"
        );
        assert_eq!(
            client.selection().mode,
            Some("plan".into()),
            "the remembered mode persists"
        );

        // The mode list fills in after the first session: the card the
        // pickers read now advertises the scripted agent's modes.
        client.refresh_card().await.unwrap();
        let modes = client.modes();
        let ids: Vec<&str> = modes.iter().map(|mode| mode.id.as_str()).collect();
        assert_eq!(ids, vec!["build", "plan", "review"]);
    }

    // A selection with an unadvertised mode id is refused before the
    // turn runs — the extension's error semantics, surfaced as the
    // turn's error event.
    {
        let (client, mut events) = client_for(&door, "echo").await;
        client.prompt(
            "bad mode",
            &Selection {
                mode: Some("vibes".into()),
                model: None,
            },
        );
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Error(_))).await;
        let StreamEvent::Error(message) = seen.last().unwrap() else {
            unreachable!("stopped on the error");
        };
        assert!(
            message.contains("does not advertise mode 'vibes'"),
            "the rejection names the mode and the fix: {message}"
        );
        assert_eq!(
            client.selection(),
            Selection::default(),
            "a refused selection leaves the conversation untouched"
        );
    }

    // ── the config-options shape (opencode ≥ 1.18) ──────────────────
    // The agent reports `configOptions` selects (no legacy modes); the
    // same selection extension must light up over them: the catalog is
    // advertised, selections forward on the config channel, and the
    // model still rides every prompt regardless.
    {
        let (client, mut events) = client_for(&door, "config").await;
        // Cold card: nothing is advertised before the first session.
        assert_eq!(client.models(), None, "nothing is fabricated");

        client.prompt("hello", &Selection::default());
        events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        client.refresh_card().await.unwrap();

        // The model select became the advertised catalog; the mode
        // select became the mode list, defaults included.
        let catalog = client.models().expect("the catalog is advertised");
        let ids: Vec<&str> = catalog.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["citadel/glm", "opencode/zen"]);
        assert_eq!(
            client.default_model().as_deref(),
            Some("citadel/glm"),
            "the select's current value is the default"
        );
        let mode_options = client.modes();
        let mode_ids: Vec<&str> = mode_options.iter().map(|mode| mode.id.as_str()).collect();
        assert_eq!(mode_ids, vec!["build", "plan"]);

        // Both legs forward on the config channel; the model rides
        // every prompt regardless of the channel.
        client.prompt(
            "selected",
            &Selection {
                mode: Some("plan".into()),
                model: Some("opencode/zen".into()),
            },
        );
        events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        // The guard must drop before the next await point.
        {
            let forwarded = lock(&recorder.set_config_options);
            assert!(
                forwarded.contains(&("mode".to_string(), "plan".to_string())),
                "the mode leg forwards on the config channel: {forwarded:?}"
            );
            assert!(
                forwarded.contains(&("model".to_string(), "opencode/zen".to_string())),
                "the model leg forwards on the config channel: {forwarded:?}"
            );
        }
        assert_eq!(
            lock(&recorder.prompt_models).last(),
            Some(&Some("opencode/zen".to_string())),
            "the model rides the prompt whatever the channel"
        );
        client.refresh_selection();
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Selection)).await;
        assert!(!seen.is_empty(), "the read-back pokes the TUI");
        assert_eq!(
            client.selection(),
            Selection {
                mode: Some("plan".into()),
                model: Some("opencode/zen".into()),
            },
            "the read-back covers the config-channel selections"
        );

        // A model outside the advertised catalog is refused before the
        // turn runs, naming the ids.
        client.prompt(
            "bad model",
            &Selection {
                mode: None,
                model: Some("gibberish".into()),
            },
        );
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Error(_))).await;
        let StreamEvent::Error(message) = seen.last().unwrap() else {
            unreachable!("stopped on the error");
        };
        assert!(
            message.contains("does not advertise model 'gibberish'"),
            "the rejection names the model: {message}"
        );
        assert!(
            message.contains("citadel/glm") && message.contains("opencode/zen"),
            "the rejection names the catalog: {message}"
        );
    }

    // The extension uri the client opts into is the documented one —
    // the wire contract this whole flow hangs off.
    assert_eq!(
        SELECTION_EXTENSION_URI,
        "https://qreta.io/a2acp/extensions/selection/v1"
    );
}
