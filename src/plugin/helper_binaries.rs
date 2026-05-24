use agentsdk::{AgentPlugin, PluginContext};
use async_trait::async_trait;
use std::borrow::Cow;
use std::process::Command;

#[derive(Debug, Default)]
pub struct HelperBinariesPlugin {
    scan_cache: String,
}

impl HelperBinariesPlugin {
    pub fn new() -> Self {
        let scan_cache = Self::generate_descriptions();
        Self { scan_cache }
    }

    fn generate_descriptions() -> String {
        let mut descriptions = Vec::new();
        let mut bin_dirs = vec![crate::config::pie_home().join("bin")];
        if let Some(git_root) = crate::utils::git_repo_root() {
            bin_dirs.push(std::path::PathBuf::from(git_root).join(".pie").join("bin"));
        }

        for dir in bin_dirs {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file()
                        && let Some(name) = path.file_name().and_then(|n| n.to_str())
                        && let Ok(output) = Command::new(&path).arg("--help").output()
                    {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if let Some(first_line) = stdout.lines().next()
                            && !first_line.trim().is_empty()
                        {
                            descriptions.push(format!("- `{}`: {}", name, first_line.trim()));
                        }
                    }
                }
            }
        }

        if descriptions.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n### Available Helper Binaries\nYou can run these binaries via the `shell` tool:\n{}",
                descriptions.join("\n")
            )
        }
    }
}

#[async_trait]
impl AgentPlugin for HelperBinariesPlugin {
    fn name(&self) -> &'static str {
        "helper_binaries"
    }

    async fn prepare_system_prompt(
        &mut self,
        _ctx: &mut PluginContext,
    ) -> Option<Cow<'static, str>> {
        if self.scan_cache.is_empty() {
            None
        } else {
            Some(Cow::Owned(self.scan_cache.clone()))
        }
    }
}
