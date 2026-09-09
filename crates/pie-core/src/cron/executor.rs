use crate::agent::{AgentConfig, PieAgent};
use crate::config::CONFIG;
use crate::instructions::Instructions;
use crate::registry::Registry;
use crate::session::Session;
use p1e_sandbox::{Permission, SandboxConfig};
use std::collections::HashSet;
use std::sync::Arc;

pub async fn prompt_exec(
    session: &mut Session,
    prompt: &str,
    cwd: &std::path::Path,
    registry: Arc<Registry>,
    sandbox: Arc<SandboxConfig>,
    grants: HashSet<Permission>,
) -> i64 {
    let Some(config) = CONFIG.get() else {
        tracing::error!("config not initialized");
        return 1;
    };

    let instructions = Instructions::new(prompt);
    let mentions: Vec<String> = instructions.mentions.iter().cloned().collect();
    let resolved_skills = crate::registry::resolve_skills(&registry.skills, &mentions);

    for skill in resolved_skills {
        if let Err(e) = session
            .add_system(&format!(
                "## Skill: {}\n{}\n---\n",
                skill.name, skill.content
            ))
            .await
        {
            tracing::error!("failed to add skill context to session: {e}");
        }
    }

    if let Some(agent) = mentions
        .iter()
        .find_map(|m| registry.agents.iter().find(|a| a.name == *m))
        && let Err(e) = session
            .add_system(&format!("## Agent: {}\n{}", agent.name, agent.content))
            .await
    {
        tracing::warn!("failed to inject agent {}: {e}", agent.name);
    }

    let model = config.provider.build_client();

    let mut agent = PieAgent::new(
        model,
        registry,
        sandbox,
        session.clone(),
        AgentConfig {
            retry: config.retry.clone(),
            grants,
            cwd: Some(cwd.to_path_buf()),
            ..AgentConfig::default()
        },
    );

    match agent.run(prompt).await {
        Ok(_) => 0,
        Err(e) => {
            tracing::error!("prompt cron job failed: {e}");
            1
        }
    }
}
