//! The pie engine boundary: the turn vocabulary every frontend of pie's
//! agent consumes, plus the shared turn driver. Ids and usage map between
//! pie types and this vocabulary here, and nowhere else.
//!
//! The vocabulary is deliberately small — exactly what crosses a frontend
//! boundary today: text deltas, soft errors, and tool-call halves
//! ([`Event`]); permission asks with their answer channel ([`Ask`]); and
//! the cancel signal ([`TurnIO`]). Final text, usage totals, and stop
//! reasons ride the turn's [`TurnEnd`] instead of the event channel
//! because every consumer (ACP server loop, A2A door) maps them onto its
//! own terminal shape.

use crate::agent::{AgentConfig, AgentEvent, PieAgent};
use crate::config::{ResolvedProvider, RetryConfig};
use crate::db::DbPool;
use crate::p1e_sandbox::SandboxConfig;
use crate::plugin::{AgentMode, GateAsk};
use crate::registry::{Registry, RegistryCache};
use crate::session::{Session, SessionId as PieSessionId};
use crate::usage::RunUsage;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use tokio::sync::{mpsc, oneshot, watch};

fn lock<T>(lock: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the turn vocabulary ────────────────────────────────────────────

/// One bridge event: progress from a running turn. A tool call is emitted
/// twice per call: once before execution (`display` carries
/// `name(args)`, `output` empty) and once after (`display` empty,
/// `output` the result text, `failed` whether it errored). `id` pairs the
/// halves — consumers building call → result views key on it.
#[derive(Debug, Clone)]
pub enum Event {
    Delta(String),
    /// A mid-turn, non-fatal failure (the turn keeps running).
    Error(String),
    ToolCall {
        id: String,
        name: String,
        display: String,
        output: String,
        failed: bool,
    },
}

/// A gated tool call awaiting the frontend's yes/no: `call_id` pairs the
/// ask with the pre-execution tool-call event the frontend saw; `tool`
/// names the permission, so an "always allow" answer can grant it for the
/// rest of the session; `title` is what to show the user.
#[derive(Debug)]
pub struct Ask {
    pub call_id: String,
    pub tool: String,
    pub title: String,
    pub response_tx: oneshot::Sender<bool>,
}

/// The channels a running turn reports through: progress events,
/// permission asks, and the cancel signal raised by the frontend.
#[derive(Debug)]
pub struct TurnIO {
    pub events: mpsc::UnboundedSender<Event>,
    pub asks: mpsc::UnboundedSender<Ask>,
    pub cancel: watch::Receiver<()>,
}

/// How a driven turn ended. `Completed` means the engine already emitted
/// its terminal events (or nothing further); consumers turn `Cancelled`
/// and `Failed` into their own terminal shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    Completed,
    Cancelled,
    Failed(String),
}

/// One conversation of the pie agent, driven one turn at a time.
pub trait Engine: Send + Sync + 'static {
    /// Run one turn to completion, reporting through `io`.
    fn run_turn(&self, prompt: String, io: TurnIO) -> impl Future<Output = TurnEnd> + Send;
}

// ── the pie engine ─────────────────────────────────────────────────

/// The pinned mode a remote client (ACP, the a2acp gateway) controls
/// through `session/set_mode`: shared between the engine that reads it per
/// turn and the assembly that writes it.
pub type ModeCell = Arc<StdMutex<Option<AgentMode>>>;

/// The remote-client door profile: the client owns the mode selector and
/// the workspace, so every turn pins the session's mode (the model gets
/// no `switch_mode` tool), runs in the session's `cwd`, and gates
/// Write/Edit/Bash behind the engine's [`Ask`] channel instead of the
/// skill permission channel. The serving loop records "always allow"
/// grants on its side, so the engine-side grant set stays empty and every
/// gated call asks.
#[derive(Debug)]
pub struct RemoteDoor {
    pub cwd: PathBuf,
    pub mode: ModeCell,
}

/// Everything the pie engine needs to drive turns.
pub struct PieEngineDeps {
    pub pool: Arc<DbPool>,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub provider: ResolvedProvider,
    pub retry: RetryConfig,
    pub agent_name: Option<String>,
    /// The conversation the engine drives.
    pub session: Session,
    /// The remote-client door: pinned mode, session cwd, tool gating.
    pub door: RemoteDoor,
}

