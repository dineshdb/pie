use std::{
    cmp::{max, min},
    path::PathBuf,
};

const MAX_HISTORY: usize = 10_000;

pub struct InputHistory {
    cache: Vec<String>,
    index: usize,
    path: PathBuf,
}

impl InputHistory {
    pub fn new(path: PathBuf) -> Self {
        let cache = load_history_from_file(&path);
        let index = cache.len();
        Self { cache, index, path }
    }

    pub fn prev(&mut self) -> Option<&str> {
        self.index = max(self.index - 1, 0);
        self.cache.get(self.index).map(String::as_str)
    }

    pub fn next(&mut self) -> Option<&str> {
        self.index = min(self.index + 1, self.cache.len() - 1);
        self.cache.get(self.index).map(String::as_str)
    }

    pub fn append(&mut self, text: &str) {
        use std::io::Write;

        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        if self.cache.len() >= MAX_HISTORY {
            self.cache.remove(0);
        }
        self.cache.push(trimmed.clone());
        self.index = self.cache.len();

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{trimmed}");
        }
    }

    pub fn find_hint(&self, prefix: &str) -> Option<String> {
        self.cache
            .iter()
            .rev()
            .find(|h| h.starts_with(prefix) && h.as_str() != prefix)
            .and_then(|h| h.get(prefix.len()..).map(ToString::to_string))
    }
}

fn load_history_from_file(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect()
}
