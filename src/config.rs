use std::path::PathBuf;

pub fn pie_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".pie")
}

pub fn logs_dir() -> PathBuf {
    let dir = pie_home().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}
