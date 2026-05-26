use crate::registry::Registry;
use agentsdk::{AgentPlugin, PluginContext, PreToolAction};
use async_trait::async_trait;
use p1e_sandbox::Permission;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub struct PermissionRequest {
    pub skill: String,
    pub permissions: Vec<Permission>,
    pub response_tx: oneshot::Sender<bool>,
}

impl std::fmt::Debug for PermissionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionRequest")
            .field("skill", &self.skill)
            .field("permissions", &self.permissions)
            .finish_non_exhaustive()
    }
}

pub struct PermissionsPlugin {
    registry: Arc<Registry>,
    grants: HashSet<Permission>,
    permission_tx: Option<mpsc::UnboundedSender<PermissionRequest>>,
}

impl PermissionsPlugin {
    pub fn new(
        registry: Arc<Registry>,
        grants: HashSet<Permission>,
        permission_tx: Option<mpsc::UnboundedSender<PermissionRequest>>,
    ) -> Self {
        Self {
            registry,
            grants,
            permission_tx,
        }
    }

    fn ungranted_permissions(&self, skill: &crate::registry::Skill) -> Option<Vec<Permission>> {
        let perms_val = skill.extra.get("permissions")?;
        let perms: Vec<Permission> = serde_json::from_value(perms_val.clone()).ok()?;
        let ungranted: Vec<Permission> = perms
            .iter()
            .filter(|p| !self.grants.contains(p))
            .cloned()
            .collect();
        if ungranted.is_empty() {
            None
        } else {
            Some(ungranted)
        }
    }

    async fn prompt_permissions(&self, skill: &str, permissions: Vec<Permission>) -> bool {
        let Some(tx) = &self.permission_tx else {
            return false;
        };
        let (response_tx, response_rx) = oneshot::channel();
        let req = PermissionRequest {
            skill: skill.to_string(),
            permissions,
            response_tx,
        };
        if tx.send(req).is_err() {
            return false;
        }
        response_rx.await.unwrap_or(false)
    }
}

#[derive(Deserialize)]
struct LoadSkillsArgs {
    #[serde(default)]
    skills: Vec<String>,
    references: Option<Vec<SkillRefArg>>,
}

#[derive(Deserialize)]
struct SkillRefArg {
    skill: String,
}

#[derive(Deserialize)]
struct RunScriptArgs {
    skill: String,
}

#[async_trait]
impl AgentPlugin for PermissionsPlugin {
    fn name(&self) -> &'static str {
        "permissions"
    }

    async fn on_tool_pre_execute(
        &mut self,
        _ctx: &mut PluginContext,
        _id: &str,
        tool_name: &str,
        args: &Value,
    ) -> PreToolAction {
        match tool_name {
            "load_skills" => self.handle_load_skills(args).await,
            "run_skill_script" => self.handle_run_skill_script(args).await,
            _ => PreToolAction::Proceed(None),
        }
    }
}

impl PermissionsPlugin {
    async fn handle_load_skills(&self, args: &Value) -> PreToolAction {
        let Ok(parsed) = serde_json::from_value::<LoadSkillsArgs>(args.clone()) else {
            return PreToolAction::Proceed(None);
        };

        let mut all_names: Vec<String> = parsed
            .skills
            .iter()
            .map(|s| s.trim_start_matches('/').to_string())
            .collect();
        if let Some(refs) = &parsed.references {
            for sr in refs {
                let name = sr.skill.trim_start_matches('/').to_string();
                if !all_names.contains(&name) {
                    all_names.push(name);
                }
            }
        }

        if all_names.is_empty() {
            return PreToolAction::Proceed(None);
        }

        let resolved = crate::registry::resolve_skills(&self.registry.skills, &all_names);

        let mut missing: Vec<(String, Vec<Permission>)> = Vec::new();
        for skill in &resolved {
            if let Some(perms) = self.ungranted_permissions(skill) {
                missing.push((skill.name.clone(), perms));
            }
        }

        if missing.is_empty() {
            return PreToolAction::Proceed(None);
        }

        let all_perms: Vec<Permission> = missing
            .iter()
            .flat_map(|(_, perms)| perms.clone())
            .collect();
        let skills_label: Vec<&str> = missing.iter().map(|(name, _)| name.as_str()).collect();
        let skills_label = skills_label.join(", ");

        tracing::info!(skills = %skills_label, ?all_perms, "prompting for permissions");
        let granted = self.prompt_permissions(&skills_label, all_perms).await;
        tracing::info!(skills = %skills_label, granted, "permission response");

        if granted {
            PreToolAction::Proceed(None)
        } else {
            PreToolAction::Stop(format!(
                "Permission denied: skills '{skills_label}' require additional permissions.",
            ))
        }
    }

    async fn handle_run_skill_script(&self, args: &Value) -> PreToolAction {
        let Ok(parsed) = serde_json::from_value::<RunScriptArgs>(args.clone()) else {
            return PreToolAction::Proceed(None);
        };

        let skill_name = parsed.skill.trim_start_matches('/');
        let Some(skill) = self.registry.skills.iter().find(|s| s.name == skill_name) else {
            return PreToolAction::Proceed(None);
        };

        let Some(ungranted) = self.ungranted_permissions(skill) else {
            return PreToolAction::Proceed(None);
        };

        let perm_display: Vec<String> = ungranted.iter().map(ToString::to_string).collect();
        tracing::info!(skill = %skill_name, ?perm_display, "prompting for permissions");
        let granted = self.prompt_permissions(skill_name, ungranted).await;
        tracing::info!(skill = %skill_name, granted, "permission response");

        if granted {
            PreToolAction::Proceed(None)
        } else {
            PreToolAction::Stop(format!(
                "Permission denied: skill '{}' requires: {}",
                skill_name,
                perm_display.join(", ")
            ))
        }
    }
}
