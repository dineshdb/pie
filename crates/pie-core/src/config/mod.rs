mod loader;
mod resolver;
mod types;

pub use loader::{
    EMBEDDED_PIE_DIR, get_providers_data, load_config, load_launch_config, logs_dir, pie_home,
};
pub use resolver::{CliOverrides, ResolvedConfig, ResolvedProvider, build_sandbox};
use std::sync::OnceLock;
pub use types::{
    ApiErrorConfig, GlobalAgentConfig, LaunchConfig, McpAuthConfig, McpServerConfig, ModelPricing,
    PieConfig, ProviderConfig, RateLimitConfig, RetryConfig, ServerAgentConfig, ServerConfig,
};

pub static CONFIG: OnceLock<ResolvedConfig> = OnceLock::new();
