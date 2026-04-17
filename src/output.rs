use serde::{Deserialize, Serialize};

/// Output format requested by the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Markdown,
    Json,
}

impl OutputFormat {
    pub fn is_json(self) -> bool {
        self == Self::Json
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonResponse {
    pub response: String,
    pub session_id: Option<String>,
    pub model_used: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl JsonResponse {
    pub fn new(response: String, session_id: Option<String>, model_used: Option<String>) -> Self {
        Self {
            response,
            session_id,
            model_used,
            timestamp: chrono::Utc::now(),
        }
    }
}
