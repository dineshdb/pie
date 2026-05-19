use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Plugin error: {0}")]
    Plugin(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("UI error: {0}")]
    Ui(String),

    #[error("Database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("API error: {0}")]
    Api(Box<agentsdk::error::AgentSdkError>),

    #[error("Config parse error: {0}")]
    Figment(Box<figment::Error>),

    #[error("JSON error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),

    #[error("{0}")]
    Anyhow(#[from] anyhow::Error),
}

impl From<figment::Error> for AppError {
    fn from(e: figment::Error) -> Self {
        Self::Figment(Box::new(e))
    }
}

impl From<agentsdk::error::AgentSdkError> for AppError {
    fn from(e: agentsdk::error::AgentSdkError) -> Self {
        Self::Api(Box::new(e))
    }
}

pub type Result<T, E = AppError> = std::result::Result<T, E>;
