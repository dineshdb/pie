//! `pie server` — pie served over A2A through the real `a2acp` gateway.
//!
//! The assembly is thin on purpose: pie's `[server]` configuration maps
//! onto [`a2acp::Config`], pie's agent roster is registered as the
//! gateway's in-process agents (the default `pie` entry plus one per
//! registry agent, each the same [`pie_acp::PieHost`] the interactive
//! TUI drives — see [`crate::roster`]), and the crate's router is served
//! on pie's own listener. The wire behavior — agent card, task
//! lifecycle, the `INPUT_REQUIRED` permission flow — is the crate's,
//! not a re-implementation.
//!
//! What pie keeps for itself: the bind (pie's listener), the Host-header
//! allowlist (the DNS-rebinding guard, loopback plus `[server]
//! allowed_hosts`), and the auth policy — `OpenID` Connect when
//! `[server] openid_connect_url` is set, else the crate's loopback model
//! where the tailnet (via `tailscale serve`) is the authentication. A
//! non-loopback bind without OIDC is refused; the old static bearer key
//! (`[server] api_key`, `pie server token`) has no counterpart in the
//! crate and is rejected loudly rather than silently ignored.

use crate::roster::{PIE_AGENT, RosterDeps, install};
use anyhow::Context;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use pie_core::config::{ResolvedConfig, ServerConfig};
use pie_core::db::DbPool;
use pie_core::p1e_sandbox::SandboxConfig;
use pie_core::registry::Registry;
use std::sync::Arc;

/// Everything `pie server` needs to host pie in process — the TUI path's
/// dependencies minus the startup session: the gateway mints one pie
/// session per conversation instead.
pub(crate) struct ServerDeps {
    pub pool: Arc<DbPool>,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
}

/// Build the gateway config from pie's `[server]` section (plus the CLI
/// `--bind` override). Pure mapping plus the auth policy; the gateway
/// itself is assembled from the result.
///
/// # Errors
///
/// Refuses an obsolete `[server] api_key` (static bearer auth was
/// removed with the pie-a2a fork) and a non-loopback bind without
/// `OpenID` Connect — the a2acp security model is OIDC or
/// loopback-plus-tailnet, and pie does not weaken it.
pub(crate) fn gateway_config(
    server: &ServerConfig,
    bind_override: Option<&str>,
) -> anyhow::Result<a2acp::Config> {
    if server.api_key.is_some() {
        anyhow::bail!(
            "[server] api_key is obsolete: static bearer auth was removed with the a2acp \
             gateway — delete it and authenticate with [server] openid_connect_url (OIDC) \
             or expose the loopback bind through tailscale"
        );
    }
    let bind = bind_override.unwrap_or(&server.bind).to_string();
    let effective = ServerConfig {
        bind: bind.clone(),
        ..ServerConfig::default()
    };
    if !effective.is_loopback_bind() && server.openid_connect_url.is_none() {
        anyhow::bail!(
            "refusing non-loopback bind '{bind}' without auth: set [server] openid_connect_url \
             (the agent card then demands provider-issued bearer tokens) or keep the bind \
             loopback and expose it through tailscale"
        );
    }
    Ok(a2acp::Config {
        // Delegated runs are the delegation posture the old daemon had:
        // the sandbox is the boundary, no interactive gate. The gateway's
        // INPUT_REQUIRED flow is still there for agents that ask.
        permission: a2acp::PermissionMode::Auto,
        trust_mcp_credentials: false,
        idle_grace_secs: 600,
        // Only pie's own explicitly configured agents are served — none
        // of the crate's built-in process pool.
        agents: server
            .agents
            .iter()
            .map(|(name, agent)| {
                (
                    name.clone(),
                    a2acp::AgentSpec {
                        command: agent.command.clone(),
                        args: agent.args.clone(),
                        ..a2acp::AgentSpec::new("", &[])
                    },
                )
            })
            .collect(),
        a2a: a2acp::config::A2aConfig {
            bind,
            url: server.url.clone(),
            default_agent: PIE_AGENT.to_string(),
            openid_connect_url: server.openid_connect_url.clone(),
            audience: server.audience.clone(),
        },
    })
}

