//! Reduction and replay of AI SDK UI message stream chunks.
//!
//! [`MessageStateTracker`] keeps the current assistant `UIMessage` projection
//! while a stream is in flight. Its synthesized chunk sequence can be applied
//! to a fresh tracker to recover the same state, which makes it suitable for
//! reconnect and late-subscriber flows without coupling persistence policy to
//! this protocol adapter.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

fn present(src: &Value, key: &str) -> Option<Value> {
    src.get(key).cloned()
}

fn non_null(src: &Value, key: &str) -> Option<Value> {
    src.get(key).filter(|value| !value.is_null()).cloned()
}

fn insert_non_null(dst: &mut Map<String, Value>, src: &Value, key: &str) {
    if let Some(value) = non_null(src, key) {
        dst.insert(key.to_string(), value);
    }
}

fn copy_key(dst: &mut Map<String, Value>, src: &Value, key: &str) {
    if let Some(value) = src.get(key) {
        dst.insert(key.to_string(), value.clone());
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(_) => true,
    }
}

fn part_type(part: &Value) -> &str {
    part.get("type").and_then(Value::as_str).unwrap_or("")
}

fn is_tool_part(part: &Value) -> bool {
    let kind = part_type(part);
    kind.starts_with("tool-") || kind == "dynamic-tool"
}

fn is_dynamic_tool(part: &Value) -> bool {
    part_type(part) == "dynamic-tool"
}

fn tool_name(part: &Value) -> String {
    if is_dynamic_tool(part) {
        part.get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        part_type(part)
            .strip_prefix("tool-")
            .unwrap_or("")
            .to_string()
    }
}

fn error_text(part: &Value) -> Value {
    match part.get("errorText") {
        None | Some(Value::Null) => json!(""),
        Some(value) => value.clone(),
    }
}

struct PartialToolCall {
    text: String,
}

#[derive(Default)]
struct ToolUpsert {
    tool_name: String,
    dynamic: bool,
    state: &'static str,
    input: Option<Value>,
    output: Option<Value>,
    raw_input: Option<Value>,
    error_text: Option<Value>,
    title: Option<Value>,
    tool_metadata: Option<Value>,
    provider_executed: Option<Value>,
    call_provider_metadata: Option<Value>,
    result_provider_metadata: Option<Value>,
    preliminary: Option<Value>,
}

/// Reduces AI SDK UI message stream chunks into one assistant message.
///
/// Unknown chunks and chunks that refer to unknown part ids are ignored.
/// This tolerance lets old consumers follow streams containing newer chunk
/// types. Transient `data-*` chunks are deliberately not retained.
pub struct MessageStateTracker {
    message_id: String,
    metadata: Option<Value>,
    parts: Vec<Value>,
    active_text_ids: HashMap<String, usize>,
    active_reasoning_ids: HashMap<String, usize>,
    partial_tool_calls: HashMap<String, PartialToolCall>,
    tool_part_index: HashMap<String, usize>,
    approvals_by_tool_call: HashMap<String, String>,
    finish_observed: bool,
    finish_reason: Option<Value>,
}

