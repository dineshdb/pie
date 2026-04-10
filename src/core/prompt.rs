use crate::core::config::pie_home;
use crate::core::skill::Skill;
use crate::core::utils::{find_upward_in_repo, load_file};
use minijinja::Environment;
use std::collections::HashSet;

const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("./prompt.md");
const SKILL_RULES_TEMPLATE: &str = include_str!("./skill_rules.md");

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
pub fn system_prompt(skills: &[Skill], format_instructions: &str) -> String {
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
            skills,
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
    let empty: &[Skill] = &[];
    render_template(
        "system_prompt",
        SYSTEM_PROMPT_TEMPLATE,
        minijinja::context! {
            is_subagent => true,
            skills => empty,
            global_agents_md => String::new(),
            local_agents_md => String::new(),
            date,
            pwd,
            repo_root,
            format_instructions => String::new(),
        },
    )
}

/// Resolve skills mentioned as `/skill-name` in the given sources (user messages, queries).
/// Single pass — does NOT scan skill content for further mentions.
/// Also auto-resolves explicit `needs` dependencies from resolved skills.
pub fn resolve_mentioned<'a>(sources: &[&str], skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut resolved = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for source in sources {
        for skill in skills {
            if seen.contains(&skill.name) {
                continue;
            }
            if source.contains(&format!("/{}", skill.name)) {
                seen.insert(skill.name.clone());
                resolved.push(skill);
            }
        }
    }

    // Auto-resolve explicit `needs` dependencies
    let needs_names: Vec<String> = resolved
        .iter()
        .flat_map(|s| s.needs.iter().cloned())
        .filter(|n| !seen.contains(n))
        .collect();
    for need_name in &needs_names {
        if let Some(skill) = skills.iter().find(|s| &s.name == need_name) {
            seen.insert(skill.name.clone());
            resolved.push(skill);
        }
    }

    resolved
}

/// Build a skill rules message for skills mentioned in the given sources.
/// Returns None if no skills are mentioned.
pub fn mentioned_skills_message(skills: &[Skill], scan_sources: &[&str]) -> Option<String> {
    let mentioned = resolve_mentioned(scan_sources, skills);
    if mentioned.is_empty() {
        return None;
    }
    let mentioned_names: HashSet<String> = mentioned.iter().map(|s| s.name.clone()).collect();
    let available: Vec<&Skill> = skills
        .iter()
        .filter(|s| !mentioned_names.contains(&s.name))
        .collect();
    Some(render_template(
        "skill_rules",
        SKILL_RULES_TEMPLATE,
        minijinja::context! { mentioned, available },
    ))
}

// ── Helpers for deterministic test rendering ──────────────────────────

#[cfg(test)]
mod test_helpers {
    use crate::core::skill::Skill;

