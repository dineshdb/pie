use crate::hook::CommandHook;

#[derive(Debug, Clone)]
pub struct StaticPlugin {
    pub name: String,
    pub hooks: Vec<CommandHook>,
}
