use lexiclean::Lexiclean;

/// Replace the user's home directory with `~/` in a path string.
pub fn anonymize_path(path: &str) -> String {
    let p = std::path::Path::new(path).lexiclean();
    let path_str = p.display().to_string();

    if let Some(home) = dirs::home_dir() {
        let home = home.lexiclean();
        let home_str = home.display().to_string();
        if path_str == home_str {
            return "~".to_string();
        }
        if let Some(rest) = path_str.strip_prefix(&home_str) {
            return format!("~{rest}");
        }
    }
    path_str
}

/// A newtype wrapper for paths that automatically anonymizes the home directory on creation.
/// Transparently serializes to its string representation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct AnonymizedPath(String);

impl std::fmt::Display for AnonymizedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for AnonymizedPath {
    fn from(path: String) -> Self {
        Self(anonymize_path(&path))
    }
}

impl From<&str> for AnonymizedPath {
    fn from(path: &str) -> Self {
        Self(anonymize_path(path))
    }
}

impl From<std::path::PathBuf> for AnonymizedPath {
    fn from(path: std::path::PathBuf) -> Self {
        Self(anonymize_path(&path.display().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anonymized_path() {
        if let Some(home) = dirs::home_dir() {
            let home_str = home.display().to_string();
            let p = format!("{home_str}/src");
            let anonymized = AnonymizedPath::from(p);
            assert_eq!(anonymized.0.as_str(), "~/src");

            // Test normalization with lexiclean
            if let Some(file_name) = home.file_name().and_then(|n| n.to_str()) {
                let p_complex = format!("{home_str}/../{file_name}/src/./tools");
                let anonymized_complex = AnonymizedPath::from(p_complex);
                assert_eq!(anonymized_complex.0.as_str(), "~/src/tools");
            }
        }
    }
}
