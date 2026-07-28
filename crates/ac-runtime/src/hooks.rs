//! The lifecycle-phase hook taxonomy ([docs/ac-hooks.md]).
//!
//! The loop's one historical extension seam — a step hook that edits the
//! outgoing request — is the right seam for exactly one job (per-request
//! shaping) and the wrong *lifetime* for every other kind of host logic (§1).
//! The fix is **phase honesty**: split the one hook into phases, each with the
//! least authority its purpose needs, so over-reach is a type error, not a
//! review finding (I6, authority by shape).
//!
//! Two phases ship wired into the loop:
//!
//! - **step-prepare** ([`StepPrepareHook`]) — the live hook, unchanged in
//!   authority: it edits the outgoing request (model, tool filter, system
//!   prompt, tool choice), its edits live for that one request, and contributors
//!   fold in registration order, each seeing its predecessors' edits (R5). A
//!   step-prepare hook MUST be a pure function of (step index, request) and MUST
//!   NOT carry state from step to step (§3). Because the request already carries
//!   the effective history as `request.messages`, a precondition-gating hook
//!   derives its verdict from *that* — never from a process-local flag that a
//!   resume or fork would desynchronize. [`ForcedChainHook`] is the worked
//!   example: the stateless forced chain the RFC's §3 prescribes.
//! - **observation** ([`ObservationHook`]) — sees tool traffic and contributes
//!   NOTHING: its input is immutable and there is no return, so an observer
//!   cannot mutate what it watches (R4/I6). Removing every observation
//!   contributor changes no model-visible byte of any request or history item
//!   (I4, passivity). Pairing is not guaranteed — a `ToolFinish` MAY arrive
//!   without its `ToolStart` if a call is cancelled before dispatch.
//!
//! The two **contributing** phases of the taxonomy — *session-context* (durable
//! per-window fragments) and *turn-input* (per-turn mention injections) — are
//! deferred: their contributions enter history as *marked* fragments
//! ([docs/ac-context.md] R1), so they land together with ac-context's
//! window/turn cadence DRIVERS (deferred there for the same reason) and a
//! concrete host consumer. The **lifecycle** phase (scope brackets for
//! private-state seeding and flush) lands with its first consumer. Defining a
//! phase ahead of any caller would be authority without a use; the taxonomy's
//! value is authority-by-shape at the point of use, so each phase arrives with
//! the code that needs it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ac_provider::{CompletionRequest, ToolChoice};
use ac_types::{CacheMark, CacheTtl, ContentPart, Message};

/// The step-prepare phase: edits the request about to be sampled. Composes in
/// registration order; each edit lives for one request (the loop rebuilds from
/// scratch each step). MUST be a pure function of (step index, request) — see
/// the module docs on stateless derivation.
pub trait StepPrepareHook: Send + Sync {
    fn prepare(&self, iteration: usize, request: &mut CompletionRequest);
}

/// Mark the last `message_count` cacheable messages, and optionally the system
/// prompt, as provider-cache breakpoints on every step.
///
/// Marks are rebuilt from the request each time: messages that leave the tail
/// are explicitly cleared, so the hook carries no process-local state. A
/// message is cacheable when its wire encoding can actually carry a mark:
/// it has a text part or a tool result. Image-only transient rows are skipped.
pub struct TailCacheHook {
    message_count: usize,
    mark: CacheMark,
    cache_system: bool,
}

impl TailCacheHook {
    pub fn new(message_count: usize, ttl: CacheTtl, cache_system: bool) -> Self {
        Self {
            message_count,
            mark: CacheMark::WithTtl(ttl),
            cache_system,
        }
    }
}

impl StepPrepareHook for TailCacheHook {
    fn prepare(&self, _iteration: usize, request: &mut CompletionRequest) {
        for message in &mut request.messages {
            message.cache = CacheMark::Off;
        }
        for message in request
            .messages
            .iter_mut()
            .rev()
            .filter(|message| {
                message.content.iter().any(|part| {
                    matches!(part, ContentPart::Text { .. } | ContentPart::ToolResult(_))
                })
            })
            .take(self.message_count)
        {
            message.cache = self.mark;
        }
        request.cache_system = if self.cache_system && request.system.is_some() {
            self.mark
        } else {
            CacheMark::Off
        };
    }
}