/// Start the `pie server` daemon: assemble the a2acp gateway (pie's
/// roster in process — the default entry plus one per registry agent —
/// and any `[server.agents]` as spawned process specs) and serve the
/// crate's router on pie's own listener, behind pie's Host-header
/// allowlist.
///
/// # Errors
///
/// Returns an error when the config fails the auth policy
/// ([`gateway_config`]), the gateway cannot assemble, or the listener
/// cannot bind.
pub(crate) async fn serve(
    bind_override: Option<String>,
    deps: ServerDeps,
    server: ServerConfig,
    config: &ResolvedConfig,
) -> anyhow::Result<()> {
    let mut a2a_config = gateway_config(&server, bind_override.as_deref())?;
    let roster = RosterDeps {
        pool: deps.pool,
        registry: deps.registry,
        sandbox: deps.sandbox,
        config,
    };
    let in_process = install(&mut a2a_config, &roster, roster.host_deps(None));
    let gateway = a2acp::a2a::gateway_from_config(&a2a_config, &in_process)
        .context("assembling the gateway")?;

    let listener = tokio::net::TcpListener::bind(&a2a_config.a2a.bind).await?;
    println!(
        "pie server listening on http://{} (A2A; agent card at {CARD_PATH})",
        a2a_config.a2a.bind
    );
    tracing::info!(bind = %a2a_config.a2a.bind, "pie server listening (A2A)");

    let allowed_hosts = HostAllowlist::from_config(&server);
    let app = a2acp::a2a::router(gateway).layer(from_fn_with_state(allowed_hosts, host_guard));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
        })
        .await?;
    Ok(())
}

const CARD_PATH: &str = "/.well-known/agent-card.json";

// ── the Host-header allowlist (DNS-rebinding guard) ─────────────────

/// Loopback always allowed; `[server] allowed_hosts` extends it.
#[derive(Clone, Debug)]
struct HostAllowlist(Arc<Vec<String>>);

impl HostAllowlist {
    fn from_config(server: &ServerConfig) -> Self {
        // Configured hosts extend the loopback defaults — they never
        // replace them.
        let mut hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        hosts.extend(server.allowed_hosts.iter().cloned());
        Self(Arc::new(hosts))
    }
}

/// Reject any request whose Host header is not allowlisted, before the
/// router sees it: a browser-crafted cross-site request must not learn
/// anything, whatever the auth mode.
async fn host_guard(State(allowed): State<HostAllowlist>, req: Request, next: Next) -> Response {
    if !host_in_allowlist(req.headers(), &allowed.0) {
        return (StatusCode::FORBIDDEN, "host not allowed\n").into_response();
    }
    next.run(req).await
}

