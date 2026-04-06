use crate::agent::ParsedInput;
use crate::skill::Skill;
use std::collections::HashSet;

pub struct Prompt {
    pub system: String,
    pub user: String,
}

const SYSTEM_PROMPT: &str = r#"
You are a helpful assistant. Follow the instructions carefully. Be concise and accurate on your output.
Don't trust your internal knowledge base but only the instructions provided, or the results
from web search.

If no skill is mentioned in the user query, select the best skill for the task and call the
subagent tool with /<skill-name> and a rephrased query that includes all necessary context.
"#;

impl Prompt {
    pub fn router(args: &ParsedInput, skills: &[Skill]) -> Self {
        let instructions = skills_instructions(args, skills);
        let query = resolve_query(args, skills);
        let system = Self::system_prompt(skills);
        let user = if instructions.is_empty() {
            query
        } else {
            format!("Mentioned Skills Contents:\n{instructions}\n\nQuestion: {query}")
        };
        Self { system, user }
    }

    fn system_prompt(skills: &[Skill]) -> String {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let catalog = skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");

        format!("{SYSTEM_PROMPT}\n\nAvailable Skills:\n{catalog}\nDate: {today}")
    }
}

/// Resolve instructions: include skill content if specific skills are mentioned.
/// When no skills are mentioned, return empty — the router agent will use the subagent tool.
fn skills_instructions(args: &ParsedInput, skills: &[Skill]) -> String {
    let mut mentioned_skills = HashSet::new();
    if let Some(name) = &args.skill {
        mentioned_skills.insert(name.clone());
    }

    for mentioned in find_mentioned_skills(&args.query, skills) {
        mentioned_skills.insert(mentioned);
    }

    if mentioned_skills.is_empty() {
        return String::new();
    }

    mentioned_skills
        .iter()
        .filter_map(|s| skills.iter().find(|skill| &skill.name == s))
        .map(|s| format!("---\nSkill: {}\n{}\n---", s.name, s.content))
        .collect()
}

/// Resolve query: strip /skill-name prefix if present.
fn resolve_query(args: &ParsedInput, skills: &[Skill]) -> String {
    let mentioned = find_mentioned_skills(&args.query, skills);
    if mentioned.len() == 1 {
        let name = &mentioned[0];
        let stripped = args
            .query
            .replace(&format!("/{name}"), "")
            .trim()
            .to_string();
        if !stripped.is_empty() {
            return stripped;
        }
    }
    if args.query.is_empty() {
        args.skill.clone().unwrap_or_default()
    } else {
        args.query.clone()
    }
}

/// Find all skills mentioned in a query that actually exist.
fn find_mentioned_skills(query: &str, skills: &[Skill]) -> Vec<String> {
    let available: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    extract_skill_mentions(query)
        .into_iter()
        .filter(|m| available.contains(&m.as_str()))
        .collect()
}

/// Extract /skillname mentions from text.
pub fn extract_skill_mentions(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let valid = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';

    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && i + 1 < chars.len() && valid(chars[i + 1]) {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && valid(chars[end]) {
                end += 1;
            }
            mentions.push(chars[start..end].iter().collect());
            i = end;
        } else {
            i += 1;
        }
    }
    mentions
}
