use serde::{Deserialize, Serialize};

/// Output format requested by the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Markdown,
    Json,
}

impl OutputFormat {
    /// System instructions for the model based on the output format.
    pub fn to_instructions(self) -> &'static str {
        match self {
            Self::Markdown => "",
            Self::Json => {
                r#"
## JSON Output Mode

The user has requested JSON output. You MUST follow these rules:

- Respond with ONLY valid JSON. No markdown fences, no preamble, no commentary.
- The response must be a JSON object with this schema:
  { "response": "<your answer here>" }
- Do NOT wrap the JSON in ```json``` code blocks.
- Keep the response value as plain text — no nested JSON, no markdown within the value.
- If the answer naturally involves structured data, put it all inside the "response" string value.
"#
            }
        }
    }

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
