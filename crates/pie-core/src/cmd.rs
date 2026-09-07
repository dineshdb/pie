use crate::config::{LaunchConfig, ResolvedConfig};
use crate::db::DbPool;
use crate::registry::Registry;
use crate::utils::output::OutputFormat;
use p1e_sandbox::Permission;
use serde::Serialize;
use std::collections::HashMap;
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
    Mode => ["/mode"],
    Skills => ["/skills", "/ls"],
    Clear => ["/clear"],
    New => ["/new"],
}

const HELP_DESC: &str = "Show help and available commands";
const QUIT_DESC: &str = "Exit the application";
const MODEL_DESC: &str = "Switch or view the current model";
const MODE_DESC: &str = "Switch mode: plan, build, debug, test, review, architect";
const SKILLS_DESC: &str = "List available commands and skills";
const CLEAR_DESC: &str = "Start a new session";
const NEW_DESC: &str = "Start a new session";

impl BuiltinCommand {
    pub fn description(self) -> &'static str {
        match self {
            Self::Help => HELP_DESC,
            Self::Quit => QUIT_DESC,
            Self::Model => MODEL_DESC,
            Self::Mode => MODE_DESC,
            Self::Skills => SKILLS_DESC,
            Self::Clear => CLEAR_DESC,
            Self::New => NEW_DESC,
        }
    }
}

#[derive(Serialize)]
struct McpServerStatus {
    name: String,
    url: String,
}

#[derive(Serialize)]
struct StatusOutput<'a> {
    provider: &'a crate::config::ResolvedProvider,
    log_level: &'a str,
    output_format: OutputFormat,
    skills: Vec<String>,
    agents: Vec<String>,
    mcp_servers: Vec<McpServerStatus>,
}

fn mcp_server_status(config: &ResolvedConfig) -> Vec<McpServerStatus> {
    let mut servers: Vec<_> = config
        .mcp
        .iter()
        .map(|(name, server)| McpServerStatus {
            name: name.clone(),
            url: server.url.to_string(),
        })
        .collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    servers
}