    pub fn skill(name: &str, desc: &str, content: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: content.to_string(),
            needs: Vec::new(),
        }
    }

    pub fn skill_with_needs(name: &str, desc: &str, content: &str, needs: Vec<&str>) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: content.to_string(),
            needs: needs.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn mentioned_names(skills: &[Skill], sources: &[&str]) -> Vec<String> {
        super::resolve_mentioned(sources, skills)
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// Render the main agent prompt with deterministic values.
    pub fn render_main(
        skills: &[Skill],
        repo_root: Option<&str>,
        format_instructions: &str,
    ) -> String {
        super::render_template(
            "system_prompt",
            super::SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                is_subagent => false,
                skills,
                global_agents_md => String::new(),
                local_agents_md => String::new(),
                date => "2026-04-10",
                pwd => "/test/project",
                repo_root => repo_root.map(|s| s.to_string()),
                format_instructions,
            },
        )
    }

    /// Render the subagent prompt with deterministic values.
    pub fn render_sub(repo_root: Option<&str>) -> String {
        let empty: &[Skill] = &[];
        super::render_template(
            "system_prompt",
            super::SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                is_subagent => true,
                skills => empty,
                global_agents_md => String::new(),
                local_agents_md => String::new(),
                date => "2026-04-10",
                pwd => "/test/project",
                repo_root => repo_root.map(|s| s.to_string()),
                format_instructions => String::new(),
            },
        )
    }

    /// Render with global/local agents md.
    pub fn render_with_agents(global_agents_md: &str, local_agents_md: &str) -> String {
        let empty: &[Skill] = &[];
        super::render_template(
            "system_prompt",
            super::SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                is_subagent => false,
                skills => empty,
                global_agents_md,
                local_agents_md,
                date => "2026-04-10",
                pwd => "/test/project",
                repo_root => Option::<String>::None,
                format_instructions => String::new(),
            },
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::test_helpers::*;

    #[test]
    fn main_agent_has_all_tools() {
        let result = render_main(&[], None, "");
        assert!(
            result.contains("shell_tool")
                && result.contains("load_skills")
                && result.contains("load_references")
                && result.contains("subagent"),
            "main agent must have shell_tool, load_skills, load_references, and subagent"
        );
    }

    #[test]
    fn subagent_has_core_tools_but_cannot_spawn_subagents() {
        let result = render_sub(None);
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(role.contains("shell_tool"), "subagent must have shell_tool");
        assert!(
            role.contains("load_skills"),
            "subagent must have load_skills"
        );
        assert!(
            role.contains("load_references"),
            "subagent must have load_references"
        );
        assert!(
            !role.contains("subagent"),
            "subagent must NOT have subagent tool"
        );
    }

    // ── Immutability: core rules cannot be overridden ──────────────

    #[test]
    fn immutable_rules_appear_in_both_modes() {
        let main = render_main(&[], None, "");
        let sub = render_sub(None);
        let boundary = "START OF USER SECTION";
        assert!(
            main.contains(boundary),
            "main prompt missing user section boundary"
        );
        assert!(
            sub.contains(boundary),
            "subagent prompt missing user section boundary"
        );

        let main_immutable = main.split(boundary).next().unwrap_or("");
        let sub_immutable = sub.split(boundary).next().unwrap_or("");
        assert!(!main_immutable.is_empty(), "main has no immutable section");
        assert!(
            !sub_immutable.is_empty(),
            "subagent has no immutable section"
        );

        assert_eq!(
            main_immutable.trim(),
            sub_immutable.trim(),
            "main and subagent must share the same immutable rules"
        );
    }

    #[test]
    fn subagent_rules_define_when_to_and_not_to_spawn() {
        let result = render_main(&[], None, "");
        let immutable = result.split("START OF USER SECTION").next().unwrap_or("");
        assert!(immutable.contains("DO spawn"), "missing spawn permissions");
        assert!(
            immutable.contains("DO NOT spawn"),
            "missing spawn restrictions"
        );
    }

    // ── Self-sufficiency ─────────────────────────────────────────

    #[test]
    fn main_agent_must_be_self_sufficient() {
        let result = render_main(&[], None, "");
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(
            role.contains("NEVER ask") || role.contains("use your tools"),
            "main agent must be told to use tools instead of asking user"
        );
    }

    #[test]
    fn subagent_must_be_concise_and_immediate() {
        let result = render_sub(None);
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(
            role.contains("final answer immediately"),
            "subagent must provide answer immediately after tool results"
        );
    }

    // ── Repo-awareness ─────────────────────────────────────────

    #[test]
    fn main_agent_does_not_hardcode_repo_instructions() {
        let result = render_main(&[], Some("/my/project"), "");
        assert!(
            !result.contains("/my/project"),
            "repo root must not be hardcoded in system prompt"
        );
    }

    #[test]
    fn main_agent_outside_repo_has_no_repo_instructions() {
        let result = render_main(&[], None, "");
        assert!(
            !result.contains("git repo"),
            "should not mention git repo when not in one"
        );
    }

    #[test]
    fn subagent_in_repo_cannot_delegate_further() {
        let result = render_sub(Some("/my/repo"));
        let repo_section = result.split("git repo").nth(1).unwrap_or("");
        assert!(
            !repo_section.contains("subagent"),
            "subagent repo section must not reference subagent spawning"
        );
    }

    // ── Config layering ─────────────────────────────────────────

    #[test]
    fn skills_appear_only_when_provided() {
        let with = render_main(&[skill("my-skill", "desc", "content")], None, "");
        let without = render_main(&[], None, "");
        assert!(with.contains("my-skill"), "provided skill must appear");
        assert!(
            !without.contains("my-skill"),
            "missing skill must not appear"
        );
    }

    #[test]
    fn agents_md_sections_appear_only_when_provided() {
        let with_global = render_with_agents("use rustfmt", "");
        let with_local = render_with_agents("", "test first");
        let with_neither = render_with_agents("", "");
        assert!(
            with_global.contains("use rustfmt"),
            "global config must appear"
        );
        assert!(
            with_local.contains("test first"),
            "local config must appear"
        );
        assert!(
            !with_neither.contains("Global Agents Config"),
            "empty global config must not produce a section header"
        );
        assert!(
            !with_neither.contains("Project Agents Config"),
            "empty local config must not produce a section header"
        );
    }

    #[test]
    fn runtime_context_includes_date_and_working_directory() {
        let result = render_main(&[], None, "");
        assert!(result.contains("2026-04-10"), "date must appear");
        assert!(result.contains("/test/project"), "pwd must appear");
    }

    #[test]
    fn format_instructions_injected_when_provided() {
        let with = render_main(&[], None, "respond in YAML");
        let without = render_main(&[], None, "");
        assert!(
            with.contains("respond in YAML"),
            "format instructions must appear"
        );
        let trimmed = without.trim();
        assert!(
            !trimmed.ends_with("format_instructions"),
            "no dangling format instructions"
        );
    }

    // ── Template integrity ─────────────────────────────────────────

    #[test]
    fn all_template_variables_resolve() {
        let result = render_main(
            &[skill("bash", "commands", "content")],
            Some("/repo"),
            "format",
        );
        assert!(!result.contains("{%"), "unrendered Jinja block tag");
        assert!(!result.contains("{{"), "unrendered Jinja expression");
    }

    // ── Skill mention resolution ──────────────────────────────────

    #[test]
    fn mentioned_skills_resolve_from_query() {
        let skills = vec![
            skill("review", "code review", "review content"),
            skill("filesystem", "file ops", "fs content"),
        ];
        let names = mentioned_names(&skills, &["/review this file"]);
        assert!(names.contains(&"review".to_string()));
        assert!(
            !names.contains(&"filesystem".to_string()),
            "unmentioned skill must not resolve"
        );
    }

    #[test]
    fn mentioned_skills_does_not_scan_skill_content() {
        let skills = vec![
            skill("review", "code review", "needs /filesystem and /developer"),
            skill("filesystem", "file ops", "fs content"),
            skill("developer", "dev workflow", "dev content"),
        ];
        // Only the source is scanned, not the skill content
        let names = mentioned_names(&skills, &["/review this"]);
        assert!(names.contains(&"review".to_string()));
        assert!(
            !names.contains(&"filesystem".to_string()),
            "must NOT resolve from skill content — only from sources"
        );
    }

    #[test]
    fn needs_deps_auto_loaded() {
        let skills = vec![
            skill_with_needs("review", "code review", "content", vec!["filesystem"]),
            skill("filesystem", "file ops", "fs content"),
        ];
        let names = mentioned_names(&skills, &["/review this"]);
        assert!(names.contains(&"review".to_string()));
        assert!(
            names.contains(&"filesystem".to_string()),
            "needs dep must auto-load"
        );
    }

    #[test]
    fn mentioned_skills_deduplicates() {
        let skills = vec![skill("review", "code review", "content")];
        let names = mentioned_names(&skills, &["/review and /review again"]);
        assert_eq!(
            names.iter().filter(|n| **n == "review").count(),
            1,
            "must not duplicate"
        );
    }

    #[test]
    fn mentioned_skills_message_shows_loaded_and_available() {
        let skills = vec![
            skill("review", "code review", "review content"),
            skill("filesystem", "file ops", "fs content"),
        ];
        let result = super::mentioned_skills_message(&skills, &["/review this"]).unwrap();
        assert!(result.contains("Skill: review"), "loaded skill must appear");
        assert!(
            result.contains("Other available skills"),
            "available section must appear"
        );
        assert!(
            result.contains("filesystem"),
            "non-loaded skill must be listed as available"
        );
    }

    #[test]
    fn mentioned_skills_returns_none_when_nothing_mentioned() {
        let skills = vec![skill("review", "code review", "content")];
        assert!(
            super::mentioned_skills_message(&skills, &["nothing relevant"]).is_none(),
            "must return None when no skills mentioned"
        );
    }
}
