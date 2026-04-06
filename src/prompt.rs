use std::collections::HashSet;

use crate::skill::Skill;
use crate::utils::{find_upward_in_repo, load_file, pie_home};
use minijinja::Environment;

const DEFAULT_SYSTEM_PROMPT: &str = include_str!("SYSTEM_PROMPT.md");

/// Recursively resolve skills mentioned in `text` (and in those skills' contents).
fn resolve_mentioned<'a>(text: &str, skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: Vec<&str> = vec![text];

    while let Some(source) = queue.pop() {
        for skill in skills {
            if seen.contains(&skill.name) {
                continue;
            }
            if source.contains(&format!("/{}", skill.name)) {
                seen.insert(skill.name.clone());
                queue.push(&skill.content);
                resolved.push(skill);
            }
        }
    }

    resolved
}

pub fn system(query: &str, skills: &[Skill]) -> String {
    let system_prompt = load_file(pie_home().join("SYSTEM_PROMPT.md"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());

    let mentioned_skills = resolve_mentioned(query, skills);

    let mut env = Environment::new();
    env.add_template("prompt", &system_prompt)
        .expect("invalid system prompt");

    let global_agents_md = load_file(pie_home().join("AGENTS.md"));
    let local_agents_md = find_upward_in_repo("AGENTS.md");

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let pwd = std::env::var("PWD").unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string()
    });
    let tmpl = env.get_template("prompt").unwrap();
    let rendered = tmpl
        .render(minijinja::context! { skills, mentioned_skills, date, pwd, global_agents_md, local_agents_md, query})
        .unwrap_or_else(|e| {
            tracing::warn!("template render error: {e}, using raw template");
            system_prompt.to_string()
        });

    rendered
}

const DEFAULT_SUBAGENT_PROMPT: &str = include_str!("SUBAGENT_PROMPT.md");

pub fn subagent(query: &str, skills: &[Skill]) -> String {
    let template = load_file(pie_home().join("SUBAGENT_PROMPT.md"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SUBAGENT_PROMPT.to_string());

    let mentioned_skills = resolve_mentioned(query, skills);

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let pwd = std::env::var("PWD").unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string()
    });

    let mut env = Environment::new();
    env.add_template("subagent", &template)
        .expect("invalid subagent prompt");
    let tmpl = env.get_template("subagent").unwrap();
    let rendered = tmpl
        .render(minijinja::context! { mentioned_skills, date, pwd, query })
        .unwrap_or_else(|e| {
            tracing::warn!("subagent template render error: {e}, using raw template");
            template.to_string()
        });
    rendered
}
