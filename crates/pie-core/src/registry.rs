use crate::agent::{Agent, get_all_agents_from};
use crate::cmd::BuiltinCommand;
use crate::config::{EMBEDDED_PIE_DIR, pie_home};
use agentsdk_plugin_skills::parse_skill;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError};
use strum::IntoEnumIterator;

pub use agentsdk_plugin_skills::{Reference, Skill};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompletionKind {
    Builtin,
    Skill,
    Agent,
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub description: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone)]
pub struct Registry {
    pub agents: Vec<Agent>,
    pub skills: Vec<Skill>,
    pub completions: Vec<CompletionItem>,
}

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

/// Registries keyed by canonical workspace root. The CLI loads one registry
/// for the process cwd ([`Registry::load`]); servers serve turns in many
/// workspaces, so project-level `.pie/{agents,skills}` discovery is per root
/// and cached here.
#[derive(Default)]
pub struct RegistryCache(StdMutex<HashMap<PathBuf, Arc<Registry>>>);

impl std::fmt::Debug for RegistryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryCache").finish_non_exhaustive()
    }
}

impl RegistryCache {
    /// The registry for a workspace, loaded on first use and cached by
    /// canonical path.
    pub fn get(&self, cwd: &std::path::Path) -> Arc<Registry> {
        let key = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(registry) = map.get(&key) {
            return registry.clone();
        }
        let registry = Registry::load_from(&key);
        map.insert(key, registry.clone());
        registry
    }
}

impl Registry {
    /// Load the registry for the process working directory and memoize it
    /// (the CLI frontends). Servers serving multiple directories use
    /// [`Registry::load_from`] with their own cache instead.
    pub fn load() -> Arc<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let registry = Self::load_from(&cwd);
        let _ = REGISTRY.set(registry.clone());
        registry
    }

    /// Load the registry for a run rooted at `cwd`: project-level
    /// `.pie/{agents,skills}` discovery walks up from `cwd`. Never
    /// memoized — the caller decides the cache key.
    pub fn load_from(cwd: &std::path::Path) -> Arc<Self> {
        let agents = get_all_agents_from(cwd);
        let skills = get_all_skills_from(cwd);

        let mut completions = Vec::new();

        for cmd in BuiltinCommand::iter() {
            for name in cmd.names() {
                completions.push(CompletionItem {
                    label: name.to_string(),
                    description: cmd.description().to_string(),
                    kind: CompletionKind::Builtin,
                });
            }
        }

        for skill in &skills {
            completions.push(CompletionItem {
                label: format!("/{}", skill.name),
                description: skill.description.clone(),
                kind: CompletionKind::Skill,
            });
        }

        for agent in &agents {
            completions.push(CompletionItem {
                label: format!("/{}", agent.name),
                description: agent.description.clone(),
                kind: CompletionKind::Agent,
            });
        }

        completions.sort_by_key(|c| c.kind);

        Arc::new(Self {
            agents,
            skills,
            completions,
        })
    }
}

pub fn resolve_skills<'a>(all: &'a [Skill], names: &[String]) -> Vec<&'a Skill> {
    let mut resolved = Vec::new();
    let mut visited = HashSet::new();
    let mut stack: Vec<&str> = names.iter().map(String::as_str).collect();
    while let Some(name) = stack.pop() {
        if !visited.insert(name) {
            continue;
        }
        if let Some(skill) = all.iter().find(|s| s.name == name) {
            resolved.push(skill);
            for need in &skill.needs {
                stack.push(need.as_str());
            }
        }
    }
    resolved.reverse();
    resolved
}

fn skills_root_local_from(cwd: &std::path::Path) -> Option<PathBuf> {
    crate::utils::git_repo_root_from(cwd)
        .map(|root| PathBuf::from(root).join(".pie").join("skills"))
        .filter(|p| p.is_dir())
}

fn load_embedded_skills() -> Vec<Skill> {
    let Some(skills_dir) = EMBEDDED_PIE_DIR.get_dir("skills") else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for dir in skills_dir.dirs() {
        for file in dir.files() {
            if file.path().ends_with("SKILL.md")
                && let Some(content) = file.contents_utf8()
                && let Some(mut skill) = parse_skill(content)
            {
                skill.path = dir.path().to_path_buf();
                skills.push(skill);
            }
        }
    }
    skills
}

pub fn get_all_skills() -> Vec<Skill> {
    get_all_skills_from(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

pub fn get_all_skills_from(cwd: &std::path::Path) -> Vec<Skill> {
    crate::utils::load_resources(
        load_embedded_skills(),
        &pie_home().join("skills"),
        skills_root_local_from(cwd),
        load_skills_from_dir,
        |s| &s.name,
    )
}

fn load_skills_from_dir(dir: &std::path::Path) -> Vec<Skill> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let md_path = e.path().join("SKILL.md");
            let raw = fs::read_to_string(&md_path).ok()?;
            let mut skill = parse_skill(&raw)?;
            skill.path = e.path();
            Some(skill)
        })
        .collect()
}
