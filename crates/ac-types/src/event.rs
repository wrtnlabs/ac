use serde::{Deserialize, Serialize};

use crate::content::ToolUse;

/// The unified provider-agnostic completion stream. Every wire crate maps its
/// native SSE events into this enum; nothing above the provider layer ever
/// sees a provider's own wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompletionEvent {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse(ToolUse),
    /// A source citation surfaced by a provider-executed server tool (e.g. web
    /// search). These arrive inline as annotations, not as tool results — there
    /// is no local execution and nothing to feed back — so they ride their own
    /// event rather than the tool-call path.
    Citation(Citation),
    UsageUpdate(TokenUsage),
    Stop(StopReason),
}

/// A cited source (URL + optional title) returned by a server-side tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Citation {
    pub url: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refusal,
}

/// Server-reported usage. This is the source of truth for token accounting —
/// never client-side tokenization.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}
