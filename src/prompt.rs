use crate::agent::{Agent, resolve_mentioned_agents};
use crate::config::pie_home;
use crate::skill::Skill;
use crate::utils::{find_upward_in_repo, load_file};
use minijinja::Environment;
use std::collections::HashSet;

const SYSTEM_PROMPT_TEMPLATE: &str = include_str!("../.pie/SYSTEM.md");

/// Render a `MiniJinja` template with context, falling back to raw template on error.
#[allow(clippy::panic, clippy::unwrap_used)]
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

/// Render the system prompt with pre-loaded skills injected.
pub fn system_prompt_with_loaded(
    skills: &[Skill],
    agents: &[Agent],
    json_output: bool,
    loaded: &[&Skill],
) -> String {
    let global_agents_md = load_file(pie_home().join("AGENTS.md"));
    let local_agents_md = find_upward_in_repo("AGENTS.md");
    let (date, pwd) = context_vars();
    let repo_root = crate::utils::git_repo_root();

    tracing::debug!(repo_root = ?repo_root, "system prompt context");

    render_template(
        "system_prompt",
        SYSTEM_PROMPT_TEMPLATE,
        minijinja::context! {
            agent_name => Option::<String>::None,
            agent_content => Option::<String>::None,
            skills,
            agents,
            loaded_skills => loaded,
            global_agents_md,
            local_agents_md,
            date,
            pwd,
            repo_root,
            json_output,
            interactivity => "none",
        },
    )
}

#[allow(clippy::needless_pass_by_value)]
pub fn subagent_prompt(
    repo_root: Option<String>,
    skills: &[Skill],
    agents: &[Agent],
    agent_name: Option<&str>,
    loaded: &[&Skill],
) -> String {
    let agent = agent_name.and_then(|name| agents.iter().find(|a| a.name == name));
    let agent_content = agent.map(|a| a.content.as_str());
    let interactivity = agent.map_or("none", |a| a.interactivity.as_ref());
    let (date, pwd) = context_vars();
    render_template(
        "system_prompt",
        SYSTEM_PROMPT_TEMPLATE,
        minijinja::context! {
            agent_name,
            agent_content,
            skills,
            agents,
            loaded_skills => loaded,
            global_agents_md => String::new(),
            local_agents_md => String::new(),
            date,
            pwd,
            repo_root,
            json_output => false,
            interactivity,
        },
    )
}

/// Resolve skill names to skills, auto-loading their `needs` dependencies.
/// Deduplicates and handles circular needs gracefully.
pub fn resolve_with_needs<'a>(names: &[String], skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();

    for name in names {
        if let Some(skill) = skills.iter().find(|s| s.name == *name)
            && seen.insert(skill.name.clone())
        {
            resolved.push(skill);
            for need in &skill.needs {
                if seen.insert(need.clone())
                    && let Some(dep) = skills.iter().find(|s| s.name == *need)
                {
                    resolved.push(dep);
                }
            }
        }
    }
    resolved
}

/// Resolve skills mentioned as `/skill-name` in the given sources (user messages, queries).
/// Single pass — does NOT scan skill content for further mentions.
/// Also auto-resolves explicit `needs` dependencies from resolved skills.
/// Auto-detects common patterns (e.g. "summarize repo" → explore) for small models.
pub fn resolve_mentioned<'a>(sources: &[&str], skills: &'a [Skill]) -> Vec<&'a Skill> {
    let mut mentioned_names: Vec<String> = skills
        .iter()
        .filter(|s| {
            sources
                .iter()
                .any(|src| src.contains(&format!("/{}", s.name)))
        })
        .map(|s| s.name.clone())
        .collect();

    // Auto-detect repo exploration patterns → load explore skill
    let text: String = sources.join(" ").to_lowercase();
    let repo_words = ["repo", "repository", "project", "codebase"];
    let explore_words = ["summarize", "summary", "explore", "analyze", "overview"];
    let has_repo = repo_words.iter().any(|w| text.contains(w));
    let has_explore = explore_words.iter().any(|w| text.contains(w));
    if has_repo && has_explore && !mentioned_names.iter().any(|n| n == "explore") {
        mentioned_names.push("explore".to_string());
    }

    resolve_with_needs(&mentioned_names, skills)
}

/// Resolve all skills that should be pre-loaded: directly mentioned skills
/// plus skills mentioned inside mentioned agents' content.
pub fn resolve_preloaded_skills<'a>(
    skills: &'a [Skill],
    agents: &[Agent],
    scan_sources: &[&str],
) -> Vec<&'a Skill> {
    let mentioned_agents = resolve_mentioned_agents(scan_sources, agents);

    // Scan mentioned agents' content for skill references (/skill-name)
    let agent_contents: Vec<&str> = mentioned_agents
        .iter()
        .map(|a| a.content.as_str())
        .collect();
    let agent_skills = resolve_mentioned(&agent_contents, skills);

    let mentioned_skills = resolve_mentioned(scan_sources, skills);
    let skill_names: Vec<String> = mentioned_skills
        .iter()
        .chain(agent_skills.iter())
        .map(|s| s.name.clone())
        .collect();

    resolve_with_needs(&skill_names, skills)
}

