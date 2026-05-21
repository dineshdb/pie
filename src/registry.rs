use crate::agent::{Agent, get_all_agents};
use crate::cmd::BuiltinCommand;
use crate::skill::{Skill, get_all_skills};
use figment::providers::Format;
use serde::Deserialize;
use std::sync::{Arc, OnceLock};
use strum::IntoEnumIterator;

/// Kind of completion item — used for visual differentiation in the popup.
/// Sort order: Builtin (0) < Skill (1) < Agent (2).
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

/// A single entry in the completion popup.
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
    /// Parse plugin metadata from a TOML string.
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
                kind: CompletionKind::Skill, // Reuse Skill kind for now
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
