use serde::{Deserialize, Serialize};

use crate::content::ToolUse;

/// The unified provider-agnostic completion stream. Every wire crate maps its
/// native SSE events into this enum; nothing above the provider layer ever
/// sees a provider's own wire format.
///
/// Adjacently tagged (`{"type": …, "data": …}`) — internal tagging cannot
/// represent newtype variants of primitives (`Text(String)` would fail at
/// runtime, not compile time). The tag layout is public surface; change it
/// deliberately.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
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
///
/// Field contract (wire crates must normalize to this): `input_tokens` is the
/// TOTAL prompt-side count, and the `cache_*` fields are breakdowns (subsets)
/// of it — never additional tokens. Context occupancy is therefore
/// `input_tokens + output_tokens`; adding the cache fields on top double
/// counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag layout is public surface: every variant must round-trip.
    /// (Internal tagging silently fails on newtype-of-primitive variants at
    /// RUNTIME — this test is what makes that class of regression loud.)
    #[test]
    fn every_completion_event_variant_round_trips() {
        let events = vec![
            CompletionEvent::Text("hi".into()),
            CompletionEvent::Thinking {
                text: "hm".into(),
                signature: Some("sig".into()),
            },
            CompletionEvent::ToolUse(ToolUse {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a.txt" }),
            }),
            CompletionEvent::Citation(Citation {
                url: "https://example.com".into(),
                title: None,
            }),
            CompletionEvent::UsageUpdate(TokenUsage::default()),
            CompletionEvent::Stop(StopReason::EndTurn),
        ];
        for event in events {
            let json = serde_json::to_string(&event)
                .unwrap_or_else(|e| panic!("serialize {event:?}: {e}"));
            let back: CompletionEvent =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("deserialize {json}: {e}"));
            assert_eq!(
                std::mem::discriminant(&event),
                std::mem::discriminant(&back)
            );
        }
    }
}
