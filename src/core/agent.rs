use crate::core::output::{JsonResponse, OutputFormat};
use crate::core::prompt;
use crate::core::session::{Role, Session};
use crate::core::skill::get_all_skills;
use crate::core::skill::{parse_frontmatter, parse_list_field};
use crate::core::tools::{load_references_tool, load_skills_tool, shell_tool, subagent_tool};
use crate::providers::Model;
use crate::ui::markdown::MarkdownRenderer;
use aisdk::core::utils::step_count_is;
use aisdk::core::{AssistantMessage, LanguageModelRequest, Message, UserMessage};
use aisdk::core::{LanguageModel, ToolResultInfo};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::warn;

// ── Agent Definition ──────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub skills: Vec<String>,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub content: String,
}

fn agents_root_global() -> PathBuf {
    crate::core::config::pie_home().join("agents")
}

fn agents_root_local() -> Option<PathBuf> {
    prompt::git_repo_root()
        .map(|root| PathBuf::from(root).join(".pie").join("agents"))
        .filter(|p| p.is_dir())
}

/// Parse a raw markdown string with frontmatter into an Agent.
fn parse_agent(raw: &str) -> Option<Agent> {
    let (meta, content) = parse_frontmatter(raw);
    let name = meta.get("name")?.trim().to_string();
    let description = meta.get("description")?.trim().to_string();
    let skills = parse_list_field(meta.get("skills").map(|s| s.as_str()));
    let model = meta.get("model").map(|s| s.trim().to_string());
    let temperature = meta
        .get("temperature")
        .and_then(|s| s.trim().parse::<f32>().ok());
    Some(Agent {
        name,
        description,
        skills,
        model,
        temperature,
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
            parse_agent(&raw)
        })
        .collect()
}

/// Load embedded agents from .pie/agents/*.md compiled into the binary.
fn load_embedded_agents() -> Vec<Agent> {
    let Some(dir) = crate::core::skill::embedded_agents_dir() else {
        return Vec::new();
    };
    dir.files()
        .filter(|f| f.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|f| {
            let raw = f.contents_utf8()?;
            parse_agent(raw)
        })
        .collect()
}

