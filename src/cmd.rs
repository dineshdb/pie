use crate::config::ResolvedConfig;
use std::sync::Arc;
use tracing::{info, warn};

pub fn handle_status(config: &ResolvedConfig, registry: &Arc<crate::registry::Registry>) {
    println!("Provider:    {}", config.provider.name);
    println!("Model:       {}", config.provider.model);
    println!("Base URL:    {}", config.provider.openai_url);
    if let Some(ref url) = config.provider.anthropic_url {
        println!("Anthropic:   {url}");
    }
    println!("Log Level:   {}", config.log_level);
    println!("Output:      {:?}", config.output_format);
    println!("Max Steps:   {}", config.max_steps);

    println!("\n--- Hooks Manager ---");
    println!("Timeout: {}ms", config.hooks.timeout_ms);
    println!("Total Hooks: {}", config.hooks.hooks.len());
    for hook in &config.hooks.hooks {
        println!(
            " - {}: event={}, scope={:?}, strategy={:?}, kind={:?}",
            hook.name, hook.event, hook.scope, hook.strategy, hook.kind
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

    if !config.known_commands.is_empty() {
        println!("\n--- Known Commands ---");
        let mut commands: Vec<_> = config.known_commands.iter().collect();
        commands.sort_by_key(|(name, _)| *name);
        for (name, cfg) in commands {
            info!(
                " - {name}: {}",
                cfg.description.as_deref().unwrap_or(&cfg.command)
            );
        }
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