/// The pie agent behind a remote door. One conversation; turns reload the
/// session from the pool, so concurrent requests see consistent state.
pub struct PieEngine {
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    provider: StdMutex<ResolvedProvider>,
    retry: RetryConfig,
    agent_name: Option<String>,
    door: RemoteDoor,
    session_id: PieSessionId,
}

impl PieEngine {
    #[must_use]
    pub fn new(deps: PieEngineDeps) -> Self {
        Self {
            pool: deps.pool,
            registry: deps.registry,
            sandbox: deps.sandbox,
            provider: StdMutex::new(deps.provider),
            retry: deps.retry,
            agent_name: deps.agent_name,
            door: deps.door,
            session_id: deps.session.id,
        }
    }

    /// The agent for one remote-door turn: the client owns the mode
    /// selector and the workspace, so the mode is pinned (the model gets
    /// no `switch_mode` tool), the run happens in the session's cwd, and
    /// the machine-changing tools are gated behind the `Ask` channel.
    /// Skill-permission prompts stay unwired (denied engine-side),
    /// matching the door's documented behavior.
    fn door_agent(&self, session: Session, io: &TurnIO) -> PieAgent {
        let (gate_tx, mut gate_rx) = mpsc::unbounded_channel::<GateAsk>();
        let asks = io.asks.clone();
        tokio::spawn(async move {
            while let Some(ask) = gate_rx.recv().await {
                let GateAsk {
                    id,
                    tool,
                    title,
                    response_tx,
                } = ask;
                let _ = asks.send(Ask {
                    call_id: id,
                    tool,
                    title,
                    response_tx,
                });
            }
        });
        let mode = lock(&self.door.mode).unwrap_or_default();
        PieAgent::new(
            lock(&self.provider).build_client(),
            Arc::clone(&self.registry),
            Arc::clone(&self.sandbox),
            session,
            AgentConfig {
                agent_name: self.agent_name.clone(),
                retry: self.retry.clone(),
                mode: Some(mode),
                mode_switching: false,
                cwd: Some(self.door.cwd.clone()),
                ..AgentConfig::default()
            },
        )
        .with_tool_gate((gate_tx, Arc::new(StdMutex::new(HashSet::new()))))
    }

    async fn load_session(&self) -> Result<Session, String> {
        Session::load(self.pool.clone(), self.session_id.clone())
            .await
            .map_err(|e| format!("failed to load session: {e}"))
    }
}

impl Engine for PieEngine {
    async fn run_turn(&self, prompt: String, io: TurnIO) -> TurnEnd {
        let session = match self.load_session().await {
            Ok(session) => session,
            Err(e) => return TurnEnd::Failed(e),
        };
        let agent = self.door_agent(session, &io);
        match run_turn(agent, prompt, io.cancel, io.events.clone()).await {
            PieTurnEnd::Completed { .. } => TurnEnd::Completed,
            PieTurnEnd::Cancelled => TurnEnd::Cancelled,
            PieTurnEnd::Failed(e) => TurnEnd::Failed(e),
        }
    }
}

// ── shared turn driving (the pie engine behind any door) ─────────────

/// How a driven pie turn ended.
#[derive(Debug, Clone)]
pub enum PieTurnEnd {
    Completed {
        text: String,
        usage: RunUsage,
        cost_usd: Option<f64>,
    },
    Cancelled,
    Failed(String),
}

/// Drive one `PieAgent` turn to completion, forwarding engine events as
/// bridge events. Cancellation wins over completion and drops the engine
/// future, aborting any in-flight LLM request. Every event the engine
/// emitted before the future resolved is forwarded — the channel is
/// drained before returning.
pub async fn run_turn(
    mut agent: PieAgent,
    prompt: String,
    mut cancel: watch::Receiver<()>,
    events: mpsc::UnboundedSender<Event>,
) -> PieTurnEnd {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let mut run = Box::pin(agent.stream(&prompt, event_tx));

    let end = loop {
        tokio::select! {
            biased;
            _ = cancel.changed() => break PieTurnEnd::Cancelled,
            Some(event) = event_rx.recv() => forward(&events, event),
            result = &mut run => {
                break match result {
                    Ok(outcome) => PieTurnEnd::Completed {
                        text: outcome.text,
                        usage: outcome.usage,
                        cost_usd: outcome.cost_usd,
                    },
                    Err(e) => PieTurnEnd::Failed(e.to_string()),
                };
            }
        }
    };
    drop(run);

    // The engine sends its final events before the future resolves, but
    // the biased select may have returned on the completion branch first —
    // drain what is still queued so no delta is lost.
    while let Ok(event) = event_rx.try_recv() {
        forward(&events, event);
    }
    end
}

