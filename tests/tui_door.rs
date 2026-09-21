//! The TUI-facing flow over a real gateway: `pie_tui`'s door client
//! drives an in-process scripted agent through `FrontDoor` — prompt →
//! streamed events → done, the `INPUT_REQUIRED` permission ask → answer,
//! cancel, and `/new`. This is the exact path interactive `pie` runs,
//! minus only the process: the frontend cannot tell the difference.
//!
//! One test function on purpose: the gateway's task store location is
//! redirected through `A2A_ACP_HOME`, and environment variables cannot
//! be raced by parallel tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use acp::Responder;
use acp::schema::ProtocolVersion;
use acp::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol as acp;
use pie_tui::StreamEvent;
use pie_tui::door;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Session ids minted by the scripted agent — unique per connection, the
/// way a real agent's `session/new` answers.
static SESSIONS: AtomicU64 = AtomicU64::new(0);

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

// ── the scripted agent ─────────────────────────────────────────────

/// What the scripted agent does with every prompt.
#[derive(Debug, Clone)]
enum Script {
    /// Stream the chunks as agent messages, then end the turn.
    Echo(Vec<&'static str>),
    /// Ask permission for one gated `Bash` call, then echo the answer.
    Ask,
    /// Stream one chunk, then hang until `session/cancel`.
    Hang,
}

/// The scripted agent as an in-process a2acp agent: one connection per
/// conversation, each served by the same script.
#[derive(Debug, Clone)]
struct ScriptedAgent {
    script: Script,
}

impl a2acp::InProcessAgent for ScriptedAgent {
    fn connect(
        &self,
        transport: acp::Channel,
    ) -> Pin<Box<dyn Future<Output = Result<(), acp::Error>> + Send>> {
        let script = self.script.clone();
        Box::pin(scripted_agent(transport, script))
    }
}

async fn scripted_agent(
    transport: impl acp::ConnectTo<acp::Agent> + 'static,
    script: Script,
) -> Result<(), acp::Error> {
    let cancel = Arc::new(tokio::sync::Notify::new());
    let prompt_cancel = Arc::clone(&cancel);
    let _ = acp::Agent
        .builder()
        .name("scripted")
        .on_receive_request(
            async |_req: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                let _ = responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(AgentCapabilities::new()),
                );
                Ok(())
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            async |_req: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                let id = format!("s-{}", SESSIONS.fetch_add(1, Ordering::Relaxed));
                let _ = responder.respond(NewSessionResponse::new(SessionId::from(id)));
                Ok(())
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let cancel = Arc::clone(&prompt_cancel);
                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: acp::ConnectionTo<acp::Client>| {
                    let session_id = req.session_id.clone();
                    let task_cx = cx.clone();
                    let notify = Arc::clone(&cancel);
                    let task_script = script.clone();
                    // Never await a counterpart response inside a handler
                    // (the dispatch loop would deadlock on its own reply):
                    // the turn runs in a spawned task.
                    cx.spawn(async move {
                        match task_script {
                            Script::Echo(chunks) => {
                                for chunk in chunks {
                                    chunk_notification(&task_cx, &session_id, chunk);
                                }
                                let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                            }
                            Script::Ask => {
                                let ask = RequestPermissionRequest::new(
                                    session_id.clone(),
                                    ToolCallUpdate::new(
                                        ToolCallId::new("call-1"),
                                        ToolCallUpdateFields::new()
                                            .title(Some("Bash rm -rf /tmp/x".to_owned())),
                                    ),
                                    vec![
                                        PermissionOption::new(
                                            "allow_once",
                                            "Allow once",
                                            PermissionOptionKind::AllowOnce,
                                        ),
                                        PermissionOption::new(
                                            "reject_once",
                                            "Reject",
                                            PermissionOptionKind::RejectOnce,
                                        ),
                                    ],
                                );
                                let Ok(answer) = task_cx.send_request(ask).block_task().await
                                else {
                                    let _ =
                                        responder.respond(PromptResponse::new(StopReason::EndTurn));
                                    return Ok(());
                                };
                                let allowed = matches!(
                                    &answer.outcome,
                                    RequestPermissionOutcome::Selected(selected)
                                        if selected.option_id.to_string() == "allow_once"
                                );
                                chunk_notification(
                                    &task_cx,
                                    &session_id,
                                    if allowed { "granted" } else { "denied" },
                                );
                                let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                            }
                            Script::Hang => {
                                chunk_notification(&task_cx, &session_id, "hanging");
                                notify.notified().await;
                                let _ =
                                    responder.respond(PromptResponse::new(StopReason::Cancelled));
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let cancel = Arc::clone(&cancel);
                async move |_notif: CancelNotification, _cx: acp::ConnectionTo<acp::Client>| {
                    cancel.notify_one();
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
        .connect_to(transport)
        .await;
    Ok(())
}

fn chunk_notification(cx: &acp::ConnectionTo<acp::Client>, session_id: &SessionId, text: &str) {
    let _ = cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text.to_owned(),
        )))),
    ));
}

// ── the harness ────────────────────────────────────────────────────

/// The gateway config the interactive TUI runs on (mirrors the private
/// `tui_gateway_config` in the binary): asks forwarded, no process
/// agents, no HTTP anywhere.
fn tui_config() -> a2acp::Config {
    a2acp::Config {
        permission: a2acp::PermissionMode::Ask,
        agents: BTreeMap::new(),
        ..a2acp::Config::default()
    }
}

fn gateway(agents: &[(&str, Script)]) -> a2acp::FrontDoor {
    let mut in_process = BTreeMap::new();
    for (name, script) in agents {
        let agent: Arc<dyn a2acp::InProcessAgent> = Arc::new(ScriptedAgent {
            script: script.clone(),
        });
        in_process.insert((*name).to_string(), agent);
    }
    let mut config = tui_config();
    config.a2a.default_agent = agents
        .first()
        .map_or_else(String::new, |(name, _)| (*name).into());
    let gateway = a2acp::a2a::gateway_from_config(&config, &in_process).expect("gateway assembles");
    gateway.connect()
}

type Events = mpsc::UnboundedReceiver<StreamEvent>;

fn client_for(door: &a2acp::FrontDoor, agent: &str) -> (door::Client, Events) {
    door::open(door.clone(), agent, std::env::temp_dir())
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
    // SAFETY: set before the gateway (and its worker threads) exist and
    // before any spawned task could read the environment; this is the
    // only environment-touching test in the binary.
    let store = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("A2A_ACP_HOME", store.path()) };

