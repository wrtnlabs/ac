//! Provider-safe history normalization.
//!
//! Tool-call pairing and non-empty message content are provider wire
//! invariants, not host-domain policy. This module is the single repair path
//! for legacy/imported flat histories and for defensive model-view projection.

use std::collections::HashSet;

use ac_types::{CacheMark, ContentPart, Message, Role, ToolResult};

/// The generic result text synthesized when a tool call ended without a
/// result, normally because the user cancelled its turn.
pub const ABORTED_TOOL_RESULT: &str = "Tool call aborted by user";

/// Whether a message has no model-visible content.
///
/// A structurally empty row and a single blank text part are equivalent here.
/// Rich parts (including an image or tool item) are never dropped merely
/// because they carry no text.
pub fn is_contentless(message: &Message) -> bool {
    match message.content.as_slice() {
        [] => true,
        [ContentPart::Text { text }] => text.trim().is_empty(),
        _ => false,
    }
}

/// Insert an error result immediately after every unanswered tool call.
///
/// Existing answers may appear anywhere later in the history. The operation
/// is idempotent: a second pass observes the synthesized results and adds
/// nothing.
pub fn repair_dangling_tool_uses(messages: &mut Vec<Message>) {
    let answered: HashSet<String> = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult(result) => Some(result.tool_use_id.clone()),
            _ => None,
        })
        .collect();

    let mut index = 0;
    while index < messages.len() {
        let dangling: Vec<String> = messages[index]
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolUse(call) if !answered.contains(&call.id) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        if dangling.is_empty() {
            index += 1;
            continue;
        }
        messages.insert(
            index + 1,
            Message {
                role: Role::User,
                content: dangling
                    .into_iter()
                    .map(|tool_use_id| {
                        ContentPart::ToolResult(ToolResult {
                            tool_use_id,
                            content: ABORTED_TOOL_RESULT.to_string(),
                            is_error: true,
                        })
                    })
                    .collect(),
                cache: CacheMark::Off,
            },
        );
        index += 2;
    }
}

/// Remove contentless rows and close unanswered tool calls in place.
pub fn sanitize_messages(messages: &mut Vec<Message>) {
    messages.retain(|message| !is_contentless(message));
    repair_dangling_tool_uses(messages);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::ToolUse;

    fn call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse(ToolUse {
                id: id.into(),
                name: "probe".into(),
                input: serde_json::json!({}),
            })],
            cache: CacheMark::Off,
        }
    }

    fn answer(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            })],
            cache: CacheMark::Off,
        }
    }

    #[test]
    fn sanitizes_empty_rows_and_repairs_only_unanswered_calls() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![],
                cache: CacheMark::Off,
            },
            Message::text(Role::Assistant, " \n "),
            call("answered"),
            answer("answered"),
            call("dangling"),
        ];

        sanitize_messages(&mut messages);

        assert_eq!(messages.len(), 4);
        let repair = &messages[3];
        assert!(matches!(
            repair.content.as_slice(),
            [ContentPart::ToolResult(result)]
                if result.tool_use_id == "dangling"
                    && result.is_error
                    && result.content == ABORTED_TOOL_RESULT
        ));
    }

    #[test]
    fn repair_is_idempotent_and_keeps_rich_content() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentPart::Image {
                    media_type: "image/png".into(),
                    data: "AA==".into(),
                }],
                cache: CacheMark::Off,
            },
            call("c1"),
        ];
        sanitize_messages(&mut messages);
        let once = messages.clone();
        sanitize_messages(&mut messages);
        assert_eq!(messages, once);
        assert!(matches!(
            messages[0].content.as_slice(),
            [ContentPart::Image { .. }]
        ));
    }
}
