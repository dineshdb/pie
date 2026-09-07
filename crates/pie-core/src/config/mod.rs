mod loader;
mod resolver;
mod types;

pub use loader::{
    EMBEDDED_PIE_DIR, get_providers_data, load_config, load_launch_config, logs_dir, pie_home,
};
pub use resolver::{CliOverrides, ResolvedConfig, ResolvedProvider, build_sandbox};
use std::sync::OnceLock;
pub use types::{
    ApiErrorConfig, LaunchConfig, McpServerConfig, ModelPricing, PieConfig, ProviderConfig,
    RateLimitConfig, RetryConfig,
};

pub static CONFIG: OnceLock<ResolvedConfig> = OnceLock::new();
