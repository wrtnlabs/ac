//! The `load_skill` built-in: the model-facing surface of [`SkillsResolver`].

use std::sync::Arc;

use ac_tool::{Capability, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use serde::Deserialize;

use crate::resolver::{LoadError, SkillsResolver};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LoadSkillInput {
    /// Name of the skill to load, exactly as it appears in the skills list.
    pub name: String,
}

/// Loads a skill's instructions into context. Read-only: skills are data the
/// host laid out on disk, never something the model writes.
pub struct LoadSkillTool {
    resolver: Arc<SkillsResolver>,
}

impl LoadSkillTool {
    pub fn new(resolver: Arc<SkillsResolver>) -> Self {
        Self { resolver }
    }
}

impl Tool for LoadSkillTool {
    type Input = LoadSkillInput;

    fn name(&self) -> &'static str {
        "load_skill"
    }

    fn description(&self) -> String {
        "Load a skill by name and return its full instructions. Skills are named \
         packs of instructions; load one when its listed description matches the \
         current task, then follow what it says."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            match self.resolver.load(&input.name) {
                Ok(body) => ToolOutput::ok(body),
                Err(LoadError::UnknownSkill(name)) => {
                    let names: Vec<String> = self
                        .resolver
                        .list()
                        .skills
                        .into_iter()
                        .map(|s| s.name)
                        .collect();
                    if names.is_empty() {
                        ToolOutput::error(format!(
                            "unknown skill: {name}. No skills are available."
                        ))
                    } else {
                        ToolOutput::error(format!(
                            "unknown skill: {name}. Available skills: {}",
                            names.join(", ")
                        ))
                    }
                }
                Err(e) => ToolOutput::error(e.to_string()),
            }
        })
    }
}
