//! The Vercel AI SDK adapter: AC's turn stream ⇆ the AI SDK **UI Message
//! Stream Protocol** (v5). This is the seam that lets a stock `useChat` React
//! app render an AC agent with zero custom client code — the same shape any
//! host on the AI SDK's client half already speaks.
//!
//! It is the sibling of `ac-acp`: both are thin, mechanical adapters off the
//! one canonical [`AgentEvent`] stream. Neither is stacked on the other — a
//! host picks the wire its ecosystem wants (ACP for editors, this for React).
//!
//! Three protocol operations:
//! - **out** — [`ChunkEncoder`] turns each [`AgentEvent`] into one or more
//!   `UIMessageChunk`s (the SSE `data:` payloads `useChat` consumes),
//!   bracketing text/reasoning parts and mapping tool calls through the AI
//!   SDK's `tool-input-*` / `tool-output-*` lifecycle.
//! - **track / replay** — [`MessageStateTracker`] reduces those chunks to an
//!   in-flight assistant `UIMessage` and synthesizes an equivalent sequence
//!   for reconnecting or late subscribers.
//! - **in / hydrate** — [`hydrate_messages`] renders stored [`Message`]s back
//!   as `UIMessage`s so a resumed chat repaints, and [`user_text`] pulls the
//!   prompt text out of an incoming `UIMessage`.
//!
//! Transport-free on purpose (no axum, no HTTP): the mapping is reusable; the
//! demo host in `main.rs` supplies the SSE framing.

use ac_runtime::AgentEvent;
use ac_types::{ContentPart, Message, Role};
use serde_json::{Value, json};

mod message_state;

pub use message_state::MessageStateTracker;

/// The SSE terminator `useChat` expects after the last chunk.
pub const DONE: &str = "[DONE]";

/// Turns the flat [`AgentEvent`] stream into a valid `UIMessageChunk`
/// sequence. The AI SDK models an assistant message as *parts* with explicit
/// start/end brackets, so this carries the small amount of state that requires:
/// which text or reasoning part is currently open, and a per-part id counter.
///
/// Lifecycle per turn: [`start`](Self::start) once, then [`encode`](Self::encode)
/// per event, then [`finish`](Self::finish) once. Every method returns the
/// chunks to emit, in order.
pub struct ChunkEncoder {
    message_id: String,
    part_seq: u64,
    open: Option<OpenPart>,
    config: ChunkEncoderConfig,
    /// Tool call ids already announced via `tool-input-start` (the streaming
    /// path), so the assembled call doesn't emit a duplicate start.
    announced: std::collections::HashSet<String>,
}

/// How tool parts should be represented by an AI SDK consumer.
///
/// Dynamic parts use `dynamic-tool` and retain `toolName` in the projected
/// message. Static parts use `tool-<name>`, which lets a host register
/// type-specific UI for a known tool catalog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolPartStyle {
    #[default]
    Dynamic,
    Static,
}

/// Optional decoding applied to successful tool outputs before they are put
/// on the UI stream.
///
/// AC tool results are strings because that is the portable model-facing
/// representation. Some AI SDK hosts expose structured tool results to their
/// UI, so they can opt into decoding JSON objects and arrays at this adapter
/// boundary. Scalars and invalid JSON always remain strings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolOutputDecoding {
    #[default]
    String,
    JsonObjectsAndArrays,
}

/// Host-selectable protocol projection behavior.
///
/// The default preserves the adapter's original wire output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChunkEncoderConfig {
    pub tool_part_style: ToolPartStyle,
    pub tool_output_decoding: ToolOutputDecoding,
}

fn successful_tool_output(output: impl Into<String>, config: ChunkEncoderConfig) -> Value {
    let output = output.into();
    if config.tool_output_decoding == ToolOutputDecoding::JsonObjectsAndArrays
        && let Ok(parsed) = serde_json::from_str::<Value>(&output)
        && (parsed.is_object() || parsed.is_array())
    {
        return parsed;
    }
    Value::String(output)
}

#[derive(PartialEq)]
enum OpenPart {
    Text(String),
    Reasoning(String),
}

impl ChunkEncoder {
    pub fn new(message_id: impl Into<String>) -> Self {
        Self::with_config(message_id, ChunkEncoderConfig::default())
    }

