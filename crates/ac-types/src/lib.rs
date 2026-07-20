//! Zero-policy foundation types for AC: messages, content parts, the unified
//! completion event stream, tool specs, usage accounting, and the completion
//! error taxonomy. No I/O, no runtime, no host concepts.

mod content;
mod error;
mod event;
mod tool;

pub use content::{ContentPart, Message, Role, ToolResult, ToolUse};
pub use error::CompletionError;
pub use event::{Citation, CompletionEvent, StopReason, TokenUsage};
pub use tool::ToolSpec;
