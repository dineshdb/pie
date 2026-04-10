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

// ── Helpers for deterministic test rendering ──────────────────────────

#[cfg(test)]
mod test_helpers {
    use crate::core::skill::Skill;

    pub fn skill(name: &str, desc: &str, content: &str, embedded: bool) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            content: content.to_string(),
            is_embedded: embedded,
        }
    }

    /// Render the main agent prompt with deterministic values.
    pub fn render_main(
        system_skills: &[Skill],
        user_skills: &[Skill],
        repo_root: Option<&str>,
        format_instructions: &str,
    ) -> String {
        super::render_template(
            "system_prompt",
            super::SYSTEM_PROMPT_TEMPLATE,
            minijinja::context! {
                is_subagent => false,
                system_skills,
                user_skills,
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
                system_skills => empty,
                user_skills => empty,
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
                system_skills => empty,
                user_skills => empty,
                global_agents_md,
                local_agents_md,
                date => "2026-04-10",
                pwd => "/test/project",
                repo_root => Option::<String>::None,
                format_instructions => String::new(),
            },
        )
    }

    pub fn mentioned<'a>(skills: &'a [Skill], sources: &[&str]) -> Vec<&'a Skill> {
        super::resolve_mentioned(sources, skills)
    }

    pub fn mentioned_names(skills: &[Skill], sources: &[&str]) -> Vec<String> {
        mentioned(skills, sources)
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────
//
// These tests verify SEMANTIC REQUIREMENTS of the prompt, not implementation
// details. Each test corresponds to a behavioral contract the prompt must
// uphold. If a test name reads like it's checking for a specific word, the
// test is wrong — it should check for a capability or constraint.

#[cfg(test)]
mod tests {
    use super::test_helpers::*;
    #[test]
    fn main_agent_has_all_three_tools() {
        let result = render_main(&[], &[], None, "");
        assert!(
            result.contains("shell_tool")
                && result.contains("load_skills")
                && result.contains("subagent"),
            "main agent must have shell_tool, load_skills, and subagent"
        );
    }

    #[test]
    fn subagent_has_shell_and_load_skills_but_cannot_spawn_subagents() {
        let result = render_sub(None);
        let role = result.split("Agent Role").nth(1).unwrap_or("");
        assert!(role.contains("shell_tool"), "subagent must have shell_tool");
        assert!(
            role.contains("load_skills"),
            "subagent must have load_skills"
        );
        assert!(
            !role.contains("subagent"),
            "subagent must NOT have subagent tool"
        );
    }

    // ── Immutability: core rules cannot be overridden ──────────────
    //
    // The prompt has a strict priority system. Rules above the user
    // section boundary must appear in both main and subagent prompts,
    // ensuring no user message or skill can override them.

    #[test]
    fn immutable_rules_appear_in_both_modes() {
        let main = render_main(&[], &[], None, "");
        let sub = render_sub(None);
        // The subagent rules are above the user section boundary
        let boundary = "START OF USER SECTION";
        assert!(
            main.contains(boundary),
            "main prompt missing user section boundary"
        );
        assert!(
            sub.contains(boundary),
            "subagent prompt missing user section boundary"
        );

        // Everything before the boundary is immutable — check it's non-empty
        let main_immutable = main.split(boundary).next().unwrap_or("");
        let sub_immutable = sub.split(boundary).next().unwrap_or("");
        assert!(
            !main_immutable.is_empty(),
            "main prompt has no immutable section"
        );
        assert!(
            !sub_immutable.is_empty(),
            "subagent prompt has no immutable section"
        );

        // Both modes share the same immutable section
        assert_eq!(
            main_immutable.trim(),
            sub_immutable.trim(),
            "main and subagent must share the same immutable rules"
        );
    }

    // ── Subagent spawning policy ───────────────────────────────────
    //
    // The immutable section contains rules about when to spawn subagents.
    // These rules prevent the model from delegating single tasks or
    // creating unnecessary subagent overhead.

    #[test]
    fn subagent_rules_define_when_to_and_not_to_spawn() {
        let result = render_main(&[], &[], None, "");
        let immutable = result.split("START OF USER SECTION").next().unwrap_or("");
        // Must have both positive and negative rules
        assert!(immutable.contains("DO spawn"), "missing spawn permissions");
        assert!(
            immutable.contains("DO NOT spawn"),
            "missing spawn restrictions"
        );
    }

    // ── Self-sufficiency: agents must act, not ask ─────────────────
    //
    // Both agent modes must be instructed to use their tools rather
    // than asking the user to provide information they could gather
    // themselves.

    #[test]
    fn main_agent_must_be_self_sufficient() {
        let result = render_main(&[], &[], None, "");
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

    // ── Repo-awareness: file reading behavior changes in git repos ─
    //
    // When inside a git repo, the prompt must instruct the agent to
    // read actual files. The main agent uses load_skills + shell_tool
    // directly (NOT subagent delegation). The subagent reads files
    // itself since it only has shell_tool.

    #[test]
    fn main_agent_in_repo_reads_files_directly() {
        let result = render_main(&[], &[], Some("/my/project"), "");
        assert!(
            result.contains("/my/project"),
            "repo root must appear in prompt"
        );
        // Must NOT delegate to subagent for repo summarization
        let repo_section = result.split("/my/project").nth(1).unwrap_or("");
        assert!(
            !repo_section.contains("subagent tool with skill_name"),
            "main agent must not delegate repo exploration to subagent"
        );
    }

    #[test]
    fn main_agent_outside_repo_has_no_repo_instructions() {
        let result = render_main(&[], &[], None, "");
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

    // ── Config layering: user config sections are conditional ──────
    //
    // User skills, global AGENTS.md, and project AGENTS.md are
    // injected conditionally. Empty config must not produce ghost
    // sections that confuse the model.

    #[test]
    fn user_skills_appear_only_when_provided() {
        let with = render_main(
            &[],
            &[skill("my-skill", "desc", "content", false)],
            None,
            "",
        );
        let without = render_main(&[], &[], None, "");
        assert!(with.contains("my-skill"), "provided user skill must appear");
        assert!(
            !without.contains("my-skill"),
            "missing user skill must not appear"
        );
    }

    #[test]
    fn agents_md_sections_appear_only_when_provided() {
        let with_global = render_with_agents("use rustfmt", "");
        let with_local = render_with_agents("", "test first");
        let with_neither = render_with_agents("", "");
        assert!(
            with_global.contains("use rustfmt"),
            "global config must appear when provided"
        );
        assert!(
            with_local.contains("test first"),
            "local config must appear when provided"
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
        let result = render_main(&[], &[], None, "");
        // These are injected from the test helpers with fixed values
        assert!(
            result.contains("2026-04-10"),
            "date must appear in runtime context"
        );
        assert!(
            result.contains("/test/project"),
            "pwd must appear in runtime context"
        );
    }

    #[test]
    fn format_instructions_injected_when_provided() {
        let with = render_main(&[], &[], None, "respond in YAML");
        let without = render_main(&[], &[], None, "");
        assert!(
            with.contains("respond in YAML"),
            "format instructions must appear"
        );
        // Empty format instructions should not produce visible artifacts
        let trimmed = without.trim();
        assert!(
            !trimmed.ends_with("format_instructions"),
            "no dangling format instructions"
        );
    }

    // ── Skill resolution: transitive, deduplicating ────────────────
    //
    // Skills can reference other skills via /<skill-name> in their
    // content. Resolution must be transitive (follows chains) and
    // deduplicating (handles circular references).

    #[test]
    fn skill_resolution_is_transitive() {
        let skills = vec![
            skill(
                "review",
                "code review",
                "needs /filesystem and /developer",
                false,
            ),
            skill("filesystem", "file ops", "content", true),
            skill("developer", "dev workflow", "content", true),
        ];
        let names = mentioned_names(&skills, &["/review this"]);
        assert!(names.contains(&"review".to_string()));
        assert!(
            names.contains(&"filesystem".to_string()),
            "must transitively resolve /filesystem"
        );
        assert!(
            names.contains(&"developer".to_string()),
            "must transitively resolve /developer"
        );
    }

    #[test]
    fn skill_resolution_deduplicates_circular_refs() {
        let skills = vec![
            skill("a", "skill a", "refs /b", false),
            skill("b", "skill b", "refs /a", false),
        ];
        let names = mentioned_names(&skills, &["/a and /b"]);
        assert_eq!(
            names.iter().filter(|n| **n == "a").count(),
            1,
            "circular ref must not duplicate skill a"
        );
        assert_eq!(
            names.iter().filter(|n| **n == "b").count(),
            1,
            "circular ref must not duplicate skill b"
        );
    }

    #[test]
    fn skill_resolution_returns_nothing_for_unmentioned_skills() {
        let skills = vec![skill("bash", "commands", "content", true)];
        assert!(
            mentioned_names(&skills, &["nothing relevant here"]).is_empty(),
            "unmentioned skills must not resolve"
        );
    }

    // ── Template integrity ─────────────────────────────────────────

    #[test]
    fn all_template_variables_resolve() {
        let result = render_main(
            &[skill("bash", "commands", "content", true)],
            &[skill("custom", "user skill", "content", false)],
            Some("/repo"),
            "format",
        );
        assert!(!result.contains("{%"), "unrendered Jinja block tag");
        assert!(!result.contains("{{"), "unrendered Jinja expression");
    }
}