    let door = gateway(&[
        ("echo", Script::Echo(vec!["hello", " world"])),
        ("ask", Script::Ask),
        ("hang", Script::Hang),
    ]);

    // Prompt → deltas → done, with the final artifact as the done text.
    {
        let (client, mut events) = client_for(&door, "echo");
        client.prompt("hi");
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
        let (client, mut events) = client_for(&door, "ask");
        client.prompt("run it");
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
        let (client, mut events) = client_for(&door, "ask");
        client.prompt("run it");
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
        let (client, mut events) = client_for(&door, "hang");
        client.prompt("hang");
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
        let (client, mut events) = client_for(&door, "echo");
        client.new_session();
        let seen = events_until(&mut events, |ev| {
            matches!(ev, StreamEvent::SessionSwitched(_))
        })
        .await;
        let StreamEvent::SessionSwitched(id) = seen.last().unwrap() else {
            unreachable!("stopped on the switch");
        };
        assert!(!id.0.is_empty(), "the switch carries a display id");

        client.prompt("after reset");
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(
            seen.last(),
            Some(&StreamEvent::Done("hello world".into())),
            "the fresh conversation answers: {seen:?}"
        );
    }

    // The tempdir outlives the gateway below; keep it named so the store
    // path stays valid for the whole flow.
    drop(store);

    // Follow-up prompts continue one conversation (same contextId → the
    // agent keeps its memory): no session switch between turns.
    {
        let (client, mut events) = client_for(&door, "echo");
        client.prompt("first");
        let seen = events_until(&mut events, |ev| matches!(ev, StreamEvent::Done(_))).await;
        assert_eq!(seen.last(), Some(&StreamEvent::Done("hello world".into())));

        client.prompt("second");
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
}
