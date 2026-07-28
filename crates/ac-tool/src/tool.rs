use std::sync::Arc;

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::ctx::ToolCtx;

/// Coarse effect classification every tool must declare.
///
/// [`Guarded`](Self::Guarded) names a tool whose operation may mutate in a
/// normal run but whose host policy can collapse it to read-only-safe effects
/// (the canonical example is a shell whose kernel sandbox removes workspace
/// write roots). An unclassified tool cannot exist: capability is part of both
/// tool traits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    ReadOnly,
    Guarded,
    Mutating,
}

impl Capability {
    /// Whether a tool with this capability may remain visible in a read-only
    /// run. Guarded tools survive because the host's runtime policy, rather
    /// than a name table, constrains their effects.
    pub const fn allowed_in_read_only(self) -> bool {
        matches!(self, Self::ReadOnly | Self::Guarded)
    }
}

/// What a tool returns. Failures the model should see (bad input, policy
/// refusal, file not found) are `error(...)` — data, not `Err`. There is no
/// `Err` channel here by design; infrastructure failures belong to the runtime.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// The result visible to the current turn and emitted to live observers.
    pub content: String,
    pub is_error: bool,
    /// Optional small fallback recorded in durable history instead of
    /// [`content`](Self::content). This is for results whose useful live form
    /// depends on transient parts (for example an image): a resumed session
    /// keeps a truthful placeholder without persisting the transient payload.
    /// `None` means `content` itself is durable. If history-derived control
    /// logic reads this result (forced chains, conditional tool reveal, and
    /// similar hooks), both strings must preserve the same control facts:
    /// current-turn hooks see the live projection while resume/fork see this
    /// fallback.
    pub durable_content: Option<String>,
    /// Non-persistent content made available to the next model step in this
    /// turn. The runtime never writes these parts to the rollout.
    pub transient_parts: Vec<ToolOutputPart>,
}

/// A live-only content part returned by a tool.
///
/// Payloads use `Arc` so a host can share one base64 allocation with its own
/// live-preview cache. Converting to a provider request necessarily materializes
/// the provider-facing [`ac_types::ContentPart`], but no extra copy is required
/// when the tool first hands the payload to the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutputPart {
    Image { media_type: String, data: Arc<str> },
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            durable_content: None,
            transient_parts: Vec::new(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            durable_content: None,
            transient_parts: Vec::new(),
        }
    }

    /// Record `content` for live use, but persist this fallback in its place.
    pub fn with_durable_content(mut self, content: impl Into<String>) -> Self {
        self.durable_content = Some(content.into());
        self
    }

    /// Add one live-only image for the next model step.
    pub fn with_image(mut self, media_type: impl Into<String>, data: impl Into<Arc<str>>) -> Self {
        self.transient_parts.push(ToolOutputPart::Image {
            media_type: media_type.into(),
            data: data.into(),
        });
        self
    }

    /// The string safe to record in a rollout or host persistence layer.
    pub fn durable_content(&self) -> &str {
        self.durable_content
            .as_deref()
            .unwrap_or(self.content.as_str())
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