impl MessageStateTracker {
    pub fn new(message_id: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            metadata: None,
            parts: Vec::new(),
            active_text_ids: HashMap::new(),
            active_reasoning_ids: HashMap::new(),
            partial_tool_calls: HashMap::new(),
            tool_part_index: HashMap::new(),
            approvals_by_tool_call: HashMap::new(),
            finish_observed: false,
            finish_reason: None,
        }
    }

    /// Apply one AI SDK `UIMessageChunk`.
    pub fn apply(&mut self, chunk: &Value) {
        let Some(chunk_type) = chunk.get("type").and_then(Value::as_str) else {
            return;
        };
        match chunk_type {
            "start" => {
                if let Some(id) = chunk.get("messageId").and_then(Value::as_str)
                    && !id.is_empty()
                {
                    self.message_id = id.to_string();
                }
                if let Some(metadata) = present(chunk, "messageMetadata") {
                    self.metadata = Some(metadata);
                }
            }
            "finish" => {
                self.finish_observed = true;
                if let Some(reason) = non_null(chunk, "finishReason") {
                    self.finish_reason = Some(reason);
                }
                if let Some(metadata) = present(chunk, "messageMetadata") {
                    self.metadata = Some(metadata);
                }
            }
            "message-metadata" => self.metadata = present(chunk, "messageMetadata"),
            "start-step" => self.parts.push(json!({ "type": "step-start" })),
            "finish-step" | "abort" | "error" => {}
            "text-start" => self.stream_start(chunk, "text"),
            "text-delta" => self.stream_delta(chunk, "text"),
            "text-end" => self.stream_end(chunk, "text"),
            "reasoning-start" => self.stream_start(chunk, "reasoning"),
            "reasoning-delta" => self.stream_delta(chunk, "reasoning"),
            "reasoning-end" => self.stream_end(chunk, "reasoning"),
            "file" => {
                let mut part = Map::new();
                part.insert("type".to_string(), json!("file"));
                copy_key(&mut part, chunk, "mediaType");
                copy_key(&mut part, chunk, "url");
                insert_non_null(&mut part, chunk, "providerMetadata");
                self.parts.push(Value::Object(part));
            }
            "source-url" => {
                let mut part = Map::new();
                part.insert("type".to_string(), json!("source-url"));
                copy_key(&mut part, chunk, "sourceId");
                copy_key(&mut part, chunk, "url");
                insert_non_null(&mut part, chunk, "title");
                insert_non_null(&mut part, chunk, "providerMetadata");
                self.parts.push(Value::Object(part));
            }
            "source-document" => {
                let mut part = Map::new();
                part.insert("type".to_string(), json!("source-document"));
                copy_key(&mut part, chunk, "sourceId");
                copy_key(&mut part, chunk, "mediaType");
                copy_key(&mut part, chunk, "title");
                insert_non_null(&mut part, chunk, "filename");
                insert_non_null(&mut part, chunk, "providerMetadata");
                self.parts.push(Value::Object(part));
            }
            "tool-input-start" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let dynamic = chunk.get("dynamic") == Some(&Value::Bool(true));
                let name = chunk
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.partial_tool_calls.insert(
                    tool_call_id.to_string(),
                    PartialToolCall {
                        text: String::new(),
                    },
                );
                self.upsert_tool_part(
                    tool_call_id,
                    ToolUpsert {
                        tool_name: name,
                        dynamic,
                        state: "input-streaming",
                        title: present(chunk, "title"),
                        tool_metadata: present(chunk, "toolMetadata"),
                        provider_executed: present(chunk, "providerExecuted"),
                        call_provider_metadata: present(chunk, "providerMetadata"),
                        ..ToolUpsert::default()
                    },
                );
            }
            "tool-input-delta" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let Some(partial) = self.partial_tool_calls.get_mut(tool_call_id) else {
                    return;
                };
                partial.text.push_str(
                    chunk
                        .get("inputTextDelta")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
            }
            "tool-input-available" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                self.upsert_tool_part(
                    tool_call_id,
                    ToolUpsert {
                        tool_name: chunk
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        dynamic: chunk.get("dynamic") == Some(&Value::Bool(true)),
                        state: "input-available",
                        input: present(chunk, "input"),
                        title: present(chunk, "title"),
                        tool_metadata: present(chunk, "toolMetadata"),
                        provider_executed: present(chunk, "providerExecuted"),
                        call_provider_metadata: present(chunk, "providerMetadata"),
                        ..ToolUpsert::default()
                    },
                );
                if let Some(partial) = self.partial_tool_calls.get_mut(tool_call_id) {
                    partial.text.clear();
                }
            }
            "tool-input-error" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                self.upsert_tool_part(
                    tool_call_id,
                    ToolUpsert {
                        tool_name: chunk
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        dynamic: chunk.get("dynamic") == Some(&Value::Bool(true)),
                        state: "output-error",
                        raw_input: present(chunk, "input"),
                        error_text: present(chunk, "errorText"),
                        provider_executed: present(chunk, "providerExecuted"),
                        call_provider_metadata: present(chunk, "providerMetadata"),
                        title: present(chunk, "title"),
                        tool_metadata: present(chunk, "toolMetadata"),
                        ..ToolUpsert::default()
                    },
                );
                self.partial_tool_calls.remove(tool_call_id);
            }
            "tool-output-available" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let Some(&index) = self.tool_part_index.get(tool_call_id) else {
                    return;
                };
                let part = &self.parts[index];
                let upsert = ToolUpsert {
                    tool_name: tool_name(part),
                    dynamic: is_dynamic_tool(part),
                    state: "output-available",
                    input: part.get("input").cloned(),
                    output: present(chunk, "output"),
                    preliminary: present(chunk, "preliminary"),
                    provider_executed: present(chunk, "providerExecuted"),
                    result_provider_metadata: present(chunk, "providerMetadata"),
                    title: part.get("title").cloned(),
                    tool_metadata: part.get("toolMetadata").cloned(),
                    ..ToolUpsert::default()
                };
                self.upsert_tool_part(tool_call_id, upsert);
                self.partial_tool_calls.remove(tool_call_id);
            }
            "tool-output-error" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let Some(&index) = self.tool_part_index.get(tool_call_id) else {
                    return;
                };
                let part = &self.parts[index];
                let upsert = ToolUpsert {
                    tool_name: tool_name(part),
                    dynamic: is_dynamic_tool(part),
                    state: "output-error",
                    input: part.get("input").cloned(),
                    raw_input: part.get("rawInput").cloned(),
                    error_text: present(chunk, "errorText"),
                    provider_executed: present(chunk, "providerExecuted"),
                    result_provider_metadata: present(chunk, "providerMetadata"),
                    title: part.get("title").cloned(),
                    tool_metadata: part.get("toolMetadata").cloned(),
                    ..ToolUpsert::default()
                };
                self.upsert_tool_part(tool_call_id, upsert);
                self.partial_tool_calls.remove(tool_call_id);
            }
            "tool-output-denied" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let Some(&index) = self.tool_part_index.get(tool_call_id) else {
                    return;
                };
                self.parts[index]
                    .as_object_mut()
                    .expect("tool part must be an object")
                    .insert("state".to_string(), json!("output-denied"));
                self.partial_tool_calls.remove(tool_call_id);
            }
            "tool-approval-request" => {
                let Some(tool_call_id) = chunk.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let approval_id = chunk
                    .get("approvalId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.approvals_by_tool_call
                    .insert(tool_call_id.to_string(), approval_id.clone());
                if let Some(&index) = self.tool_part_index.get(tool_call_id) {
                    let part = self.parts[index]
                        .as_object_mut()
                        .expect("tool part must be an object");
                    part.insert("state".to_string(), json!("approval-requested"));
                    part.insert("approval".to_string(), json!({ "id": approval_id }));
                }
            }
            other if other.starts_with("data-") && !truthy(chunk.get("transient")) => {
                let mut part = Map::new();
                part.insert("type".to_string(), json!(other));
                insert_non_null(&mut part, chunk, "id");
                copy_key(&mut part, chunk, "data");
                self.parts.push(Value::Object(part));
            }
            _ => {}
        }
    }

    /// Return a deep copy of the projected assistant `UIMessage`.
    pub fn snapshot(&self) -> Value {
        let mut message = Map::new();
        message.insert("id".to_string(), json!(self.message_id));
        message.insert("role".to_string(), json!("assistant"));
        message.insert("parts".to_string(), Value::Array(self.parts.clone()));
        if let Some(metadata) = &self.metadata {
            message.insert("metadata".to_string(), metadata.clone());
        }
        Value::Object(message)
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn apply_all(tracker: &mut MessageStateTracker, chunks: &[Value]) {
        for chunk in chunks {
            tracker.apply(chunk);
        }
    }

    fn assert_roundtrip(chunks: &[Value]) -> MessageStateTracker {
        let mut original = MessageStateTracker::new("initial");
        apply_all(&mut original, chunks);
        let synthesized = original.synthesize_chunks();

        let mut replayed = MessageStateTracker::new("initial");
        apply_all(&mut replayed, &synthesized);
        assert_eq!(replayed.snapshot(), original.snapshot());
        assert_eq!(replayed.synthesize_chunks(), synthesized);
        original
    }

    #[test]
    fn text_reasoning_metadata_and_finish_roundtrip() {
        let tracker = assert_roundtrip(&[
            json!({"type":"start","messageId":"m1","messageMetadata":{"model":"test"}}),
            json!({"type":"start-step"}),
            json!({"type":"reasoning-start","id":"r1"}),
            json!({"type":"reasoning-delta","id":"r1","delta":"considering"}),
            json!({"type":"reasoning-end","id":"r1"}),
            json!({"type":"text-start","id":"t1"}),
            json!({"type":"text-delta","id":"t1","delta":"done"}),
            json!({"type":"text-end","id":"t1"}),
            json!({"type":"finish","finishReason":"stop","messageMetadata":{"final":true}}),
        ]);
        assert_eq!(tracker.snapshot()["id"], "m1");
        assert_eq!(tracker.snapshot()["metadata"], json!({"final": true}));
        assert_eq!(tracker.snapshot()["parts"][1]["text"], "considering");
        assert_eq!(tracker.snapshot()["parts"][2]["text"], "done");
    }

    #[test]
    fn static_and_dynamic_tool_parts_roundtrip() {
        let tracker = assert_roundtrip(&[
            json!({"type":"tool-input-start","toolCallId":"s1","toolName":"lookup"}),
            json!({"type":"tool-input-delta","toolCallId":"s1","inputTextDelta":"{\"q\":\"x\"}"}),
            json!({"type":"tool-input-available","toolCallId":"s1","toolName":"lookup","input":{"q":"x"}}),
            json!({"type":"tool-output-available","toolCallId":"s1","output":{"value":1}}),
            json!({"type":"tool-input-start","toolCallId":"d1","toolName":"remote_lookup","dynamic":true}),
            json!({"type":"tool-input-available","toolCallId":"d1","toolName":"remote_lookup","input":{},"dynamic":true}),
            json!({"type":"tool-output-available","toolCallId":"d1","output":[1,2],"dynamic":true}),
        ]);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot["parts"][0]["type"], "tool-lookup");
        assert_eq!(snapshot["parts"][0]["state"], "output-available");
        assert_eq!(snapshot["parts"][0]["output"], json!({"value": 1}));
        assert_eq!(snapshot["parts"][1]["type"], "dynamic-tool");
        assert_eq!(snapshot["parts"][1]["toolName"], "remote_lookup");
        assert_eq!(snapshot["parts"][1]["output"], json!([1, 2]));
    }

    #[test]
    fn pending_streams_and_tool_input_replay_with_live_ids() {
        let tracker = assert_roundtrip(&[
            json!({"type":"text-start","id":"t-live"}),
            json!({"type":"text-delta","id":"t-live","delta":"partial"}),
            json!({"type":"tool-input-start","toolCallId":"c-live","toolName":"lookup"}),
            json!({"type":"tool-input-delta","toolCallId":"c-live","inputTextDelta":"{\"q\":"}),
        ]);
        let synthesized = tracker.synthesize_chunks();
        assert!(
            synthesized
                .iter()
                .any(|chunk| chunk["type"] == "text-delta" && chunk["id"] == "t-live")
        );
        assert!(synthesized.iter().any(|chunk| {
            chunk["type"] == "tool-input-delta"
                && chunk["toolCallId"] == "c-live"
                && chunk["inputTextDelta"] == "{\"q\":"
        }));
    }

    #[test]
    fn tool_error_denial_and_approval_states_roundtrip() {
        let tracker = assert_roundtrip(&[
            json!({"type":"tool-input-start","toolCallId":"e1","toolName":"run"}),
            json!({"type":"tool-input-available","toolCallId":"e1","toolName":"run","input":{"cmd":"false"}}),
            json!({"type":"tool-output-error","toolCallId":"e1","errorText":"failed"}),
            json!({"type":"tool-input-start","toolCallId":"e2","toolName":"run"}),
            json!({"type":"tool-input-error","toolCallId":"e2","toolName":"run","input":"{","errorText":"invalid"}),
            json!({"type":"tool-input-start","toolCallId":"d1","toolName":"run"}),
            json!({"type":"tool-input-available","toolCallId":"d1","toolName":"run","input":{}}),
            json!({"type":"tool-output-denied","toolCallId":"d1"}),
            json!({"type":"tool-input-start","toolCallId":"a1","toolName":"run"}),
            json!({"type":"tool-input-available","toolCallId":"a1","toolName":"run","input":{}}),
            json!({"type":"tool-approval-request","toolCallId":"a1","approvalId":"approval-1"}),
        ]);
        let parts = tracker.snapshot()["parts"].as_array().unwrap().clone();
        assert_eq!(parts[0]["state"], "output-error");
        assert_eq!(parts[0]["input"], json!({"cmd":"false"}));
        assert_eq!(parts[1]["state"], "output-error");
        assert!(parts[1].get("input").is_none());
        assert_eq!(parts[1]["rawInput"], "{");
        assert_eq!(parts[2]["state"], "output-denied");
        assert_eq!(parts[3]["state"], "approval-requested");
        assert_eq!(parts[3]["approval"]["id"], "approval-1");
    }

    #[test]
    fn file_sources_and_persistent_data_roundtrip_while_transient_data_drops() {
        let tracker = assert_roundtrip(&[
            json!({"type":"file","mediaType":"image/png","url":"image.png"}),
            json!({"type":"source-url","sourceId":"s1","url":"https://example.test","title":"Example"}),
            json!({"type":"source-document","sourceId":"d1","mediaType":"text/plain","title":"Doc","filename":"doc.txt"}),
            json!({"type":"data-note","id":"n1","data":{"x":1}}),
            json!({"type":"data-notice","data":"ephemeral","transient":true}),
        ]);
        let parts = tracker.snapshot()["parts"].as_array().unwrap().clone();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0]["type"], "file");
        assert_eq!(parts[1]["type"], "source-url");
        assert_eq!(parts[2]["type"], "source-document");
        assert_eq!(parts[3]["type"], "data-note");
    }

    #[test]
    fn malformed_or_unknown_references_are_noops() {
        let mut tracker = MessageStateTracker::new("m1");
        apply_all(
            &mut tracker,
            &[
                json!({"type":"unknown","value":1}),
                json!({"type":"text-delta","id":"missing","delta":"x"}),
                json!({"type":"tool-output-available","toolCallId":"missing","output":{}}),
                json!({"notType":"ignored"}),
            ],
        );
        assert_eq!(
            tracker.snapshot(),
            json!({"id":"m1","role":"assistant","parts":[]})
        );
        assert!(tracker.synthesize_chunks().is_empty());
    }

    #[test]
    fn metadata_chunks_replace_whole_values_and_can_clear_them() {
        let mut tracker = MessageStateTracker::new("m1");
        tracker.apply(&json!({"type":"start","messageId":"m2","messageMetadata":{"a":1,"z":9}}));
        tracker.apply(&json!({"type":"message-metadata","messageMetadata":{"b":2}}));
        assert_eq!(tracker.snapshot()["metadata"], json!({"b": 2}));
        tracker.apply(&json!({"type":"message-metadata"}));
        assert!(
            !tracker
                .snapshot()
                .as_object()
                .unwrap()
                .contains_key("metadata")
        );

        tracker.apply(&json!({"type":"message-metadata","messageMetadata":{"final":true}}));
        assert_eq!(
            tracker.synthesize_chunks(),
            vec![json!({
                "type":"start",
                "messageId":"m2",
                "messageMetadata":{"final":true}
            })]
        );
    }

    #[test]
    fn restreaming_input_clears_canonical_input_but_parse_error_does_not() {
        let mut tracker = MessageStateTracker::new("m1");
        apply_all(
            &mut tracker,
            &[
                json!({"type":"tool-input-start","toolCallId":"c1","toolName":"write"}),
                json!({"type":"tool-input-available","toolCallId":"c1","toolName":"write","input":{"a":1}}),
                json!({"type":"tool-input-start","toolCallId":"c1","toolName":"write"}),
            ],
        );
        let part = &tracker.snapshot()["parts"][0];
        assert_eq!(part["state"], "input-streaming");
        assert!(part.get("input").is_none());

        let mut tracker = MessageStateTracker::new("m1");
        apply_all(
            &mut tracker,
            &[
                json!({"type":"tool-input-start","toolCallId":"c1","toolName":"write"}),
                json!({"type":"tool-input-available","toolCallId":"c1","toolName":"write","input":{"a":1}}),
                json!({"type":"tool-input-error","toolCallId":"c1","toolName":"write","input":"raw","errorText":"bad"}),
            ],
        );
        let part = &tracker.snapshot()["parts"][0];
        assert_eq!(part["input"], json!({"a": 1}));
        assert_eq!(part["rawInput"], "raw");
    }

    #[test]
    fn synthesis_distinguishes_execution_and_input_parse_errors() {
        let mut executed = MessageStateTracker::new("m1");
        apply_all(
            &mut executed,
            &[
                json!({"type":"tool-input-start","toolCallId":"c1","toolName":"write"}),
                json!({"type":"tool-input-available","toolCallId":"c1","toolName":"write","input":{"a":1}}),
                json!({"type":"tool-output-error","toolCallId":"c1","errorText":"boom"}),
            ],
        );
        assert_eq!(
            executed.synthesize_chunks(),
            vec![
                json!({"type":"start","messageId":"m1"}),
                json!({"type":"tool-input-start","toolCallId":"c1","toolName":"write"}),
                json!({"type":"tool-input-available","toolCallId":"c1","toolName":"write","input":{"a":1}}),
                json!({"type":"tool-output-error","toolCallId":"c1","errorText":"boom"}),
            ]
        );

        let mut parse = MessageStateTracker::new("m1");
        apply_all(
            &mut parse,
            &[
                json!({"type":"tool-input-start","toolCallId":"c2","toolName":"remote","dynamic":true}),
                json!({"type":"tool-input-error","toolCallId":"c2","toolName":"remote","dynamic":true,"input":"{","errorText":"bad json"}),
            ],
        );
        assert_eq!(
            parse.synthesize_chunks(),
            vec![
                json!({"type":"start","messageId":"m1"}),
                json!({"type":"tool-input-start","toolCallId":"c2","toolName":"remote","dynamic":true}),
                json!({"type":"tool-input-error","toolCallId":"c2","toolName":"remote","input":"{","dynamic":true,"errorText":"bad json"}),
            ]
        );
    }

    #[test]
    fn synthesized_stream_ids_share_one_counter_and_live_ids_survive() {
        let tracker = assert_roundtrip(&[
            json!({"type":"text-start","id":"t1"}),
            json!({"type":"text-delta","id":"t1","delta":"a"}),
            json!({"type":"text-end","id":"t1"}),
            json!({"type":"reasoning-start","id":"r1"}),
            json!({"type":"reasoning-delta","id":"r1","delta":"b"}),
            json!({"type":"reasoning-end","id":"r1"}),
            json!({"type":"text-start","id":"live"}),
            json!({"type":"text-delta","id":"live","delta":"c"}),
        ]);
        let synthesized = tracker.synthesize_chunks();
        assert_eq!(synthesized[1]["id"], "synth-text-0");
        assert_eq!(synthesized[4]["id"], "synth-reasoning-1");
        assert_eq!(synthesized[7]["id"], "live");
    }

    #[test]
    fn optional_source_keys_and_false_boolean_follow_non_null_gates() {
        let mut tracker = MessageStateTracker::new("m1");
        apply_all(
            &mut tracker,
            &[
                json!({"type":"source-url","sourceId":"s1","url":"https://example.test"}),
                json!({"type":"source-document","sourceId":"d1","mediaType":"text/plain","title":"Doc"}),
                json!({"type":"tool-input-start","toolCallId":"c1","toolName":"write","providerExecuted":false,"providerMetadata":null}),
            ],
        );
        let snapshot = tracker.snapshot();
        assert!(snapshot["parts"][0].get("title").is_none());
        assert!(snapshot["parts"][1].get("filename").is_none());
        assert!(snapshot["parts"][1].get("providerMetadata").is_none());
        assert_eq!(snapshot["parts"][2]["providerExecuted"], false);
        assert_eq!(snapshot["parts"][2]["callProviderMetadata"], Value::Null);
        assert_eq!(tracker.synthesize_chunks()[3]["providerExecuted"], false);
    }
}