/// Keep provider-executed server tools on step zero and remove them from every
/// later step of the turn.
///
/// This is useful for request-level capabilities such as automatic web search:
/// unlike a client tool, they can fire merely because they are present on the
/// request, so repeating them after tool results is both costly and surprising.
pub struct FirstStepServerToolsOnly;

impl StepPrepareHook for FirstStepServerToolsOnly {
    fn prepare(&self, iteration: usize, request: &mut CompletionRequest) {
        if iteration > 0 {
            request.server_tools.clear();
        }
    }
}

/// Stateless visibility for a set of latent tools.
///
/// The configured tools remain in the registry, but their schemas stay out of
/// completion requests until a successful `reveal_tool` result names them in
/// `{"matched":[{"name":"..."}]}`. Visibility is derived from effective
/// message history on every step, never retained in process-local state, so
/// resume, fork, and compaction cannot desynchronize it.
///
/// Dispatch enforcement is owned by `Session`: a provider-emitted call is
/// executable only when that tool was actually offered on the corresponding
/// request. Filtering here is therefore both a context-cost optimization and a
/// capability boundary.
pub struct ConditionalToolsHook {
    gated: HashSet<String>,
    reveal_tool: String,
}

impl ConditionalToolsHook {
    pub fn new(gated: impl IntoIterator<Item = String>, reveal_tool: impl Into<String>) -> Self {
        Self {
            gated: gated.into_iter().collect(),
            reveal_tool: reveal_tool.into(),
        }
    }

    pub fn gated(&self) -> &HashSet<String> {
        &self.gated
    }