fn forward(events: &mpsc::UnboundedSender<Event>, event: AgentEvent) {
    if let Some(event) = translate(event) {
        let _ = events.send(event);
    }
}

/// Map one engine event onto the bridge vocabulary. Permission requests
/// carry a live channel and cannot cross this boundary; the engine impl
/// intercepts them and routes them through the [`Ask`] channel instead.
fn translate(event: AgentEvent) -> Option<Event> {
    let event = match event {
        AgentEvent::Delta(text) => Event::Delta(text),
        AgentEvent::Error(text) => Event::Error(text),
        AgentEvent::ToolCall {
            id,
            name,
            display,
            output,
            failed,
        } => Event::ToolCall {
            id,
            name,
            display,
            output,
            failed,
        },
        // No bridge counterpart: final text and stop reasons ride the
        // turn's end, the frontend already has the user's own message,
        // and usage totals ride `PieTurnEnd::Completed`.
        AgentEvent::Done(_)
        | AgentEvent::UserMessage(_)
        | AgentEvent::Usage { .. }
        | AgentEvent::TurnUsage { .. }
        | AgentEvent::PermissionRequest(_) => return None,
    };
    Some(event)
}

/// Why a turn could not be set up. Door-specific error types map the
/// message onto their own wire format.
pub type PrepareError = String;

/// Build the agent for one delegated (server-side) turn in `session`'s
/// workspace: the daemon's registry cache for that root, the workspace
/// granted read+write in its sandbox copy, depth 1 so no door can nest
/// through itself.
///
/// # Errors
///
/// Fails when the daemon's global config was never set or the provider
/// client cannot be built.
pub fn delegated_agent(
    provider: &ResolvedProvider,
    retry: &RetryConfig,
    base_sandbox: &Arc<SandboxConfig>,
    registries: &RegistryCache,
    session: &Session,
    agent_name: Option<&str>,
) -> Result<PieAgent, PrepareError> {
    // The engine reads pricing, `[mcp.*]`, and debug flags from the global
    // config; refuse to run if the daemon never set it.
    let Some(_config) = crate::config::CONFIG.get() else {
        return Err("server config not initialized".into());
    };
    let cwd = std::path::Path::new(&session.cwd);
    let registry = registries.get(cwd);
    let sandbox = Arc::new(crate::sandbox_grant::granted_sandbox(
        base_sandbox,
        &[cwd.to_path_buf()],
    ));
    let agent_config = AgentConfig {
        retry: retry.clone(),
        agent_name: agent_name.map(str::to_owned),
        cwd: Some(cwd.to_path_buf()),
        // Depth 1: this run is itself a subagent — served through a pie
        // daemon — so agents cannot nest through the same door.
        depth: 1,
        ..AgentConfig::default()
    };
    let model = provider.build_client();
    Ok(PieAgent::new(
        model,
        registry,
        sandbox,
        session.clone(),
        agent_config,
    ))
}

#[cfg(test)]
mod tests {
    //! Behavior tests for the pie engine's turn driving: failure and
    //! cancellation. The provider points at a dead port with zero retries,
    //! so turns fail fast and deterministically with no network leaving
    //! the machine; a hanging provider pins the turn until cancel.

    use super::*;
    use crate::config::{
        ApiErrorConfig, GlobalAgentConfig, PieConfig, ProviderConfig, RateLimitConfig,
    };
    use redact::Secret;
    use std::collections::HashMap;
    use std::time::Duration;

