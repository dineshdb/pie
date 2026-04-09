use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    #[serde(skip)]
    pub is_embedded: bool,
}

fn skills_root() -> PathBuf {
    crate::core::config::pie_home().join("skills")
}

const EMBEDDED_SKILLS: &[&str] = &[
    include_str!("../../.pie/skills/filesystem/SKILL.md"),
    include_str!("../../.pie/skills/developer/SKILL.md"),
];

/// Parse a raw markdown string with `---` frontmatter into a Skill.
fn parse_skill(raw: &str) -> Option<Skill> {
    let (meta, content) = parse_frontmatter(raw);
    let name = meta.get("name")?.trim().to_string();
    let description = meta.get("description")?.trim().to_string();
    Some(Skill {
        name,
        description,
        content,
        is_embedded: false,
    })
}

/// List all skills: embedded + filesystem. Filesystem skills override embedded ones with the same name.
pub fn get_all_skills() -> Vec<Skill> {
    let mut skills: Vec<Skill> = EMBEDDED_SKILLS
        .iter()
        .filter_map(|s| {
            let mut skill = parse_skill(s)?;
            skill.is_embedded = true;
            Some(skill)
        })
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
        let md_path = entry.path().join("SKILL.md");
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