// ── Helpers for deterministic test rendering ──────────────────────────

#[cfg(test)]
mod test_helpers {
    use crate::agent::{Agent, Interactivity};
    use crate::skill::Skill;

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
            needs: needs.into_iter().map(ToString::to_string).collect(),
        }
    }

    pub fn agent(name: &str, desc: &str, content: &str) -> Agent {
        Agent {
            name: name.to_string(),
            description: desc.to_string(),
            interactivity: Interactivity::None,
            model: None,
            temperature: None,
            content: content.to_string(),
        }
    }

    pub fn mentioned_names(skills: &[Skill], sources: &[&str]) -> Vec<String> {
        super::resolve_mentioned(sources, skills)
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// Render the main agent prompt with deterministic values.
    pub fn render_main(skills: &[Skill], repo_root: Option<&str>, json_output: bool) -> String {
        super::render_template(
            "system_prompt",
            super::SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                agent_name => Option::<String>::None,
                agent_content => Option::<String>::None,
                skills,
                agents => Vec::<Agent>::new(),
                loaded_skills => Vec::<&Skill>::new(),
                global_agents_md => String::new(),
                local_agents_md => String::new(),
                date => "2026-04-10",
                pwd => "/test/project",
                repo_root => repo_root.map(ToString::to_string),
                json_output,
                interactivity => "none",
            },
        )
    }

    /// Render the subagent prompt with deterministic values.
    pub fn render_sub(repo_root: Option<&str>) -> String {
        let empty: &[Skill] = &[];
        let empty_agents: &[Agent] = &[];
        super::render_template(
            "system_prompt",
            super::SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                agent_name => Some("test-agent"),
                agent_content => Some("You are a test agent."),
                skills => empty,
                agents => empty_agents,
                loaded_skills => Vec::<&Skill>::new(),
                global_agents_md => String::new(),
                local_agents_md => String::new(),
                date => "2026-04-10",
                pwd => "/test/project",
                repo_root => repo_root.map(ToString::to_string),
                json_output => false,
                interactivity => "none",
            },
        )
    }

    /// Render with global/local agents md.
    pub fn render_with_agents_md(global_agents_md: &str, local_agents_md: &str) -> String {
        let empty: &[Skill] = &[];
        let empty_agents: &[Agent] = &[];
        super::render_template(
            "system_prompt",
            super::SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                agent_name => Option::<String>::None,
                agent_content => Option::<String>::None,
                skills => empty,
                agents => empty_agents,
                loaded_skills => Vec::<&Skill>::new(),
                global_agents_md,
                local_agents_md,
                date => "2026-04-10",
                pwd => "/test/project",
                repo_root => Option::<String>::None,
                json_output => false,
                interactivity => "none",
            },
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::Agent;
    use super::test_helpers::*;

    #[test]
    fn main_agent_instructs_tool_use() {
        let result = render_main(&[], None, false);
        assert!(
            result.contains("shell_tool"),
            "main agent must reference shell_tool"
        );
        assert!(
            result.contains("load_skills"),
            "main agent prompt must reference load_skills in skills section"
        );
    }

    #[test]
    fn subagent_has_core_tools() {
        let result = render_sub(None);
        assert!(
            result.contains("shell_tool"),
            "subagent must reference shell_tool"
        );
        assert!(
            result.contains("load_skills"),
            "subagent must reference load_skills"
        );
    }

    // ── Immutability: core rules cannot be overridden ──────────────

    #[test]
    fn immutable_rules_appear_in_both_modes() {
        let main = render_main(&[], None, false);
        let sub = render_sub(None);

        // Both must have core sections
        for section in &["## Rules", "## Known Commands"] {
            assert!(
                main.contains(section),
                "main missing {section}"
            );
            assert!(
                sub.contains(section),
                "subagent missing {section}"
            );
        }

        // Both must warn about not calling skills as tools
        assert!(
            main.contains("Never call a skill name as a tool"),
            "main must have tool discipline rule"
        );
        assert!(
            sub.contains("Never call a skill name as a tool"),
            "subagent must have tool discipline rule"
        );
    }

    #[test]
    fn main_agent_must_be_self_sufficient() {
        let result = render_main(&[], None, false);
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(
            role.contains("NEVER ask") || role.contains("use your tools"),
            "main agent must be told to use tools instead of asking user"
        );
    }

    #[test]
    fn subagent_with_agent_name_includes_persona() {
        let result = render_sub(None);
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(
            role.contains("You are a test agent."),
            "subagent with agent_name must include agent_content persona"
        );
    }

    // ── Repo-awareness ─────────────────────────────────────────

    #[test]
    fn main_agent_does_not_hardcode_repo_instructions() {
        let result = render_main(&[], Some("/my/project"), false);
        assert!(
            !result.contains("/my/project"),
            "repo root must not be hardcoded in system prompt"
        );
    }

    #[test]
    fn main_agent_outside_repo_has_no_repo_instructions() {
        let result = render_main(&[], None, false);
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
        let with = render_main(&[skill("my-skill", "desc", "content")], None, false);
        let without = render_main(&[], None, false);
        assert!(with.contains("my-skill"), "provided skill must appear");
        assert!(
            !without.contains("my-skill"),
            "missing skill must not appear"
        );
    }

    #[test]
    fn agents_md_sections_appear_only_when_provided() {
        let with_global = render_with_agents_md("use rustfmt", "");
        let with_local = render_with_agents_md("", "test first");
        let with_neither = render_with_agents_md("", "");
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
        let result = render_main(&[], None, false);
        assert!(result.contains("2026-04-10"), "date must appear");
        assert!(result.contains("/test/project"), "pwd must appear");
    }

    #[test]
    fn json_output_mode_injected_when_enabled() {
        let with = render_main(&[], None, true);
        let without = render_main(&[], None, false);
        assert!(
            with.contains("JSON Output Mode"),
            "json output mode must appear when enabled"
        );
        assert!(
            !without.contains("JSON Output Mode"),
            "json output mode must not appear when disabled"
        );
    }

    // ── Template integrity ─────────────────────────────────────────

    #[test]
    fn all_template_variables_resolve() {
        let result = render_main(
            &[skill("bash", "commands", "content")],
            Some("/repo"),
            false,
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

    // ── resolve_with_needs (shared by mention resolution and load_skills) ──

    #[test]
    fn resolve_with_needs_auto_loads_deps() {
        let skills = vec![
            skill_with_needs(
                "review",
                "code review",
                "content",
                vec!["filesystem", "developer"],
            ),
            skill("filesystem", "file ops", "fs content"),
            skill("developer", "dev workflow", "dev content"),
        ];
        let resolved = super::resolve_with_needs(&["review".to_string()], &skills);
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"review"), "requested skill must resolve");
        assert!(names.contains(&"filesystem"), "needs dep must auto-load");
        assert!(names.contains(&"developer"), "needs dep must auto-load");
    }

    #[test]
    fn resolve_with_needs_deduplicates_circular() {
        let skills = vec![
            skill_with_needs("a", "a", "a", vec!["b"]),
            skill_with_needs("b", "b", "b", vec!["a"]),
        ];
        let resolved = super::resolve_with_needs(&["a".to_string(), "b".to_string()], &skills);
        assert_eq!(resolved.len(), 2, "circular needs must not duplicate");
    }

    #[test]
    fn resolve_with_needs_empty_for_unknown() {
        let skills = vec![skill("a", "a", "a")];
        let resolved = super::resolve_with_needs(&["nonexistent".to_string()], &skills);
        assert!(resolved.is_empty());
    }

    // ── Agent + skill integration ──────────────────────────────────

    #[test]
    fn resolve_preloaded_skills_auto_loads_from_agent_content() {
        let skills = vec![
            skill("explore", "explore codebase", "explore content"),
            skill("review", "code review", "review content"),
        ];
        let agents = vec![agent(
            "reviewer",
            "code reviewer",
            "Use /explore and /review to check this code.",
        )];
        let resolved = super::resolve_preloaded_skills(&skills, &agents, &["/reviewer check this"]);
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"explore"),
            "skill mentioned in agent content must auto-load"
        );
        assert!(
            names.contains(&"review"),
            "skill mentioned in agent content must auto-load"
        );
    }

    #[test]
    fn resolve_preloaded_skills_includes_direct_mentions() {
        let skills = vec![skill("review", "code review", "review content")];
        let agents: Vec<Agent> = vec![];
        let resolved = super::resolve_preloaded_skills(&skills, &agents, &["/review this"]);
        let names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"review"),
            "directly mentioned skill must resolve"
        );
    }

    #[test]
    fn no_agents_no_mentions_returns_empty() {
        let skills = vec![skill("review", "code review", "content")];
        let agents = vec![agent("reviewer", "reviewer", "content")];
        let resolved = super::resolve_preloaded_skills(&skills, &agents, &["nothing relevant"]);
        assert!(resolved.is_empty(), "no mentions should resolve to empty");
    }
}
