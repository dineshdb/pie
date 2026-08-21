//! YAML agent definitions.
//!
//! Drop a `*.yaml` (or `*.yml`) file into `~/.pie/agents/` or `.pie/agents/`
//! and run it with `pie <name> [query...]`. YAML agents are strictly
//! explicit about tools: without a `plugins:` list the agent has no tools
//! at all — capabilities are opt-in via plugin names.

use super::definition::{Agent, OutputMode};
use p1e_sandbox::{Permission, SandboxConfig};
use serde::Deserialize;
use std::path::Path;

/// Serde-deserializable YAML agent file.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AgentYaml {
    name: Option<String>,
    description: Option<String>,
    output_mode: OutputMode,
    model: Option<String>,
    system_prompt: Option<String>,
    /// Path to a file containing the system prompt (relative to this YAML
    /// file). Ignored when `system_prompt` is set inline.
    system_prompt_file: Option<String>,
    needs: Vec<String>,
    tools: Vec<String>,
    sandbox: Option<SandboxConfig>,
    grants: Vec<Permission>,
    readonly: bool,
    plugins: Option<Vec<String>>,
    skills_paths: Vec<String>,
    max_steps: Option<u32>,
}

/// Parse a raw YAML string into an [`Agent`]. `dir` is the directory
/// containing the file, used to resolve `system_prompt_file`.
///
/// Returns `None` when the file has no system prompt at all (neither
/// `system_prompt` nor `system_prompt_file`) — an agent with nothing to
/// run as is a mistake, not a default.
pub fn parse_agent_yaml(raw: &str, dir: &Path, filename: &str) -> Option<Agent> {
    let meta: AgentYaml = serde_yaml::from_str(raw).ok()?;

    let content = match (&meta.system_prompt, &meta.system_prompt_file) {
        (Some(inline), _) => inline.clone(),
        (None, Some(file)) => {
            let path = dir.join(expand_home(file));
            std::fs::read_to_string(&path).ok()?
        }
        (None, None) => return None,
    };

    let name = meta.name.map_or_else(
        || {
            Path::new(filename)
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
        temperature: None,
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

fn expand_home(path: &str) -> String {
    path.strip_prefix("~/")
        .and_then(|rest| dirs::home_dir().map(|h| h.join(rest).to_string_lossy().to_string()))
        .unwrap_or_else(|| path.to_string())
}

fn load_yaml_agents_from_dir(dir: &Path) -> Vec<Agent> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.path()
                    .extension()
                    .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            let filename = e.file_name().to_string_lossy().to_string();
            let parent = e.path().parent().unwrap_or(Path::new(".")).to_path_buf();
            parse_agent_yaml(&raw, &parent, &filename)
        })
        .collect()
}

fn agents_yaml_root_global() -> std::path::PathBuf {
    crate::config::pie_home().join("agents")
}

fn agents_yaml_root_local() -> Option<std::path::PathBuf> {
    crate::utils::git_repo_root()
        .map(|root| std::path::PathBuf::from(root).join(".pie").join("agents"))
        .filter(|p| p.is_dir())
}

/// Load YAML agents: `~/.pie/agents/` then `.pie/agents/` (local overrides
/// global by name). `base` is the markdown agent set — a YAML file with the
/// same name replaces its markdown twin.
pub fn load_yaml_agents(base: Vec<Agent>) -> Vec<Agent> {
    crate::utils::load_resources(
        base,
        &agents_yaml_root_global(),
        agents_yaml_root_local(),
        load_yaml_agents_from_dir,
        |a| &a.name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_yaml() {
        let raw = r#"
name: reviewer
description: Reviews code
model: deep
max_steps: 42
output_mode: json
readonly: true
plugins: [fs-readonly, shell]
skills_paths: ["~/src/skills"]
system_prompt: |
  You are a reviewer.
  Be blunt.
grants: ["fs-read:/tmp"]
sandbox:
  allow_write: ["."]
needs: ["repo"]
"#;
        let agent = parse_agent_yaml(raw, Path::new("/x"), "reviewer.yaml")
            .unwrap_or_else(|| panic!("parse failed"));
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description, "Reviews code");
        assert_eq!(agent.model.as_deref(), Some("deep"));
        assert_eq!(agent.max_steps, Some(42));
        assert_eq!(agent.output_mode, OutputMode::Json);
        assert!(agent.readonly);
        assert_eq!(
            agent.plugins.as_deref(),
            Some(["fs-readonly".to_string(), "shell".to_string()].as_slice())
        );
        assert_eq!(agent.skills_paths, vec!["~/src/skills"]);
        assert_eq!(agent.content, "You are a reviewer.\nBe blunt.\n");
        assert_eq!(agent.grants.len(), 1);
        assert_eq!(agent.sandbox.as_ref().unwrap().allow_write, vec!["."]);
        assert_eq!(agent.needs, vec!["repo"]);
    }

    #[test]
    fn defaults_name_from_file_and_description_from_prompt() {
        let raw = "system_prompt: |\n  First line is the description.\n  More.";
        let agent = parse_agent_yaml(raw, Path::new("/x"), "explorer.yaml").expect("parse failed");
        assert_eq!(agent.name, "explorer");
        assert_eq!(agent.description, "First line is the description.");
        assert_eq!(agent.plugins, None);
        assert_eq!(agent.max_steps, None);
    }

    #[test]
    fn system_prompt_file_resolved_relative_to_yaml_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("prompt.md"), "From file.").unwrap();
        let raw = format!("system_prompt_file: prompt.md\nname: fromfile\n");
        let agent = parse_agent_yaml(&raw, tmp.path(), "fromfile.yaml").expect("parse failed");
        assert_eq!(agent.content, "From file.");
    }

    #[test]
    fn no_system_prompt_is_rejected() {
        assert!(parse_agent_yaml("name: empty\n", Path::new("/x"), "empty.yaml").is_none());
    }
}
