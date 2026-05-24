use crate::config::EMBEDDED_PIE_DIR;
use agentsdk_plugin_skills::split_frontmatter;
use include_dir::Dir;
use p1e_sandbox::{Permission, SandboxConfig};
use serde::Deserialize;
use std::path::PathBuf;
use strum::{AsRefStr, Display, EnumString};

/// Embedded agents directory (from .pie/agents/ in the crate root).
pub fn embedded_agents_dir() -> Option<&'static Dir<'static>> {
    EMBEDDED_PIE_DIR.get_dir("commands")
}

/// Controls the format and level of interactivity for the agent's output.
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    AsRefStr,
    Display,
    EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum OutputMode {
    #[default]
    Md,
    Json,
    Interactive,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub output_mode: OutputMode,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub content: String,
    pub needs: Vec<String>,
    pub tools: Vec<String>,
    pub sandbox: Option<SandboxConfig>,
    #[allow(dead_code)]
    pub grants: Vec<Permission>,
}

/// Serde-deserializable frontmatter for agent files.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AgentFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(alias = "interactivity")]
    output_mode: OutputMode,
    model: Option<String>,
    temperature: Option<f32>,
    needs: Vec<String>,
    tools: Vec<String>,
    sandbox: Option<SandboxConfig>,
    #[serde(default)]
    grants: Vec<Permission>,
}

fn agents_root_global() -> PathBuf {
    crate::config::pie_home().join("commands")
}

fn agents_root_local() -> Option<PathBuf> {
    crate::utils::git_repo_root()
        .map(|root| PathBuf::from(root).join(".pie").join("commands"))
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
        serde_yaml::from_str(&yaml).unwrap_or_default()
    };
    let name = meta.name.map_or_else(
        || {
            std::path::Path::new(filename)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        },
        |s| s.trim().to_string(),
    );
    if name.is_empty() {
        return None;
    }
    let description = meta.description.map_or_else(
        || {
            content
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .unwrap_or_default()
        },
        |s| s.trim().to_string(),
    );
    Some(Agent {
        name,
        description,
        output_mode: meta.output_mode,
        model: meta.model,
        temperature: meta.temperature,
        content,
        needs: meta.needs,
        tools: meta.tools,
        sandbox: meta.sandbox,
        grants: meta.grants,
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

/// Load all agents: embedded + global (~/.pie/agents/) + local (.pie/agents/).
/// Local overrides global, global overrides embedded.
pub fn get_all_agents() -> Vec<Agent> {
    crate::utils::load_resources(
        load_embedded_agents(),
        &agents_root_global(),
        agents_root_local(),
        load_agents_from_dir,
        |a| &a.name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn parse_agent_full() -> Result<()> {
        let raw = "---\nname: reviewer\ndescription: code reviewer\noutput_mode: interactive\nmodel: llama3\ntemperature: 0.3\n---\nBe direct and thorough.";
        let agent =
            parse_agent(raw, "reviewer.md").ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description, "code reviewer");
        assert_eq!(agent.output_mode, OutputMode::Interactive);
        assert_eq!(agent.model.as_deref(), Some("llama3"));
        let temp = agent
            .temperature
            .ok_or_else(|| anyhow::anyhow!("expected temperature"))?;
        assert!((temp - 0.3).abs() < f32::EPSILON);
        assert_eq!(agent.content, "Be direct and thorough.");
        Ok(())
    }

    #[test]
    fn parse_agent_output_mode_values() -> Result<()> {
        for (val, expected) in [
            ("md", OutputMode::Md),
            ("json", OutputMode::Json),
            ("interactive", OutputMode::Interactive),
        ] {
            let raw = format!("---\nname: t\noutput_mode: {val}\n---\ncontent");
            let agent = parse_agent(&raw, "t.md")
                .ok_or_else(|| anyhow::anyhow!("parse failed for {val}"))?;
            assert_eq!(agent.output_mode, expected, "failed for {val}");
        }
        Ok(())
    }

    #[test]
    fn parse_agent_no_frontmatter() -> Result<()> {
        let raw = "You are a codebase analyst.\nReport findings concisely.";
        let agent =
            parse_agent(raw, "explore.md").ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert_eq!(agent.name, "explore");
        assert_eq!(agent.description, "You are a codebase analyst.");
        assert_eq!(agent.output_mode, OutputMode::Md);
        assert_eq!(
            agent.content,
            "You are a codebase analyst.\nReport findings concisely."
        );
        Ok(())
    }
}
