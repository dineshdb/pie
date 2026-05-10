use crate::{hook::Hook, plugin::Plugin};
use std::sync::Arc;

#[derive(Debug)]
pub struct StaticPlugin {
    pub name: String,
    pub hooks: Vec<Arc<dyn Hook>>,
}

impl Plugin for StaticPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn hooks(&self) -> &[Arc<dyn Hook>] {
        &self.hooks
    }
}