impl MessageStateTracker {
    /// Synthesize a chunk sequence that reconstructs the current projection.
    ///
    /// A completed tracker includes its terminal `finish` chunk. An untouched
    /// tracker returns no chunks.
    pub fn synthesize_chunks(&self) -> Vec<Value> {
        if self.parts.is_empty() && self.metadata.is_none() && !self.finish_observed {
            return Vec::new();
        }

        let mut start = Map::new();
        start.insert("type".to_string(), json!("start"));
        start.insert("messageId".to_string(), json!(self.message_id));
        if let Some(metadata) = &self.metadata {
            start.insert("messageMetadata".to_string(), metadata.clone());
        }
        let mut out = vec![Value::Object(start)];

        let text_id_by_index: HashMap<usize, &String> = self
            .active_text_ids
            .iter()
            .map(|(id, &index)| (index, id))
            .collect();
        let reasoning_id_by_index: HashMap<usize, &String> = self
            .active_reasoning_ids
            .iter()
            .map(|(id, &index)| (index, id))
            .collect();
        let mut synth_seq = 0usize;

        for (index, part) in self.parts.iter().enumerate() {
            match part_type(part) {
                "step-start" => out.push(json!({ "type": "start-step" })),
                "text" => Self::synthesize_stream_part(
                    part,
                    "text",
                    text_id_by_index.get(&index).copied(),
                    &mut synth_seq,
                    &mut out,
                ),
                "reasoning" => Self::synthesize_stream_part(
                    part,
                    "reasoning",
                    reasoning_id_by_index.get(&index).copied(),
                    &mut synth_seq,
                    &mut out,
                ),
                "file" => {
                    let mut chunk = Map::new();
                    chunk.insert("type".to_string(), json!("file"));
                    copy_key(&mut chunk, part, "mediaType");
                    copy_key(&mut chunk, part, "url");
                    insert_non_null(&mut chunk, part, "providerMetadata");
                    out.push(Value::Object(chunk));
                }
                "source-url" => {
                    let mut chunk = Map::new();
                    chunk.insert("type".to_string(), json!("source-url"));
                    copy_key(&mut chunk, part, "sourceId");
                    copy_key(&mut chunk, part, "url");
                    insert_non_null(&mut chunk, part, "title");
                    insert_non_null(&mut chunk, part, "providerMetadata");
                    out.push(Value::Object(chunk));
                }
                "source-document" => {
                    let mut chunk = Map::new();
                    chunk.insert("type".to_string(), json!("source-document"));
                    copy_key(&mut chunk, part, "sourceId");
                    copy_key(&mut chunk, part, "mediaType");
                    copy_key(&mut chunk, part, "title");
                    insert_non_null(&mut chunk, part, "filename");
                    insert_non_null(&mut chunk, part, "providerMetadata");
                    out.push(Value::Object(chunk));
                }
                other if is_tool_part(part) => self.synthesize_tool_part(part, &mut out),
                other if other.starts_with("data-") => {
                    let mut chunk = Map::new();
                    chunk.insert("type".to_string(), json!(other));
                    insert_non_null(&mut chunk, part, "id");
                    copy_key(&mut chunk, part, "data");
                    out.push(Value::Object(chunk));
                }
                _ => {}
            }
        }

        if self.finish_observed {
            let mut finish = Map::new();
            finish.insert("type".to_string(), json!("finish"));
            if let Some(reason) = &self.finish_reason {
                finish.insert("finishReason".to_string(), reason.clone());
            }
            if let Some(metadata) = &self.metadata {
                finish.insert("messageMetadata".to_string(), metadata.clone());
            }
            out.push(Value::Object(finish));
        }
        out
    }

