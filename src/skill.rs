use crate::config::EMBEDDED_PIE_DIR;
use include_dir::Dir;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub needs: Vec<String>,
}

/// Serde-deserializable frontmatter for skill files.
#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    needs: Vec<String>,
}

fn skills_root() -> PathBuf {
    crate::config::pie_home().join("skills")
}

/// Embedded skills directory (from .pie/skills/ in the crate root).
fn embedded_skills_dir() -> Option<&'static Dir<'static>> {
    EMBEDDED_PIE_DIR.get_dir("skills")
}

/// Parse a raw markdown string with `---` frontmatter into a Skill.
fn parse_skill(raw: &str) -> Option<Skill> {
    let (yaml, content) = split_frontmatter(raw);
    let meta: SkillFrontmatter = serde_yml::from_str(&yaml).ok()?;
    Some(Skill {
        name: meta.name.trim().to_string(),
        description: meta.description.trim().to_string(),
        content,
        needs: meta.needs,
    })
}

/// List all skills: embedded + filesystem. Filesystem skills override embedded ones with the same name.
pub fn get_all_skills() -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();

    // Load embedded skills: iterate subdirectories and find SKILL.md in each
    if let Some(skills_dir) = embedded_skills_dir() {
        for dir in skills_dir.dirs() {
            for file in dir.files() {
                if file.path().ends_with("SKILL.md")
                    && let Some(content) = file.contents_utf8()
                    && let Some(skill) = parse_skill(content)
                {
                    skills.push(skill);
                }
            }
        }
    }

    let mut names: HashSet<String> = skills.iter().map(|s| s.name.clone()).collect();
    let root = skills_root();
    let filesystem_skills = load_skills_from_dir(&root);

    crate::utils::merge_by_name(&mut skills, &mut names, filesystem_skills, |s| &s.name);
    skills
}

/// Load skills from a filesystem directory of skill subdirectories.
fn load_skills_from_dir(dir: &std::path::Path) -> Vec<Skill> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let md_path = e.path().join("SKILL.md");
            let raw = fs::read_to_string(&md_path).ok()?;
            parse_skill(&raw)
        })
        .collect()
}

/// Resolve the directory path for a filesystem skill by name.
/// Returns None for embedded-only skills with no filesystem override.
pub fn skill_dir(name: &str) -> Option<PathBuf> {
    let dir = skills_root().join(name);
    dir.join("SKILL.md").exists().then_some(dir)
}

/// Load a reference file for a skill. Checks filesystem skills first (user overrides),
/// then falls back to embedded skills. Returns None if not found in either.
pub fn load_reference(skill_name: &str, ref_name: &str) -> Option<String> {
    // Filesystem override first
    if let Some(dir) = skill_dir(skill_name)
        && let Ok(content) = fs::read_to_string(dir.join(ref_name))
    {
        return Some(content);
    }
    // Fall back to embedded: find the child dir and iterate its files
    let full_path = format!("{skill_name}/{ref_name}");
    let path = std::path::Path::new(&full_path);
    embedded_skills_dir().and_then(|dir| {
        dir.dirs()
            .find(|d| d.path() == std::path::Path::new(skill_name))
            .and_then(|skill_dir| skill_dir.files().find(|f| f.path() == path))
            .and_then(|file| file.contents_utf8())
            .map(ToString::to_string)
    })
}

/// Check whether a skill exists (embedded or filesystem).
pub fn skill_exists(name: &str) -> bool {
    skill_dir(name).is_some()
        || embedded_skills_dir().is_some_and(|dir| dir.get_dir(name).is_some())
}

/// Split raw markdown into (frontmatter YAML string, body content).
/// Returns ("", `trimmed_body`) when no frontmatter delimiters are found.
pub fn split_frontmatter(raw: &str) -> (String, String) {
    let lines: Vec<&str> = raw.lines().collect();
    let mut i = 0;
    if lines.get(i).is_some_and(|l| l.trim() == "---") {
        i += 1;
        let start = i;
        while lines.get(i).is_some_and(|l| l.trim() != "---") {
            i += 1;
        }
        let yaml = lines
            .get(start..i)
            .map(|s| s.join("\n"))
            .unwrap_or_default();
        if lines.get(i).is_some() {
            i += 1;
        }
        let body = lines.get(i..).map(|s| s.join("\n")).unwrap_or_default();
        return (yaml, body.trim().to_string());
    }
    (String::new(), raw.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_dir_returns_none_for_unknown() {
        assert!(skill_dir("nonexistent-skill-xyz").is_none());
    }

    #[test]
    fn parse_skill_with_needs() -> anyhow::Result<()> {
        let raw = "---\nname: review\ndescription: code review\nneeds: [filesystem, developer]\n---\nContent here";
        let skill = parse_skill(raw).ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert_eq!(skill.name, "review");
        assert_eq!(skill.needs, vec!["filesystem", "developer"]);
        assert_eq!(skill.content, "Content here");
        Ok(())
    }
}