    pub fn with_config(message_id: impl Into<String>, config: ChunkEncoderConfig) -> Self {
        Self {
            message_id: message_id.into(),
            part_seq: 0,
            open: None,
            config,
            announced: std::collections::HashSet::new(),
        }
    }

    fn dynamic_tools(&self) -> bool {
        self.config.tool_part_style == ToolPartStyle::Dynamic
    }

    fn successful_tool_output(&self, output: String) -> Value {
        successful_tool_output(output, self.config)
    }

    /// The opening chunks of a turn: `start` (naming the assistant message)
    /// then `start-step`.
    pub fn start(&self) -> Vec<Value> {
        vec![
            json!({ "type": "start", "messageId": self.message_id }),
            json!({ "type": "start-step" }),
        ]
    }

    fn next_part_id(&mut self) -> String {
        self.part_seq += 1;
        format!("{}-p{}", self.message_id, self.part_seq)
    }

    /// Close whatever text/reasoning part is open, if any.
    fn close_open(&mut self) -> Option<Value> {
        match self.open.take()? {
            OpenPart::Text(id) => Some(json!({ "type": "text-end", "id": id })),
            OpenPart::Reasoning(id) => Some(json!({ "type": "reasoning-end", "id": id })),
        }
    }

    pub fn encode(&mut self, event: AgentEvent) -> Vec<Value> {
        let mut out = Vec::new();
        match event {
            AgentEvent::Text(delta) => {
                let id = match &self.open {
                    Some(OpenPart::Text(id)) => id.clone(),
                    _ => {
                        if let Some(end) = self.close_open() {
                            out.push(end);
                        }
                        let id = self.next_part_id();
                        out.push(json!({ "type": "text-start", "id": id }));
                        self.open = Some(OpenPart::Text(id.clone()));
                        id
                    }
                };
                out.push(json!({ "type": "text-delta", "id": id, "delta": delta }));
            }
            AgentEvent::Thinking(delta) => {
                let id = match &self.open {
                    Some(OpenPart::Reasoning(id)) => id.clone(),
                    _ => {
                        if let Some(end) = self.close_open() {
                            out.push(end);
                        }
                        let id = self.next_part_id();
                        out.push(json!({ "type": "reasoning-start", "id": id }));
                        self.open = Some(OpenPart::Reasoning(id.clone()));
                        id
                    }
                };
                out.push(json!({ "type": "reasoning-delta", "id": id, "delta": delta }));
            }
            AgentEvent::ToolInputDelta { id, name, delta } => {
                if let Some(end) = self.close_open() {
                    out.push(end);
                }
                if self.announced.insert(id.clone()) {
                    let mut chunk = json!({
                        "type": "tool-input-start",
                        "toolCallId": id,
                        "toolName": name,
                    });
                    if self.dynamic_tools() {
                        chunk["dynamic"] = json!(true);
                    }
                    out.push(chunk);
                }
                out.push(json!({
                    "type": "tool-input-delta",
                    "toolCallId": id,
                    "inputTextDelta": delta,
                }));
            }
            AgentEvent::ToolCall { id, name, input } => {
                if let Some(end) = self.close_open() {
                    out.push(end);
                }
                // Streamed deltas already announced this call; only the start
                // is skippable — the available chunk is always authoritative.
                if !self.announced.contains(&id) {
                    let mut chunk = json!({
                        "type": "tool-input-start",
                        "toolCallId": id,
                        "toolName": name,
                    });
                    if self.dynamic_tools() {
                        chunk["dynamic"] = json!(true);
                    }
                    out.push(chunk);
                }
                let mut chunk = json!({
                    "type": "tool-input-available",
                    "toolCallId": id,
                    "toolName": name,
                    "input": input,
                });
                if self.dynamic_tools() {
                    chunk["dynamic"] = json!(true);
                }
                out.push(chunk);
            }
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
                ..
            } => {
                if is_error {
                    let mut chunk = json!({
                        "type": "tool-output-error",
                        "toolCallId": id,
                        "errorText": output,
                    });
                    if self.dynamic_tools() {
                        chunk["dynamic"] = json!(true);
                    }
                    out.push(chunk);
                } else {
                    let output = self.successful_tool_output(output);
                    let mut chunk = json!({
                        "type": "tool-output-available",
                        "toolCallId": id,
                        "output": output,
                    });
                    if self.dynamic_tools() {
                        chunk["dynamic"] = json!(true);
                    }
                    out.push(chunk);
                }
            }
            AgentEvent::Citation { url, title } => {
                // Sources are their own parts; they don't disturb an open text
                // part (the AI SDK reconciles parts by id).
                let source_id = self.next_part_id();
                let mut chunk = json!({
                    "type": "source-url",
                    "sourceId": source_id,
                    "url": url,
                });
                if let Some(title) = title {
                    chunk["title"] = json!(title);
                }
                out.push(chunk);
            }
            AgentEvent::Usage(usage) => {
                out.push(json!({
                    "type": "message-metadata",
                    "messageMetadata": {
                        "usage": {
                            "inputTokens": usage.input_tokens,
                            "outputTokens": usage.output_tokens,
                            "cacheReadTokens": usage.cache_read_input_tokens,
                            "cacheCreationTokens": usage.cache_creation_input_tokens,
                        }
                    }
                }));
            }
            // A context compaction is a transient notification, not part of the
            // message the client keeps — it rides a `data-*` part with
            // `transient: true` so a client can surface "context compacted"
            // without it entering the persisted message.
            AgentEvent::Compacted {
                trigger,
                tokens_before,
                tokens_after,
                ..
            } => {
                out.push(json!({
                    "type": "data-compaction",
                    "data": {
                        "trigger": trigger,
                        "tokensBefore": tokens_before,
                        "tokensAfter": tokens_after,
                    },
                    "transient": true,
                }));
            }
            // The turn outcome rides finish(); a mid-turn error becomes an
            // error chunk so the client surfaces it. Close any open part first
            // so the stream stays well-formed.
            AgentEvent::Error(message) => {
                if let Some(end) = self.close_open() {
                    out.push(end);
                }
                out.push(json!({ "type": "error", "errorText": message }));
            }
            AgentEvent::TurnComplete { .. } => {}
        }
        out
    }

    /// The closing chunks: end any open part, then `finish-step` + `finish`.
    /// The host emits the `[DONE]` terminator after these.
    pub fn finish(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(end) = self.close_open() {
            out.push(end);
        }
        out.push(json!({ "type": "finish-step" }));
        out.push(json!({ "type": "finish" }));
        out
    }

    /// A standalone error terminator for a failed turn (transport/runtime
    /// failure that never produced a normal finish).
    pub fn error(&mut self, message: impl Into<String>) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(end) = self.close_open() {
            out.push(end);
        }
        out.push(json!({ "type": "error", "errorText": message.into() }));
        out
    }
}

