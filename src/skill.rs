use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
}

fn skills_root() -> PathBuf {
    dirs::home_dir()
        .expect("no home directory")
        .join(".pie")
        .join("skills")
}

/// List all skills from ~/.pie/skills/*/SKILL.md.
pub fn get_all_skills() -> Vec<Skill> {
    let root = skills_root();
    let Ok(entries) = fs::read_dir(&root) else {
        return vec![];
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let md_path = entry.path().join("SKILL.md");
        let Ok(raw) = fs::read_to_string(&md_path) else {
            continue;
        };
        let (meta, content) = parse_frontmatter(&raw);
        if let (Some(n), Some(d)) = (meta.get("name"), meta.get("description")) {
            skills.push(Skill {
                name: n.trim().to_string(),
                description: d.trim().to_string(),
                content,
            });
        }
    }
    skills
}

/// Split raw markdown into (frontmatter key-value map, body content).
/// Parses the `---` delimited block in a single pass.
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
