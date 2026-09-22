//! pie's gateway roster: every registry agent addressable on the a2acp
//! gateway.
//!
//! The gateway's agent card is the agent directory (one skill per
//! registered agent, `skill.id` == the `metadata.agent` selector, the
//! default first). pie's contribution: one in-process [`PieHost`] per
//! registry agent — each building its turns with THAT agent's config
//! (model tier via the CLI's `resolve_agent_provider` logic, the
//! agent's sandbox merged onto the base one, the persona via
//! `agent_name`; output mode rides the persona) — plus a display-only
//! [`a2acp::AgentSpec`] so the card carries the agent's registry
//! description. The turn logic itself is never duplicated: the factory
//! only varies [`pie_acp::HostDeps`].
//!
//! Collision rule: an explicitly configured gateway agent name always
//! wins over a registry agent of the same name — the default `pie`
//! entry, and any `[server.agents]` process spec. The losing registry
//! agent is skipped with a warning, never silently shadowed.

use crate::resolve_agent_provider;
use a2acp::InProcessAgent;
use pie_acp::{HostDeps, PieHost};
use pie_core::agent::Agent;
use pie_core::config::ResolvedConfig;
use pie_core::db::DbPool;
use pie_core::p1e_sandbox::SandboxConfig;
use pie_core::registry::Registry;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Pie's name on the gateway: the default (in-process) agent entry.
pub(crate) const PIE_AGENT: &str = "pie";

/// Everything the roster factory needs: the shared handles, the base
/// sandbox (BEFORE any agent's merge), and the resolved config (default
/// provider, model tiers, retry).
pub(crate) struct RosterDeps<'a> {
    pub pool: Arc<DbPool>,
    pub registry: Arc<Registry>,
    pub sandbox: Arc<SandboxConfig>,
    pub config: &'a ResolvedConfig,
}

impl RosterDeps<'_> {
    /// The host dependencies for one agent's entry: the agent's model
    /// (tier name or literal on the default provider), its sandbox
    /// merged onto the base, its persona pinned. `None` is the bare
    /// default entry (the user's provider and model, no persona).
    /// The tier table rides along so selection-extension model picks
    /// resolve the same way this resolver works.
    pub(crate) fn host_deps(&self, agent: Option<&Agent>) -> HostDeps {
        let mut sandbox = (*self.sandbox).clone();
        if let Some(agent_sandbox) = agent.and_then(|a| a.sandbox.as_ref()) {
            sandbox.merge(agent_sandbox);
        }
        HostDeps {
            pool: Arc::clone(&self.pool),
            registry: Arc::clone(&self.registry),
            sandbox: Arc::new(sandbox),
            provider: resolve_agent_provider(
                agent,
                &self.config.provider,
                &self.config.model_tiers,
            ),
            retry: self.config.retry.clone(),
            model_tiers: self.config.model_tiers.clone(),
            agent_name: agent.map(|a| a.name.clone()),
            resume: None,
        }
    }
}

/// Install pie's roster on a gateway config: the default `pie` entry
/// (the given host deps) plus one in-process entry per registry agent,
/// each with a display-only spec so the card carries its registry
/// description. Returns the in-process map for
/// [`a2acp::a2a::gateway_from_config`]. Names already configured on the
/// gateway (`[server.agents]`, the default itself) win over registry
/// agents of the same name — the loser is skipped with a warning.
pub(crate) fn install(
    config: &mut a2acp::Config,
    deps: &RosterDeps<'_>,
    default: HostDeps,
) -> BTreeMap<String, Arc<dyn InProcessAgent>> {
    let mut hosts = BTreeMap::from([(PIE_AGENT.to_string(), host_of(default))]);
    config
        .agents
        .entry(PIE_AGENT.to_string())
        .or_insert_with(|| display_spec(PIE_DEFAULT_DESCRIPTION));
    for agent in &deps.registry.agents {
        if agent.name == PIE_AGENT || config.agents.contains_key(&agent.name) {
            tracing::warn!(
                agent = %agent.name,
                "registry agent collides with a configured gateway agent — the configured entry wins, skipping the registry one"
            );
            continue;
        }
        config
            .agents
            .insert(agent.name.clone(), display_spec(&agent.description));
        hosts.insert(agent.name.clone(), host_of(deps.host_deps(Some(agent))));
    }
    hosts
}

/// The card's line for the default entry.
pub(crate) const PIE_DEFAULT_DESCRIPTION: &str =
    "pie's default agent (in-process): the configured provider and model";

/// Wrap host deps as the gateway's in-process agent.
fn host_of(deps: HostDeps) -> Arc<dyn InProcessAgent> {
    Arc::new(PieHost::new(deps))
}