/// Render stored history with the default projection for a resumed chat.
pub fn hydrate_messages(history: &[Message]) -> Vec<Value> {
    hydrate_messages_with_config(history, ChunkEncoderConfig::default())
}

/// Render stored history with the same host-selected projection used for live
/// chunks. Tool calls are paired with results from following messages. System
/// messages are dropped because the host owns the system prompt.
pub fn hydrate_messages_with_config(history: &[Message], config: ChunkEncoderConfig) -> Vec<Value> {
    // First pass: collect tool outputs by id so an assistant tool-use part can
    // carry its result.
    let mut outputs: std::collections::HashMap<&str, (&str, bool)> =
        std::collections::HashMap::new();
    for message in history {
        for part in &message.content {
            if let ContentPart::ToolResult(result) = part {
                outputs.insert(
                    result.tool_use_id.as_str(),
                    (result.content.as_str(), result.is_error),
                );
            }
        }
    }

    let mut messages = Vec::new();
    for (index, message) in history.iter().enumerate() {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => continue,
        };
        let mut parts = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text { text } => parts.push(json!({ "type": "text", "text": text })),
                ContentPart::Thinking { text, .. } => {
                    parts.push(json!({ "type": "reasoning", "text": text }))
                }
                ContentPart::ToolUse(tool_use) => {
                    let dynamic = config.tool_part_style == ToolPartStyle::Dynamic;
                    let mut tool = json!({
                        "type": if dynamic {
                            "dynamic-tool".to_string()
                        } else {
                            format!("tool-{}", tool_use.name)
                        },
                        "toolCallId": tool_use.id,
                        "input": tool_use.input,
                    });
                    if dynamic {
                        tool["toolName"] = json!(tool_use.name);
                    }
                    match outputs.get(tool_use.id.as_str()) {
                        Some((output, true)) => {
                            tool["state"] = json!("output-error");
                            tool["errorText"] = json!(output);
                        }
                        Some((output, false)) => {
                            tool["state"] = json!("output-available");
                            tool["output"] = successful_tool_output(*output, config);
                        }
                        None => tool["state"] = json!("input-available"),
                    }
                    parts.push(tool);
                }
                // Tool results are folded into the assistant's tool part above;
                // don't also render them as a standalone user message.
                ContentPart::ToolResult(_) => {}
                ContentPart::RedactedThinking { .. } | ContentPart::Image { .. } => {}
            }
        }
        // A message that was only tool results contributes no visible parts.
        if parts.is_empty() {
            continue;
        }
        messages.push(json!({
            "id": format!("hist-{index}"),
            "role": role,
            "parts": parts,
        }));
    }
    messages
}

