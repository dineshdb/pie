pub fn load_file(path: impl AsRef<std::path::Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Walk from cwd upward to find a file, stopping at the git repo root or home directory.
pub fn find_upward_in_repo(name: &str) -> Option<String> {
    let home = dirs::home_dir();
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..32 {
        let path = dir.join(name);
        if let Some(content) = load_file(&path) {
            return Some(content);
        }
        if dir.join(".git").exists() {
            return None;
        }
        if home.as_ref().is_some_and(|h| dir == *h) {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
    None
}
