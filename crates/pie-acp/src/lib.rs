//! ACP — the Agent Client Protocol (v1) frontend for the pie agent, as a
//! thin assembly: `pie acp` is `PieEngine` (from `pie-core::bridge`) plus
//! a stdio transport, and [`PieHost`] serves the same loop in process to
//! an a2acp gateway. All protocol knowledge — handlers, the permission
//! round trip, the event mapping — lives in this crate's server loop
//! ([`server::serve_acp`]); the assembly only opens pie sessions and
//! builds per-session engines, pinning their modes (`session/set_mode`)
//! and providers (the selection extension's model leg on
//! `session/prompt` `_meta`).
//!
//! Runs carry their working directory in the session's cwd; the engine
//! never touches the process cwd, so sessions in different workspaces
//! run concurrently in one process. The cwd (and any
//! `additionalDirectories`) is granted read+write in the session's
//! sandbox copy — an ACP client explicitly points the agent at that
//! workspace, which is exactly the trust pie's CLI gets from being
//! started inside a project. `deny_read`/`deny_write` still apply on
//! top, so `~/.ssh` and `.env` stay off limits even under a broad root.
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

mod host;
mod server;

pub use host::{HostDeps, PieHost};
pub use server::{
    ModeInfo, Modes, OpenError, OpenSession, OpenedSession, ReplayAuthor, ReplayEntry, ServerInfo,
    SessionSource, serve_acp,
};

use agent_client_protocol as acp;
use pie_core::bridge::{ModeCell, PieEngine, PieEngineDeps, ProviderCell, RemoteDoor};
use pie_core::config::{ResolvedConfig, ResolvedProvider};
use pie_core::db::DbPool;
use pie_core::p1e_sandbox::SandboxConfig;
use pie_core::plugin::AgentMode;
use pie_core::registry::Registry;
use pie_core::sandbox_grant::granted_sandbox;
use pie_core::session::{Role, Session, SessionId as PieSessionId};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, PoisonError};

fn lock<T>(lock: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The pie assembly behind the ACP server loop: opens pie sessions,
/// builds one `PieEngine` per session, and steers their pinned modes and
/// provider selections. Everything here is pie-shaped; the protocol
/// lives in [`server`].
pub struct PieSessions {
    // Manual `Debug`: pool/registry handles have no useful
    // representation, and provider config must not leak its api key.
    pub pool: Arc<DbPool>,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub provider: ResolvedProvider,
    pub retry: pie_core::config::RetryConfig,
    /// The configured `[model.<name>]` tiers — what a model selection
    /// resolves against (a tier name wins wholesale; see
    /// [`resolve_model_selection`]).
    model_tiers: HashMap<String, ResolvedProvider>,
    /// The agent persona turns run under (`pie <agent>`); `None` is the
    /// default pie agent.
    pub(crate) agent_name: Option<String>,
    /// Each session's pinned mode, swapped by `session/set_mode` and
    /// read by the engine at the next turn.
    pinned: StdMutex<HashMap<String, ModeCell>>,
    /// Each session's pinned provider, swapped by the selection
    /// extension's model leg and read by the engine at the next turn.
    models: StdMutex<HashMap<String, ProviderCell>>,
    /// A session the next open resumes regardless of the wire request —
    /// the interactive host's startup conversation, taken once.
    resume_first: StdMutex<Option<PieSessionId>>,
}

impl std::fmt::Debug for PieSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PieSessions").finish_non_exhaustive()
    }
}

impl PieSessions {
    #[must_use]
    pub fn new(
        pool: Arc<DbPool>,
        registry: Arc<Registry>,
        sandbox: Arc<SandboxConfig>,
        provider: ResolvedProvider,
        retry: pie_core::config::RetryConfig,
        model_tiers: HashMap<String, ResolvedProvider>,
    ) -> Self {
        Self {
            pool,
            registry,
            sandbox,
            provider,
            retry,
            model_tiers,
            agent_name: None,
            pinned: StdMutex::new(HashMap::new()),
            models: StdMutex::new(HashMap::new()),
            resume_first: StdMutex::new(None),
        }
    }

    /// Seed the next session open to resume `id` (interactive hosting).
    pub(crate) fn set_resume_first(&mut self, id: PieSessionId) {
        *lock(&self.resume_first) = Some(id);
    }
}

impl SessionSource for PieSessions {
    type Engine = PieEngine;