    fn active_ids(&mut self, kind: &str) -> &mut HashMap<String, usize> {
        if kind == "text" {
            &mut self.active_text_ids
        } else {
            &mut self.active_reasoning_ids
        }
    }

    fn stream_start(&mut self, chunk: &Value, kind: &'static str) {
        let mut part = Map::new();
        part.insert("type".to_string(), json!(kind));
        part.insert("text".to_string(), json!(""));
        part.insert("state".to_string(), json!("streaming"));
        insert_non_null(&mut part, chunk, "providerMetadata");
        self.parts.push(Value::Object(part));
        if let Some(id) = chunk.get("id").and_then(Value::as_str) {
            let index = self.parts.len() - 1;
            self.active_ids(kind).insert(id.to_string(), index);
        }
    }

    fn stream_delta(&mut self, chunk: &Value, kind: &'static str) {
        let Some(id) = chunk.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(&index) = self.active_ids(kind).get(id) else {
            return;
        };
        let delta = chunk.get("delta").and_then(Value::as_str).unwrap_or("");
        let provider_metadata = non_null(chunk, "providerMetadata");
        let part = self.parts[index]
            .as_object_mut()
            .expect("stream part must be an object");
        if let Some(Value::String(text)) = part.get_mut("text") {
            text.push_str(delta);
        }
        if let Some(provider_metadata) = provider_metadata {
            part.insert("providerMetadata".to_string(), provider_metadata);
        }
    }

