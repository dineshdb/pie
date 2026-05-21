use crate::hook::CommandHook;

#[derive(Debug, Clone)]
pub struct ExternalPlugin {
    pub name: String,
    pub hooks: Vec<CommandHook>,
}
