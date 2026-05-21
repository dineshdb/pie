use crate::hook::CommandHook;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ExternalPlugin {
    pub name: String,
    pub hooks: Vec<CommandHook>,
}