    fn stream_end(&mut self, chunk: &Value, kind: &'static str) {
        let Some(id) = chunk.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(index) = self.active_ids(kind).remove(id) else {
            return;
        };
        let provider_metadata = non_null(chunk, "providerMetadata");
        let part = self.parts[index]
            .as_object_mut()
            .expect("stream part must be an object");
        part.insert("state".to_string(), json!("done"));
        if let Some(provider_metadata) = provider_metadata {
            part.insert("providerMetadata".to_string(), provider_metadata);
        }
    }

    fn upsert_tool_part(&mut self, tool_call_id: &str, args: ToolUpsert) {
        let index = match self.tool_part_index.get(tool_call_id) {
            Some(&index) => {
                self.parts[index]
                    .as_object_mut()
                    .expect("tool part must be an object")
                    .insert("state".to_string(), json!(args.state));
                index
            }
            None => {
                let mut part = Map::new();
                let part_type = if args.dynamic {
                    "dynamic-tool".to_string()
                } else {
                    format!("tool-{}", args.tool_name)
                };
                part.insert("type".to_string(), json!(part_type));
                part.insert("toolCallId".to_string(), json!(tool_call_id));
                part.insert("state".to_string(), json!(args.state));
                if args.dynamic {
                    part.insert("toolName".to_string(), json!(args.tool_name));
                }
                self.parts.push(Value::Object(part));
                let index = self.parts.len() - 1;
                self.tool_part_index.insert(tool_call_id.to_string(), index);
                index
            }
        };

        let part = self.parts[index]
            .as_object_mut()
            .expect("tool part must be an object");
        if args.input.is_some() || args.state == "input-streaming" {
            match args.input {
                Some(input) => {
                    part.insert("input".to_string(), input);
                }
                None => {
                    part.remove("input");
                }
            }
        }
        for (key, value) in [
            ("output", args.output),
            ("rawInput", args.raw_input),
            ("errorText", args.error_text),
            ("title", args.title),
            ("toolMetadata", args.tool_metadata),
            ("providerExecuted", args.provider_executed),
            ("callProviderMetadata", args.call_provider_metadata),
            ("resultProviderMetadata", args.result_provider_metadata),
            ("preliminary", args.preliminary),
        ] {
            if let Some(value) = value {
                part.insert(key.to_string(), value);
            }
        }
    }

