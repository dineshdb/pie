use crate::config::{CliConfig, ResolvedConfig};
use crate::registry::Registry;
use crate::utils::output::OutputFormat;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use strum::{AsRefStr, EnumIter, EnumString};
use tracing::warn;

macro_rules! define_builtin_commands {
    ($($variant:ident => [$($name:expr),+]),* $(,)?) => {
        #[derive(Debug, Clone, Copy, EnumIter, EnumString, AsRefStr, PartialEq, Eq)]
        pub enum BuiltinCommand {
            $(
                $(#[strum(serialize = $name)])+
                $variant,
            )*
        }

        impl BuiltinCommand {
            #[allow(dead_code)]
            pub fn all_commands() -> Vec<&'static str> {
                vec![$($($name),+),*]
            }

            pub fn names(&self) -> &[&'static str] {
                match self {
                    $(Self::$variant => &[$($name),+],)*
                }
            }
        }
    };
}

define_builtin_commands! {
    Help => ["/help", "/h"],
    Quit => ["/quit", "/exit", "/q"],
    Model => ["/model"],
    Skills => ["/skills", "/ls"],
    Clear => ["/clear"],
    New => ["/new"],
}

const HELP_DESC: &str = "Show help and available commands";
const QUIT_DESC: &str = "Exit the application";
const MODEL_DESC: &str = "Switch or view the current model";
const SKILLS_DESC: &str = "List available agents and skills";
const CLEAR_DESC: &str = "Clear the chat history";
const NEW_DESC: &str = "Start a new session";

impl BuiltinCommand {
    pub fn description(self) -> &'static str {
        match self {
            Self::Help => HELP_DESC,
            Self::Quit => QUIT_DESC,
            Self::Model => MODEL_DESC,
            Self::Skills => SKILLS_DESC,
            Self::Clear => CLEAR_DESC,
            Self::New => NEW_DESC,
        }
    }
}

#[derive(Serialize)]
struct StatusOutput<'a> {
    provider: &'a crate::config::ResolvedProvider,
    log_level: &'a str,
    output_format: OutputFormat,
    max_steps: u32,
    plugins: Vec<PluginStatus>,
    skills: Vec<String>,
    agents: Vec<String>,
    known_commands: &'a HashMap<String, CliConfig>,
}

#[derive(Serialize)]
struct PluginStatus {
    name: String,
    hooks: Vec<HookStatus>,
}

#[derive(Serialize)]
struct HookStatus {
    name: String,
    event: String,
    scope: String,
    strategy: String,
}

pub fn handle_status(config: &ResolvedConfig, registry: &Arc<Registry>) {
    if config.output_format == OutputFormat::Json {
        let plugins = config
            .plugins
            .plugins
            .iter()
            .map(|p| PluginStatus {
                name: p.name().to_string(),
                hooks: p
                    .hooks()
                    .iter()
                    .map(|h| HookStatus {
                        name: h.name().to_string(),
                        event: h.event().to_string(),
                        scope: format!("{:?}", h.scope()),
                        strategy: format!("{:?}", h.strategy()),
                    })
                    .collect(),
            })
            .collect();

        let status = StatusOutput {
            provider: &config.provider,
            log_level: &config.log_level,
            output_format: config.output_format,
            max_steps: config.max_steps,
            plugins,
            skills: registry.skills.iter().map(|s| s.name.clone()).collect(),
            agents: registry.agents.iter().map(|a| a.name.clone()).collect(),
            known_commands: &config.known_commands,
        };

        if let Ok(json) = serde_json::to_string_pretty(&status) {
            println!("{json}");
            return;
        }
    }

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
    println!("Timeout: {}ms", config.plugins.timeout_ms);
    println!("Plugins: {}", config.plugins.plugins.len());
    for plugin in &config.plugins.plugins {
        println!(" - Plugin: {}", plugin.name());
        for hook in plugin.hooks() {
            println!(
                "   - {}: event={}, scope={:?}, strategy={:?}",
                hook.name(),
                hook.event(),
                hook.scope(),
                hook.strategy()
            );
        }
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
        print!("{}", format_known_commands(&config.known_commands));
    }
}

#[derive(Serialize)]
struct SkillsOutput {
    skills: Vec<SkillInfo>,
    agents: Vec<SkillInfo>,
}

#[derive(Serialize)]
struct SkillInfo {
    name: String,
    description: String,
}

pub fn handle_skills(config: &ResolvedConfig, registry: &Arc<Registry>) {
    if config.output_format == OutputFormat::Json {
        let output = SkillsOutput {
            skills: registry
                .skills
                .iter()
                .map(|s| SkillInfo {
                    name: s.name.clone(),
                    description: s.description.clone(),
                })
                .collect(),
            agents: registry
                .agents
                .iter()
                .map(|a| SkillInfo {
                    name: a.name.clone(),
                    description: a.description.clone(),
                })
                .collect(),
        };

        if let Ok(json) = serde_json::to_string_pretty(&output) {
            println!("{json}");
            return;
        }
    }

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

pub fn format_known_commands(commands: &HashMap<String, CliConfig>) -> String {
    let mut out = String::from("You can run known external commands via shell tool:\n");
    let mut sorted: Vec<_> = commands.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    for (name, cfg) in sorted {
        let _ = writeln!(
            out,
            "- {name}: {}",
            cfg.description.as_deref().unwrap_or(&cfg.command)
        );
    }
    out
}
