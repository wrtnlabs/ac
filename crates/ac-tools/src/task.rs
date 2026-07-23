//! The `task` tool: delegate a scoped sub-task to a child agent that runs to
//! completion and returns its result ([docs/ac-subagents.md] §2, §5).
//!
//! It is the model-facing surface over the injected [`ac_tool::AgentSpawner`]
//! capability. It is **not** a default built-in ([`crate::register_builtins`]
//! omits it): a host registers it on a parent run and leaves it out of a child's
//! surface — that, plus the child context's absent spawner, is the structural
//! recursion guard. When no spawner is installed it refuses as data (R5), never
//! a fault.

use std::sync::Arc;

use ac_tool::{Capability, Effort, SpawnRequest, SpawnStatus, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use serde::Deserialize;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskInput {
    /// The agent to delegate to (by definition name).
    pub agent: String,
    /// The task for the child — exactly what it should do and return.
    pub prompt: String,
    /// A short label for this delegation (for traces/UI).
    #[serde(default)]
    pub description: Option<String>,
    /// Per-child model override; omit to use the agent's default.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-child reasoning-effort override (reserved).
    #[serde(default)]
    pub effort: Option<String>,
}

/// Delegate a scoped sub-task to a child agent (see the module docs).
pub struct Task;

impl Tool for Task {
    type Input = TaskInput;

    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> String {
        "Delegate a scoped sub-task to a child agent, which runs to completion in \
         its own fresh context and returns only its result. Launch independent \
         tasks concurrently in one step; do not duplicate work you have delegated; \
         the result is not shown to the user, so summarize what matters. State \
         exactly what the child should investigate and return."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(self: Arc<Self>, input: TaskInput, ctx: Arc<ToolCtx>) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            // Refuse as data when the seam is absent — also the child-side guard:
            // a child ctx has `spawner: None`, so even a mis-registered `task`
            // self-refuses here rather than recursing.
            let Some(spawner) = ctx.spawner.clone() else {
                return ToolOutput::error("sub-agent delegation is not available here");
            };

            let result = spawner
                .spawn(SpawnRequest {
                    agent: input.agent,
                    prompt: input.prompt,
                    description: input.description,
                    model: input.model,
                    // The model writes a tier name; an unknown one is ignored
                    // (treated as no override), never a fault.
                    effort: input.effort.as_deref().and_then(Effort::parse),
                    // The parent's token; the spawner derives the child's via
                    // `child_token()` so cancel flows down, not up.
                    cancel: ctx.cancel.clone(),
                })
                .await;

            let envelope = |status: &str| {
                serde_json::json!({
                    "session_id": result.session_id,
                    "status": status,
                    "output": result.output,
                })
                .to_string()
            };

            match &result.status {
                SpawnStatus::Completed => ToolOutput::ok(envelope("completed")),
                // A bounded/aborted child is an error result, but its partial
                // output still rides along (§5) so the parent is not left blind.
                SpawnStatus::Aborted => ToolOutput::error(envelope("aborted")),
                SpawnStatus::Error(msg) => {
                    ToolOutput::error(format!("sub-agent delegation failed: {msg}"))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::{AgentSpawner, RefusingSpawner, SpawnResult, SubtreePolicy, as_dyn};
    use futures::future::BoxFuture;

    fn ctx_with(spawner: Option<Arc<dyn AgentSpawner>>) -> Arc<ToolCtx> {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir for the test's lifetime (kept simple; the policy only
        // needs the path to exist during the call).
        let path = dir.keep();
        let mut ctx = ToolCtx::new(Arc::new(SubtreePolicy::new(&path).unwrap()));
        if let Some(s) = spawner {
            ctx = ctx.with_spawner(s);
        }
        Arc::new(ctx)
    }

    fn input(agent: &str) -> TaskInput {
        TaskInput {
            agent: agent.into(),
            prompt: "do it".into(),
            description: None,
            model: None,
            effort: None,
        }
    }

    #[tokio::test]
    async fn refuses_as_data_when_no_spawner() {
        let out = Arc::new(Task).run(input("explore"), ctx_with(None)).await;
        assert!(out.is_error);
        assert!(out.content.contains("not available"));
        // A refusal is data, not a JSON envelope — nothing was spawned.
        assert!(!out.content.contains("session_id"));
    }

    #[tokio::test]
    async fn a_completed_child_returns_the_envelope() {
        struct Ok;
        impl AgentSpawner for Ok {
            fn spawn(&self, req: SpawnRequest) -> BoxFuture<'static, SpawnResult> {
                Box::pin(async move {
                    SpawnResult {
                        session_id: "s_child".into(),
                        output: format!("did: {}", req.prompt),
                        status: SpawnStatus::Completed,
                    }
                })
            }
        }
        let out = Arc::new(Task)
            .run(input("explore"), ctx_with(Some(as_dyn(Ok))))
            .await;
        assert!(
            !out.is_error,
            "completed delegation is not an error: {}",
            out.content
        );
        assert!(out.content.contains("\"session_id\":\"s_child\""));
        assert!(out.content.contains("\"status\":\"completed\""));
        assert!(out.content.contains("did: do it"));
    }

    #[tokio::test]
    async fn an_errored_child_is_error_data() {
        let out = Arc::new(Task)
            .run(input("explore"), ctx_with(Some(as_dyn(RefusingSpawner))))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("sub-agent delegation failed"));
    }
}
