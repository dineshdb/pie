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
        r#"
Run the command below that best matches the user's query.

Skills: <skill-name>: <skill-description>
{skill_commands}

Subagent format: pie --skill <skill-name> "{escaped_query}"

Pick ONE command from the list above and run it. Do not modify the command. Do not run anything else."#
    );

    if !mentioned_skills.is_empty() {
        let names = mentioned_skills.join(", ");
        prompt.push_str(&format!(
            "\n\nThe user explicitly requested these skills: {names}. You MUST call ALL of them."
        ));
    }

    prompt
}
