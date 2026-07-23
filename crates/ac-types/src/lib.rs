//! Zero-policy foundation types for AC: messages, content parts, the unified
//! completion event stream, tool specs, usage accounting, and the completion
//! error taxonomy. No I/O, no runtime, no host concepts.

mod content;
mod effort;
mod error;
mod event;
mod marker;
mod tool;

pub use content::{ContentPart, Message, Role, ToolResult, ToolUse};
pub use effort::Effort;
pub use error::CompletionError;
pub use event::{Citation, CompletionEvent, StopReason, TokenUsage};
pub use marker::INTERRUPTION_MARKER;
pub use tool::ToolSpec;
