use super::types::{LaunchConfig, PieConfig, ProviderBaseUrl};
use crate::utils::git_repo_root;
use anyhow::Context;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use include_dir::{Dir, include_dir};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The embedded `.pie/` directory compiled into the binary.
pub static EMBEDDED_PIE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/.pie");

pub fn pie_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".pie")
}

pub fn logs_dir() -> PathBuf {
    let dir = pie_home().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

static PROVIDERS_DATA: OnceLock<HashMap<String, ProviderBaseUrl>> = OnceLock::new();

pub fn get_providers_data() -> anyhow::Result<&'static HashMap<String, ProviderBaseUrl>> {
    if let Some(data) = PROVIDERS_DATA.get() {
        return Ok(data);
    }
    let data = load_providers_data()?;
    Ok(PROVIDERS_DATA.get_or_init(|| data))
}

fn load_providers_data() -> anyhow::Result<HashMap<String, ProviderBaseUrl>> {
    let file = EMBEDDED_PIE_DIR
        .get_file("providers.toml")
        .context("critical asset 'providers.toml' must exist in embedded dir")?;
    let content = file
        .contents_utf8()
        .context("critical asset 'providers.toml' must be valid UTF-8")?;
    let data: HashMap<String, ProviderBaseUrl> = Figment::new()
        .merge(Toml::string(content))
        .extract()
        .context("failed to parse embedded 'providers.toml'")?;
    Ok(data)
}

/// Load the PIE configuration from global and project-specific paths.
pub fn load_config() -> anyhow::Result<PieConfig> {
    let mut pie_config: PieConfig = load_toml_config(&["pie.toml", "secrets.toml"], false, true)?;

    for provider in pie_config.provider.values_mut() {
        if let Some(ref api_key) = provider.api_key {
            let secret_str = api_key.expose_secret();
            if let Some(val) = pie_config.secrets.get(secret_str) {
                provider.api_key = Some(val.clone());
            }
        }
    }

    Ok(pie_config)
}

/// A generic helper to load TOML configuration from multiple sources.
/// Sources are merged in order: Embedded -> Global -> Project.
/// Filenames are merged in order within each source.
pub fn load_toml_config<T>(
    filenames: &[&str],
    use_embedded: bool,
    use_local: bool,
) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let home = pie_home();
    let project = if use_local {
        git_repo_root().map(PathBuf::from)
    } else {
        None
    };

    load_toml_config_impl(filenames, use_embedded, &home, project.as_deref())
}

fn load_toml_config_impl<T>(
    filenames: &[&str],
    use_embedded: bool,
    home_dir: &std::path::Path,
    project_root: Option<&std::path::Path>,
) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let mut figment = Figment::new();

    // 1. Embedded defaults
    if use_embedded {
        for filename in filenames {
            if let Some(file) = EMBEDDED_PIE_DIR.get_file(filename) {
                let content = file
                    .contents_utf8()
                    .context(format!("embedded {filename} is not UTF-8"))?;
                figment = figment.merge(Toml::string(content));
            }
        }
    }

    // 2. Global overrides (from home_dir, which is already ~/.pie in prod)
    for filename in filenames {
        let global = home_dir.join(filename);
        if global.exists() {
            figment = figment.merge(Toml::file_exact(global));
        }
    }

    // 3. Local (Project) overrides
    if let Some(root) = project_root {
        for filename in filenames {
            let project_file = root.join(".pie").join(filename);
            if project_file.exists() {
                figment = figment.merge(Toml::file_exact(project_file));
            }
        }
    }

    figment
        .extract()
        .map_err(|e| anyhow::anyhow!("failed to parse configuration: {e}"))
}

/// Load launch configurations from embedded and global paths.
pub fn load_launch_config() -> anyhow::Result<HashMap<String, LaunchConfig>> {
    load_toml_config(&["launch.toml"], true, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::fs;
    use tempfile::tempdir;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestConfig {
        #[serde(default)]
        key: String,
        #[serde(default)]
        secret: String,
    }

    #[test]
    fn test_load_toml_config_merging() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let home = temp.path().join("home");
        let project = temp.path().join("project");

        fs::create_dir_all(&home)?;
        fs::create_dir_all(project.join(".pie"))?;

        // 1. Global config
        fs::write(home.join("test.toml"), "key = 'global'")?;

        // 2. Project config overrides global
        fs::write(project.join(".pie").join("test.toml"), "key = 'project'")?;

        // 3. Secrets merged if requested
        fs::write(project.join(".pie").join("secrets.toml"), "secret = 'shh'")?;

        let config: TestConfig =
            load_toml_config_impl(&["test.toml", "secrets.toml"], false, &home, Some(&project))?;

        assert_eq!(config.key, "project");
        assert_eq!(config.secret, "shh");

        Ok(())
    }

    #[test]
    fn test_load_toml_config_global_only() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let home = temp.path().join("home");
        fs::create_dir_all(&home)?;

        fs::write(home.join("test.toml"), "key = 'global'")?;

        let config: TestConfig = load_toml_config_impl(&["test.toml"], false, &home, None)?;

        assert_eq!(config.key, "global");
        Ok(())
    }
}