    fn revealed(&self, messages: &[Message]) -> HashSet<String> {
        let mut uses: HashMap<&str, &str> = HashMap::new();
        let mut revealed = HashSet::new();

        for part in messages.iter().flat_map(|message| message.content.iter()) {
            match part {
                ContentPart::ToolUse(tool_use) => {
                    uses.insert(tool_use.id.as_str(), tool_use.name.as_str());
                }
                ContentPart::ToolResult(result)
                    if !result.is_error
                        && uses.get(result.tool_use_id.as_str()).copied()
                            == Some(self.reveal_tool.as_str()) =>
                {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&result.content)
                    else {
                        continue;
                    };
                    let Some(matched) = value.get("matched").and_then(|value| value.as_array())
                    else {
                        continue;
                    };
                    for hit in matched {
                        if let Some(name) = hit.get("name").and_then(|value| value.as_str())
                            && self.gated.contains(name)
                        {
                            revealed.insert(name.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        revealed
    }
}

impl StepPrepareHook for ConditionalToolsHook {
    fn prepare(&self, _iteration: usize, request: &mut CompletionRequest) {
        if self.gated.is_empty() {
            return;
        }
        let revealed = self.revealed(&request.messages);
        request
            .tools
            .retain(|tool| !self.gated.contains(&tool.name) || revealed.contains(&tool.name));
    }
}

/// What an [`ObservationHook`] is told. Immutable by construction — observation
/// has no authority to change anything (I6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// A tool call is about to be dispatched.
    ToolStart { id: String, name: String },
    /// A tool call produced a result (success or tool-authored error).
    ToolFinish {
        id: String,
        name: String,
        is_error: bool,
    },
}

/// The observation phase: watches tool traffic, contributes nothing. Attribution
/// and accounting live here — anything that needs to *see* the loop's work but
/// must not shape it.
pub trait ObservationHook: Send + Sync {
    fn observe(&self, event: &Observation);
}

/// The frozen-at-construction registry of phase contributors, one ordered list
/// per wired phase (§3). Composition within a phase is registration order (R5);
/// the runtime never reorders.
#[derive(Default)]
pub struct HookRegistry {
    step_prepare: Vec<Arc<dyn StepPrepareHook>>,
    observation: Vec<Arc<dyn ObservationHook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_step_prepare(&mut self, hook: Arc<dyn StepPrepareHook>) {
        self.step_prepare.push(hook);
    }

    pub fn add_observation(&mut self, hook: Arc<dyn ObservationHook>) {
        self.observation.push(hook);
    }

    /// The step-prepare contributors, in registration order — the loop folds the
    /// request through them.
    pub(crate) fn step_prepare(&self) -> &[Arc<dyn StepPrepareHook>] {
        &self.step_prepare
    }

    /// Fan an observation out to every observer, in registration order. A no-op
    /// when none are registered (I4).
    pub(crate) fn observe(&self, event: &Observation) {
        for hook in &self.observation {
            hook.observe(event);
        }
    }
}

/// A stateless forced-chain step-prepare hook (§3): forces the model to call
/// `tool` until the effective history contains a **successful** result of it,
/// then releases the choice. The verdict is read from `request.messages` — the
/// effective history `E(L)` — so resume and fork are correct for free (I5): a
/// resumed session whose log shows the bind does not re-force, and a branch cut
/// before the bind forces again. There is no second source of truth (a flag) to
/// desynchronize — the anti-pattern §1 names.
pub struct ForcedChainHook {
    tool: String,
}

impl ForcedChainHook {
    pub fn new(tool: impl Into<String>) -> Self {
        Self { tool: tool.into() }
    }

    /// Has `tool` produced a successful result anywhere in `messages`? True iff
    /// some assistant `ToolUse` named `tool` has a matching non-error
    /// `ToolResult`. An errored result does not satisfy — the chain keeps
    /// forcing until the precondition genuinely holds.
    fn satisfied(messages: &[Message], tool: &str) -> bool {
        let ids: HashSet<&str> = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolUse(tu) if tu.name == tool => Some(tu.id.as_str()),
                _ => None,
            })
            .collect();
        messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|part| match part {
                ContentPart::ToolResult(tr) => {
                    !tr.is_error && ids.contains(tr.tool_use_id.as_str())
                }
                _ => false,
            })
    }
}

impl StepPrepareHook for ForcedChainHook {
    fn prepare(&self, _iteration: usize, request: &mut CompletionRequest) {
        if !Self::satisfied(&request.messages, &self.tool) {
            request.tool_choice = ToolChoice::Force(self.tool.clone());
        }
    }
}

/// A [`ForcedChainHook`] that releases after `max_error_attempts` failed calls.
///
/// Both success and the retry budget are derived from effective history, so
/// resume and fork preserve the verdict without a mutable counter. Releasing
/// the hard tool choice does not make the precondition true; host tools must
/// still enforce their own policy when the model continues unbound.
pub struct BoundedForcedChainHook {
    inner: ForcedChainHook,
    tool: String,
    max_error_attempts: usize,
}

impl BoundedForcedChainHook {
    pub fn new(tool: impl Into<String>, max_error_attempts: usize) -> Self {
        let tool = tool.into();
        Self {
            inner: ForcedChainHook::new(tool.clone()),
            tool,
            max_error_attempts,
        }
    }

    fn error_attempts(&self, messages: &[Message]) -> usize {
        let ids: HashSet<&str> = messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|part| match part {
                ContentPart::ToolUse(tool_use) if tool_use.name == self.tool => {
                    Some(tool_use.id.as_str())
                }
                _ => None,
            })
            .collect();
        messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter(|part| {
                matches!(
                    part,
                    ContentPart::ToolResult(result)
                        if result.is_error && ids.contains(result.tool_use_id.as_str())
                )
            })
            .count()
    }
}

