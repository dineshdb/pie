use crate::skill::split_frontmatter;
use include_dir::{Dir, include_dir};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;
use strum::{AsRefStr, EnumString};

static EMBEDDED_PIE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/.pie");

/// Embedded agents directory (from .pie/agents/ in the crate root).
pub fn embedded_agents_dir() -> Option<&'static Dir<'static>> {
    EMBEDDED_PIE_DIR.get_dir("agents")
}

// ── Interactivity ─────────────────────────────────────────────────

/// Controls whether and how an agent may ask the user questions.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    AsRefStr,
    EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Interactivity {
    #[default]
    None,
    Minimal,
    Interactive,
}

// ── Agent Definition ──────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub interactivity: Interactivity,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub content: String,
}

/// Serde-deserializable frontmatter for agent files.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AgentFrontmatter {
    name: Option<String>,
    description: Option<String>,
    interactivity: Interactivity,
    model: Option<String>,
    temperature: Option<f32>,
}

fn agents_root_global() -> PathBuf {
    crate::config::pie_home().join("agents")
}

fn agents_root_local() -> Option<PathBuf> {
    crate::utils::git_repo_root()
        .map(|root| PathBuf::from(root).join(".pie").join("agents"))
        .filter(|p| p.is_dir())
}

/// Parse a raw markdown string with optional frontmatter into an Agent.
/// When frontmatter is absent or incomplete, falls back to:
///   - name from the filename (stem, without extension)
///   - description from the first non-empty line of the body
fn parse_agent(raw: &str, filename: &str) -> Option<Agent> {
    let (yaml, content) = split_frontmatter(raw);
    let meta: AgentFrontmatter = if yaml.is_empty() {
        AgentFrontmatter::default()
    } else {
        serde_yml::from_str(&yaml).unwrap_or_default()
    };
    let name = meta.name.map(|s| s.trim().to_string()).unwrap_or_else(|| {
        std::path::Path::new(filename)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    if name.is_empty() {
        return None;
    }
    let description = meta
        .description
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            content
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .unwrap_or_default()
        });
    Some(Agent {
        name,
        description,
        interactivity: meta.interactivity,
        model: meta.model,
        temperature: meta.temperature,
        content,
    })
}

fn load_agents_from_dir(dir: &std::path::Path) -> Vec<Agent> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.path().extension().is_some_and(|ext| ext == "md")
        })
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            let filename = e.file_name().to_string_lossy().to_string();
            parse_agent(&raw, &filename)
        })
        .collect()
}

/// Load embedded agents from .pie/agents/*.md compiled into the binary.
fn load_embedded_agents() -> Vec<Agent> {
    let Some(dir) = embedded_agents_dir() else {
        return Vec::new();
    };
    dir.files()
        .filter(|f| f.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|f| {
            let raw = f.contents_utf8()?;
            let filename = f.path().file_name()?.to_string_lossy().to_string();
            parse_agent(raw, &filename)
        })
        .collect()
}

/// Merge agents into the list, overriding by name.
fn merge_agents(agents: &mut Vec<Agent>, names: &mut HashSet<String>, incoming: Vec<Agent>) {
    for agent in incoming {
        if names.contains(&agent.name) {
            if let Some(existing) = agents.iter_mut().find(|a| a.name == agent.name) {
                *existing = agent;
            }
        } else {
            names.insert(agent.name.clone());
            agents.push(agent);
        }
    }
}

/// Load all agents: embedded + global (~/.pie/agents/) + local (.pie/agents/).
/// Local overrides global, global overrides embedded.
pub fn get_all_agents() -> Vec<Agent> {
    let mut agents: Vec<Agent> = load_embedded_agents();
    let mut names: HashSet<String> = agents.iter().map(|a| a.name.clone()).collect();

    merge_agents(
        &mut agents,
        &mut names,
        load_agents_from_dir(&agents_root_global()),
    );

    if let Some(local_dir) = agents_root_local() {
        merge_agents(&mut agents, &mut names, load_agents_from_dir(&local_dir));
    }

    agents
}

