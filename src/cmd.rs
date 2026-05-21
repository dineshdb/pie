use crate::config::ResolvedConfig;
use crate::registry::Registry;
use crate::utils::output::OutputFormat;
use serde::Serialize;
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
const CLEAR_DESC: &str = "Start a new session";
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
    if config.output_format.is_json() {
        let plugins = config
            .plugins
            .iter()
            .map(|p| PluginStatus {
                name: p.name.clone(),
                hooks: p
                    .hooks
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
            output_format: config.output_format.clone(),
            max_steps: config.max_steps,
            plugins,
            skills: registry.skills.iter().map(|s| s.name.clone()).collect(),
            agents: registry.agents.iter().map(|a| a.name.clone()).collect(),
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
    println!("Plugins: {}", config.plugins.len());
    for plugin in &config.plugins {
        println!(" - Plugin: {}", plugin.name);
        for hook in &plugin.hooks {
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
    if config.output_format.is_json() {
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

pub fn handle_launch(
    config: &ResolvedConfig,
    all_args: &[String],
    no_sandbox: bool,
) -> anyhow::Result<()> {
    let (command, args) = if let Some((cmd, rest)) = all_args.split_first() {
        (cmd.clone(), rest.to_vec())
    } else {
        anyhow::bail!("no command provided to launch");
    };

    if command == "claude" && config.provider.anthropic_url.is_none() {
        anyhow::bail!(
            "launching 'claude' is only supported on providers with an anthropic endpoint (e.g., 'anthropic', 'openrouter', 'ollama', 'zai')"
        );
    }

    let env = config.provider.env_vars();
    let launch_configs = crate::config::load_launch_config()?;

    // Resolve alias
    let (actual_command, launch_cfg) = if let Some(cfg) = launch_configs.get(&command) {
        (command.clone(), Some(cfg))
    } else if let Some((name, cfg)) = launch_configs
        .iter()
        .find(|(_, cfg)| cfg.aliases.contains(&command))
    {
        (name.clone(), Some(cfg))
    } else {
        (command.clone(), None)
    };

    let mut final_args = args;
    if let Some(cfg) = launch_cfg
        && final_args.is_empty()
    {
        final_args.clone_from(&cfg.args);
    }

    // If the command itself contains spaces and no args were provided, split it.
    // This handles cases like `pie launch "claude --version"`.
    let (actual_command, extra_args) = if actual_command.contains(' ') && final_args.is_empty() {
        let parts: Vec<String> = actual_command
            .split_whitespace()
            .map(String::from)
            .collect();
        if let (Some(cmd), Some(args)) = (parts.first(), parts.get(1..)) {
            (cmd.clone(), args.to_vec())
        } else {
            (actual_command, Vec::new())
        }
    } else {
        (actual_command, Vec::new())
    };
    if !extra_args.is_empty() {
        final_args = extra_args;
    }

    let mut cmd = if no_sandbox {
        let mut c = std::process::Command::new(&actual_command);
        c.args(&final_args);
        c
    } else if let Some(cfg) = launch_cfg
        && let Some(sandbox) = &cfg.sandbox
    {
        p1e_sandbox::build_command(&actual_command, &final_args, sandbox)
    } else {
        let mut c = std::process::Command::new(&actual_command);
        c.args(&final_args);
        c
    };

    // Explicitly inherit stdio for interaction
    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    // 1. Provider env vars
    for (k, v) in env {
        cmd.env(k, v);
    }

    // 2. Extra env vars from launch.toml
    if let Some(cfg) = launch_cfg {
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
    }

    // Process Handoff (Unix):
    // On Unix-like systems, we use `execvp` to replace the current `pie` process with the target command.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        anyhow::bail!("failed to launch command: {err}");
    }

    // Fallback (Non-Unix):
    // Spawning is used where process replacement is not supported (e.g., Windows).
    #[cfg(not(unix))]
    {
        let mut child = cmd.spawn().context("failed to launch command")?;
        let status = child.wait().context("failed to wait for child")?;
        std::process::exit(status.code().unwrap_or(0));
    }
}