impl StepPrepareHook for BoundedForcedChainHook {
    fn prepare(&self, iteration: usize, request: &mut CompletionRequest) {
        if self.error_attempts(&request.messages) < self.max_error_attempts {
            self.inner.prepare(iteration, request);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::{CacheMark, Role, ToolResult, ToolUse};

    fn tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse(ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::Value::Null,
            })],
            cache: CacheMark::Off,
        }
    }

    fn tool_result(tool_use_id: &str, is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ToolResult {
                tool_use_id: tool_use_id.into(),
                content: "r".into(),
                is_error,
            })],
            cache: CacheMark::Off,
        }
    }

    fn search_result(tool_use_id: &str, names: &[&str]) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: serde_json::json!({
                    "matched": names
                        .iter()
                        .map(|name| serde_json::json!({"name": name}))
                        .collect::<Vec<_>>()
                })
                .to_string(),
                is_error: false,
            })],
            cache: CacheMark::Off,
        }
    }

    fn choice_for(hook: &ForcedChainHook, messages: Vec<Message>) -> ToolChoice {
        let mut req = CompletionRequest::new("m");
        req.messages = messages;
        hook.prepare(0, &mut req);
        req.tool_choice
    }

    #[test]
    fn forces_until_a_successful_result_is_in_history() {
        let hook = ForcedChainHook::new("bind");
        // Empty history → force.
        assert_eq!(choice_for(&hook, vec![]), ToolChoice::Force("bind".into()));
        // The tool was called but errored → still force (precondition unmet).
        assert_eq!(
            choice_for(&hook, vec![tool_use("c1", "bind"), tool_result("c1", true)]),
            ToolChoice::Force("bind".into())
        );
        // A successful result → release (default Auto).
        assert_eq!(
            choice_for(
                &hook,
                vec![tool_use("c2", "bind"), tool_result("c2", false)]
            ),
            ToolChoice::Auto
        );
    }

    #[test]
    fn a_success_for_a_different_tool_does_not_satisfy() {
        let hook = ForcedChainHook::new("bind");
        // `other` succeeded, `bind` never did.
        assert_eq!(
            choice_for(
                &hook,
                vec![tool_use("c1", "other"), tool_result("c1", false)]
            ),
            ToolChoice::Force("bind".into())
        );
    }

    #[test]
    fn the_verdict_is_the_same_on_a_resumed_history() {
        // The whole point (I5): the decision is a function of history, so a
        // session rebuilt from that history reaches the identical verdict — no
        // flag resets to "unbound" on resume.
        let hook = ForcedChainHook::new("bind");
        let bound_history = vec![tool_use("c2", "bind"), tool_result("c2", false)];
        assert_eq!(choice_for(&hook, bound_history.clone()), ToolChoice::Auto);
        // Same history handed to a fresh hook (the resume case) → still Auto.
        let resumed = ForcedChainHook::new("bind");
        assert_eq!(choice_for(&resumed, bound_history), ToolChoice::Auto);
    }

    #[test]
    fn observation_registry_fans_out_and_is_a_noop_when_empty() {
        use std::sync::Mutex;
        struct Recorder(Arc<Mutex<Vec<String>>>);
        impl ObservationHook for Recorder {
            fn observe(&self, event: &Observation) {
                if let Observation::ToolStart { name, .. } = event {
                    self.0.lock().unwrap().push(name.clone());
                }
            }
        }
        let mut reg = HookRegistry::new();
        // No observers: observe is a no-op (I4).
        reg.observe(&Observation::ToolStart {
            id: "x".into(),
            name: "dropped".into(),
        });

        let log = Arc::new(Mutex::new(Vec::new()));
        reg.add_observation(Arc::new(Recorder(log.clone())));
        reg.add_observation(Arc::new(Recorder(log.clone())));
        reg.observe(&Observation::ToolStart {
            id: "y".into(),
            name: "seen".into(),
        });
        // Both observers ran, in order; the pre-registration event left nothing.
        assert_eq!(*log.lock().unwrap(), vec!["seen", "seen"]);
    }

    #[test]
    fn conditional_tools_are_derived_from_durable_history() {
        let hook =
            ConditionalToolsHook::new(["mcp__tracker__create_item".to_string()], "tool_search");
        let mut request = CompletionRequest::new("model");
        request.tools = vec![
            ac_types::ToolSpec {
                name: "tool_search".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            ac_types::ToolSpec {
                name: "mcp__tracker__create_item".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type":"object"}),
            },
        ];

        hook.prepare(0, &mut request);
        assert_eq!(
            request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["tool_search"]
        );

        request.messages = vec![
            tool_use("search-1", "tool_search"),
            search_result("search-1", &["mcp__tracker__create_item"]),
        ];
        request.tools.push(ac_types::ToolSpec {
            name: "mcp__tracker__create_item".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        hook.prepare(1, &mut request);
        assert!(
            request
                .tools
                .iter()
                .any(|tool| tool.name == "mcp__tracker__create_item")
        );
    }

    #[test]
    fn conditional_tools_ignore_unknown_names() {
        let hook = ConditionalToolsHook::new(["mcp__known".to_string()], "tool_search");
        let mut request = CompletionRequest::new("model");
        request.tools = vec![ac_types::ToolSpec {
            name: "mcp__known".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
        }];
        request.messages = vec![
            tool_use("search-1", "tool_search"),
            search_result("search-1", &["mcp__unknown"]),
        ];
        hook.prepare(0, &mut request);
        assert!(request.tools.is_empty());
    }

    #[test]
    fn bounded_forced_chain_releases_after_the_error_budget() {
        let hook = BoundedForcedChainHook::new("bind", 2);
        let mut request = CompletionRequest::new("model");
        hook.prepare(0, &mut request);
        assert_eq!(request.tool_choice, ToolChoice::Force("bind".into()));

        request.messages = vec![
            tool_use("c1", "bind"),
            tool_result("c1", true),
            tool_use("c2", "bind"),
            tool_result("c2", true),
        ];
        request.tool_choice = ToolChoice::Auto;
        hook.prepare(2, &mut request);
        assert_eq!(request.tool_choice, ToolChoice::Auto);
    }

    #[test]
    fn tail_cache_rebuilds_marks_and_marks_system_only_when_present() {
        let hook = TailCacheHook::new(2, CacheTtl::OneHour, true);
        let mut request = CompletionRequest::new("model");
        request.messages = vec![
            Message::text(Role::User, "one"),
            Message::text(Role::Assistant, "two"),
            Message::text(Role::User, "three"),
        ];
        request.messages[0].cache = CacheMark::On;
        request.system = Some("system".to_string());

        hook.prepare(0, &mut request);
        assert_eq!(request.messages[0].cache, CacheMark::Off);
        assert_eq!(
            request.messages[1].cache,
            CacheMark::WithTtl(CacheTtl::OneHour)
        );
        assert_eq!(
            request.messages[2].cache,
            CacheMark::WithTtl(CacheTtl::OneHour)
        );
        assert_eq!(request.cache_system, CacheMark::WithTtl(CacheTtl::OneHour));
    }

    #[test]
    fn tail_cache_skips_transient_image_only_rows() {
        let hook = TailCacheHook::new(2, CacheTtl::OneHour, false);
        let mut request = CompletionRequest::new("model");
        request.messages = vec![
            Message::text(Role::User, "one"),
            Message {
                role: Role::User,
                content: vec![ContentPart::ToolResult(ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "result".into(),
                    is_error: false,
                })],
                cache: CacheMark::Off,
            },
            Message {
                role: Role::User,
                content: vec![ContentPart::Image {
                    media_type: "image/png".into(),
                    data: "QUJD".into(),
                }],
                cache: CacheMark::On,
            },
        ];

        hook.prepare(0, &mut request);

        assert_eq!(
            request.messages[0].cache,
            CacheMark::WithTtl(CacheTtl::OneHour)
        );
        assert_eq!(
            request.messages[1].cache,
            CacheMark::WithTtl(CacheTtl::OneHour)
        );
        assert_eq!(
            request.messages[2].cache,
            CacheMark::Off,
            "the provider encoder cannot attach a mark to an image-only row"
        );
    }

    #[test]
    fn provider_server_tools_are_first_step_only() {
        let hook = FirstStepServerToolsOnly;
        let mut request = CompletionRequest::new("model");
        request.server_tools = vec![ac_provider::ServerTool::WebSearch {
            max_results: Some(5),
        }];
        hook.prepare(0, &mut request);
        assert_eq!(request.server_tools.len(), 1);
        hook.prepare(1, &mut request);
        assert!(request.server_tools.is_empty());
    }
}
