//! Context compaction — [docs/ac-compaction.md]. When a session's effective
//! history `H` grows toward the model's context window `W`, compaction applies
//! `C : H → H′` with `τ(H′) ≪ τ(H)` that *preserves the task* rather than
//! truncating or refusing it.
//!
//! The load-bearing choice (R1) is that `C` is framed as a **handoff**: one
//! agent checkpointing a task for another to resume, not "shorten this text".
//! This module holds the configuration, the pure transformation helpers, and
//! the handoff prompt; the lifecycle (triggers, the summarization turn, the
//! record) lives on `Session` in `lib.rs`, since it needs the loop and the
//! provider.

use ac_provider::{CompletionRequest, ToolChoice};
use ac_types::{ContentPart, Message, Role, TokenUsage};

/// How the summary `σ` is produced ([docs/ac-compaction.md] §4). The two axes —
/// trigger and strategy — are orthogonal, and (R4) observationally equivalent
/// except through `σ` and the recorded trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionStrategy {
    /// The model produces `σ` under the handoff contract (R1). The default.
    #[default]
    Summarize,
    /// `σ = ∅`: `H′` is `context′ ⧺ U` alone — a deliberate reset that keeps
    /// user input verbatim but discards the agent's traffic without a summary.
    FreshWindow,
}

/// Which trigger fired a compaction ([docs/ac-compaction.md] §4). Recorded on
/// the `κ` record and emitted on [`AgentEvent::Compacted`](crate::AgentEvent);
/// it is the *only* thing that distinguishes one compaction from another (R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// A host asked for it explicitly.
    Manual,
    /// `τ ≥ β` before a turn's first step — clears the runway before new work.
    PreTurn,
    /// `follow_up ∧ τ ≥ β` after a step — checkpoint, then continue the same
    /// turn. The trigger that saves long tasks.
    MidTurn,
}

impl CompactionTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            CompactionTrigger::Manual => "manual",
            CompactionTrigger::PreTurn => "pre_turn",
            CompactionTrigger::MidTurn => "mid_turn",
        }
    }
}

/// Budget and policy for compaction. `None` on [`AgentConfig`](crate::AgentConfig)
/// disables compaction entirely: no trigger ever fires and manual
/// [`compact`](crate::Session::compact) returns [`CompactionError::Disabled`].
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// The budget `β ≤ W` in tokens. A compaction trigger fires when the
    /// measured context occupancy `τ` reaches it. The host sets this to fit the
    /// model's window; there is no universal default that is right for every
    /// model.
    pub budget_tokens: u64,
    /// Per-message cap for the verbatim user inputs `U` (R2): a single
    /// pathological input cannot monopolize the fresh window. Within the cap,
    /// survival is byte-for-byte.
    pub per_message_cap_tokens: u64,
    /// Upper bound on the handoff summary the model may emit.
    pub summary_max_tokens: u32,
    /// Exclude a provider-cached prefix from `τ` ([docs/ac-compaction.md] §4):
    /// a large invariant prefix cached at the provider consumes no marginal
    /// cost and ought not trigger compaction. Subtracts `cache_read_input_tokens`
    /// from the input side of `τ`.
    pub exclude_cached_prefix: bool,
    /// How `σ` is produced.
    pub strategy: CompactionStrategy,
    /// Override the built-in handoff instruction. `None` uses [`HANDOFF_SYSTEM`].
    pub handoff_system: Option<String>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            budget_tokens: 120_000,
            per_message_cap_tokens: 4_096,
            summary_max_tokens: 2_048,
            exclude_cached_prefix: false,
            strategy: CompactionStrategy::Summarize,
            handoff_system: None,
        }
    }
}

