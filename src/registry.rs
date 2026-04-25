use crate::agent::{Agent, get_all_agents};
use crate::skill::{Skill, get_all_skills};
use crate::ui::tui::command::BuiltinCommand;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Registry {
    pub agents: Vec<Agent>,
    pub skills: Vec<Skill>,
    pub completions: Vec<String>,
}

impl Registry {
    pub fn load() -> Arc<Self> {
        let agents = get_all_agents();
        let skills = get_all_skills();

        let mut completions = Vec::new();
        for agent in &agents {
            completions.push(format!("/{}", agent.name));
        }

        for cmd in BuiltinCommand::all_commands() {
            completions.push(cmd.to_string());
        }

        for skill in &skills {
            completions.push(format!("/{}", skill.name));
        }

        Arc::new(Self {
            agents,
            skills,
            completions,
        })
    }
}
