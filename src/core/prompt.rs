use crate::core::config::pie_home;
use crate::core::skill::Skill;
use crate::core::utils::{find_upward_in_repo, load_file};
use minijinja::Environment;
use std::collections::HashSet;

const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("./prompt.md");
const SKILL_RULES_TEMPLATE: &str = include_str!("./skill_rules.md");

/// Recursively resolve skills mentioned in any of `sources` (and in those skills' contents).
/// Scans for `/<skill-name>` patterns and follows transitive mentions.
pub fn resolve_mentioned<'a>(sources: &[&str], skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut resolved = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
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

/// Resolve skills by name, then recursively follow `/<skill-name>` mentions in their content.
pub fn resolve_by_name<'a>(names: &[String], skills: &'a [Skill]) -> Vec<&'a Skill> {
    let name_sources: Vec<String> = names.iter().map(|n| format!("/{n}")).collect();
    let sources: Vec<&str> = name_sources.iter().map(|s| s.as_str()).collect();
    resolve_mentioned(&sources, skills)
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

pub fn context_vars() -> (String, String) {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let pwd = std::env::var("PWD").unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string()
    });
    (date, pwd)
}

/// Find the git repo root by walking up from cwd looking for `.git`.
/// Stops at the user's home directory to avoid scanning system paths.
pub fn git_repo_root() -> Option<String> {
    let home = dirs::home_dir();
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..32 {
        if dir.join(".git").exists() {
            return Some(dir.display().to_string());
        }
        if home.as_ref().is_some_and(|h| dir == *h) {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}

/// Render the full system prompt as a single message.
pub fn system_prompt(
    system_skills: &[Skill],
    user_skills: &[Skill],
    format_instructions: &str,
) -> String {
    let global_agents_md = load_file(pie_home().join("AGENTS.md"));
    let local_agents_md = find_upward_in_repo("AGENTS.md");
    let (date, pwd) = context_vars();
    let repo_root = git_repo_root();

    tracing::debug!(repo_root = ?repo_root, "system prompt context");

    render_template(
        "system_prompt",
        SYSTEM_PROMPT_TEMPLATE,
        minijinja::context! {
            is_subagent => false,
            system_skills,
            user_skills,
            global_agents_md,
            local_agents_md,
            date,
            pwd,
            repo_root,
            format_instructions,
        },
    )
}

pub fn subagent_prompt(repo_root: Option<String>) -> String {
    let (date, pwd) = context_vars();
    render_template(
        "system_prompt",
        SYSTEM_PROMPT_TEMPLATE,
        minijinja::context! {
            is_subagent => true,
            system_skills => &[] as &[Skill],
            user_skills => &[] as &[Skill],
            global_agents_md => String::new(),
            local_agents_md => String::new(),
            date,
            pwd,
            repo_root,
            format_instructions => String::new(),
        },
    )
}

pub fn mentioned_skills_message(skills: &[Skill], scan_sources: &[&str]) -> Option<String> {
    let mentioned = resolve_mentioned(scan_sources, skills);
    if mentioned.is_empty() {
        return None;
    }
    Some(render_template(
        "skill_rules",
        SKILL_RULES_TEMPLATE,
        minijinja::context! { mentioned },
    ))
}