pub fn handle_status(config: &ResolvedConfig, registry: &Arc<Registry>) {
    if config.output_format.is_json() {
        let status = StatusOutput {
            provider: &config.provider,
            log_level: &config.log_level,
            output_format: config.output_format.clone(),
            skills: registry.skills.iter().map(|s| s.name.clone()).collect(),
            agents: registry.agents.iter().map(|a| a.name.clone()).collect(),
            mcp_servers: mcp_server_status(config),
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

    let mcp_servers = mcp_server_status(config);
    if !mcp_servers.is_empty() {
        println!("\n--- MCP ---");
        for server in &mcp_servers {
            println!(" - {}: {}", server.name, server.url);
        }
    }

    println!("\n--- Registry ---");
    println!("Skills: {}", registry.skills.len());
    for skill in &registry.skills {
        println!(" - {}", skill.name);
    }
    println!("Commands: {}", registry.agents.len());
    for agent in &registry.agents {
        println!(" - {}", agent.name);
    }
}

#[derive(Serialize)]
struct UsageOutput<'a> {
    /// Time window in days; 0 means all time.
    days: u32,
    models: &'a [crate::usage::ModelUsage],
    total: crate::usage::UsageReport,
}

/// `pie usage`: LLM spend per model over a window. `--json` (empty value,
/// e.g. `pie usage --json=`) switches to machine-readable output.
pub async fn handle_usage(
    config: &ResolvedConfig,
    pool: Arc<DbPool>,
    days: Option<u32>,
) -> anyhow::Result<()> {
    let days = days.unwrap_or(30);
    let since_ms = if days == 0 {
        0
    } else {
        chrono::Utc::now().timestamp_millis() - i64::from(days) * 86_400_000
    };
    let rows = crate::usage::by_model(&pool, since_ms).await?;

    if config.output_format.is_json() {
        let out = UsageOutput {
            days,
            models: &rows,
            total: crate::usage::totals(&rows),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    print!("{}", crate::usage::render_report(&rows, days));
    Ok(())
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    references: Vec<crate::registry::Reference>,
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
                    references: s.references.clone(),
                })
                .collect(),
            agents: registry
                .agents
                .iter()
                .map(|a| SkillInfo {
                    name: a.name.clone(),
                    description: a.description.clone(),
                    references: Vec::new(),
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
        (&s.name, &s.description, &s.references)
    });
    if !skills.is_empty() && !agents.is_empty() {
        println!();
    }
    print_named_section("Available commands", agents.iter(), |a| {
        (&a.name, &a.description, &[])
    });

    if skills.is_empty() && agents.is_empty() {
        warn!("No skills or commands found.");
    }
}

fn print_named_section<'a, T, F>(header: &str, items: impl Iterator<Item = T>, get_info: F)
where
    F: Fn(&T) -> (&'a String, &'a String, &'a [crate::registry::Reference]),
{
    let collected: Vec<_> = items.collect();
    if collected.is_empty() {
        return;
    }
    println!("{header}:");
    for item in &collected {
        let (name, desc, refs) = get_info(item);
        println!(" - {name}: {desc}");
        for r in refs {
            println!("   - {}: {}", r.title, r.path);
        }
    }
}

pub fn handle_exec(
    _config: &ResolvedConfig,
    registry: &Arc<Registry>,
    skill_name: Option<String>,
    script_args: &[String],
) -> anyhow::Result<()> {
    let (skill_name_resolved, script, extra_args) = parse_exec_args(skill_name, script_args)?;

    let ext = std::path::Path::new(&script)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| anyhow::anyhow!("script '{script}' has no extension"))?;

    let valid = ["sh", "bash", "py", "js", "ts", "rb", "pl"];
    if !valid.iter().any(|a| ext.eq_ignore_ascii_case(a)) {
        anyhow::bail!(
            "invalid script extension '.{ext}': allowed: .sh, .bash, .py, .js, .ts, .rb, .pl"
        );
    }

    let script_path =
        resolve_skill_script_path(&skill_name_resolved, &script).ok_or_else(|| {
            anyhow::anyhow!("script '{script}' not found for skill '{skill_name_resolved}'")
        })?;

    let dir = script_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid script path"))?
        .to_path_buf();

    let mut extra_bin_dirs = Vec::new();
    let bin_dir = dir.join("bin");
    if bin_dir.is_dir() {
        extra_bin_dirs.push(bin_dir);
    }

    let pie_config = crate::config::load_config()?;
    let sandbox = crate::config::build_sandbox(&pie_config);

    let mut sandbox_cfg = (*sandbox).clone();

    // Merge permissions from skill or agent
    if let Some(skill) = registry
        .skills
        .iter()
        .find(|s| s.name == skill_name_resolved)
    {
        if let Some(perms_val) = skill.extra.get("permissions") {
            let perms: Vec<Permission> = serde_json::from_value(perms_val.clone())?;
            for perm in perms {
                perm.apply_to(&mut sandbox_cfg);
            }
        }
    } else if let Some(agent) = registry
        .agents
        .iter()
        .find(|a| a.name == skill_name_resolved)
    {
        for perm in &agent.grants {
            perm.apply_to(&mut sandbox_cfg);
        }
        if let Some(agent_sandbox) = &agent.sandbox {
            sandbox_cfg.merge(agent_sandbox);
        }
    } else {
        anyhow::bail!("skill or agent '{skill_name_resolved}' not found");
    }

    let dir_str = dir.to_string_lossy().to_string();
    if !sandbox_cfg.allow_read.contains(&dir_str) {
        sandbox_cfg.allow_read.push(dir_str);
    }

    for bin in &extra_bin_dirs {
        let bin_str = bin.to_string_lossy().to_string();
        if !sandbox_cfg.allow_read.contains(&bin_str) {
            sandbox_cfg.allow_read.push(bin_str);
        }
    }

    let script_quoted = shell_quote(&script_path.to_string_lossy());
    let cmd = if extra_args.is_empty() {
        script_quoted
    } else {
        let args_quoted: Vec<String> = extra_args.iter().map(|a| shell_quote(a)).collect();
        format!("{script_quoted} {}", args_quoted.join(" "))
    };

    let exit_code =
        crate::tools::run_sandboxed_command_streaming(&cmd, &sandbox_cfg, &extra_bin_dirs)
            .map_err(|e| anyhow::anyhow!("execution failed: {e}"))?;

    std::process::exit(exit_code);
}

fn parse_exec_args(
    skill_name: Option<String>,
    script_args: &[String],
) -> anyhow::Result<(String, String, Vec<String>)> {
    let (skill, script, extra) = if let Some(skill) = skill_name {
        let script = script_args
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing script name"))?;
        (
            skill,
            script.clone(),
            script_args.get(1..).unwrap_or_default().to_vec(),
        )
    } else {
        let skill = script_args
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing skill name"))?;
        let script = script_args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("missing script name"))?;
        (
            skill.clone(),
            script.clone(),
            script_args.get(2..).unwrap_or_default().to_vec(),
        )
    };
    Ok((skill, script, extra))
}

