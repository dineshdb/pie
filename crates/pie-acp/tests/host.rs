//! The in-process serving path: [`PieHost`] (pie's
//! `a2acp::InProcessAgent`) driven over an in-memory `acp::Channel` the
//! way the gateway's pool drives it — initialize, session/new,
//! session/prompt. The provider points at a dead port with zero
//! retries, so the prompt fails fast and deterministically without any
//! network leaving the machine.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use acp::schema::ProtocolVersion;
use acp::schema::v1::{
    ClientCapabilities, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, TextContent,
};
use agent_client_protocol as acp;
use pie_acp::{HostDeps, PieHost};
use pie_core::config::{ApiErrorConfig, RateLimitConfig, ResolvedProvider, RetryConfig};
use pie_core::db::DbPool;
use pie_core::p1e_sandbox::SandboxConfig;
use pie_core::registry::Registry;
use pie_core::session::Session;
use redact::Secret;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

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

async fn test_pool() -> Arc<DbPool> {
    Arc::new(pie_core::db::create_test_pool().await.unwrap())
}

fn host(pool: Arc<DbPool>, resume: Option<pie_core::session::SessionId>) -> PieHost {
    PieHost::new(HostDeps {
        pool,
        registry: empty_registry(),
        sandbox: Arc::new(SandboxConfig::default()),
        provider: dead_provider(),
        retry: fail_fast_retry(),
        agent_name: None,
        resume,
    })
}

/// Drive one connection against the host the way the pool does: as an
/// ACP client over the channel's other half, running the whole
/// initialize → session/new → session/prompt handshake and reporting
/// each step's outcome on `report`.
async fn drive(
    host: PieHost,
    cwd: std::path::PathBuf,
    report: mpsc::UnboundedSender<Step>,
) -> acp::Result<()> {
    let (client_half, agent_half) = acp::Channel::duplex();
    tokio::spawn(async move {
        if let Err(e) = a2acp::InProcessAgent::connect(&host, agent_half).await {
            panic!("the hosted agent connection failed: {e}");
        }
    });
    let runner = |cx: acp::ConnectionTo<acp::Agent>| async move {
        let init = cx
            .send_request(
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_info(Implementation::new("pool-test", "0.0.0"))
                    .client_capabilities(ClientCapabilities::new()),
            )
            .block_task()
            .await?;
        let _ = report.send(Step::Init(Box::new(init)));
        let new = cx
            .send_request(NewSessionRequest::new(cwd.clone()))
            .block_task()
            .await?;
        let session_id = new.session_id.clone();
        let _ = report.send(Step::New(Box::new(new)));
        let prompt = cx
            .send_request(PromptRequest::new(
                session_id,
                vec![ContentBlock::Text(TextContent::new("hi".to_string()))],
            ))
            .block_task()
            .await;
        let _ = report.send(Step::Prompt(prompt));
        Ok(())
    };
    acp::Client
        .builder()
        .name("pool-test")
        .with_spawned(runner)
        .connect_to(client_half)
        .await
}

/// One step of the handshake, as reported out of the connection runner.
enum Step {
    Init(Box<InitializeResponse>),
    New(Box<NewSessionResponse>),
    Prompt(Result<PromptResponse, acp::Error>),
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init(_) => f.write_str("Init"),
            Self::New(_) => f.write_str("New"),
            Self::Prompt(result) => write!(f, "Prompt({result:?})"),
        }
    }
}

/// Run the handshake to completion, collecting its steps. The handshake
/// is done when the runner has reported every step and dropped its
/// channel — the connection itself stays open (its teardown is covered
/// by `a_dropped_channel_half_ends_the_connection_cleanly`).
async fn handshake(host: PieHost, cwd: &std::path::Path) -> Vec<Step> {
    let (report, mut rx) = mpsc::unbounded_channel();
    let cwd = cwd.to_path_buf();
    tokio::spawn(drive(host, cwd, report));
    let mut steps = Vec::new();
    while let Some(step) = rx.recv().await {
        steps.push(step);
    }
    steps
}

#[tokio::test]
async fn the_channel_serves_initialize_session_and_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let steps = handshake(host(test_pool().await, None), tmp.path()).await;

    let [Step::Init(init), Step::New(new), Step::Prompt(prompt)] = &steps[..] else {
        panic!("the handshake must report all three steps: {steps:?}");
    };
    assert_eq!(init.protocol_version, ProtocolVersion::V1);
    assert!(
        init.agent_capabilities.load_session,
        "pie advertises loadSession"
    );
    assert_eq!(
        init.agent_info.as_ref().map(|info| info.name.as_str()),
        Some("pie")
    );
    assert!(
        new.modes
            .as_ref()
            .is_some_and(|modes| modes.current_mode_id.0.contains("build")),
        "the session advertises pie's modes: {:?}",
        new.modes
    );
    // The dead provider fails the turn as a JSON-RPC internal error —
    // never a hang, never the auth-reserved -32000.
    let error = prompt
        .as_ref()
        .expect_err("the dead provider must fail the turn");
    assert_eq!(i32::from(error.code), -32603, "{error}");
}

#[tokio::test]
async fn the_first_connection_resumes_the_seeded_session() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = test_pool().await;
    let seeded = Session::create(pool.clone(), tmp.path()).await.unwrap();

    let steps = handshake(host(pool.clone(), Some(seeded.id.clone())), tmp.path()).await;
    let [Step::Init(_), Step::New(new), Step::Prompt(_)] = &steps[..] else {
        panic!("the handshake must report all three steps: {steps:?}");
    };
    assert_eq!(
        new.session_id.to_string(),
        seeded.id.to_string(),
        "the first session open resumes the startup conversation"
    );

    // A fresh host (a re-minted connection) with no seed opens a fresh
    // conversation instead.
    let steps = handshake(host(pool, None), tmp.path()).await;
    let [Step::Init(_), Step::New(fresh), Step::Prompt(_)] = &steps[..] else {
        panic!("the handshake must report all three steps: {steps:?}");
    };
    assert_ne!(fresh.session_id.to_string(), seeded.id.to_string());
}

#[tokio::test]
async fn a_dropped_channel_half_ends_the_connection_cleanly() {
    let (client_half, agent_half) = acp::Channel::duplex();
    let host = host(test_pool().await, None);
    let serving =
        tokio::spawn(async move { a2acp::InProcessAgent::connect(&host, agent_half).await });
    // The pool dropping its half is the shutdown signal.
    drop(client_half);
    let result = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("the connection must end on EOF")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
}
