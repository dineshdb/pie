use agentsdk_plugin_skills::split_frontmatter;
use p1e_sandbox::{Permission, SandboxConfig};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Schedule {
    pub id: String,
    /// When it is time. `None` means the schedule is driven purely by `when`.
    pub cron: Option<String>,
    /// CEL precondition. `None` means "no precondition".
    pub when: Option<String>,
    pub description: String,
    pub enabled: bool,
    pub prompt: String,
    pub source_path: PathBuf,
    pub sandbox: Option<SandboxConfig>,
    pub grants: Vec<Permission>,
}

impl Schedule {
    /// A schedule with neither trigger can never fire, and is far more likely
    /// to be a mistake than an intent.
    pub fn has_trigger(&self) -> bool {
        self.cron.is_some() || self.when.is_some()
    }
}

/// Schedules are enabled unless they say otherwise.
///
/// This defaulted to `false`, which meant a file that never mentioned
/// `enabled` silently did nothing — indistinguishable from a broken cron
/// expression or a daemon that was not running. Opting out is the rarer
/// intent and is the one worth spelling.
const fn enabled_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ScheduleFrontmatter {
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    id: String,
    #[serde(default = "enabled_default")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox: Option<SandboxConfig>,
    #[serde(default)]
    grants: Vec<Permission>,
}

fn schedule_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let global = crate::config::pie_home().join("schedules");
    dirs.push(global);
    if let Some(root) = crate::utils::git_repo_root() {
        let local = PathBuf::from(root).join(".pie").join("schedules");
        if local.is_dir() {
            dirs.push(local);
        }
    }
    dirs
}

/// Load all schedule files from global and local directories.
/// Local schedules override global ones with the same id.
pub fn load_all_schedules() -> Vec<Schedule> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut schedules = Vec::new();

    let dirs = schedule_dirs();
    // dirs[0] = global (processed first), dirs[1] = local (overrides)

    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(ext) = path.extension() else {
                continue;
            };
            if ext != "md" {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (yaml, body) = split_frontmatter(&raw);
            if yaml.is_empty() || body.is_empty() {
                continue;
            }
            let Ok(meta) = serde_yaml::from_str::<ScheduleFrontmatter>(&yaml) else {
                eprintln!("schedule '{name}' has invalid frontmatter");
                continue;
            };
            let id = if meta.id.is_empty() {
                name.to_string()
            } else {
                meta.id
            };

            if !seen.insert(id.clone()) {
                if let Some(pos) = schedules.iter().position(|s: &Schedule| s.id == id)
                    && let Some(entry) = schedules.get_mut(pos)
                {
                    *entry = Schedule {
                        id,
                        cron: meta.cron,
                        when: meta.when,
                        description: meta.description,
                        enabled: meta.enabled,
                        prompt: body,
                        source_path: path,
                        sandbox: meta.sandbox.clone(),
                        grants: meta.grants.clone(),
                    };
                }
                continue;
            }
            schedules.push(Schedule {
                id,
                cron: meta.cron,
                when: meta.when,
                description: meta.description,
                enabled: meta.enabled,
                prompt: body,
                source_path: path,
                sandbox: meta.sandbox,
                grants: meta.grants,
            });
        }
    }

    schedules
}