/// Resolve agents mentioned as `/agent-name` in the given sources.
pub fn resolve_mentioned_agents<'a>(sources: &[&str], agents: &'a [Agent]) -> Vec<&'a Agent> {
    let patterns: Vec<String> = agents.iter().map(|a| format!("/{}", a.name)).collect();
    agents
        .iter()
        .zip(&patterns)
        .filter(|(_, pat)| sources.iter().any(|s| s.contains(pat.as_str())))
        .map(|(agent, _)| agent)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_full() {
        let raw = "---\nname: reviewer\ndescription: code reviewer\ninteractivity: minimal\nmodel: llama3\ntemperature: 0.3\n---\nBe direct and thorough.";
        let agent = parse_agent(raw, "reviewer.md").unwrap();
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description, "code reviewer");
        assert_eq!(agent.interactivity, Interactivity::Minimal);
        assert_eq!(agent.model.as_deref(), Some("llama3"));
        assert!((agent.temperature.unwrap() - 0.3).abs() < f32::EPSILON);
        assert_eq!(agent.content, "Be direct and thorough.");
    }

    #[test]
    fn parse_agent_minimal() {
        let raw = "---\nname: helper\ndescription: helps\n---\nContent";
        let agent = parse_agent(raw, "helper.md").unwrap();
        assert_eq!(agent.name, "helper");
        assert_eq!(agent.interactivity, Interactivity::None);
        assert!(agent.model.is_none());
        assert!(agent.temperature.is_none());
    }

    #[test]
    fn parse_agent_interactivity_values() {
        for (val, expected) in [
            ("none", Interactivity::None),
            ("minimal", Interactivity::Minimal),
            ("interactive", Interactivity::Interactive),
        ] {
            let raw = format!("---\nname: t\ninteractivity: {val}\n---\ncontent");
            let agent = parse_agent(&raw, "t.md").unwrap();
            assert_eq!(agent.interactivity, expected, "failed for {val}");
        }
    }

    #[test]
    fn parse_agent_no_frontmatter() {
        let raw = "You are a codebase analyst.\nReport findings concisely.";
        let agent = parse_agent(raw, "explore.md").unwrap();
        assert_eq!(agent.name, "explore");
        assert_eq!(agent.description, "You are a codebase analyst.");
        assert_eq!(agent.interactivity, Interactivity::None);
        assert_eq!(
            agent.content,
            "You are a codebase analyst.\nReport findings concisely."
        );
    }

    #[test]
    fn parse_agent_no_frontmatter_empty_file() {
        let raw = "";
        let agent = parse_agent(raw, "empty.md");
        let a = agent.unwrap();
        assert_eq!(a.name, "empty");
        assert!(a.content.is_empty());
    }

    #[test]
    fn resolve_mentioned_agents_from_query() {
        let agents = vec![
            Agent {
                name: "reviewer".into(),
                description: "reviews code".into(),
                interactivity: Interactivity::Minimal,
                model: None,
                temperature: None,
                content: "Be thorough.".into(),
            },
            Agent {
                name: "planner".into(),
                description: "plans tasks".into(),
                interactivity: Interactivity::Interactive,
                model: None,
                temperature: None,
                content: "Think step by step.".into(),
            },
        ];
        let mentioned = resolve_mentioned_agents(&["/reviewer check this"], &agents);
        assert_eq!(mentioned.len(), 1);
        assert_eq!(mentioned[0].name, "reviewer");
    }

    #[test]
    fn resolve_mentioned_agents_no_match() {
        let agents = vec![Agent {
            name: "reviewer".into(),
            description: "reviews".into(),
            interactivity: Interactivity::None,
            model: None,
            temperature: None,
            content: String::new(),
        }];
        let mentioned = resolve_mentioned_agents(&["nothing relevant"], &agents);
        assert!(mentioned.is_empty());
    }
}
