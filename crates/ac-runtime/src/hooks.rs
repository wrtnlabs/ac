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

use std::collections::HashSet;
use std::sync::Arc;

use ac_provider::{CompletionRequest, ToolChoice};
use ac_types::{ContentPart, Message};

/// The step-prepare phase: edits the request about to be sampled. Composes in
/// registration order; each edit lives for one request (the loop rebuilds from
/// scratch each step). MUST be a pure function of (step index, request) — see
/// the module docs on stateless derivation.
pub trait StepPrepareHook: Send + Sync {
    fn prepare(&self, iteration: usize, request: &mut CompletionRequest);
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

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::{Role, ToolResult, ToolUse};

    fn tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse(ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::Value::Null,
            })],
            cache: false,
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
            cache: false,
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
}
