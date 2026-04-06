use std::path::PathBuf;

pub fn pie_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".pie")
}

pub fn load_file(path: impl AsRef<std::path::Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Walk from cwd upward to find a file, stopping at the git repo root.
pub fn find_upward_in_repo(name: &str) -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let path = dir.join(name);
        if let Some(content) = load_file(&path) {
            return Some(content);
        }
        if dir.join(".git").exists() {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}
