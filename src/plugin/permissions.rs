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

    fn check_skill_permissions(&self, skill_name: &str) -> Option<Vec<Permission>> {
        let skill = self.registry.skills.iter().find(|s| s.name == skill_name)?;
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
struct SkillExecuteArgs {
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
        if tool_name != "skills__execute" {
            return PreToolAction::Proceed(None);
        }

        let Ok(parsed) = serde_json::from_value::<SkillExecuteArgs>(args.clone()) else {
            return PreToolAction::Proceed(None);
        };

        let skill_name = parsed.skill.trim_start_matches('/');
        let Some(ungranted) = self.check_skill_permissions(skill_name) else {
            return PreToolAction::Proceed(None);
        };

        let perm_display: Vec<String> = ungranted.iter().map(ToString::to_string).collect();
        tracing::info!(skill = %skill_name, ?perm_display, "prompting for permissions");
        let granted = self.prompt_permissions(skill_name, ungranted).await;
        tracing::info!(skill = %skill_name, granted, "permission response");

        if granted {
            PreToolAction::Proceed(None)
        } else {
            PreToolAction::Abort(format!(
                "Permission denied: skill '{}' requires: {}",
                parsed.skill,
                perm_display.join(", ")
            ))
        }
    }
}
