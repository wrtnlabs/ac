use std::sync::Arc;

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::ctx::ToolCtx;

/// Coarse classification every tool must declare — the hook for read-only
/// permission modes. An unclassified tool cannot exist: it's part of the trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    ReadOnly,
    Mutating,
}

/// What a tool returns. Failures the model should see (bad input, policy
/// refusal, file not found) are `error(...)` — data, not `Err`. There is no
/// `Err` channel here by design; infrastructure failures belong to the runtime.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub trait Tool: Send + Sync + 'static {
    type Input: DeserializeOwned + JsonSchema + Send + 'static;

    fn name(&self) -> &'static str;
    fn description(&self) -> String;
    fn capability(&self) -> Capability;
    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput>;
}

/// A tool whose name, description, and input schema are only known at runtime —
/// the registration path for tools that arrive over a wire (MCP servers) rather
/// than being compiled in. Input reaches `run` as the model's raw JSON
/// arguments; validating it is the tool's own job (an MCP server validates
/// against the schema it advertised), and invalid input must come back as
/// [`ToolOutput::error`] data, never a panic.
pub trait RawTool: Send + Sync + 'static {
    fn spec(&self) -> ac_types::ToolSpec;
    fn capability(&self) -> Capability;
    fn run(
        self: Arc<Self>,
        input: serde_json::Value,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput>;
}