    fn synthesize_stream_part(
        part: &Value,
        kind: &str,
        live_id: Option<&String>,
        synth_seq: &mut usize,
        out: &mut Vec<Value>,
    ) {
        let id = match live_id {
            Some(id) => id.clone(),
            None => {
                let id = format!("synth-{kind}-{synth_seq}");
                *synth_seq += 1;
                id
            }
        };
        let mut start = Map::new();
        start.insert("type".to_string(), json!(format!("{kind}-start")));
        start.insert("id".to_string(), json!(id));
        insert_non_null(&mut start, part, "providerMetadata");
        out.push(Value::Object(start));
        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            out.push(json!({ "type": format!("{kind}-delta"), "id": id, "delta": text }));
        }
        if part.get("state").and_then(Value::as_str) == Some("done") {
            out.push(json!({ "type": format!("{kind}-end"), "id": id }));
        }
    }

    fn synthesize_tool_part(&self, part: &Value, out: &mut Vec<Value>) {
        let tool_call_id = part
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = tool_name(part);
        let dynamic = is_dynamic_tool(part);

        let mut start = Map::new();
        start.insert("type".to_string(), json!("tool-input-start"));
        start.insert("toolCallId".to_string(), json!(tool_call_id));
        start.insert("toolName".to_string(), json!(name));
        if dynamic {
            start.insert("dynamic".to_string(), json!(true));
        }
        insert_non_null(&mut start, part, "providerExecuted");
        if let Some(metadata) = non_null(part, "callProviderMetadata") {
            start.insert("providerMetadata".to_string(), metadata);
        }
        insert_non_null(&mut start, part, "title");
        insert_non_null(&mut start, part, "toolMetadata");
        out.push(Value::Object(start));

        match part.get("state").and_then(Value::as_str).unwrap_or("") {
            "input-streaming" => {
                if let Some(partial) = self.partial_tool_calls.get(&tool_call_id)
                    && !partial.text.is_empty()
                {
                    out.push(json!({
                        "type": "tool-input-delta",
                        "toolCallId": tool_call_id,
                        "inputTextDelta": partial.text,
                    }));
                }
            }
            state @ ("input-available" | "approval-requested" | "output-available"
            | "output-denied") => {
                let mut available = Map::new();
                available.insert("type".to_string(), json!("tool-input-available"));
                available.insert("toolCallId".to_string(), json!(tool_call_id));
                available.insert("toolName".to_string(), json!(name));
                copy_key(&mut available, part, "input");
                if dynamic {
                    available.insert("dynamic".to_string(), json!(true));
                }
                insert_non_null(&mut available, part, "providerExecuted");
                if let Some(metadata) = non_null(part, "callProviderMetadata") {
                    available.insert("providerMetadata".to_string(), metadata);
                }
                insert_non_null(&mut available, part, "title");
                insert_non_null(&mut available, part, "toolMetadata");
                out.push(Value::Object(available));

                if state == "approval-requested"
                    && let Some(approval_id) = self.approvals_by_tool_call.get(&tool_call_id)
                    && !approval_id.is_empty()
                {
                    out.push(json!({
                        "type": "tool-approval-request",
                        "approvalId": approval_id,
                        "toolCallId": tool_call_id,
                    }));
                }
                if state == "output-available" {
                    let mut result = Map::new();
                    result.insert("type".to_string(), json!("tool-output-available"));
                    result.insert("toolCallId".to_string(), json!(tool_call_id));
                    copy_key(&mut result, part, "output");
                    insert_non_null(&mut result, part, "providerExecuted");
                    if let Some(metadata) = non_null(part, "resultProviderMetadata") {
                        result.insert("providerMetadata".to_string(), metadata);
                    }
                    insert_non_null(&mut result, part, "preliminary");
                    if dynamic {
                        result.insert("dynamic".to_string(), json!(true));
                    }
                    out.push(Value::Object(result));
                } else if state == "output-denied" {
                    out.push(json!({
                        "type": "tool-output-denied",
                        "toolCallId": tool_call_id,
                    }));
                }
            }
            "output-error" if part.get("input").is_some() => {
                let mut available = Map::new();
                available.insert("type".to_string(), json!("tool-input-available"));
                available.insert("toolCallId".to_string(), json!(tool_call_id));
                available.insert("toolName".to_string(), json!(name));
                copy_key(&mut available, part, "input");
                if dynamic {
                    available.insert("dynamic".to_string(), json!(true));
                }
                insert_non_null(&mut available, part, "title");
                insert_non_null(&mut available, part, "toolMetadata");
                out.push(Value::Object(available));

                let mut error = Map::new();
                error.insert("type".to_string(), json!("tool-output-error"));
                error.insert("toolCallId".to_string(), json!(tool_call_id));
                error.insert("errorText".to_string(), error_text(part));
                insert_non_null(&mut error, part, "providerExecuted");
                if dynamic {
                    error.insert("dynamic".to_string(), json!(true));
                }
                out.push(Value::Object(error));
            }
            "output-error" => {
                let mut error = Map::new();
                error.insert("type".to_string(), json!("tool-input-error"));
                error.insert("toolCallId".to_string(), json!(tool_call_id));
                error.insert("toolName".to_string(), json!(name));
                if let Some(raw_input) = part.get("rawInput") {
                    error.insert("input".to_string(), raw_input.clone());
                }
                if dynamic {
                    error.insert("dynamic".to_string(), json!(true));
                }
                error.insert("errorText".to_string(), error_text(part));
                insert_non_null(&mut error, part, "providerExecuted");
                insert_non_null(&mut error, part, "title");
                insert_non_null(&mut error, part, "toolMetadata");
                out.push(Value::Object(error));
            }
            _ => {}
        }
    }
}
