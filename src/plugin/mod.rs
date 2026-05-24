mod command;
mod debug;
mod developer;
mod doom_loop;
mod helper_binaries;
mod jewels;
mod permissions;
mod persistence;
mod shell;
mod system_prompts;
mod websearch;

pub use command::UserCommandPlugin;
pub use debug::DebugPlugin;
pub use developer::DeveloperPlugin;
pub use doom_loop::DoomLoopPlugin;
pub use helper_binaries::HelperBinariesPlugin;
pub use jewels::JewelsPlugin;
pub use permissions::{PermissionRequest, PermissionsPlugin};
pub use persistence::PersistencePlugin;
pub use shell::ShellPlugin;
pub use system_prompts::{
    EmbeddedSystemPromptPlugin, SystemPromptComponent, build_agentsmd_plugin,
};
pub use websearch::WebsearchPlugin;
