use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
}

fn skills_root() -> PathBuf {
    dirs::home_dir()
        .expect("no home directory")
        .join(".pie")
        .join("skills")
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

/// Load a skill's content (everything after YAML frontmatter).
pub fn load_skill(name: &str) -> Result<Option<String>> {
    let path = skills_root().join(name).join("SKILL.md");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let content = strip_frontmatter(&raw);
    Ok(Some(content))
}

/// List all skills from ~/.pie/skills/*/SKILL.md.
pub fn get_all_skills() -> Vec<Skill> {
    let root = skills_root();
    let Ok(entries) = fs::read_dir(&root) else {
        return vec![];
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map_or(false, |t| t.is_dir()) {
            continue;
        }
        let md_path = entry.path().join("SKILL.md");
        let Ok(raw) = fs::read_to_string(&md_path) else {
            continue;
        };
        let name = parse_yaml_field(&raw, "name");
        let desc = parse_yaml_field(&raw, "description");
        if let (Some(n), Some(d)) = (name, desc) {
            skills.push(Skill {
                name: n.trim().to_string(),
                description: d.trim().to_string(),
            });
        }
    }
    skills
}

/// Strip YAML frontmatter (between --- delimiters) from markdown.
fn strip_frontmatter(content: &str) -> String {
    let mut lines = content.lines().peekable();
    // Skip opening ---
    if lines.peek().map_or(false, |l| l.trim() == "---") {
        lines.next();
        // Skip until closing ---
        while lines.peek().map_or(false, |l| l.trim() != "---") {
            lines.next();
        }
        lines.next(); // skip closing ---
    }
    lines.collect::<Vec<_>>().join("\n").trim().to_string()
}

/// Extract a single field from YAML frontmatter.
fn parse_yaml_field(content: &str, field: &str) -> Option<String> {
    let pattern = format!("{}:", field);
    let mut inside_frontmatter = false;
    for line in content.lines() {
        if line.trim() == "---" {
            if inside_frontmatter {
                break; // closing ---
            }
            inside_frontmatter = true; // opening ---
            continue;
        }
        if inside_frontmatter && line.starts_with(&pattern) {
            return Some(line[pattern.len()..].trim().to_string());
        }
    }
    None
}

/// Find all skills mentioned in a query that actually exist.
pub fn find_mentioned_skills(query: &str, skills: &[Skill]) -> Vec<String> {
    let available: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    crate::skill::extract_skill_mentions(query)
        .into_iter()
        .filter(|m| available.contains(&m.as_str()))
        .collect()
}
