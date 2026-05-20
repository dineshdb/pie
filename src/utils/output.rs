use serde::{Deserialize, Serialize};

/// Output format requested by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    #[default]
    Default,
    Markdown,
    Json(Option<String>),
}

impl OutputFormat {
    pub fn is_json(&self) -> bool {
        matches!(self, Self::Json(_))
    }

    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Markdown | Self::Json(_))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonResponse {
    pub response: serde_json::Value,
    pub session_id: Option<String>,
    pub model_used: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl JsonResponse {
    pub fn new(
        response: serde_json::Value,
        session_id: Option<String>,
        model_used: Option<String>,
    ) -> Self {
        Self {
            response,
            session_id,
            model_used,
            timestamp: chrono::Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_explicit_only_true_for_markdown_and_json() {
        assert!(!OutputFormat::Default.is_explicit());
        assert!(OutputFormat::Markdown.is_explicit());
        assert!(OutputFormat::Json(None).is_explicit());
        assert!(OutputFormat::Json(Some("{}".to_string())).is_explicit());
    }
}