/// Extract the prompt text from an incoming `UIMessage` (the last message a
/// `useChat` send posts). Concatenates its `text` parts.
pub fn user_text(message: &Value) -> String {
    let mut text = String::new();
    if let Some(parts) = message.get("parts").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = part.get("text").and_then(Value::as_str)
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::{CacheMark, TokenUsage, ToolResult, ToolUse};

    fn types(chunks: &[Value]) -> Vec<String> {
        chunks
            .iter()
            .map(|c| c["type"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn text_then_tool_then_text_brackets_parts_correctly() {
        let mut enc = ChunkEncoder::new("m1");
        let mut all = enc.start();
        all.extend(enc.encode(AgentEvent::Text("Hel".into())));
        all.extend(enc.encode(AgentEvent::Text("lo".into())));
        all.extend(enc.encode(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            input: json!({ "path": "a.txt" }),
        }));
        all.extend(enc.encode(AgentEvent::ToolResult {
            id: "c1".into(),
            name: "read_file".into(),
            output: "contents".into(),
            is_error: false,
        }));
        all.extend(enc.encode(AgentEvent::Text("done".into())));
        all.extend(enc.finish());

        assert_eq!(
            types(&all),
            vec![
                "start",
                "start-step",
                "text-start",
                "text-delta",
                "text-delta",
                "text-end", // closed when the tool call arrives
                "tool-input-start",
                "tool-input-available",
                "tool-output-available",
                "text-start", // a fresh text part after the tool
                "text-delta",
                "text-end", // closed by finish
                "finish-step",
                "finish",
            ]
        );
        // The two "Hel"+"lo" deltas share one text part id; the post-tool text
        // is a new part.
        let text_ids: Vec<_> = all
            .iter()
            .filter(|c| c["type"] == "text-delta")
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(text_ids[0], text_ids[1]);
        assert_ne!(text_ids[0], text_ids[2]);
    }

    #[test]
    fn streamed_tool_input_announces_once_then_deltas_then_available() {
        let mut enc = ChunkEncoder::new("m1");
        let mut all = enc.start();
        all.extend(enc.encode(AgentEvent::Text("thinking".into())));
        all.extend(enc.encode(AgentEvent::ToolInputDelta {
            id: "c1".into(),
            name: "read_file".into(),
            delta: "{\"pa".into(),
        }));
        all.extend(enc.encode(AgentEvent::ToolInputDelta {
            id: "c1".into(),
            name: "read_file".into(),
            delta: "th\":\"a.txt\"}".into(),
        }));
        all.extend(enc.encode(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            input: json!({ "path": "a.txt" }),
        }));
        all.extend(enc.finish());

        assert_eq!(
            types(&all),
            vec![
                "start",
                "start-step",
                "text-start",
                "text-delta",
                "text-end", // closed when the first delta arrives
                "tool-input-start",
                "tool-input-delta",
                "tool-input-delta",
                "tool-input-available", // no duplicate tool-input-start
                "finish-step",
                "finish",
            ]
        );
        let start = all
            .iter()
            .find(|c| c["type"] == "tool-input-start")
            .unwrap();
        assert_eq!(start["toolCallId"], "c1");
        assert_eq!(start["toolName"], "read_file");
        let deltas: Vec<_> = all
            .iter()
            .filter(|c| c["type"] == "tool-input-delta")
            .collect();
        assert!(deltas.iter().all(|c| c["toolCallId"] == "c1"));
        let concat: String = deltas
            .iter()
            .map(|c| c["inputTextDelta"].as_str().unwrap())
            .collect();
        assert_eq!(concat, r#"{"path":"a.txt"}"#);
        let available = all
            .iter()
            .find(|c| c["type"] == "tool-input-available")
            .unwrap();
        assert_eq!(available["input"], json!({ "path": "a.txt" }));
    }

    #[test]
    fn tool_call_without_prior_deltas_still_emits_start_and_available() {
        // The no-streaming path (e.g. a mock provider) is unchanged: the
        // assembled call announces itself.
        let mut enc = ChunkEncoder::new("m1");
        let chunks = enc.encode(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            input: json!({ "path": "a.txt" }),
        });
        assert_eq!(
            types(&chunks),
            vec!["tool-input-start", "tool-input-available"]
        );
    }

    #[test]
    fn defaults_keep_dynamic_parts_and_string_outputs() {
        let mut enc = ChunkEncoder::new("m1");
        let call = enc.encode(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "lookup".into(),
            input: json!({ "query": "x" }),
        });
        assert_eq!(call[0]["dynamic"], true);
        assert_eq!(call[1]["dynamic"], true);

        let result = enc.encode(AgentEvent::ToolResult {
            id: "c1".into(),
            name: "lookup".into(),
            output: r#"{"value":1}"#.into(),
            is_error: false,
        });
        assert_eq!(result[0]["dynamic"], true);
        assert_eq!(result[0]["output"], r#"{"value":1}"#);
    }

    #[test]
    fn static_structured_projection_is_host_selectable() {
        let mut enc = ChunkEncoder::with_config(
            "m1",
            ChunkEncoderConfig {
                tool_part_style: ToolPartStyle::Static,
                tool_output_decoding: ToolOutputDecoding::JsonObjectsAndArrays,
            },
        );
        let call = enc.encode(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "lookup".into(),
            input: json!({ "query": "x" }),
        });
        assert!(call.iter().all(|chunk| chunk.get("dynamic").is_none()));

        let object = enc.encode(AgentEvent::ToolResult {
            id: "c1".into(),
            name: "lookup".into(),
            output: r#"{"value":1}"#.into(),
            is_error: false,
        });
        assert_eq!(object[0]["output"], json!({ "value": 1 }));
        assert!(object[0].get("dynamic").is_none());

        let array = enc.encode(AgentEvent::ToolResult {
            id: "c2".into(),
            name: "lookup".into(),
            output: "[1,2]".into(),
            is_error: false,
        });
        assert_eq!(array[0]["output"], json!([1, 2]));

        for output in ["42", "true", "\"text\"", "not-json"] {
            let result = enc.encode(AgentEvent::ToolResult {
                id: "c3".into(),
                name: "lookup".into(),
                output: output.into(),
                is_error: false,
            });
            assert_eq!(result[0]["output"], output);
        }
    }

    #[test]
    fn configured_encoder_and_tracker_form_one_projection_pipeline() {
        let mut enc = ChunkEncoder::with_config(
            "m1",
            ChunkEncoderConfig {
                tool_part_style: ToolPartStyle::Static,
                tool_output_decoding: ToolOutputDecoding::JsonObjectsAndArrays,
            },
        );
        let mut tracker = MessageStateTracker::new("m1");
        for chunk in enc
            .encode(AgentEvent::ToolCall {
                id: "c1".into(),
                name: "lookup".into(),
                input: json!({ "query": "x" }),
            })
            .into_iter()
            .chain(enc.encode(AgentEvent::ToolResult {
                id: "c1".into(),
                name: "lookup".into(),
                output: r#"{"value":1}"#.into(),
                is_error: false,
            }))
        {
            tracker.apply(&chunk);
        }
        assert_eq!(tracker.snapshot()["parts"][0]["type"], "tool-lookup");
        assert_eq!(
            tracker.snapshot()["parts"][0]["output"],
            json!({ "value": 1 })
        );
    }

    #[test]
    fn tool_error_and_usage_and_citation_map() {
        let mut enc = ChunkEncoder::new("m1");
        let err = enc.encode(AgentEvent::ToolResult {
            id: "c1".into(),
            name: "shell".into(),
            output: "boom".into(),
            is_error: true,
        });
        assert_eq!(err[0]["type"], "tool-output-error");
        assert_eq!(err[0]["errorText"], "boom");

        let usage = enc.encode(AgentEvent::Usage(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }));
        assert_eq!(usage[0]["type"], "message-metadata");
        assert_eq!(usage[0]["messageMetadata"]["usage"]["inputTokens"], 10);

        let cite = enc.encode(AgentEvent::Citation {
            url: "https://example.com".into(),
            title: Some("Example".into()),
        });
        assert_eq!(cite[0]["type"], "source-url");
        assert_eq!(cite[0]["url"], "https://example.com");
        assert_eq!(cite[0]["title"], "Example");
    }

    #[test]
    fn a_runtime_error_event_becomes_an_error_chunk() {
        // The host forwards run_turn's Err as AgentEvent::Error; it must reach
        // the client as an `error` chunk (with an open text part closed first),
        // not vanish into a clean finish.
        let mut enc = ChunkEncoder::new("m1");
        let _ = enc.encode(AgentEvent::Text("partial".into()));
        let chunks = enc.encode(AgentEvent::Error("provider 429".into()));
        assert_eq!(types(&chunks), vec!["text-end", "error"]);
        assert_eq!(chunks[1]["errorText"], "provider 429");
    }

    #[test]
    fn hydrate_pairs_tool_use_with_its_result() {
        let history = vec![
            Message::text(Role::User, "read it"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text {
                        text: "on it".into(),
                    },
                    ContentPart::ToolUse(ToolUse {
                        id: "c1".into(),
                        name: "read_file".into(),
                        input: json!({ "path": "a.txt" }),
                    }),
                ],
                cache: CacheMark::Off,
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::ToolResult(ToolResult {
                    tool_use_id: "c1".into(),
                    content: "hello".into(),
                    is_error: false,
                })],
                cache: CacheMark::Off,
            },
        ];
        let ui = hydrate_messages(&history);
        // The tool-result-only user message collapses; two visible messages.
        assert_eq!(ui.len(), 2);
        assert_eq!(ui[0]["role"], "user");
        assert_eq!(ui[0]["parts"][0]["text"], "read it");
        assert_eq!(ui[1]["role"], "assistant");
        let tool = &ui[1]["parts"][1];
        assert_eq!(tool["type"], "dynamic-tool");
        assert_eq!(tool["toolCallId"], "c1");
        assert_eq!(tool["state"], "output-available");
        assert_eq!(tool["output"], "hello");
    }

    #[test]
    fn configured_hydration_matches_live_tool_projection() {
        let history = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse(ToolUse {
                    id: "c1".into(),
                    name: "lookup".into(),
                    input: json!({ "query": "x" }),
                })],
                cache: CacheMark::Off,
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::ToolResult(ToolResult {
                    tool_use_id: "c1".into(),
                    content: r#"{"value":1}"#.into(),
                    is_error: false,
                })],
                cache: CacheMark::Off,
            },
        ];
        let ui = hydrate_messages_with_config(
            &history,
            ChunkEncoderConfig {
                tool_part_style: ToolPartStyle::Static,
                tool_output_decoding: ToolOutputDecoding::JsonObjectsAndArrays,
            },
        );
        let tool = &ui[0]["parts"][0];
        assert_eq!(tool["type"], "tool-lookup");
        assert!(tool.get("toolName").is_none());
        assert_eq!(tool["state"], "output-available");
        assert_eq!(tool["output"], json!({ "value": 1 }));
    }

    #[test]
    fn user_text_concatenates_text_parts() {
        let message = json!({
            "role": "user",
            "parts": [
                { "type": "text", "text": "line one" },
                { "type": "step-start" },
                { "type": "text", "text": "line two" },
            ],
        });
        assert_eq!(user_text(&message), "line one\nline two");
    }
}