/// A display-only spec: never spawned (the in-process registration
/// shadows it), purely the card's skill description.
fn display_spec(description: &str) -> a2acp::AgentSpec {
    a2acp::AgentSpec {
        description: (!description.is_empty()).then(|| description.to_string()),
        ..a2acp::AgentSpec::new("", &[])
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pie_core::agent::OutputMode;
    use pie_core::config::RetryConfig;

    /// A minimal named agent — the shared fixture for this crate's
    /// gateway tests (`roster`, `server`, the interactive assembly).
    pub(crate) fn minimal_agent(name: &str) -> Agent {
        Agent {
            name: name.to_string(),
            description: String::new(),
            output_mode: OutputMode::default(),
            model: None,
            temperature: None,
            content: String::new(),
            needs: Vec::new(),
            tools: Vec::new(),
            sandbox: None,
            grants: Vec::new(),
            readonly: false,
            plugins: None,
            skills_paths: Vec::new(),
            max_steps: None,
        }
    }

    fn agent(name: &str, model: Option<&str>, sandbox: Option<SandboxConfig>) -> Agent {
        Agent {
            description: format!("the {name} agent"),
            model: model.map(str::to_string),
            sandbox,
            ..minimal_agent(name)
        }
    }

    fn registry(agents: Vec<Agent>) -> Arc<Registry> {
        Arc::new(Registry {
            agents,
            skills: Vec::new(),
            completions: Vec::new(),
        })
    }

    async fn deps(agents: Vec<Agent>) -> RosterDeps<'static> {
        let mut tiers = std::collections::HashMap::new();
        tiers.insert(
            "heavy".to_string(),
            pie_core::config::ResolvedProvider {
                name: "tier-provider".into(),
                model: "tier-model".into(),
                ..default_provider()
            },
        );
        let config = Box::leak(Box::new(ResolvedConfig {
            provider: default_provider(),
            retry: RetryConfig::default(),
            model_tiers: tiers,
            mcp: std::collections::HashMap::new(),
            pricing: std::collections::HashMap::new(),
            output_format: pie_core::utils::output::OutputFormat::default(),
            log_level: "warn".to_string(),
            debug: false,
        }));
        RosterDeps {
            pool: Arc::new(pie_core::db::create_test_pool().await.unwrap()),
            registry: registry(agents),
            sandbox: Arc::new(SandboxConfig::default()),
            config,
        }
    }

    fn default_provider() -> pie_core::config::ResolvedProvider {
        pie_core::config::ResolvedProvider {
            name: "default-provider".into(),
            model: "default-model".into(),
            anthropic_url: None,
            openai_url: "http://127.0.0.1:9/v1".parse().unwrap(),
            api_key: redact::Secret::new("k".into()),
            temperature: None,
        }
    }

    #[tokio::test]
    async fn host_deps_resolves_the_agents_model_tier() {
        let d = deps(vec![agent("review", Some("heavy"), None)]).await;
        let host = d.host_deps(d.registry.agents.first());
        assert_eq!(host.provider.name, "tier-provider");
        assert_eq!(host.provider.model, "tier-model");
        assert_eq!(host.agent_name.as_deref(), Some("review"));
    }

    #[tokio::test]
    async fn host_deps_falls_back_to_a_literal_model() {
        let d = deps(vec![agent("review", Some("some-literal"), None)]).await;
        let host = d.host_deps(d.registry.agents.first());
        assert_eq!(host.provider.name, "default-provider");
        assert_eq!(host.provider.model, "some-literal");
    }

    #[tokio::test]
    async fn host_deps_without_a_model_keeps_the_default_provider() {
        let d = deps(vec![agent("review", None, None)]).await;
        let host = d.host_deps(d.registry.agents.first());
        assert_eq!(host.provider.model, "default-model");
        let bare = d.host_deps(None);
        assert_eq!(bare.provider.model, "default-model");
        assert_eq!(bare.agent_name, None);
    }

    #[tokio::test]
    async fn host_deps_merges_the_agents_sandbox() {
        let mut agent_sandbox = SandboxConfig::default();
        agent_sandbox.deny_write.push("~/secrets".into());
        let d = deps(vec![agent("review", None, Some(agent_sandbox))]).await;
        let host = d.host_deps(d.registry.agents.first());
        assert!(host.sandbox.deny_write.contains(&"~/secrets".to_string()));
        let bare = d.host_deps(None);
        assert!(!bare.sandbox.deny_write.contains(&"~/secrets".to_string()));
    }

    /// The door-shaped gateway config: no crate built-ins, no process
    /// agents — the way both of pie's doors build it.
    fn bare_config() -> a2acp::Config {
        a2acp::Config {
            agents: BTreeMap::new(),
            ..a2acp::Config::default()
        }
    }

    #[tokio::test]
    async fn install_registers_default_first_then_the_roster() {
        let d = deps(vec![
            agent("review", None, None),
            agent("explore", None, None),
        ])
        .await;
        let mut config = bare_config();
        let hosts = install(&mut config, &d, d.host_deps(None));
        let mut names: Vec<&str> = hosts.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["explore", "pie", "review"]);
        assert_eq!(
            config.agents["review"].description.as_deref(),
            Some("the review agent")
        );
        assert_eq!(
            config.agents[PIE_AGENT].description.as_deref(),
            Some(PIE_DEFAULT_DESCRIPTION)
        );
    }

    #[tokio::test]
    async fn install_keeps_the_default_over_a_registry_pie() {
        let d = deps(vec![agent("pie", None, None), agent("review", None, None)]).await;
        let mut config = bare_config();
        let hosts = install(&mut config, &d, d.host_deps(None));
        assert_eq!(hosts.len(), 2, "the registry 'pie' must not add an entry");
        assert_eq!(
            config.agents[PIE_AGENT].description.as_deref(),
            Some(PIE_DEFAULT_DESCRIPTION),
            "the default's card line wins, not the registry agent's"
        );
    }

    #[tokio::test]
    async fn install_keeps_a_configured_spec_over_a_registry_agent() {
        let d = deps(vec![agent("opencode", None, None)]).await;
        let mut config = bare_config();
        config.agents.insert(
            "opencode".to_string(),
            a2acp::AgentSpec::new("opencode", &["acp"]),
        );
        let hosts = install(&mut config, &d, d.host_deps(None));
        assert!(!hosts.contains_key("opencode"), "the process spec hosts it");
        assert_eq!(
            config.agents["opencode"].command, "opencode",
            "the explicit spec survives untouched"
        );
    }
}
