use crate::skill::split_frontmatter;
use serde::Deserialize;
use std::path::PathBuf;

/// A file-based schedule defined in a `.md` file with frontmatter.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub id: String,
    pub cron: String,
    pub description: String,
    pub enabled: bool,
    pub prompt: String,
    pub source_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ScheduleFrontmatter {
    cron: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    enabled: bool,
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
                        description: meta.description,
                        enabled: meta.enabled,
                        prompt: body,
                        source_path: path,
                    };
                }
                continue;
            }
            schedules.push(Schedule {
                id,
                cron: meta.cron,
                description: meta.description,
                enabled: meta.enabled,
                prompt: body,
                source_path: path,
            });
        }
    }

    schedules
}
