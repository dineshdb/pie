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
    pub grants: Vec<Permission>,
    /// Restrict this agent's filesystem tools to read-only operations.
    pub readonly: bool,
    /// Explicit plugin selection — the agent's complete tool set.
    /// `None` (the default in legacy `commands/` frontmatter and agent-less
    /// runs) means the full built-in set; `Some(..)` is exactly that list,
    /// so `Some(vec![])` means no tools. Agents loaded from the `agents/`
    /// directories are normalized to `Some(vec![])` unless the frontmatter
    /// lists plugins — new agent drops start with no tools.
    /// Known names: `fs`, `fs-readonly`, `shell`, `websearch`, `skills`,
    /// `agentsmd`, `mcp` (all `[mcp.*]` servers from config), or
    /// `mcp:<server>` for specific ones. Unknown names fail the agent run.
    pub plugins: Option<Vec<String>>,
    /// Extra skill search directories for the skills plugin.
    pub skills_paths: Vec<String>,
    /// Override the configured max steps for this agent.
    pub max_steps: Option<u32>,
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
    /// Restrict this agent's filesystem tools to read-only operations.
    #[serde(default)]
    readonly: bool,
    #[serde(default)]
    plugins: Option<Vec<String>>,
    #[serde(default)]
    skills_paths: Vec<String>,
    #[serde(default)]
    max_steps: Option<u32>,
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
        serde_yaml::from_str(&yaml).unwrap_or_else(|e| {
            tracing::warn!("agent '{filename}': invalid frontmatter ({e}) — using defaults");
            AgentFrontmatter::default()
        })
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
        readonly: meta.readonly,
        plugins: meta.plugins,
        skills_paths: meta.skills_paths,
        max_steps: meta.max_steps,
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

/// Load all agents. Markdown everywhere: embedded + `~/.pie/commands/` +
/// `.pie/commands/` (legacy locations — full default tool set), then
/// `~/.pie/agents/` + `.pie/agents/` (strict — no tools unless `plugins:`
/// says otherwise). Later layers override by name.
pub fn get_all_agents() -> Vec<Agent> {
    let base = crate::utils::load_resources(
        load_embedded_agents(),
        &agents_root_global(),
        agents_root_local(),
        load_agents_from_dir,
        |a| &a.name,
    );
    crate::utils::load_resources(
        base,
        &agents_strict_root_global(),
        agents_strict_root_local(),
        load_strict_agents_from_dir,
        |a| &a.name,
    )
}

fn agents_strict_root_global() -> PathBuf {
    crate::config::pie_home().join("agents")
}

fn agents_strict_root_local() -> Option<PathBuf> {
    crate::utils::git_repo_root()
        .map(|root| PathBuf::from(root).join(".pie").join("agents"))
        .filter(|p| p.is_dir())
}

/// Agents from the `agents/` directories are strict about tools: without an
/// explicit `plugins:` list they get none at all.
fn load_strict_agents_from_dir(dir: &std::path::Path) -> Vec<Agent> {
    load_agents_from_dir(dir)
        .into_iter()
        .map(|mut a| {
            a.plugins.get_or_insert_with(Vec::new);
            a
        })
        .collect()
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
    fn parse_agent_readonly() -> Result<()> {
        let raw = "---\nname: explorer\ndescription: reads only\nreadonly: true\n---\ncontent";
        let agent =
            parse_agent(raw, "explorer.md").ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert!(agent.readonly);

        let raw = "---\nname: writer\ndescription: writes\n---\ncontent";
        let agent = parse_agent(raw, "writer.md").ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert!(!agent.readonly);
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

    #[test]
    fn parse_agent_tool_configuration() -> Result<()> {
        let raw = "---\nname: reviewer\nplugins: [fs-readonly, shell]\nmax_steps: 42\nskills_paths: [\"~/src/skills\"]\n---\nYou review.";
        let agent =
            parse_agent(raw, "reviewer.md").ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert_eq!(
            agent.plugins.as_deref(),
            Some(["fs-readonly".to_string(), "shell".to_string()].as_slice())
        );
        assert_eq!(agent.max_steps, Some(42));
        assert_eq!(agent.skills_paths, vec!["~/src/skills"]);

        // frontmatter without plugin keys keeps the legacy default (None)
        let raw = "---\nname: legacy\n---\nBody.";
        let agent = parse_agent(raw, "legacy.md").ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert_eq!(agent.plugins, None);
        Ok(())
    }

    #[test]
    fn strict_agents_dir_defaults_to_no_tools() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("bare.md"),
            "---\nname: bare\n---\nNo plugins listed.",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("tooled.md"),
            "---\nname: tooled\nplugins: [fs]\n---\nExplicit plugins.",
        )
        .unwrap();

        let mut agents = load_strict_agents_from_dir(tmp.path());
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "bare");
        assert_eq!(agents[0].plugins, Some(Vec::new()));
        assert_eq!(agents[1].name, "tooled");
        assert_eq!(
            agents[1].plugins,
            Some(vec!["fs".to_string()]),
            "explicit plugins list must be preserved"
        );
    }
}
