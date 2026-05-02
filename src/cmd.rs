use crate::config::ResolvedConfig;
use std::sync::Arc;
use tracing::warn;

pub fn handle_status(config: &ResolvedConfig, registry: &Arc<crate::registry::Registry>) {
    println!("Provider: {}", config.provider.name);
    println!("Model:    {}", config.provider.model);
    println!("Base URL: {}", config.provider.base_url);
    println!("Log Level: {}", config.log_level);
    println!("Output Format: {:?}", config.output_format);
    println!("Max Steps: {}", config.max_steps);

    println!("\n--- Hooks Manager ---");
    println!("Timeout: {}ms", config.hooks.timeout_ms);
    println!("Total Hooks: {}", config.hooks.hooks.len());
    for hook in &config.hooks.hooks {
        println!(
            " - {}: event={}, scope={:?}, kind={:?}",
            hook.name, hook.event, hook.scope, hook.kind
        );
    }

    println!("\n--- Registry ---");
    println!("Skills: {}", registry.skills.len());
    for skill in &registry.skills {
        println!(" - {}", skill.name);
    }
    println!("Agents: {}", registry.agents.len());
    for agent in &registry.agents {
        println!(" - {}", agent.name);
    }
}

pub fn handle_skills(registry: &Arc<crate::registry::Registry>) {
    let skills = &registry.skills;
    let agents = &registry.agents;

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