/// What a compaction did — returned by [`compact`](crate::Session::compact).
#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    pub trigger: CompactionTrigger,
    pub strategy: CompactionStrategy,
    pub summary_chars: usize,
    /// `τ(H)` before compaction, from server usage.
    pub tokens_before: u64,
    /// `τ(H′)` after, estimated (the real figure arrives with the next call).
    pub tokens_after: u64,
    pub messages_before: usize,
    pub messages_after: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("compaction is not configured on this session")]
    Disabled,
    /// The history holds only user input — there is nothing to compress, and a
    /// compaction would be a no-op (or a loss of nothing). The caller proceeds
    /// uncompacted.
    #[error("nothing to compact: the history holds no agent traffic")]
    NothingToCompact,
    /// R3: `C` did not bring `τ(H′)` below the budget, so it would re-trigger
    /// immediately — surfaced as an error rather than a loop.
    #[error(
        "compaction did not reduce context below the budget (budget {budget}, achieved {achieved})"
    )]
    Ineffective { budget: u64, achieved: u64 },
    #[error("summary generation failed: {0}")]
    Completion(#[from] ac_types::CompletionError),
    /// The summary round-trip stalled past the idle timeout. Distinct from
    /// `Cancelled` so a stalled provider is not mistaken for a deliberate user
    /// cancel (which would record an interruption marker).
    #[error("summary generation stalled: no event within the idle timeout")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
}

/// The handoff instruction (R1). App-agnostic by construction — it names no
/// domain, tool, or artifact. A host may override it via
/// [`CompactionConfig::handoff_system`].
pub const HANDOFF_SYSTEM: &str = "\
You are compacting a working session for a fresh AI agent that will take over this \
task exactly where it now stands. You are writing a HANDOFF, not a summary for a \
human reader, and not a transcript.

Produce a checkpoint the next agent can act on with no other context. Cover, as \
concrete content rather than vague description:
- Progress so far: what has been done and the current state of the work.
- Decisions made and why, so they are not silently reversed.
- Constraints, requirements, and preferences still in force.
- What remains, in enough detail to continue without re-deriving it.
- The specific data needed to proceed: identifiers, paths, names, values, results.

Do not restate the conversation. Do not add preamble or sign-off. Write only the \
handoff.";

/// Prepended to `σ` so the receiving model reads it as another agent's work to
/// build on, not its own prior statement.
pub const HANDOFF_PREAMBLE: &str = "\
[The following is a handoff from a previous agent that worked on this task. \
Continue from where it leaves off; do not restart or repeat completed work.]";

/// The final user turn appended to the summary request, so the model emits the
/// handoff as its response.
const SUMMARY_NUDGE: &str =
    "Write the handoff for the next agent now, following the instructions above.";

/// `τ` from server usage: total prompt tokens plus output, optionally excluding
/// the provider-cached prefix. Per [`TokenUsage`]'s contract, the cache fields
/// are subsets of `input_tokens`, so occupancy is `input + output` and
/// excluding the cached read subtracts it from the input side.
pub(crate) fn context_occupancy(u: &TokenUsage, exclude_cached: bool) -> u64 {
    let input = if exclude_cached {
        u.input_tokens.saturating_sub(u.cache_read_input_tokens)
    } else {
        u.input_tokens
    };
    input.saturating_add(u.output_tokens)
}

/// A rough token estimate from character count (÷4). Used only where a
/// server-truth figure is not yet available: the R3 effectiveness check and the
/// post-compaction `τ` reset that prevents immediate re-triggering.
pub(crate) fn estimate_tokens(msgs: &[Message]) -> u64 {
    let chars: usize = msgs
        .iter()
        .flat_map(|m| m.content.iter())
        .map(part_len)
        .sum();
    (chars / 4) as u64
}

/// Nominal character cost of an image, used only in the size estimate. An image
/// is tokenized by dimension, not by base64 length, so counting `data.len()`
/// would wildly overestimate — enough to trip a false `Ineffective` when a
/// surviving user image dwarfs the real token budget.
const IMAGE_NOMINAL_CHARS: usize = 1024;

fn part_len(p: &ContentPart) -> usize {
    match p {
        ContentPart::Text { text } => text.len(),
        ContentPart::Thinking { text, .. } => text.len(),
        ContentPart::RedactedThinking { data } => data.len(),
        ContentPart::Image { .. } => IMAGE_NOMINAL_CHARS,
        ContentPart::ToolUse(t) => t.name.len() + t.input.to_string().len(),
        ContentPart::ToolResult(r) => r.content.len(),
    }
}

