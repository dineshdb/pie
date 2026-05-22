use crate::agent::{Agent, get_all_agents};
use crate::cmd::BuiltinCommand;
use crate::config::{EMBEDDED_PIE_DIR, pie_home};
use agentsdk_plugin_skills::parse_skill;
use figment::providers::Format;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use strum::IntoEnumIterator;

pub use agentsdk_plugin_skills::Skill;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompletionKind {
    Builtin,
    Skill,
    Agent,
}

impl CompletionKind {
    pub fn color(self) -> tuirealm::ratatui::style::Color {
        use tuirealm::ratatui::style::Color;
        match self {
            Self::Builtin => Color::Yellow,
            Self::Skill => Color::Cyan,
            Self::Agent => Color::Green,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub description: String,
    pub kind: CompletionKind,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginMetadata {
    pub name: String,
    #[allow(dead_code)]
    pub version: Option<String>,
    pub description: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
}

impl PluginMetadata {
    pub fn from_toml_str(content: &str) -> anyhow::Result<Self> {
        let plugin: PluginMetadata = figment::Figment::new()
            .merge(figment::providers::Toml::string(content))
            .extract()?;
        Ok(plugin)
    }
}

#[derive(Debug, Clone)]
pub struct Registry {
    pub agents: Vec<Agent>,
    pub skills: Vec<Skill>,
    pub plugins: Vec<PluginMetadata>,
    pub completions: Vec<CompletionItem>,
}

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

impl Registry {
    pub fn load() -> Arc<Self> {
        let agents = get_all_agents();
        let skills = get_all_skills();

        let (_, plugins) = crate::plugin::scan_plugins();

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

        for plugin in &plugins {
            completions.push(CompletionItem {
                label: format!("/{}", plugin.name),
                description: plugin.description.clone(),
                kind: CompletionKind::Skill,
            });
        }

        completions.sort_by_key(|c| c.kind);

        let registry = Arc::new(Self {
            agents,
            skills,
            plugins,
            completions,
        });

        let _ = REGISTRY.set(registry.clone());
        registry
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

fn skills_root_local() -> Option<PathBuf> {
    crate::utils::git_repo_root()
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
    crate::utils::load_resources(
        load_embedded_skills(),
        &pie_home().join("skills"),
        skills_root_local(),
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
