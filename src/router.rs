use crate::skill::Skill;

/// Find all skills mentioned in a query that actually exist.
pub fn find_mentioned_skills(query: &str, skills: &[Skill]) -> Vec<String> {
    let available: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    crate::skill::extract_skill_mentions(query)
        .into_iter()
        .filter(|m| available.contains(&m.as_str()))
        .collect()
}

/// Build the orchestrator system prompt — exact same format as the TS version.
pub fn build_orchestrator_prompt(
    skills: &[Skill],
    mentioned_skills: &[String],
    user_query: &str,
) -> String {
    let escaped_query = user_query.replace('"', "\\\"");

    let skill_commands = skills
        .iter()
        .map(|s| format!("{}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        r#"You are a skill router. Given a user question, you must run the matching skill using the bash tool.

Available skills:
{skill_commands}

User question: "{escaped_query}"

Instructions:
1. Pick the best skill from the list above that matches the user question.
2. Run EXACTLY this command using the bash tool:
   pie --skill <skill-name> "{escaped_query}"
3. Do NOT answer the question yourself. Do NOT run any other commands.

Example: if the user asks "who is the PM of Nepal?", run:
pie --skill ddg-search "who is the PM of Nepal?""#
    );

    if !mentioned_skills.is_empty() {
        let names = mentioned_skills.join(", ");
        prompt.push_str(&format!(
            "\n\nThe user explicitly requested these skills: {names}. You MUST call ALL of them."
        ));
    }

    prompt
}