/// Whether a message is genuine user input (R2): a user-role message that is not
/// tool traffic. Tool-result messages are user-role on the wire but are the agent
/// traffic `σ` compresses, so they are excluded; everything else a user sends —
/// text, images, or both — is preserved.
///
/// Note: this does not yet exclude runtime-injected user fragments beyond the
/// handoff (the interruption marker, future context fragments) — the general
/// recognition predicate of ac-context.md owns that and is deferred with it.
/// The handoff `σ` *is* excluded from `U`, via [`is_injected_summary`].
pub(crate) fn is_user_input(m: &Message) -> bool {
    m.role == Role::User
        && !m
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolResult(_)))
}

/// Whether a message is a handoff `σ` this module injected on a prior compaction
/// — a user-role message whose text opens with [`HANDOFF_PREAMBLE`]. Such a
/// message is agent output, not user input, so it MUST NOT survive verbatim into
/// the next `U` (or repeated compaction accumulates every past summary, breaking
/// I5). It is still summarized *into* the next `σ`, since the summary request
/// sees the whole view.
pub(crate) fn is_injected_summary(m: &Message) -> bool {
    m.role == Role::User
        && m.content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text.starts_with(HANDOFF_PREAMBLE)))
}

/// Truncate a message's text parts to the per-message cap (R2). Non-text parts
/// pass through. The cap is a character budget (`cap_tokens × 4`); the split is
/// on a char boundary.
pub(crate) fn cap_message(m: &Message, cap_tokens: u64) -> Message {
    let cap_chars = (cap_tokens as usize).saturating_mul(4);
    let content = m
        .content
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } if text.chars().count() > cap_chars => {
                let mut t: String = text.chars().take(cap_chars).collect();
                t.push_str("… [truncated]");
                ContentPart::Text { text: t }
            }
            other => other.clone(),
        })
        .collect();
    Message {
        role: m.role,
        content,
        cache: false,
    }
}

/// `U`: the user inputs of `H`, verbatim, each capped (R2). Prior injected
/// handoffs are excluded — they are agent output that folds into the new `σ`,
/// not user input to carry forward.
pub(crate) fn survivors(view: &[Message], cap_tokens: u64) -> Vec<Message> {
    view.iter()
        .filter(|m| is_user_input(m) && !is_injected_summary(m))
        .map(|m| cap_message(m, cap_tokens))
        .collect()
}

/// `H′ = U ⧺ ⟨σ⟩`, with `σ` terminal (I4). For [`CompactionStrategy::FreshWindow`]
/// there is no `σ`, so `H′ = U`. `context′` (the system prompt) is re-applied
/// per request by the loop and is not part of the message vector.
pub(crate) fn build_replacement(
    mut u: Vec<Message>,
    summary: &str,
    strategy: CompactionStrategy,
) -> Vec<Message> {
    if strategy == CompactionStrategy::Summarize {
        u.push(Message::text(
            Role::User,
            format!("{HANDOFF_PREAMBLE}\n\n{summary}"),
        ));
    }
    u
}

