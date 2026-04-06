use crate::core::skill::Skill;
use crate::core::utils::{find_upward_in_repo, load_file, pie_home};
use minijinja::Environment;
use std::collections::HashSet;

const DEFAULT_SYSTEM_PROMPT: &str = include_str!("SYSTEM_PROMPT.md");
const DEFAULT_SUBAGENT_PROMPT: &str = include_str!("SUBAGENT_PROMPT.md");

/// Recursively resolve skills mentioned in any of `sources` (and in those skills' contents).
fn resolve_mentioned<'a>(sources: &[&str], skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: Vec<&str> = sources.to_vec();

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

/// Load a custom template from `~/.pie/<name>`, falling back to the built-in default.
fn load_template(name: &str, default: &str) -> String {
    load_file(pie_home().join(name))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Render a MiniJinja template with context, falling back to raw template on error.
fn render_template(template_name: &str, template: &str, ctx: minijinja::Value) -> String {
    let mut env = Environment::new();
    env.add_template(template_name, template)
        .unwrap_or_else(|e| panic!("invalid {template_name} template: {e}"));
    env.get_template(template_name)
        .unwrap()
        .render(ctx)
        .unwrap_or_else(|e| {
            tracing::warn!("{template_name} template render error: {e}, using raw template");
            template.to_string()
        })
}

fn context_vars() -> (String, String) {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let pwd = std::env::var("PWD").unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string()
    });
    (date, pwd)
}

pub fn system(query: &str, skills: &[Skill], scan_sources: &[&str]) -> String {
    let template = load_template("SYSTEM_PROMPT.md", DEFAULT_SYSTEM_PROMPT);
    let mentioned_skills = resolve_mentioned(scan_sources, skills);
    let global_agents_md = load_file(pie_home().join("AGENTS.md"));
    let local_agents_md = find_upward_in_repo("AGENTS.md");
    let (date, pwd) = context_vars();
    render_template(
        "system",
        &template,
        minijinja::context! { skills, mentioned_skills, date, pwd, global_agents_md, local_agents_md, query },
    )
}

pub fn subagent(query: &str, skills: &[Skill]) -> String {
    let template = load_template("SUBAGENT_PROMPT.md", DEFAULT_SUBAGENT_PROMPT);
    let mentioned_skills = resolve_mentioned(&[query], skills);
    let (date, pwd) = context_vars();
    render_template(
        "subagent",
        &template,
        minijinja::context! { mentioned_skills, date, pwd, query },
    )
}
