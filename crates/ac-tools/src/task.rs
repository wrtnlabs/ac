//! The `task` tool: delegate a scoped sub-task to a child agent that runs to
//! completion and returns its result ([docs/ac-subagents.md] §2, §5).
//!
//! It is the model-facing surface over the injected [`ac_tool::AgentSpawner`]
//! capability. A host constructs it from the same [`ac_tool::AgentDefinition`]
//! entries its spawner resolves, so the tool description advertises the exact
//! names and descriptions available to the parent model. It is **not** a default
//! built-in ([`crate::register_builtins`] omits it): a host registers it on a
//! parent run and leaves it out of a child's surface — that, plus the child
//! context's absent spawner, is the structural recursion guard. When no spawner
//! is installed it refuses as data (R5), never a fault.

use std::sync::Arc;

use ac_tool::{
    AgentDefinition, Capability, Effort, SpawnRequest, SpawnStatus, Tool, ToolCtx, ToolOutput,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentCatalogEntry {
    name: String,
    description: String,
}

/// Delegate a scoped sub-task to a child agent (see the module docs).
///
/// Construct this from the same definitions the host's spawner resolves. Task
/// keeps only the model-facing name and description; child prompts, tool scopes,
/// models, and effort remain host data behind the spawning seam.
pub struct Task {
    agents: Vec<AgentCatalogEntry>,
}

impl Task {
    pub fn new<'a>(definitions: impl IntoIterator<Item = &'a AgentDefinition>) -> Self {
        Self {
            agents: definitions
                .into_iter()
                .map(|definition| AgentCatalogEntry {
                    name: definition.name.clone(),
                    description: definition.description.clone(),
                })
                .collect(),
        }
    }
}

impl Tool for Task {
    type Input = TaskInput;

    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> String {
        let mut description =
            "Delegate a scoped sub-task to a child agent, which runs to completion in \
             its own fresh context and returns only its result. Launch independent \
             tasks concurrently in one step; do not duplicate work you have delegated; \
             the result is not shown to the user, so summarize what matters. State \
             exactly what the child should investigate and return."
                .to_string();
        if self.agents.is_empty() {
            description.push_str("\n\nNo child agents are configured.");
        } else {
            description.push_str("\n\nAvailable agents (pass the exact name in `agent`):");
            for agent in &self.agents {
                description.push_str(&format!("\n- `{}` — {}", agent.name, agent.description));
            }
        }
        description
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
            let Some(tool_call_id) = ctx.tool_call_id().map(str::to_owned) else {
                return ToolOutput::error(
                    "sub-agent delegation requires an invocation-scoped tool context",
                );
            };

            let result = spawner
                .spawn(SpawnRequest {
                    tool_call_id,
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
                // Preserve the child handle in durable error data too. A host
                // that failed before creating a child supplies an empty id;
                // failures after creation remain resumable/inspectable.
                SpawnStatus::Error(message) => ToolOutput::error(
                    serde_json::json!({
                        "session_id": result.session_id,
                        "status": "error",
                        "output": result.output,
                        "error": message,
                    })
                    .to_string(),
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::{AgentSpawner, SpawnResult, SubtreePolicy, as_dyn};
    use futures::future::BoxFuture;

    fn task() -> Task {
        Task::new(&[AgentDefinition::new(
            "explore",
            "Read-only investigation and synthesis.",
        )])
    }

    fn ctx_with(spawner: Option<Arc<dyn AgentSpawner>>) -> Arc<ToolCtx> {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir for the test's lifetime (kept simple; the policy only
        // needs the path to exist during the call).
        let path = dir.keep();
        let mut ctx = ToolCtx::new(Arc::new(SubtreePolicy::new(&path).unwrap()));
        if let Some(s) = spawner {
            ctx = ctx.with_spawner(s);
        }
        Arc::new(ctx.for_invocation("call_task"))
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
        let out = Arc::new(task()).run(input("explore"), ctx_with(None)).await;
        assert!(out.is_error);
        assert!(out.content.contains("not available"));
        // A refusal is data, not a JSON envelope — nothing was spawned.
        assert!(!out.content.contains("session_id"));
    }

    #[test]
    fn description_advertises_host_supplied_agent_names_and_descriptions() {
        let task = Task::new(&[
            AgentDefinition::new("explore", "Read-only investigation and synthesis."),
            AgentDefinition::new("general", "Full worker for self-contained implementation."),
        ]);

        let description = task.description();

        assert!(description.contains("Available agents"));
        assert!(description.contains("`explore` — Read-only investigation and synthesis."));
        assert!(description.contains("`general` — Full worker for self-contained implementation."));
    }

    #[tokio::test]
    async fn a_completed_child_returns_the_envelope() {
        struct Ok {
            seen_call_id: Arc<std::sync::Mutex<Option<String>>>,
        }
        impl AgentSpawner for Ok {
            fn spawn(&self, req: SpawnRequest) -> BoxFuture<'static, SpawnResult> {
                *self.seen_call_id.lock().unwrap() = Some(req.tool_call_id.clone());
                Box::pin(async move {
                    SpawnResult {
                        session_id: "s_child".into(),
                        output: format!("did: {}", req.prompt),
                        status: SpawnStatus::Completed,
                    }
                })
            }
        }
        let seen_call_id = Arc::new(std::sync::Mutex::new(None));
        let out = Arc::new(task())
            .run(
                input("explore"),
                ctx_with(Some(as_dyn(Ok {
                    seen_call_id: seen_call_id.clone(),
                }))),
            )
            .await;
        assert!(
            !out.is_error,
            "completed delegation is not an error: {}",
            out.content
        );
        assert!(out.content.contains("\"session_id\":\"s_child\""));
        assert!(out.content.contains("\"status\":\"completed\""));
        assert!(out.content.contains("did: do it"));
        assert_eq!(seen_call_id.lock().unwrap().as_deref(), Some("call_task"));
    }

    #[tokio::test]
    async fn an_errored_child_keeps_its_session_handle_in_error_data() {
        struct Failed;
        impl AgentSpawner for Failed {
            fn spawn(&self, _req: SpawnRequest) -> BoxFuture<'static, SpawnResult> {
                Box::pin(std::future::ready(SpawnResult {
                    session_id: "s_failed".into(),
                    output: "partial child text".into(),
                    status: SpawnStatus::Error("provider failed".into()),
                }))
            }
        }

        let out = Arc::new(task())
            .run(input("explore"), ctx_with(Some(as_dyn(Failed))))
            .await;

        assert!(out.is_error);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out.content).unwrap(),
            serde_json::json!({
                "session_id": "s_failed",
                "status": "error",
                "output": "partial child text",
                "error": "provider failed",
            })
        );
        assert_eq!(out.durable_content(), out.content);
    }
}
