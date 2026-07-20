//! The provider abstraction: one trait, one required streaming entry point.
//! Wire crates (ac-provider-openrouter, …) implement [`Provider`] and map
//! their native wire events into [`ac_types::CompletionEvent`].

use ac_types::{CompletionError, CompletionEvent, Message, ToolSpec};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

pub type EventStream = BoxStream<'static, Result<CompletionEvent, CompletionError>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    /// System prompt. Kept separate from `messages` so wire crates can apply
    /// provider-specific placement and cache marking.
    pub system: Option<String>,
    /// Cache-break after the system prompt (Anthropic `cache_control`).
    #[serde(default)]
    pub cache_system: bool,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Provider-executed server tools requested for this call (e.g. web search).
    /// These are *intent*, expressed provider-agnostically; each wire crate
    /// encodes the ones it supports and ignores the rest. They are NOT local
    /// tools — there is no `run` and they never enter the tool registry.
    #[serde(default)]
    pub server_tools: Vec<ServerTool>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// A capability the provider runs on its own infrastructure, surfacing results
/// inline (as [`ac_types::Citation`]) rather than as tool results the runtime
/// executes. Provider-agnostic by design — the kit names the capability, the
/// wire crate maps it to whatever the provider's API calls it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerTool {
    WebSearch { max_results: Option<u32> },
}

/// How the model may use tools this request. `Force` names a tool the model
/// must call — the mechanism a step hook uses to pin a forced step.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Force(String),
}

impl CompletionRequest {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            cache_system: false,
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            server_tools: Vec::new(),
            max_tokens: None,
            temperature: None,
        }
    }
}

pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// The one required entry point. Returns once the request is accepted;
    /// the stream then yields the unified event sequence, ending with
    /// `CompletionEvent::Stop`.
    fn stream_completion(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'static, Result<EventStream, CompletionError>>;

    /// Whether this provider can execute the given server tool. Hosts query
    /// this before requesting one; the default is "supports nothing", so a
    /// provider opts in by overriding. Requesting an unsupported server tool is
    /// silently ignored by the wire crate, never an error.
    fn supports_server_tool(&self, _tool: &ServerTool) -> bool {
        false
    }
}
