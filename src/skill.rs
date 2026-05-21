use crate::config::{EMBEDDED_PIE_DIR, pie_home};
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

impl Skill {
    /// Recursively resolve a list of skill names and their dependencies.
    pub fn resolve<'a>(all_skills: &'a [Skill], mentions: &[String]) -> Vec<&'a Skill> {
        let mut resolved = Vec::new();
        let mut visited = HashSet::new();
        let mut stack: Vec<&str> = mentions.iter().map(String::as_str).collect();

        while let Some(name) = stack.pop() {
            if !visited.insert(name) {
                continue;
            }
            if let Some(skill) = all_skills.iter().find(|s| s.name == name) {
                resolved.push(skill);
                for need in &skill.needs {
                    stack.push(need);
                }
            }
        }
        // Reverse to maintain some semblance of requested order (though it's a stack)
        resolved.reverse();
        resolved
    }

    pub fn format_markdown(&self) -> String {
        format!("## Skill: {}\n{}\n---\n", self.name, self.content)
    }
}

/// Format a list of skills as a single markdown string.
pub fn format_skills_markdown(skills: &[&Skill]) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    for skill in skills {
        write!(output, "{}", skill.format_markdown()).ok();
    }
    output
}

/// Serde-deserializable frontmatter for skill files.
#[derive(Debug, Deserialize, serde::Serialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    needs: Vec<String>,
}

fn skills_root_local() -> Option<PathBuf> {
    crate::utils::git_repo_root()
        .map(|root| PathBuf::from(root).join(".pie").join("skills"))
        .filter(|p| p.is_dir())
}

/// Parse a raw markdown string with `---` frontmatter into a Skill.
fn parse_skill(raw: &str) -> Option<Skill> {
    let (yaml, content) = split_frontmatter(raw);
    let meta: SkillFrontmatter = serde_yaml::from_str(&yaml).ok()?;
    Some(Skill {
        name: meta.name.trim().to_string(),
        description: meta.description.trim().to_string(),
        content,
        needs: meta.needs,
    })
}

/// Load embedded skills: iterate subdirectories and find SKILL.md in each.
fn load_embedded_skills() -> Vec<Skill> {
    let Some(skills_dir) = EMBEDDED_PIE_DIR.get_dir("skills") else {
        return Vec::new();
    };
    let mut skills = Vec::new();
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
    skills
}

/// List all skills: embedded + global + local. Overrides by name.
pub fn get_all_skills() -> Vec<Skill> {
    crate::utils::load_resources(
        load_embedded_skills(),
        &pie_home().join("skills"),
        skills_root_local(),
        load_skills_from_dir,
        |s| &s.name,
    )
}

/// Load skills from a filesystem directory of skill subdirectories.
fn load_skills_from_dir(dir: &std::path::Path) -> Vec<Skill> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let md_path = e.path().join("SKILL.md");
            let raw = fs::read_to_string(&md_path).ok()?;
            parse_skill(&raw)
        })
        .collect()
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
    fn parse_skill_with_needs() -> anyhow::Result<()> {
        let raw = "---\nname: review\ndescription: code review\nneeds: [filesystem, developer]\n---\nContent here";
        let skill = parse_skill(raw).ok_or_else(|| anyhow::anyhow!("parse failed"))?;
        assert_eq!(skill.name, "review");
        assert_eq!(skill.needs, vec!["filesystem", "developer"]);
        assert_eq!(skill.content, "Content here");
        Ok(())
    }
}