/// The one-shot request that produces `σ`: the current view, a final nudge, the
/// handoff instruction as the system prompt, no tools.
pub(crate) fn build_summary_request(
    model: &str,
    system: String,
    mut view: Vec<Message>,
    max_tokens: u32,
) -> CompletionRequest {
    view.push(Message::text(Role::User, SUMMARY_NUDGE));
    let mut req = CompletionRequest::new(model);
    req.system = Some(system);
    req.cache_system = false;
    req.messages = view;
    req.tools = Vec::new();
    req.tool_choice = ToolChoice::None;
    req.max_tokens = Some(max_tokens);
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::{ToolResult, ToolUse};

    fn user(t: &str) -> Message {
        Message::text(Role::User, t)
    }
    fn assistant(t: &str) -> Message {
        Message::text(Role::Assistant, t)
    }
    fn tool_result(t: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ToolResult {
                tool_use_id: "c1".into(),
                content: t.into(),
                is_error: false,
            })],
            cache: false,
        }
    }

    #[test]
    fn is_user_input_keeps_user_text_and_rejects_tool_traffic() {
        assert!(is_user_input(&user("hi")));
        assert!(!is_user_input(&assistant("hi")));
        assert!(
            !is_user_input(&tool_result("output")),
            "tool results are agent traffic"
        );
        // An image-only user message is still user input (must survive in U).
        let image_only = Message {
            role: Role::User,
            content: vec![ContentPart::Image {
                media_type: "image/png".into(),
                data: "base64".into(),
            }],
            cache: false,
        };
        assert!(is_user_input(&image_only), "image-only user input survives");
    }

    #[test]
    fn a_prior_handoff_is_recognized_and_excluded_from_survivors() {
        let sigma = build_replacement(vec![], "PRIOR SUMMARY", CompactionStrategy::Summarize)
            .pop()
            .unwrap();
        assert!(is_injected_summary(&sigma), "the handoff σ is recognized");
        assert!(!is_injected_summary(&user("just a user message")));
        // A view that mixes a real user message with a prior σ: only the real
        // one survives, so repeated compaction cannot accumulate summaries (I5).
        let view = vec![user("real input"), sigma, assistant("work")];
        let u = survivors(&view, 4096);
        assert_eq!(u.len(), 1, "the prior σ is dropped from U");
        match &u[0].content[0] {
            ContentPart::Text { text } => assert_eq!(text, "real input"),
            _ => panic!(),
        }
    }

    #[test]
    fn survivors_keeps_user_input_verbatim_and_caps_the_pathological() {
        let big = "x".repeat(100);
        let view = vec![
            user("keep"),
            assistant("drop"),
            tool_result("drop"),
            user(&big),
        ];
        let u = survivors(&view, 5); // cap 5 tokens → 20 chars
        assert_eq!(u.len(), 2, "only the two user-text messages survive");
        match &u[0].content[0] {
            ContentPart::Text { text } => assert_eq!(text, "keep"),
            _ => panic!(),
        }
        match &u[1].content[0] {
            ContentPart::Text { text } => {
                assert!(text.starts_with(&"x".repeat(20)));
                assert!(
                    text.ends_with("[truncated]"),
                    "the oversized input is capped"
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn build_replacement_places_the_summary_last_and_frames_it() {
        let u = vec![user("q1"), user("q2")];
        let h = build_replacement(u.clone(), "DID THE WORK", CompactionStrategy::Summarize);
        assert_eq!(h.len(), 3, "U plus the terminal summary");
        let last = h.last().unwrap();
        assert_eq!(last.role, Role::User);
        match &last.content[0] {
            ContentPart::Text { text } => {
                assert!(text.contains("DID THE WORK"));
                assert!(
                    text.contains("handoff from a previous agent"),
                    "σ is framed as a handoff"
                );
            }
            _ => panic!(),
        }
        // Fresh window keeps U with no summary appended.
        let fresh = build_replacement(u, "ignored", CompactionStrategy::FreshWindow);
        assert_eq!(fresh.len(), 2, "fresh window is U alone");
    }

    #[test]
    fn context_occupancy_optionally_excludes_the_cached_prefix() {
        let u = TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_read_input_tokens: 800,
            cache_creation_input_tokens: 0,
        };
        assert_eq!(context_occupancy(&u, false), 1200);
        assert_eq!(
            context_occupancy(&u, true),
            400,
            "cached read excluded from the input side"
        );
    }

    #[test]
    fn estimate_tokens_counts_every_content_kind() {
        let msgs = vec![
            user(&"a".repeat(40)), // 40 chars
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse(ToolUse {
                    id: "c".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                })],
                cache: false,
            },
            tool_result(&"b".repeat(40)),
        ];
        // ~ (40 + ("read" + "{}") + 40) / 4 — dominated by the text, non-zero.
        assert!(estimate_tokens(&msgs) >= 20);
    }
}
