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
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl CompletionRequest {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            cache_system: false,
            messages: Vec::new(),
            tools: Vec::new(),
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
}
