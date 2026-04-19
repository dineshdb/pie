use crate::agent::get_all_agents;
use crate::skill::get_all_skills;
use tracing::warn;

pub fn handle_list_skills() {
    let skills = get_all_skills();
    let agents = get_all_agents();

    print_named_section("Available skills", skills.iter(), |s| {
        (&s.name, &s.description)
    });
    if !skills.is_empty() && !agents.is_empty() {
        println!();
    }
    print_named_section("Available agents", agents.iter(), |a| {
        (&a.name, &a.description)
    });

    if skills.is_empty() && agents.is_empty() {
        warn!("No skills or agents found.");
    }
}

fn print_named_section<T, F>(header: &str, items: impl Iterator<Item = T>, get_info: F)
where
    F: Fn(&T) -> (&String, &String),
{
    let collected: Vec<_> = items.collect();
    if collected.is_empty() {
        return;
    }
    println!("{header}:");
    for item in &collected {
        let (name, desc) = get_info(item);
        println!(" - {name}: {desc}");
    }
}
