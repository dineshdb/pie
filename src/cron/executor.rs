use crate::agent::{AgentConfig, PieAgent};
use crate::config::CONFIG;
use crate::db::DbPool;
use crate::instructions::Instructions;
use crate::registry::Registry;
use crate::session::Session;
use p1e_sandbox::SandboxConfig;
use std::sync::Arc;

struct CwdGuard {
    original: Option<std::path::PathBuf>,
}

impl CwdGuard {
    fn new(cwd: &str) -> Self {
        let original = std::env::current_dir().ok();
        let _ = std::env::set_current_dir(cwd);
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        if let Some(ref p) = self.original {
            let _ = std::env::set_current_dir(p);
        }
    }
}

pub async fn prompt_exec(
    session: &mut Session,
    prompt: &str,
    cwd: &str,
    registry: Arc<Registry>,
    pool: Arc<DbPool>,
    sandbox: Arc<SandboxConfig>,
) -> i64 {
    let _guard = CwdGuard::new(cwd);

    let Some(config) = CONFIG.get() else {
        tracing::error!("config not initialized");
        return 1;
    };

    let instructions = Instructions::new(prompt);
    let mentions: Vec<String> = instructions.mentions.iter().cloned().collect();
    let loaded_skills = crate::registry::resolve_skills(&registry.skills, &mentions);

    for skill in &loaded_skills {
        if let Err(e) = session
            .add_system(&format!(
                "## Skill: {}\n{}\n---\n",
                skill.name, skill.content
            ))
            .await
        {
            tracing::warn!("failed to inject skill {}: {e}", skill.name);
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
        pool,
        session.clone(),
        AgentConfig {
            max_steps: config.max_steps,
            retry: config.retry.clone(),
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