/// Load all agents: embedded + global (~/.pie/agents/) + local (.pie/agents/).
/// Local overrides global, global overrides embedded.
pub fn get_all_agents() -> Vec<Agent> {
    let mut agents: Vec<Agent> = load_embedded_agents();
    let mut names: HashSet<String> = agents.iter().map(|a| a.name.clone()).collect();

    // Global filesystem agents override embedded
    for agent in load_agents_from_dir(&agents_root_global()) {
        if names.contains(&agent.name) {
            if let Some(existing) = agents.iter_mut().find(|a| a.name == agent.name) {
                *existing = agent;
            }
        } else {
            names.insert(agent.name.clone());
            agents.push(agent);
        }
    }

    if let Some(local_dir) = agents_root_local() {
        for agent in load_agents_from_dir(&local_dir) {
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

pub fn handle_list_skills() {
    let skills = get_all_skills();
    if !skills.is_empty() {
        println!("Available skills:");
        for s in &skills {
            println!(" - {}: {}", s.name, s.description);
        }
    }
    let agents = get_all_agents();
    if !agents.is_empty() {
        println!("\nAvailable agents:");
        for a in &agents {
            println!(" - {}: {}", a.name, a.description);
        }
    }
    if skills.is_empty() && agents.is_empty() {
        warn!("No skills or agents found.");
    }
}

fn build_request(
    model: &Model,
    query: &str,
    session: &Session,
    sandbox_settings: PathBuf,
) -> (LanguageModelRequest<Model>, Arc<Mutex<HashSet<String>>>) {
    let skills = get_all_skills();
    let history_entries = session.history_entries().to_vec();

    let mut scan_sources: Vec<&str> = vec![query];
    for entry in &history_entries {
        if entry.role == Role::User {
            scan_sources.push(&entry.content);
        }
    }

    let format = OutputFormat::default();
    let agents = get_all_agents();
    let system = prompt::system_prompt(&skills, &agents, format.to_instructions());

    let mut messages: Vec<Message> = Vec::new();
    if let Some(ctx_msg) = prompt::build_agent_skills_message(&skills, &agents, &scan_sources) {
        messages.push(Message::User(UserMessage::new(ctx_msg)));
    }

    for entry in &history_entries {
        match entry.role {
            Role::User => messages.push(Message::User(UserMessage::new(&entry.content))),
            Role::Assistant => messages.push(Message::Assistant(AssistantMessage::from(
                entry.content.clone(),
            ))),
            Role::System => {}
        }
    }
    messages.push(Message::User(UserMessage::new(query)));

    tracing::debug!(system = %system, query, "agent:");
    let loaded_refs = Arc::new(Mutex::new(HashSet::new()));
    let req = LanguageModelRequest::builder()
        .model(model.clone())
        .system(&system)
        .messages(messages)
        .with_tool(shell_tool(sandbox_settings.clone()))
        .with_tool(load_skills_tool(skills.clone()))
        .with_tool(load_references_tool(loaded_refs.clone()))
        .with_tool(subagent_tool(model.clone(), skills, sandbox_settings))
        .stop_when(step_count_is(25))
        .build();

    (req, loaded_refs)
}

/// Extract the output text from a response (handles both text and tool results).
fn extract_output_text(text: &str, tool_results: &Option<Vec<ToolResultInfo>>) -> String {
    if !text.is_empty() {
        return text.to_string();
    }
    if let Some(results) = tool_results {
        results
            .iter()
            .rfind(|r| r.tool.name == "shell_tool")
            .or_else(|| results.last())
            .and_then(|r| r.output.as_ref().ok())
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    }
}

pub async fn handle_query(
    model: &mut Model,
    query: &str,
    session: &mut Session,
    format: OutputFormat,
    sandbox_settings: PathBuf,
) -> Result<()> {
    let (mut req, _loaded_refs) = build_request(model, query, session, sandbox_settings);

    let response = req.generate_text().await.context("generate_text failed")?;
    let assistant_text = response.text().unwrap_or_default();
    let output = extract_output_text(&assistant_text, &response.tool_results());

    if !output.is_empty() {
        if format.is_json() {
            let json_resp = JsonResponse::new(
                output.clone(),
                Some(session.id.to_string()),
                Some(model.name()),
            );
            println!("{}", serde_json::to_string(&json_resp)?);
        } else {
            println!("{output}");
        }
    }

    session.add_user(query)?;
    if !output.is_empty() {
        session.add_assistant(&output)?;
    }

    Ok(())
}

pub async fn handle_query_streaming(
    model: &mut Model,
    query: &str,
    session: &mut Session,
    sandbox_settings: PathBuf,
) -> Result<()> {
    use aisdk::core::LanguageModelStreamChunkType;
    use futures::StreamExt;

    let (mut req, _loaded_refs) = build_request(model, query, session, sandbox_settings);

    let mut response = req.stream_text().await.context("stream_text failed")?;
    let mut renderer = MarkdownRenderer::new();

    while let Some(chunk) = response.stream.next().await {
        match chunk {
            LanguageModelStreamChunkType::TextDelta(delta) => {
                // Strip provider control tokens that leak as text
                let cleaned = if delta.contains('<') {
                    delta
                        .replace("<eos>", "")
                        .replace("<|end|>", "")
                        .replace("</think_end>", "")
                        .replace("<|end_of_turn|>", "")
                } else {
                    delta
                };
                renderer.push_delta(&cleaned);
            }
            LanguageModelStreamChunkType::Failed(err) => {
                tracing::error!("Stream failed: {err}");
                break;
            }
            _ => {}
        }
    }

    let accumulated = renderer.finish();

    let tool_results = response.tool_results().await;
    let output = extract_output_text(&accumulated, &tool_results);

    session.add_user(query)?;
    if !output.is_empty() {
        session.add_assistant(&output)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_full() {
        let raw = "---\nname: reviewer\ndescription: code reviewer\nskills: [explore, review]\nmodel: llama3\ntemperature: 0.3\n---\nBe direct and thorough.";
        let agent = parse_agent(raw).unwrap();
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description, "code reviewer");
        assert_eq!(agent.skills, vec!["explore", "review"]);
        assert_eq!(agent.model.as_deref(), Some("llama3"));
        assert!((agent.temperature.unwrap() - 0.3).abs() < f32::EPSILON);
        assert_eq!(agent.content, "Be direct and thorough.");
    }

    #[test]
    fn parse_agent_minimal() {
        let raw = "---\nname: helper\ndescription: helps\n---\nContent";
        let agent = parse_agent(raw).unwrap();
        assert_eq!(agent.name, "helper");
        assert!(agent.skills.is_empty());
        assert!(agent.model.is_none());
        assert!(agent.temperature.is_none());
    }

    #[test]
    fn resolve_mentioned_agents_from_query() {
        let agents = vec![
            Agent {
                name: "reviewer".into(),
                description: "reviews code".into(),
                skills: vec!["explore".into(), "review".into()],
                model: None,
                temperature: None,
                content: "Be thorough.".into(),
            },
            Agent {
                name: "planner".into(),
                description: "plans tasks".into(),
                skills: vec![],
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
            skills: vec![],
            model: None,
            temperature: None,
            content: String::new(),
        }];
        let mentioned = resolve_mentioned_agents(&["nothing relevant"], &agents);
        assert!(mentioned.is_empty());
    }
}