fn resolve_skill_script_path(entity: &str, script: &str) -> Option<std::path::PathBuf> {
    let candidates = |base: std::path::PathBuf| -> Vec<std::path::PathBuf> {
        vec![base.join(script), base.join("bin").join(script)]
    };

    let find = |base: std::path::PathBuf| -> Option<std::path::PathBuf> {
        candidates(base).into_iter().find(|p| p.exists())
    };

    if let Some(root) = crate::utils::git_repo_root() {
        let base = std::path::PathBuf::from(root)
            .join(".pie")
            .join("skills")
            .join(entity);
        if let Some(p) = find(base) {
            return Some(p);
        }
    }
    if let Some(p) = find(crate::config::pie_home().join("skills").join(entity)) {
        return Some(p);
    }

    if let Some(root) = crate::utils::git_repo_root() {
        let base = std::path::PathBuf::from(root)
            .join(".pie")
            .join("commands")
            .join(entity);
        if let Some(p) = find(base) {
            return Some(p);
        }
    }
    find(crate::config::pie_home().join("commands").join(entity))
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| {
        c.is_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '+' | '=' | ',' | '~')
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn handle_launch(
    config: &ResolvedConfig,
    all_args: &[String],
    no_sandbox: bool,
) -> anyhow::Result<()> {
    let (command, args) = all_args
        .split_first()
        .map(|(cmd, rest)| (cmd.clone(), rest.to_vec()))
        .ok_or_else(|| anyhow::anyhow!("no command provided to launch"))?;

    if command == "claude" && config.provider.anthropic_url.is_none() {
        anyhow::bail!(
            "launching 'claude' is only supported on providers with an anthropic endpoint (e.g., 'anthropic', 'openrouter', 'ollama', 'zai')"
        );
    }

    let env = config.provider.env_vars();
    let launch_configs = crate::config::load_launch_config()?;

    let (launch_cfg, resolved_command) = resolve_launch_command(&command, &launch_configs);
    let final_args = resolve_launch_args(&resolved_command, &args, launch_cfg);

    let mut cmd = build_launch_process(&resolved_command, &final_args, launch_cfg, no_sandbox);

    cmd.stdin(std::process::Stdio::inherit());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    for (k, v) in env {
        cmd.env(k, v);
    }

    if let Some(cfg) = launch_cfg {
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        anyhow::bail!("failed to launch command: {err}");
    }

    #[cfg(not(unix))]
    {
        let mut child = cmd.spawn().context("failed to launch command")?;
        let status = child.wait().context("failed to wait for child")?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

fn resolve_launch_command<'a>(
    command: &str,
    configs: &'a HashMap<String, LaunchConfig>,
) -> (Option<&'a LaunchConfig>, String) {
    if let Some(cfg) = configs.get(command) {
        return (Some(cfg), command.to_string());
    }
    if let Some((name, cfg)) = configs
        .iter()
        .find(|(_, cfg)| cfg.aliases.iter().any(|a| a == command))
    {
        return (Some(cfg), name.clone());
    }
    (None, command.to_string())
}

fn resolve_launch_args(
    command: &str,
    user_args: &[String],
    launch_cfg: Option<&LaunchConfig>,
) -> Vec<String> {
    if !user_args.is_empty() {
        return user_args.to_vec();
    }

    if command.contains(' ') {
        let parts: Vec<&str> = command.split_whitespace().collect();
        if let Some((_, rest)) = parts.split_first() {
            return rest.iter().map(ToString::to_string).collect();
        }
    }

    launch_cfg.map_or(vec![], |cfg| cfg.args.clone())
}

fn build_launch_process(
    command: &str,
    args: &[String],
    launch_cfg: Option<&LaunchConfig>,
    no_sandbox: bool,
) -> std::process::Command {
    if no_sandbox {
        let mut c = std::process::Command::new(command);
        c.args(args);
        return c;
    }
    if let Some(cfg) = launch_cfg
        && let Some(sandbox) = &cfg.sandbox
    {
        return p1e_sandbox::build_command(command, args, sandbox);
    }
    let mut c = std::process::Command::new(command);
    c.args(args);
    c
}

/// The cron CLI surface: subcommands of `pie cron`.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum CronCommand {
    /// List schedules loaded from files
    List,
    /// Show recent run history (optionally for a specific schedule)
    Runs { id: Option<String> },
    /// Execute due schedules (one-shot)
    Run,
    /// Evaluate a `when` CEL expression against the current state
    ///
    /// `last_run` is treated as never, so `since`/`never_run` read as they
    /// would on a schedule's first firing.
    Test {
        /// The CEL expression, e.g. 'exists("~/src/x") && `never_run`'
        expr: String,
    },
}

#[allow(clippy::too_many_lines)]
pub async fn handle_cron(
    command: CronCommand,
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    match command {
        CronCommand::List => {
            let schedules = crate::cron::load_all_schedules();
            if schedules.is_empty() {
                println!("no schedules found");
                return Ok(());
            }

            let width = schedules.iter().map(|s| s.id.len()).max().unwrap_or(4);
            for s in &schedules {
                let status = if s.enabled { "enabled " } else { "disabled" };
                let trigger = match (&s.cron, &s.when) {
                    (Some(cron), Some(when)) => format!("{cron} when {when}"),
                    (Some(cron), None) => cron.clone(),
                    (None, Some(when)) => format!("when {when}"),
                    (None, None) => "(no trigger — never fires)".to_string(),
                };
                let id = format!("{:width$}", s.id);
                println!("{id}  {status}  {trigger}  {}", s.description);
            }
            Ok(())
        }
        CronCommand::Test { expr } => {
            let ctx = crate::cron::ConditionContext {
                now: chrono::Utc::now(),
                last_run: None,
            };
            match crate::cron::evaluate(&expr, &ctx) {
                Ok(true) => {
                    println!("true — a schedule with this `when` would fire");
                    Ok(())
                }
                Ok(false) => {
                    println!("false — not due");
                    std::process::exit(1)
                }
                Err(e) => {
                    println!("error: {e}");
                    println!("a `when` that cannot be evaluated never fires");
                    std::process::exit(2)
                }
            }
        }
        CronCommand::Runs { id } => {
            let rows = match &id {
                Some(schedule_id) => {
                    crate::cron::CronRun::recent_for_schedule(&pool, schedule_id).await?
                }
                None => crate::cron::CronRun::recent_all(&pool).await?,
            };

            if rows.is_empty() {
                let label = id.as_deref().unwrap_or("any");
                println!("no runs for schedule '{label}'");
                return Ok(());
            }

            let schedules = crate::cron::load_all_schedules();
            for r in &rows {
                let started = chrono::DateTime::from_timestamp_millis(r.started_at)
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let dur_ms = r.finished_at.map_or(0, |f| f - r.started_at);
                let code = r.exit_code.map_or("-".to_string(), |c| c.to_string());
                let desc = schedules
                    .iter()
                    .find(|s| s.id == r.cron_id)
                    .and_then(|s| (!s.description.is_empty()).then_some(&s.description))
                    .unwrap_or(&r.cron_id);

                println!(
                    "{}  {}  {}  {}ms  {}  {}",
                    desc, started, r.status, dur_ms, code, r.notes
                );
            }
            Ok(())
        }
        CronCommand::Run => crate::cron::run_due_jobs(pool, registry).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_config(name: &str, aliases: &[&str]) -> (String, LaunchConfig) {
        let cfg = LaunchConfig {
            args: vec!["--default".to_string()],
            aliases: aliases.iter().map(ToString::to_string).collect(),
            ..Default::default()
        };
        (name.to_string(), cfg)
    }

    #[test]
    fn resolve_command_direct_match() {
        let mut configs = HashMap::new();
        configs.insert("claude".to_string(), LaunchConfig::default());

        let (cfg, cmd) = resolve_launch_command("claude", &configs);
        assert!(cfg.is_some());
        assert_eq!(cmd, "claude");
    }

    #[test]
    fn resolve_command_alias_match() {
        let mut configs = HashMap::new();
        configs.insert(
            "claude".to_string(),
            LaunchConfig {
                aliases: vec!["c".to_string(), "cl".to_string()],
                ..Default::default()
            },
        );

        let (cfg, cmd) = resolve_launch_command("c", &configs);
        assert!(cfg.is_some());
        assert_eq!(cmd, "claude");
    }

    #[test]
    fn resolve_command_unknown() {
        let configs = HashMap::new();
        let (cfg, cmd) = resolve_launch_command("unknown", &configs);
        assert!(cfg.is_none());
        assert_eq!(cmd, "unknown");
    }

    #[test]
    fn resolve_args_user_args_take_priority() {
        let configs = HashMap::new();
        let (cfg, _) = resolve_launch_command("anything", &configs);

        let args = resolve_launch_args("anything", &["--user".to_string()], cfg);
        assert_eq!(args, vec!["--user"]);
    }

    #[test]
    fn resolve_args_space_split_command() {
        let configs = HashMap::new();
        let (cfg, _) = resolve_launch_command("anything", &configs);

        let args = resolve_launch_args("claude --version", &[], cfg);
        assert_eq!(args, vec!["--version"]);
    }

    #[test]
    fn resolve_args_space_split_multiple() {
        let configs = HashMap::new();
        let (cfg, _) = resolve_launch_command("anything", &configs);

        let args = resolve_launch_args("claude --do --stuff", &[], cfg);
        assert_eq!(args, vec!["--do", "--stuff"]);
    }

    #[test]
    fn resolve_args_default_from_config() {
        let mut configs = HashMap::new();
        let (k, v) = make_config("claude", &[]);
        configs.insert(k, v);
        let (cfg, _) = resolve_launch_command("claude", &configs);

        let args = resolve_launch_args("claude", &[], cfg);
        assert_eq!(args, vec!["--default"]);
    }

    #[test]
    fn resolve_args_no_inputs() {
        let configs = HashMap::new();
        let (cfg, _) = resolve_launch_command("anything", &configs);

        let args = resolve_launch_args("anything", &[], cfg);
        assert!(args.is_empty());
    }
}
