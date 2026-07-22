//! The sub-agent seam ([docs/ac-subagents.md]). Delegation is an **injected
//! capability**, the same shape as the OS-sandbox launcher: the run context
//! ([`crate::ToolCtx`]) carries an optional spawner; `None` means delegation is
//! unavailable here, and a `task`-style tool refuses as data. This module is the
//! pure-data seam — it names no `Session`, `Rollout`, or `Provider` type,
//! because it lives beneath the runtime; a host (or a kit reference
//! implementation in the runtime layer) supplies the mechanism behind it.
//!
//! The recursion guard is structural: the spawner installs no spawner into a
//! child's context, so a child *cannot express* delegation — there is nothing to
//! bypass, and depth is one by default (§4).

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

/// The delegation capability. The kit expresses *intent* (a [`SpawnRequest`]); a
/// host-supplied spawner assembles and drives a fresh child run and returns its
/// outcome as data. A child **failure is data** in [`SpawnResult::status`],
/// never an `Err` — there is no error channel that escapes the seam
/// ([docs/ac-subagents.md] R5).
pub trait AgentSpawner: Send + Sync {
    fn spawn(&self, req: SpawnRequest) -> BoxFuture<'static, SpawnResult>;
}

/// A request to run one child agent to completion (§2). Carries no depth or
/// parent handle: a host that opts into bounded recursion tracks depth in its
/// own spawner state (§4), so the agnostic request stays minimal.
pub struct SpawnRequest {
    /// Which agent definition to run (resolved by the host spawner).
    pub agent: String,
    /// The child's initial input.
    pub prompt: String,
    /// A short trace/UI label for this delegation.
    pub description: Option<String>,
    /// Per-child model override; `None` inherits the definition's default.
    pub model: Option<String>,
    /// Per-child reasoning-effort override, referencing the agnostic provider
    /// tier. Reserved and inert until effort lands as a request parameter (§6);
    /// carried now so adopting it later reopens nothing.
    pub effort: Option<String>,
    /// The parent's cancellation signal. A spawner MUST *derive* the child's
    /// token from this (`cancel.child_token()`), not share it, so parent-cancel
    /// propagates down while a child abort never bubbles up (§4, I5).
    pub cancel: CancellationToken,
}

/// The outcome of a child run (§5). The status is the whole story — there is no
/// `Err` arm, so a child failure reaches the parent model only as tool data.
pub struct SpawnResult {
    /// The child's session identifier — the handle a later resume would name
    /// (resume itself is deferred).
    pub session_id: String,
    /// The child's **final text**: the content of its terminal assistant message
    /// (the step that issued no further tool calls), not a fold of every
    /// utterance. Empty when the child produced none.
    pub output: String,
    pub status: SpawnStatus,
}

/// How a child run ended (§5), mapping the loop's termination paths onto three
/// values a host reports.
pub enum SpawnStatus {
    /// A normal stop.
    Completed,
    /// Cancellation, an exhausted iteration bound, or an idle timeout. Any text
    /// the child recorded before the bound still rides [`SpawnResult::output`].
    Aborted,
    /// A turn-terminating failure (the provider's failure taxonomy), or the host
    /// being unable to assemble the child at all. The string is model-facing.
    Error(String),
}

/// A host-supplied **agent definition** — the shape the kit ships; the
/// definitions themselves are host content (§2). "Sub-agent" is not a type: a
/// definition is just a definition, and "sub" exists only relationally (the tool
/// scope that omits delegation, the fresh child context, the host's
/// spawner→child link).
pub struct AgentDefinition {
    /// The name the spawner resolves and the parent model delegates to.
    pub name: String,
    /// What the parent model reads when choosing whom to delegate to.
    pub description: String,
    /// The child's system prompt; `None` inherits the host default.
    pub prompt: Option<String>,
    /// Which tools the child's surface carries. The delegation tool is never
    /// among them (the structural recursion guard, §4).
    pub tools: ToolScope,
    /// Default model for this agent; a [`SpawnRequest`] may override.
    pub model: Option<String>,
    /// Narrow the child's containment to reads only (§4).
    pub read_only: bool,
}

impl AgentDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            prompt: None,
            tools: ToolScope::All,
            model: None,
            read_only: false,
        }
    }

    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    pub fn with_tools(mut self, tools: ToolScope) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

/// A filter over the tool registry for a child's surface. `Allow`/`Deny` name
/// tools; the host applies the filter when it (re)builds the child registry.
pub enum ToolScope {
    All,
    Allow(Vec<String>),
    Deny(Vec<String>),
}

impl ToolScope {
    /// Whether a tool named `name` is in scope. The delegation tool is excluded
    /// by the host regardless (the recursion guard does not depend on this).
    pub fn admits(&self, name: &str) -> bool {
        match self {
            ToolScope::All => true,
            ToolScope::Allow(names) => names.iter().any(|n| n == name),
            ToolScope::Deny(names) => !names.iter().any(|n| n == name),
        }
    }
}

/// A no-op spawner, for hosts and tests that want the seam present but inert:
/// every spawn returns an error result. Never runs anything.
pub struct RefusingSpawner;

impl AgentSpawner for RefusingSpawner {
    fn spawn(&self, _req: SpawnRequest) -> BoxFuture<'static, SpawnResult> {
        Box::pin(async {
            SpawnResult {
                session_id: String::new(),
                output: String::new(),
                status: SpawnStatus::Error("delegation is not available".into()),
            }
        })
    }
}

/// Convenience: an `Arc<dyn AgentSpawner>` from any spawner.
pub fn as_dyn(spawner: impl AgentSpawner + 'static) -> Arc<dyn AgentSpawner> {
    Arc::new(spawner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_scope_admits_by_rule() {
        assert!(ToolScope::All.admits("anything"));
        let allow = ToolScope::Allow(vec!["read_file".into(), "grep".into()]);
        assert!(allow.admits("read_file"));
        assert!(!allow.admits("write_file"));
        let deny = ToolScope::Deny(vec!["shell".into()]);
        assert!(deny.admits("read_file"));
        assert!(!deny.admits("shell"));
    }

    #[test]
    fn agent_definition_builds() {
        let def = AgentDefinition::new("explore", "read-only researcher")
            .with_prompt("investigate")
            .with_tools(ToolScope::Allow(vec!["read_file".into()]))
            .read_only();
        assert_eq!(def.name, "explore");
        assert!(def.read_only);
        assert!(def.tools.admits("read_file"));
        assert!(!def.tools.admits("write_file"));
    }

    #[tokio::test]
    async fn refusing_spawner_always_errors() {
        let out = RefusingSpawner
            .spawn(SpawnRequest {
                agent: "x".into(),
                prompt: "p".into(),
                description: None,
                model: None,
                effort: None,
                cancel: CancellationToken::new(),
            })
            .await;
        assert!(matches!(out.status, SpawnStatus::Error(_)));
    }
}
