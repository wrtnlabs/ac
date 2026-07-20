//! The generic host, factored into a library so the *shipped* wiring is exactly
//! what the offline e2e tests exercise. Both `src/main.rs` (the `ac` binary) and
//! `tests/e2e.rs` assemble their agent through [`build_host`] — there is one
//! wiring path, not two that can drift.
//!
//! This is the standing proof that AC works for a host with no app attached: it
//! wires a provider to the built-in tool registry over the runtime loop,
//! contained to a directory. It must never grow app-specific behavior.

use std::path::Path;
use std::sync::Arc;

use ac_provider::{Provider, ServerTool};
use ac_runtime::{AgentConfig, Session};
use ac_tool::{SubtreePolicy, ToolCtx, ToolRegistry};

/// A generic filesystem/coding agent persona. No host- or app-domain content —
/// this is the baseline for any workspace.
pub const SYSTEM_PROMPT: &str = "You are a capable, precise generic agent operating inside a single \
working directory. You can read, write, edit, and search files, and run shell commands, all \
contained to that directory. Prefer reading a file before editing it. Take the smallest set of \
actions that fully satisfies the request, verify your work when practical, and stop when done. \
Be concise.";

/// Default cap on tool-calling iterations within a single turn.
pub const MAX_ITERATIONS: usize = 24;

/// The built-in tool registry the generic host ships with. Exposed so a test
/// can assert the shipped tool set without going through a full turn.
pub fn generic_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    ac_tools::register_builtins(&mut registry);
    registry
}

/// An assembled generic agent: the session plus the shared run context, whose
/// cancel token the binary wires to Ctrl-C.
pub struct GenericHost {
    pub session: Session,
    pub ctx: Arc<ToolCtx>,
}

/// Options for [`build_host`] beyond the required provider/dir/model.
#[derive(Debug, Clone, Default)]
pub struct HostOptions {
    /// Request the provider's server-side web search each turn. The host opts
    /// in; whether it actually runs depends on the provider (a provider that
    /// can't do it ignores the request). Web search is NOT a built-in tool.
    pub web_search: bool,
}

/// Assemble the generic host over a chosen provider and sandbox directory. The
/// single wiring path — binary and tests both call it.
pub fn build_host(
    provider: Arc<dyn Provider>,
    dir: &Path,
    model: String,
    options: HostOptions,
) -> anyhow::Result<GenericHost> {
    let policy = SubtreePolicy::new(dir)
        .map_err(|e| anyhow::anyhow!("cannot use directory {}: {e}", dir.display()))?;
    let ctx = Arc::new(ToolCtx::new(Arc::new(policy)));
    let registry = Arc::new(generic_registry());

    let mut server_tools = Vec::new();
    if options.web_search {
        server_tools.push(ServerTool::WebSearch {
            max_results: Some(5),
        });
    }

    let config = AgentConfig {
        model,
        system: Some(SYSTEM_PROMPT.to_string()),
        max_iterations: MAX_ITERATIONS,
        server_tools,
        ..Default::default()
    };

    let session = Session::new(provider, registry, ctx.clone(), config);
    Ok(GenericHost { session, ctx })
}
