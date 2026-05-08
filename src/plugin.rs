use crate::config::pie_home;
use crate::hook::HookDef;
use crate::registry::Plugin;
use crate::utils::git_repo_root;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use std::path::{Path, PathBuf};

/// Scan plugin directories (global + project-local) and return discovered hooks and plugins.
pub fn scan_plugins() -> (Vec<HookDef>, Vec<Plugin>) {
    let dirs = plugin_scan_dirs();
    let mut hooks = Vec::new();
    let mut plugins = Vec::new();

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
                scan_plugin_dir(&path, &mut hooks, &mut plugins);
            } else if path.extension().is_some_and(|ext| ext == "toml") {
                scan_plugin_toml(&path, &mut hooks);
            }
        }
    }

    (hooks, plugins)
}

fn plugin_scan_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![pie_home().join("plugins")];
    if let Some(root) = git_repo_root() {
        dirs.push(PathBuf::from(root).join(".pie").join("plugins"));
    }
    dirs
}

/// Load a plugin subdirectory with `plugin.toml`.
fn scan_plugin_dir(path: &Path, hooks: &mut Vec<HookDef>, plugins: &mut Vec<Plugin>) {
    let plugin_toml = path.join("plugin.toml");
    if !plugin_toml.exists() {
        return;
    }

    // Try loading as a Plugin (for registry).
    if let Ok(plugin) = Plugin::load_from_dir(path) {
        plugins.push(plugin);
    }

    // Try loading hooks from plugin.toml.
    let Ok(content) = std::fs::read_to_string(&plugin_toml) else {
        return;
    };
    let Ok(plugin_config) = Figment::new()
        .merge(Toml::string(&content))
        .extract::<crate::config::PieConfig>()
    else {
        return;
    };

    let plugin_dir_str = path.to_string_lossy().to_string();
    for mut hook in plugin_config.hooks {
        hook.plugin_dir = Some(plugin_dir_str.clone());
        if hook.handler.starts_with("./") {
            let abs_handler = path
                .join(&hook.handler)
                .canonicalize()
                .unwrap_or_else(|_| path.join(&hook.handler));
            hook.handler = abs_handler.to_string_lossy().to_string();
        }
        hooks.push(hook);
    }
}

/// Load hooks from a standalone `.toml` plugin file.
fn scan_plugin_toml(path: &Path, hooks: &mut Vec<HookDef>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(plugin_config) = Figment::new()
        .merge(Toml::string(&content))
        .extract::<crate::config::PieConfig>()
    else {
        return;
    };
    hooks.extend(plugin_config.hooks);
}
