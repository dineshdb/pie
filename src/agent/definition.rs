use crate::config::EMBEDDED_PIE_DIR;
use crate::instructions::Instructions;
use crate::skill::split_frontmatter;
use include_dir::Dir;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;
use strum::{AsRefStr, Display, EnumString};

/// Embedded agents directory (from .pie/agents/ in the crate root).
pub fn embedded_agents_dir() -> Option<&'static Dir<'static>> {
    EMBEDDED_PIE_DIR.get_dir("agents")
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

impl OutputMode {
    pub fn prompt(self) -> String {
        match self {
            OutputMode::Md => {
                format!("{NON_CONVERSATIONAL_VIBE}\n{OUTPUT_MODE_SYSTEM_PROMPT}")
            }
            OutputMode::Json => {
                format!("{JSON_MODE}\n{NON_CONVERSATIONAL_VIBE}")
            }
            OutputMode::Interactive => {
                format!("{CONVERSATIONAL_VIBE}\n{OUTPUT_MODE_SYSTEM_PROMPT}")
            }
        }
    }
}

const JSON_MODE: &str = r"
## JSON Output Mode
Respond with ONLY valid JSON in the format and fields requested by the user.
Do not include any text outside the JSON block.
";

const NON_CONVERSATIONAL_VIBE: &str = r"
## Non-Conversational Mode (CLI)

### Core Mandates
- **Direct Action**: Provide solutions immediately without preamble or chatter.
- **Zero Follow-ups**: Do not ask the user for clarification or permission. Use your tools to resolve all unknowns autonomously.
- **Information Density**: Deliver technical details concisely.
- **Finality**: Assume each interaction is your last. Complete the task entirely in one go.
";

const CONVERSATIONAL_VIBE: &str = r#"
## Interactivity & Collaboration (Conversational)

### Proactive Clarification
- If a task is underspecified, do not guess. Ask for clarification with a few options.
- Before starting a multi-step execution phase, summarize your plan and ask if it aligns with the user's expectations.

### Collaborative Check-ins
- After completing a significant sub-task, briefly state what was done and ask if the user wants to proceed or adjust the next steps.
- Use phrases like "Does this approach look right to you?" or "Should I continue with [Next Step]?"

### Feedback Integration
- Actively seek feedback on the quality of the solutions provided.
- If the user provides a hint or correction, acknowledge it and explain how it changes your approach.
"#;

const OUTPUT_MODE_SYSTEM_PROMPT: &str = r#"
## Output Aesthetic (Senior Peer Vibe)

### Visual Hierarchy
- Use bold headers and bullet points to create a clear structure.
- Use code blocks for all technical snippets, including single-line commands.

### Tone & Style
- Maintain a professional yet helpful "senior peer" vibe.
- Use subtle emojis to categorize information (e.g., 🛠️ for implementation, ✅ for verification, 🔍 for research, 💡 for suggestions).

### Information Density
- Keep technical explanations concise but thorough.
- Avoid large walls of text; use spacing and lists to make information digestible.
"#;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub output_mode: OutputMode,
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
    #[serde(alias = "interactivity")]
    output_mode: OutputMode,
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
    let mut agents: Vec<Agent> = load_embedded_agents();
    let mut names: HashSet<String> = agents.iter().map(|a| a.name.clone()).collect();

    crate::utils::merge_by_name(
        &mut agents,
        &mut names,
        load_agents_from_dir(&agents_root_global()),
        |a| &a.name,
    );

    if let Some(local_dir) = agents_root_local() {
        crate::utils::merge_by_name(
            &mut agents,
            &mut names,
            load_agents_from_dir(&local_dir),
            |a| &a.name,
        );
    }

    agents
}

/// Resolve agents whose names appear in the given instructions.
pub fn resolve_mentioned_agents<'a>(
    instructions: &Instructions,
    agents: &'a [Agent],
) -> Vec<&'a Agent> {
    agents
        .iter()
        .filter(|a| instructions.mentions_name(&a.name))
        .collect()
}

/// Check if we should subsume the role of a single mentioned agent.
/// Returns the name of the agent if exactly one is mentioned.
/// Also handles direct invocation where the first word is the agent name (with or without /).
pub fn find_subsume_candidate(instructions: &Instructions, agents: &[Agent]) -> Option<String> {
    // 1. Check for direct invocation as the first word
    let first_word = instructions
        .raw
        .split_whitespace()
        .next()
        .map(|w| w.trim_start_matches('/'));

    if let Some(first) = first_word
        && let Some(agent) = agents.iter().find(|a| a.name == first)
    {
        return Some(agent.name.clone());
    }

    // 2. Fallback to existing mention logic
    let mentioned = resolve_mentioned_agents(instructions, agents);
    if mentioned.len() == 1 {
        mentioned.first().map(|a| a.name.clone())
    } else {
        None
    }
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

    #[test]
    fn resolve_mentioned_agents_from_query() -> Result<()> {
        let agents = vec![
            Agent {
                name: "reviewer".into(),
                description: "reviews code".into(),
                output_mode: OutputMode::Interactive,
                model: None,
                temperature: None,
                content: "Be thorough.".into(),
            },
            Agent {
                name: "planner".into(),
                description: "manages plans".into(),
                output_mode: OutputMode::Interactive,
                model: None,
                temperature: None,
                content: "Think step by step.".into(),
            },
        ];
        let instr = Instructions::new("/reviewer check this");
        let mentioned = resolve_mentioned_agents(&instr, &agents);
        assert_eq!(mentioned.len(), 1);
        assert_eq!(
            mentioned
                .first()
                .ok_or_else(|| anyhow::anyhow!("expected match"))?
                .name,
            "reviewer"
        );
        Ok(())
    }

    #[test]
    fn resolve_mentioned_agents_no_match() {
        let agents = vec![Agent {
            name: "reviewer".into(),
            description: "reviews".into(),
            output_mode: OutputMode::Md,
            model: None,
            temperature: None,
            content: String::new(),
        }];
        let instr = Instructions::new("nothing relevant");
        let mentioned = resolve_mentioned_agents(&instr, &agents);
        assert!(mentioned.is_empty());
    }

    #[test]
    fn find_subsume_candidate_direct_invocation() {
        let agents = vec![Agent {
            name: "howto".into(),
            description: "howto guide".into(),
            output_mode: OutputMode::Md,
            model: None,
            temperature: None,
            content: String::new(),
        }];

        // Test with name
        let instr = Instructions::new("howto implement x");
        assert_eq!(
            find_subsume_candidate(&instr, &agents),
            Some("howto".into())
        );

        // Test with /name
        let instr = Instructions::new("/howto implement x");
        assert_eq!(
            find_subsume_candidate(&instr, &agents),
            Some("howto".into())
        );

        // Test with name but not first word
        let instr = Instructions::new("tell me howto implement x");
        assert_eq!(find_subsume_candidate(&instr, &agents), None);
    }
}