    /// The engine reads the process-global config (system prompt
    /// assembly, pricing, mcp); set it once per test binary with the
    /// dead-provider fixture so turns fail fast and deterministically.
    fn set_global_config() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
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
                server: crate::config::ServerConfig::default(),
                pricing: HashMap::new(),
                agent: Some(GlobalAgentConfig {
                    retry: fail_fast_retry(),
                }),
                sandbox: None,
                output_format: None,
                log_level: None,
            };
            let resolved: crate::config::ResolvedConfig =
                (crate::config::CliOverrides::default(), pie)
                    .try_into()
                    .expect("test config resolves");
            let _ = crate::config::CONFIG.set(resolved);
        });
    }

    fn dead_provider() -> ResolvedProvider {
        ResolvedProvider {
            name: "test".into(),
            model: "test-model".into(),
            anthropic_url: None,
            openai_url: "http://127.0.0.1:9/v1".parse().unwrap(),
            api_key: Secret::new("k".into()),
            temperature: None,
        }
    }

    fn fail_fast_retry() -> RetryConfig {
        RetryConfig {
            rate_limit: RateLimitConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
            api_error: ApiErrorConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
        }
    }

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            agents: Vec::new(),
            skills: Vec::new(),
            completions: Vec::new(),
        })
    }

    fn door(cwd: &str) -> RemoteDoor {
        RemoteDoor {
            cwd: PathBuf::from(cwd),
            mode: Arc::new(StdMutex::new(None)),
        }
    }

    async fn test_engine(cwd: &str, provider: ResolvedProvider) -> (PieEngine, Arc<DbPool>) {
        let pool = Arc::new(crate::db::create_test_pool().await.unwrap());
        let session = Session::create(pool.clone(), std::path::Path::new(cwd))
            .await
            .unwrap();
        let engine = PieEngine::new(PieEngineDeps {
            pool: pool.clone(),
            registry: empty_registry(),
            sandbox: Arc::new(SandboxConfig::default()),
            provider,
            retry: fail_fast_retry(),
            agent_name: None,
            session,
            door: door(cwd),
        });
        (engine, pool)
    }

    /// The turn IO with its own event tap; asks are drained nowhere (a
    /// dropped answer channel denies, which these turns never trigger).
    fn turn_io(cancel: watch::Receiver<()>) -> (TurnIO, mpsc::UnboundedReceiver<Event>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (asks_tx, _asks_rx) = mpsc::unbounded_channel();
        (
            TurnIO {
                events: events_tx,
                asks: asks_tx,
                cancel,
            },
            events_rx,
        )
    }

    #[tokio::test]
    async fn a_dead_provider_fails_the_turn_with_an_error_event() {
        set_global_config();
        let (engine, _pool) = test_engine("/tmp/pie-bridge-fail", dead_provider()).await;
        let (_cancel_tx, cancel_rx) = watch::channel(());
        let (io, mut events) = turn_io(cancel_rx);
        let end = engine.run_turn("hello".into(), io).await;
        let TurnEnd::Failed(message) = end else {
            panic!("the dead provider must fail the turn: {end:?}");
        };
        assert!(!message.is_empty());
        assert!(
            matches!(events.try_recv(), Ok(Event::Error(_))),
            "the failure must surface as an Error event"
        );
    }

    /// A TCP server that accepts and never answers — the LLM request
    /// hangs, so only cancellation can end the turn.
    async fn hanging_provider() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        if sock.read(&mut buf).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn cancelling_a_hung_turn_ends_it_as_cancelled() {
        set_global_config();
        let url = hanging_provider().await;
        let provider = ResolvedProvider {
            openai_url: url.parse().unwrap(),
            ..dead_provider()
        };
        let (engine, _pool) = test_engine("/tmp/pie-bridge-cancel", provider).await;
        let (cancel_tx, cancel_rx) = watch::channel(());
        let (io, mut events) = turn_io(cancel_rx);
        let engine = Arc::new(engine);
        let turn = tokio::spawn(async move { engine.run_turn("hang please".into(), io).await });
        // Give the turn a moment to reach the provider, then cancel.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = cancel_tx.send(());
        let end = tokio::time::timeout(Duration::from_secs(10), turn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(end, TurnEnd::Cancelled, "cancel wins over the hung request");
        // No further events may arrive behind the cancellation.
        assert!(events.try_recv().is_err());
    }
}
