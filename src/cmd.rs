use crate::agent::get_all_agents;
use crate::skill::get_all_skills;
use tracing::warn;

pub fn handle_list_skills() {
    let skills = get_all_skills();
    if !skills.is_empty() {
        println!("Available skills:");
        for s in &skills {
            println!(" - {}: {}", s.name, s.description);
        }
    }
    let agents = get_all_agents();
    if !agents.is_empty() {
        println!("\nAvailable agents:");
        for a in &agents {
            println!(" - {}: {}", a.name, a.description);
        }
    }
    if skills.is_empty() && agents.is_empty() {
        warn!("No skills or agents found.");
    }
}