    fn modes(&self) -> Modes {
        Modes {
            current: AgentMode::default().short_name().to_string(),
            available: AgentMode::all()
                .iter()
                .map(|mode| ModeInfo {
                    id: mode.short_name().to_string(),
                    description: mode.to_string(),
                })
                .collect(),
        }
    }

    async fn open(&self, open: OpenSession) -> Result<OpenedSession<PieEngine>, OpenError> {
        let resume = open
            .resume
            .clone()
            .or_else(|| lock(&self.resume_first).take().map(|id| id.to_string()));
        let session = match resume {
            Some(id) => {
                let unknown = format!("unknown session '{id}'");
                Session::load(self.pool.clone(), PieSessionId::from(id))
                    .await
                    .map_err(|_| OpenError::Invalid(unknown))?
            }
            None => Session::create(self.pool.clone(), &open.cwd)
                .await
                .map_err(|e| OpenError::Internal(e.to_string()))?,
        };
        let resumed = open.resume.is_some();
        tracing::info!(session = %session.id, cwd = %open.cwd.display(), resumed, "acp: session opened");

        let replay: Vec<ReplayEntry> = session
            .history_entries()
            .iter()
            .filter_map(|entry| {
                let author = match entry.role() {
                    Role::User => ReplayAuthor::User,
                    Role::Assistant => ReplayAuthor::Assistant,
                    Role::System | Role::Tool => return None,
                };
                Some(ReplayEntry {
                    author,
                    text: entry.content(),
                })
            })
            .collect();

        let engine = PieEngine::new(PieEngineDeps {
            pool: self.pool.clone(),
            registry: self.registry.clone(),
            sandbox: Arc::new(granted_sandbox(&self.sandbox, &roots(&open))),
            provider: self.provider.clone(),
            retry: self.retry.clone(),
            agent_name: self.agent_name.clone(),
            door: RemoteDoor {
                cwd: open.cwd,
                mode: self.pin(session.id.to_string()),
                model: self.model_pin(session.id.to_string()),
            },
            session: session.clone(),
        });
        Ok(OpenedSession {
            id: session.id.to_string(),
            engine,
            replay,
        })
    }

    fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<String, String> {
        let mode: AgentMode = mode_id
            .parse()
            .map_err(|_| format!("unknown mode '{mode_id}'"))?;
        let pinned = lock(&self.pinned);
        let Some(cell) = pinned.get(session_id) else {
            return Err(format!("unknown session '{session_id}'"));
        };
        *lock(cell) = Some(mode);
        // Persist as pie always has — the system marker — so the mode
        // survives the process (a resumed conversation reads it back).
        let pool = Arc::clone(&self.pool);
        let (id, marker) = (session_id.to_string(), mode.system_marker());
        tokio::spawn(async move {
            if let Ok(mut session) = Session::load(pool, PieSessionId::from(id)).await
                && let Err(e) = session.add_system(&marker).await
            {
                tracing::warn!("acp: persisting the mode marker failed: {e}");
            }
        });
        Ok(mode.short_name().to_string())
    }

    fn select_model(&self, session_id: &str, selection: &str) -> Result<(), String> {
        let models = lock(&self.models);
        let Some(cell) = models.get(session_id) else {
            return Err(format!("unknown session '{session_id}'"));
        };
        let current = lock(cell).clone().unwrap_or_else(|| self.provider.clone());
        let resolved =
            resolve_model_selection(&self.provider, &current, &self.model_tiers, selection)?;
        *lock(cell) = Some(resolved);
        Ok(())
    }
}

/// Resolve a model selection the way the roster resolves an agent's
/// `model:` — a configured tier name wins (that tier's provider
/// wholesale), else a literal model id on the current provider. Two
/// reserved spellings: `"default"` restores the startup provider, and
/// the startup provider's own model id likewise resolves back to it (a
/// literal would otherwise ride whatever provider a previous tier
/// selection left active). Anything else is refused naming the catalog —
/// the selection extension's error semantics for a fresh selection the
/// agent rejects.
pub(crate) fn resolve_model_selection(
    default: &ResolvedProvider,
    current: &ResolvedProvider,
    tiers: &HashMap<String, ResolvedProvider>,
    selection: &str,
) -> Result<ResolvedProvider, String> {
    let known = |id: &str| {
        id == DEFAULT_MODEL_SELECTION
            || id == default.model
            || tiers.keys().any(|name| name == id)
            || tiers.values().any(|tier| tier.model == id)
    };
    if !known(selection) {
        let mut catalog: Vec<&str> = vec![DEFAULT_MODEL_SELECTION, default.model.as_str()];
        catalog.extend(tiers.keys().map(String::as_str));
        return Err(format!(
            "unknown model selection '{selection}' — pick one of: {}",
            catalog.join(", ")
        ));
    }
    if let Some(tier) = tiers.get(selection) {
        return Ok(tier.clone());
    }
    if selection == DEFAULT_MODEL_SELECTION || selection == default.model {
        return Ok(default.clone());
    }
    Ok(current.clone().with_model(selection.to_string()))
}

