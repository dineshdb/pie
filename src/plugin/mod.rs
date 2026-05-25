mod command;
mod debug;
mod doom_loop;
mod helper_binaries;
mod permissions;
mod persistence;
mod system_prompts;
mod websearch;

pub use command::UserCommandPlugin;
pub use debug::DebugPlugin;
pub use doom_loop::DoomLoopPlugin;
pub use helper_binaries::HelperBinariesPlugin;
pub use permissions::{PermissionRequest, PermissionsPlugin};
pub use persistence::PersistencePlugin;
pub use system_prompts::{
    EmbeddedSystemPromptPlugin, SystemPromptComponent, build_agentsmd_plugin,
};
pub use websearch::WebsearchPlugin;
