pub use agentsdk_plugin_skills::SkillsPlugin;

pub fn build_skills_plugin() -> anyhow::Result<SkillsPlugin> {
    let mut paths = vec![crate::config::pie_home().join("skills")];
    if let Some(root) = crate::utils::git_repo_root() {
        paths.push(std::path::PathBuf::from(root).join(".pie").join("skills"));
    }
    SkillsPlugin::builder()
        .search_paths(paths)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build skills plugin: {e}"))
}
