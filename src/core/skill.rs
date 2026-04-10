use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    /// Explicit dependencies declared via `needs` in frontmatter.
    /// These are auto-loaded alongside this skill.
    pub needs: Vec<String>,
}

fn skills_root() -> PathBuf {
    crate::core::config::pie_home().join("skills")
}

const EMBEDDED_SKILLS: &[&str] = &[
    include_str!("../../.pie/skills/filesystem/SKILL.md"),
    include_str!("../../.pie/skills/developer/SKILL.md"),
    include_str!("../../.pie/skills/explore/SKILL.md"),
    include_str!("../../.pie/skills/review/SKILL.md"),
];

/// Parse a raw markdown string with `---` frontmatter into a Skill.
fn parse_skill(raw: &str) -> Option<Skill> {
    let (meta, content) = parse_frontmatter(raw);
    let name = meta.get("name")?.trim().to_string();
    let description = meta.get("description")?.trim().to_string();
    let needs = parse_list_field(meta.get("needs").map(|s| s.as_str()));
    Some(Skill {
        name,
        description,
        content,
        needs,
    })
}

/// Parse a frontmatter list field like `[a, b, c]` into a Vec of strings.
fn parse_list_field(value: Option<&str>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let trimmed = value.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// List all skills: embedded + filesystem. Filesystem skills override embedded ones with the same name.
pub fn get_all_skills() -> Vec<Skill> {
    let mut skills: Vec<Skill> = EMBEDDED_SKILLS
        .iter()
        .filter_map(|s| parse_skill(s))
        .collect();
    let mut names: HashSet<String> = skills.iter().map(|s| s.name.clone()).collect();

    let root = skills_root();
    let Ok(entries) = fs::read_dir(&root) else {
        return skills;
    };

    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir_path = entry.path();
        let md_path = dir_path.join("SKILL.md");
        let Ok(raw) = fs::read_to_string(&md_path) else {
            continue;
        };
        let Some(skill) = parse_skill(&raw) else {
            continue;
        };
        let name = skill.name.clone();
        if names.contains(&name) {
            if let Some(existing) = skills.iter_mut().find(|s| s.name == name) {
                *existing = skill;
            }
        } else {
            names.insert(name);
            skills.push(skill);
        }
    }
    skills
}

/// Resolve the directory path for a filesystem skill by name.
/// Returns None for embedded-only skills with no filesystem override.
pub fn skill_dir(name: &str) -> Option<PathBuf> {
    let dir = skills_root().join(name);
    if dir.join("SKILL.md").exists() {
        Some(dir)
    } else {
        None
    }
}

/// Split raw markdown into (frontmatter key-value map, body content).
fn parse_frontmatter(raw: &str) -> (std::collections::HashMap<&str, String>, String) {
    let mut meta = std::collections::HashMap::new();
    let lines: Vec<&str> = raw.lines().collect();

    let mut i = 0;
    if i < lines.len() && lines[i].trim() == "---" {
        i += 1;
        while i < lines.len() && lines[i].trim() != "---" {
            if let Some((key, value)) = lines[i].split_once(':') {
                meta.insert(key.trim(), value.trim().to_string());
            }
            i += 1;
        }
        if i < lines.len() {
            i += 1; // skip closing ---
        }
    }

    let body = lines[i..].join("\n").trim().to_string();
    (meta, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_field_handles_bracketed_list() {
        let result = parse_list_field(Some("[a, b, c]"));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_list_field_handles_single_item() {
        let result = parse_list_field(Some("[filesystem]"));
        assert_eq!(result, vec!["filesystem"]);
    }

    #[test]
    fn parse_list_field_returns_empty_for_none() {
        let result = parse_list_field(None);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_list_field_returns_empty_for_empty_brackets() {
        let result = parse_list_field(Some("[]"));
        assert!(result.is_empty());
    }

    #[test]
    fn parse_list_field_trims_whitespace() {
        let result = parse_list_field(Some("[ a , b , c ]"));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn skill_dir_returns_none_for_unknown() {
        assert!(skill_dir("nonexistent-skill-xyz").is_none());
    }

    #[test]
    fn parse_skill_with_needs() {
        let raw = "---\nname: review\ndescription: code review\nneeds: [filesystem, developer]\n---\nContent here";
        let skill = parse_skill(raw).unwrap();
        assert_eq!(skill.name, "review");
        assert_eq!(skill.needs, vec!["filesystem", "developer"]);
        assert_eq!(skill.content, "Content here");
    }

    #[test]
    fn parse_skill_without_needs() {
        let raw = "---\nname: bash\ndescription: run commands\n---\nContent";
        let skill = parse_skill(raw).unwrap();
        assert!(skill.needs.is_empty());
    }
}
