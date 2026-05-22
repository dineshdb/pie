mod conversationmode;
mod debug;
mod developer;
mod externalplugin;
mod filesystem;
mod helper_binaries;
mod subagent;
mod system_prompts;

pub use crate::config::pie_home;
pub use conversationmode::ConversationModePlugin;
pub use debug::DebugPlugin;
pub use developer::DeveloperPlugin;
pub use externalplugin::ExternalPlugin;
pub use filesystem::FileSystemPlugin;
pub use helper_binaries::HelperBinariesPlugin;
pub use subagent::SubAgentPlugin;
pub use system_prompts::{EmbeddedSystemPromptPlugin, SystemPromptsPlugin};

use crate::hook::CommandHook;
use crate::registry::PluginMetadata;
use crate::utils::git_repo_root;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

struct ScanResult {
    plugins: Vec<ExternalPlugin>,
    metadata: Vec<PluginMetadata>,
}

static SCAN_CACHE: OnceLock<ScanResult> = OnceLock::new();

/// Scan plugin directories (global + project-local) and return discovered plugins and metadata.
/// Results are cached after the first call.
pub fn scan_plugins() -> (Vec<ExternalPlugin>, Vec<PluginMetadata>) {
    let result = SCAN_CACHE.get_or_init(scan_plugins_inner);
    (result.plugins.clone(), result.metadata.clone())
}

fn scan_plugins_inner() -> ScanResult {
    let dirs = plugin_scan_dirs();
    let mut plugins = Vec::new();
    let mut metadata = Vec::new();

    for dir in &dirs {
        if !dir.exists() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some((plugin, meta)) = scan_plugin_dir(&path) {
                    plugins.push(plugin);
                    metadata.push(meta);
                }
            } else if path.extension().is_some_and(|ext| ext == "toml")
                && let Some(plugin) = scan_plugin_toml(&path)
            {
                plugins.push(plugin);
            }
        }
    }

    ScanResult { plugins, metadata }
}

fn plugin_scan_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![pie_home().join("plugins")];
    if let Some(root) = git_repo_root() {
        dirs.push(PathBuf::from(root).join(".pie").join("plugins"));
    }
    dirs
}

fn parse_plugin_toml(content: &str) -> Option<crate::config::PieConfig> {
    Figment::new().merge(Toml::string(content)).extract().ok()
}

/// Load a plugin subdirectory with `plugin.toml`.
fn scan_plugin_dir(path: &Path) -> Option<(ExternalPlugin, PluginMetadata)> {
    let plugin_toml = path.join("plugin.toml");
    if !plugin_toml.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&plugin_toml).ok()?;
    let plugin_config = parse_plugin_toml(&content)?;
    let mut meta = PluginMetadata::from_toml_str(&content).ok()?;

    if meta.system_prompt.is_none() {
        let system_md = path.join("SYSTEM.md");
        if system_md.exists() {
            meta.system_prompt = std::fs::read_to_string(system_md).ok();
        }
    }

    let plugin_dir_str = path.to_string_lossy().to_string();
    let mut hooks: Vec<CommandHook> = Vec::new();

    for mut hook_def in plugin_config.hooks {
        hook_def.plugin_dir = Some(plugin_dir_str.clone());
        if hook_def.handler.starts_with("./") {
            let abs_handler = path
                .join(&hook_def.handler)
                .canonicalize()
                .unwrap_or_else(|_| path.join(&hook_def.handler));
            hook_def.handler = abs_handler.to_string_lossy().to_string();
        }
        hooks.push(CommandHook::from(hook_def));
    }

    let plugin = ExternalPlugin {
        name: meta.name.clone(),
        hooks,
    };

    Some((plugin, meta))
}

/// Load hooks from a standalone `.toml` plugin file.
fn scan_plugin_toml(path: &Path) -> Option<ExternalPlugin> {
    let content = std::fs::read_to_string(path).ok()?;
    let plugin_config = parse_plugin_toml(&content)?;

    let mut hooks: Vec<CommandHook> = Vec::new();
    for hook_def in plugin_config.hooks {
        hooks.push(CommandHook::from(hook_def));
    }

    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    Some(ExternalPlugin { name, hooks })
}