/// The selection id that restores the startup provider — the catalog's
/// default entry.
pub const DEFAULT_MODEL_SELECTION: &str = "default";

/// The session's sandbox roots: the cwd plus every additional directory.
fn roots(open: &OpenSession) -> Vec<std::path::PathBuf> {
    let mut roots = vec![open.cwd.clone()];
    roots.extend(open.additional_directories.iter().cloned());
    roots
}

impl PieSessions {
    /// Register (or refresh) a session's pinned-mode cell.
    fn pin(&self, session_id: String) -> ModeCell {
        let cell: ModeCell = Arc::new(StdMutex::new(None));
        lock(&self.pinned).insert(session_id, Arc::clone(&cell));
        cell
    }

    /// Register (or refresh) a session's pinned-provider cell.
    fn model_pin(&self, session_id: String) -> ProviderCell {
        let cell: ProviderCell = Arc::new(StdMutex::new(None));
        lock(&self.models).insert(session_id, Arc::clone(&cell));
        cell
    }
}

/// What `pie acp` says about itself on the wire.
#[must_use]
pub fn server_info() -> ServerInfo {
    ServerInfo {
        name: "pie".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// Serve ACP over stdin/stdout until the client closes the connection:
/// the pie assembly plus the server loop over `Stdio`.
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
    let sessions = PieSessions::new(
        pool,
        registry,
        pie_core::config::build_sandbox(&pie_core::config::load_config()?),
        config.provider.clone(),
        config.retry.clone(),
        config.model_tiers.clone(),
    );
    serve_transport(sessions, acp::Stdio::new()).await
}

/// Serve the pie assembly over any ACP transport — `Stdio` in
/// production, in-memory byte streams in tests, an `acp::Channel` when
/// an a2acp gateway hosts pie in process ([`PieHost`]).
///
/// # Errors
///
/// Fails when the underlying ACP connection fails on I/O.
pub async fn serve_transport(
    sessions: PieSessions,
    transport: impl acp::ConnectTo<acp::Agent> + 'static,
) -> anyhow::Result<()> {
    serve_acp(sessions, server_info(), transport)
        .await
        .map_err(|e| anyhow::anyhow!("acp connection failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::ByteStreams;
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

    async fn test_sessions() -> PieSessions {
        PieSessions::new(
            Arc::new(pie_core::db::create_test_pool().await.unwrap()),
            Arc::new(Registry {
                agents: Vec::new(),
                skills: Vec::new(),
                completions: Vec::new(),
            }),
            Arc::new(SandboxConfig::default()),
            test_provider(),
            pie_core::config::RetryConfig::default(),
            HashMap::new(),
        )
    }

    /// Run the pie assembly against the client side of a duplex pair,
    /// driving the same public entry `pie acp` serves over stdio.
    fn spawn_server(sessions: PieSessions) -> DuplexStream {
        let (client, server) = duplex(64 * 1024);
        let (read, write) = tokio::io::split(server);
        tokio::spawn(async move {
            let transport = ByteStreams::new(write.compat_write(), read.compat());
            let _ = serve_transport(sessions, transport).await;
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
        let mut client = spawn_server(test_sessions().await);
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
        let mut client = spawn_server(test_sessions().await);
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
        let mut client = spawn_server(test_sessions().await);
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
        let mut client = spawn_server(test_sessions().await);

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
        let sessions = test_sessions().await;

        // Seed a session directly in the store with history to replay.
        let mut session = Session::create(sessions.pool.clone(), tmp.path())
            .await
            .unwrap();
        session.add_user("what is 2+2").await.unwrap();
        session.add_assistant("4").await.unwrap();
        let session_id = session.id.to_string();

        let mut client = spawn_server(sessions);
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

    /// A failed turn must never carry the ACP-reserved `AuthRequired` code.
    /// -32000 makes clients (Zed) render any failure as "Authentication
    /// required" and hide the real cause behind a login prompt.
    #[tokio::test]
    async fn failed_turn_is_not_mislabeled_as_auth_required() {
        let tmp = tempfile::tempdir().unwrap();

        // The test provider points at a dead port; zero the retries so the
        // prompt fails immediately instead of backing off.
        let mut sessions = test_sessions().await;
        sessions.retry = pie_core::config::RetryConfig {
            api_error: pie_core::config::ApiErrorConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
            rate_limit: pie_core::config::RateLimitConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
        };
        let mut client = spawn_server(sessions);

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
    // ── the selection extension's model leg ─────────────────────────

    /// The session's pinned-provider cell, as the engine's door sees it.
    fn models_pin_for_test(sessions: &PieSessions, session_id: &str) -> Option<ProviderCell> {
        lock(&sessions.models).get(session_id).cloned()
    }

    fn tier(name: &str, model: &str, url: &str) -> ResolvedProvider {
        ResolvedProvider {
            name: name.into(),
            model: model.into(),
            anthropic_url: None,
            openai_url: url.parse().unwrap(),
            api_key: Secret::new("k".into()),
            temperature: None,
        }
    }

    fn resolution_fixture() -> (ResolvedProvider, HashMap<String, ResolvedProvider>) {
        let default = tier("default", "base-model", "http://default");
        let tiers = HashMap::from([
            ("deep".to_string(), tier("deep", "opus", "http://deep")),
            ("fast".to_string(), tier("fast", "mini", "http://fast")),
        ]);
        (default, tiers)
    }

    /// A tier name wins wholesale — the tier's provider, not a model
    /// swap on the current one (mirrors the roster's resolution).
    #[test]
    fn a_tier_selection_takes_the_tiers_provider() {
        let (default, tiers) = resolution_fixture();
        let resolved =
            resolve_model_selection(&default, &default, &tiers, "deep").expect("tier resolves");
        assert_eq!(resolved.model, "opus");
        assert_eq!(
            resolved.openai_url.as_str(),
            "http://deep/",
            "the whole provider rides, not just the model"
        );
    }

    /// A literal model id rides the CURRENT provider — after a tier
    /// selection swapped it, a literal stays on what is active (here:
    /// the fast tier's model id on the deep tier's endpoint).
    #[test]
    fn a_literal_selection_rides_the_current_provider() {
        let (default, tiers) = resolution_fixture();
        let current = tiers["deep"].clone();
        let resolved =
            resolve_model_selection(&default, &current, &tiers, "mini").expect("resolves");
        assert_eq!(resolved.model, "mini");
        assert_eq!(resolved.openai_url.as_str(), "http://deep/");
    }

    /// The catalog's model ids are valid literals — the picker offers
    /// exactly what the resolver accepts.
    #[test]
    fn a_catalog_model_id_is_a_valid_literal() {
        let (default, tiers) = resolution_fixture();
        let resolved =
            resolve_model_selection(&default, &default, &tiers, "mini").expect("resolves");
        assert_eq!(resolved.model, "mini");
        assert_eq!(resolved.openai_url.as_str(), "http://default/");
    }

    /// `default` (and the startup model id) restore the startup provider
    /// wholesale — a literal would otherwise ride a tier-selected
    /// provider with the wrong endpoint.
    #[test]
    fn default_spellings_restore_the_startup_provider() {
        let (default, tiers) = resolution_fixture();
        let current = tiers["deep"].clone();
        for spelling in ["default", "base-model"] {
            let resolved = resolve_model_selection(&default, &current, &tiers, spelling)
                .expect("the default spelling resolves");
            assert_eq!(resolved.model, "base-model");
            assert_eq!(resolved.openai_url.as_str(), "http://default/");
        }
    }

    /// An unknown selection is refused naming the catalog — the
    /// extension's error semantics for a fresh selection the agent
    /// rejects (the send fails; the turn never runs).
    #[test]
    fn an_unknown_selection_is_refused_naming_the_catalog() {
        let (default, tiers) = resolution_fixture();
        let err = resolve_model_selection(&default, &default, &tiers, "gibberish")
            .expect_err("unknown ids are refused");
        assert!(err.contains("unknown model selection 'gibberish'"), "{err}");
        assert!(
            err.contains("default") && err.contains("deep") && err.contains("fast"),
            "{err}"
        );
    }

    /// `select_model` pins the resolved provider per session and keeps
    /// applying it — the gateway re-sends the selection on every prompt,
    /// so resolution must be stable.
    #[tokio::test]
    async fn select_model_pins_a_resolved_provider_per_session() {
        let mut sessions = test_sessions().await;
        sessions.model_tiers =
            HashMap::from([("deep".to_string(), tier("deep", "opus", "http://deep"))]);

        let tmp = tempfile::tempdir().unwrap();
        let opened = SessionSource::open(
            &sessions,
            OpenSession {
                cwd: tmp.path().to_path_buf(),
                additional_directories: Vec::new(),
                resume: None,
            },
        )
        .await
        .expect("session opens");
        let id = opened.id;

        // Tier, then the same tier again (the gateway re-sends the
        // selection on every prompt), then the default — each resolves
        // absolutely, never compounding.
        sessions.select_model(&id, "deep").expect("pins");
        sessions.select_model(&id, "deep").expect("re-pins");
        let pinned = models_pin_for_test(&sessions, &id).expect("registered");
        let provider = lock(pinned.as_ref()).clone().expect("resolved");
        assert_eq!(provider.model, "opus");
        sessions.select_model(&id, "default").expect("restores");
        let provider = lock(models_pin_for_test(&sessions, &id).unwrap().as_ref())
            .clone()
            .unwrap();
        assert_eq!(provider.model, "test-model", "the startup model is back");

        let err = sessions
            .select_model(&id, "nope")
            .expect_err("unknown refused");
        assert!(err.contains("unknown model selection"), "{err}");
        let err = sessions
            .select_model("ghost0", "deep")
            .expect_err("unknown session refused");
        assert!(err.contains("unknown session"), "{err}");
    }

    /// `session/set_mode` persists pie's system marker — the mode
    /// survives the process the way `/mode` always wrote it.
    #[tokio::test]
    async fn set_mode_persists_the_system_marker() {
        let sessions = test_sessions().await;
        let tmp = tempfile::tempdir().unwrap();
        let opened = SessionSource::open(
            &sessions,
            OpenSession {
                cwd: tmp.path().to_path_buf(),
                additional_directories: Vec::new(),
                resume: None,
            },
        )
        .await
        .unwrap();
        let id = opened.id;

        sessions.set_mode(&id, "plan").expect("plan is a mode");
        // The marker write is spawned — give it a beat, then reload.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let reloaded = Session::load(Arc::clone(&sessions.pool), PieSessionId::from(id))
            .await
            .unwrap();
        let markers: Vec<String> = reloaded
            .history_entries()
            .iter()
            .filter(|entry| entry.role() == Role::System)
            .map(pie_core::session::HistoryEntry::content)
            .collect();
        assert!(
            markers.contains(&"[mode:plan]".to_string()),
            "the marker landed in the session history: {markers:?}"
        );
    }

    /// Over the wire: a prompt carrying `_meta.model` that resolves
    /// runs the turn (fails on the dead provider, NOT as an invalid
    /// selection); an unresolvable one fails the send with `-32602`
    /// naming the catalog — before the turn starts.
    #[tokio::test]
    async fn prompt_meta_model_resolves_or_refuses() {
        let mut sessions = test_sessions().await;
        sessions.model_tiers =
            HashMap::from([("deep".to_string(), tier("deep", "opus", "http://deep"))]);
        let tmp = tempfile::tempdir().unwrap();
        let mut client = spawn_server(sessions);
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":30,"method":"session/new","params":{
                "cwd": tmp.path().to_string_lossy(), "mcpServers": []
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 30).await;
        let session_id = response["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        // A resolvable selection: the turn runs (and dies on the dead
        // provider — an internal error, never an invalid-params one).
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":31,"method":"session/prompt","params":{
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "hi"}],
                "_meta": {"model": "deep"}
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 31).await;
        assert_eq!(response["error"]["code"], -32603, "{response}");
        assert!(
            !response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown model selection"),
            "the selection resolved; only the provider died: {response}"
        );

        // An unresolvable selection: the send itself is refused.
        send(
            &mut client,
            &json!({"jsonrpc":"2.0","id":32,"method":"session/prompt","params":{
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "hi"}],
                "_meta": {"model": "gibberish"}
            }}),
        )
        .await;
        let (response, _) = recv_response(&mut client, 32).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        let message = response["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("unknown model selection 'gibberish'"),
            "{response}"
        );
        assert!(
            message.contains("default"),
            "the catalog is named: {message}"
        );
    }
}