/// Host-header allowlist: loopback always allowed, `[server]
/// allowed_hosts` extends it. The port is stripped; a missing Host
/// header is a rejection.
fn host_in_allowlist(headers: &HeaderMap, allowed_hosts: &[String]) -> bool {
    let authority = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let Some(authority) = authority else {
        return false;
    };
    // Strip the port; bracketed IPv6 loses its brackets too.
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || allowed_hosts.iter().any(|allowed| allowed == host)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use a2acp::FrontReply;
    use axum::http::{HeaderMap, StatusCode, header};
    use pie_core::config::RetryConfig;
    use pie_core::db;
    use redact::Secret;
    use serde_json::json;

    fn server_config(toml: &str) -> ServerConfig {
        let pie: pie_core::config::PieConfig = toml::from_str(toml).unwrap();
        pie.server
    }

    #[test]
    fn gateway_config_maps_bind_hosts_auth_and_agents() {
        let server = server_config(
            r#"
[server]
bind = "127.0.0.1:8631"
allowed_hosts = ["citadel.lvh.me"]
url = "https://pie.tailnet.example"
openid_connect_url = "https://idp.example/.well-known/openid-configuration"
audience = "pie"

[server.agents.opencode]
command = "opencode"
args = ["acp"]
"#,
        );
        let config = gateway_config(&server, None).unwrap();
        assert_eq!(config.a2a.bind, "127.0.0.1:8631");
        assert_eq!(
            config.a2a.url.as_deref(),
            Some("https://pie.tailnet.example")
        );
        assert_eq!(config.a2a.default_agent, "pie");
        assert_eq!(
            config.a2a.openid_connect_url.as_deref(),
            Some("https://idp.example/.well-known/openid-configuration")
        );
        assert_eq!(config.a2a.audience.as_deref(), Some("pie"));
        // Permission asks auto-allow (the delegation posture) and idle
        // sessions re-mint after the crate's default grace.
        assert_eq!(config.permission, a2acp::PermissionMode::Auto);
        assert_eq!(config.idle_grace_secs, 600);
        // The external agent is a process spec; no built-in pool leaks in.
        assert_eq!(
            config.agents.get("opencode").map(|spec| {
                (
                    spec.command.as_str(),
                    spec.args.iter().map(String::as_str).collect::<Vec<_>>(),
                )
            }),
            Some(("opencode", vec!["acp"]))
        );
        assert!(!config.agents.contains_key("pie"));
        assert!(!config.agents.contains_key("claude"));
    }

    #[test]
    fn bind_override_wins_over_config() {
        let server = server_config("[server]\nbind = \"127.0.0.1:8629\"\n");
        let config = gateway_config(&server, Some("127.0.0.1:9000")).unwrap();
        assert_eq!(config.a2a.bind, "127.0.0.1:9000");
    }

    #[test]
    fn obsolete_api_key_is_refused() {
        let server = server_config("[server]\napi_key = \"leftover\"\n");
        let err = gateway_config(&server, None).unwrap_err().to_string();
        assert!(err.contains("api_key is obsolete"), "{err}");
    }

    #[test]
    fn non_loopback_bind_requires_oidc() {
        let plain = server_config("[server]\nbind = \"0.0.0.0:8629\"\n");
        let err = gateway_config(&plain, None).unwrap_err().to_string();
        assert!(err.contains("openid_connect_url"), "{err}");

        // The refusal follows the effective bind, not just the config's.
        let loopback = server_config("[server]\nbind = \"127.0.0.1:8629\"\n");
        assert!(gateway_config(&loopback, Some("0.0.0.0:8629")).is_err());

        let oidc = server_config(
            "[server]\nbind = \"0.0.0.0:8629\"\nopenid_connect_url = \"https://idp.example\"\n",
        );
        assert!(gateway_config(&oidc, None).is_ok());
    }

    #[test]
    fn host_allowlist_accepts_loopback_and_configured_hosts() {
        let empty: Vec<String> = vec!["localhost".into(), "127.0.0.1".into()];
        assert!(host_in_allowlist(&headers_of("localhost:8629"), &empty));
        assert!(host_in_allowlist(&headers_of("127.0.0.1:8629"), &empty));
        assert!(host_in_allowlist(&headers_of("[::1]:8629"), &empty));
        assert!(!host_in_allowlist(&headers_of("evil.example:80"), &empty));
        assert!(!host_in_allowlist(&HeaderMap::new(), &empty));

        let extended = vec!["localhost".into(), "127.0.0.1".into(), "pie.lvh.me".into()];
        assert!(host_in_allowlist(&headers_of("pie.lvh.me:80"), &extended));
        assert!(!host_in_allowlist(
            &headers_of("other.lvh.me:80"),
            &extended
        ));
    }

    fn headers_of(host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let value: header::HeaderValue = host.parse().unwrap();
        headers.insert(header::HOST, value);
        headers
    }

    /// A dead provider (zero retries, port 9) so the end-to-end drive
    /// fails fast instead of backing off.
    fn dead_provider() -> pie_core::config::ResolvedProvider {
        pie_core::config::ResolvedProvider {
            name: "test".into(),
            model: "test-model".into(),
            anthropic_url: None,
            openai_url: "http://127.0.0.1:9/v1".parse().unwrap(),
            api_key: Secret::new("k".into()),
            temperature: None,
        }
    }

    fn dead_retry() -> RetryConfig {
        RetryConfig {
            api_error: pie_core::config::ApiErrorConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
            rate_limit: pie_core::config::RateLimitConfig {
                max_errors: 0,
                retry_delay_secs: 0,
            },
        }
    }

    /// A registry agent fixture with a name and description.
    fn registry_agent(name: &str, description: &str) -> pie_core::agent::Agent {
        pie_core::agent::Agent {
            description: description.to_string(),
            ..crate::roster::tests::minimal_agent(name)
        }
    }

    /// The `pie server` assembly end to end, over both transports the
    /// daemon offers: the in-process front door (card contents, a
    /// `SendMessage` driving `PieHost` — a real pie session opens and
    /// the turn runs — and the turn's terminal state in the crate's
    /// store), then the same gateway served as the daemon serves it
    /// (the crate's router on an ephemeral listener behind pie's
    /// Host-header guard): the card over HTTP, a disallowed Host
    /// rejected, and one `SendMessage` round-trip through the real HTTP
    /// path.
    ///
    /// One test function on purpose: the gateway's task store location
    /// is redirected through `A2A_ACP_HOME`, and environment variables
    /// cannot be raced by parallel tests.
    #[allow(clippy::too_many_lines)] // one flow, deliberately one fn
    #[tokio::test]
    async fn the_server_gateway_assembly_flow() {
        let store = tempfile::tempdir().unwrap();
        // SAFETY: set before the gateway (and its worker threads) exist
        // and before any spawned task could read the environment; this
        // is the only environment-touching test in this binary.
        unsafe { std::env::set_var("A2A_ACP_HOME", store.path()) };

        let server = server_config(
            "[server]\nbind = \"127.0.0.1:8629\"\nallowed_hosts = [\"pie.lvh.me\"]\n\n[server.agents.opencode]\ncommand = \"opencode\"\nargs = [\"acp\"]\n",
        );
        let mut config = gateway_config(&server, None).unwrap();
        // A roster with a duplicate-name agent: `pie` collides with the
        // default entry and must lose to it.
        let deps = ServerDeps {
            pool: Arc::new(db::create_test_pool().await.unwrap()),
            registry: Arc::new(Registry {
                agents: vec![
                    registry_agent("review", "reviews code"),
                    registry_agent("explore", "explores codebases"),
                    registry_agent("pie", "a hostile namesake"),
                ],
                skills: Vec::new(),
                completions: Vec::new(),
            }),
            sandbox: Arc::new(SandboxConfig::default()),
        };
        let resolved = test_resolved();
        let roster = RosterDeps {
            pool: deps.pool.clone(),
            registry: deps.registry.clone(),
            sandbox: deps.sandbox.clone(),
            config: &resolved,
        };
        let in_process = install(&mut config, &roster, roster.host_deps(None));
        let gateway = a2acp::a2a::gateway_from_config(&config, &in_process)
            .expect("the server gateway assembles");
        let door = gateway.connect();

        // The card is the agent directory: the default `pie` entry
        // first, then the rest in name order — the registry roster
        // (minus the colliding `pie`) alongside the external process
        // agent. Skill ids are the `metadata.agent` selectors.
        let card = door.card();
        let ids: Vec<&str> = card["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|skill| skill["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["pie", "explore", "opencode", "review"], "{card}");
        let skill = |id: &str| {
            card["skills"]
                .as_array()
                .unwrap()
                .iter()
                .find(|skill| skill["id"] == id)
                .unwrap()
        };
        // The default's own card line wins over the registry namesake's.
        assert_eq!(
            skill("pie")["description"].as_str().unwrap(),
            crate::roster::PIE_DEFAULT_DESCRIPTION
        );
        // Registry agents advertise their registry descriptions.
        assert_eq!(
            skill("review")["description"].as_str().unwrap(),
            "reviews code"
        );

        // SendMessage drives PieHost: a real pie session opens in the
        // message's cwd, the turn runs, and (against the dead provider)
        // completes the task in the crate's terminal FAILED state.
        let tmp = tempfile::tempdir().unwrap();
        let reply = door
            .call(json!({
                "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
                "params": {
                    "message": {
                        "parts": [{"kind": "text", "text": "hi"}],
                        "metadata": {"cwd": tmp.path().to_string_lossy()},
                    },
                },
            }))
            .await;
        let FrontReply::Envelope(envelope) = reply else {
            panic!("blocking SendMessage answers an envelope");
        };
        assert!(envelope.get("error").is_none(), "{envelope}");
        let task = &envelope["result"]["task"];
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED", "{envelope}");

        // Addressing a named roster agent works the same way: the skill
        // id IS the selector, the turn runs on that agent's host.
        let reply = door
            .call(json!({
                "jsonrpc": "2.0", "id": 2, "method": "SendMessage",
                "params": {
                    "message": {
                        "parts": [{"kind": "text", "text": "review this"}],
                        "metadata": {
                            "agent": "review",
                            "cwd": tmp.path().to_string_lossy(),
                        },
                    },
                },
            }))
            .await;
        let FrontReply::Envelope(envelope) = reply else {
            panic!("blocking SendMessage answers an envelope");
        };
        assert!(envelope.get("error").is_none(), "{envelope}");
        assert_eq!(
            envelope["result"]["task"]["status"]["state"], "TASK_STATE_FAILED",
            "the named agent's turn ran: {envelope}"
        );

        // The control: a selector the roster does not serve is an
        // invalid-params error, not a silent fallback to the default.
        let reply = door
            .call(json!({
                "jsonrpc": "2.0", "id": 3, "method": "SendMessage",
                "params": {
                    "message": {
                        "parts": [{"kind": "text", "text": "hi"}],
                        "metadata": {
                            "agent": "ghost",
                            "cwd": tmp.path().to_string_lossy(),
                        },
                    },
                },
            }))
            .await;
        let FrontReply::Envelope(envelope) = reply else {
            panic!("blocking SendMessage answers an envelope");
        };
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.contains("unknown agent"), "{envelope}");

        // The task is durable in the crate's store: GetTask finds it.
        let task_id = task["id"].as_str().unwrap().to_string();
        let fetched = door
            .call(json!({
                "jsonrpc": "2.0", "id": 4, "method": "GetTask",
                "params": {"id": task_id},
            }))
            .await;
        let FrontReply::Envelope(envelope) = fetched else {
            panic!("GetTask answers an envelope");
        };
        assert!(envelope.get("error").is_none(), "{envelope}");
        assert_eq!(
            envelope["result"]["status"]["state"], "TASK_STATE_FAILED",
            "{envelope}"
        );

        // ── the same gateway, served the way the daemon serves it ──────
        // The crate's router on an ephemeral listener behind pie's
        // Host-header guard — the assembly `serve` puts on the wire.
        let app = a2acp::a2a::router(Arc::clone(&gateway)).layer(from_fn_with_state(
            HostAllowlist::from_config(&server),
            host_guard,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let http = tokio::spawn(async move { axum::serve(listener, app).await });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // The card serves over HTTP, identical in shape to the door's.
        let card = client
            .get(format!("{base}{CARD_PATH}"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(card, door.card(), "both transports serve one card");

        // The configured host passes the guard (it reaches the card); a
        // foreign Host is rejected before the router.
        let allowed = client
            .get(format!("{base}{CARD_PATH}"))
            .header(header::HOST, "pie.lvh.me")
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        let denied = client
            .post(format!("{base}/a2a"))
            .header(header::HOST, "evil.example")
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 5, "method": "GetTask",
                "params": {"id": "nope"},
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(denied.text().await.unwrap(), "host not allowed\n");

        // One SendMessage round-trip through the real HTTP path: the
        // same dispatch the door exercised, driven over the wire.
        let envelope = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client
                .post(format!("{base}/a2a"))
                .json(&json!({
                    "jsonrpc": "2.0", "id": 6, "method": "SendMessage",
                    "params": {
                        "message": {
                            "parts": [{"kind": "text", "text": "hi over http"}],
                            "metadata": {"cwd": tmp.path().to_string_lossy()},
                        },
                    },
                }))
                .send(),
        )
        .await
        .expect("the HTTP turn finishes")
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
        assert!(envelope.get("error").is_none(), "{envelope}");
        assert_eq!(
            envelope["result"]["task"]["status"]["state"], "TASK_STATE_FAILED",
            "{envelope}"
        );

        http.abort();
    }

    fn test_resolved() -> ResolvedConfig {
        ResolvedConfig {
            provider: dead_provider(),
            retry: dead_retry(),
            model_tiers: std::collections::HashMap::new(),
            mcp: std::collections::HashMap::new(),
            pricing: std::collections::HashMap::new(),
            output_format: pie_core::utils::output::OutputFormat::default(),
            log_level: "warn".to_string(),
            debug: false,
        }
    }
}
