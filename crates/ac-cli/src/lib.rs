//! The generic host, factored into a library so the *shipped* wiring is exactly
//! what the offline e2e tests exercise. Both `src/main.rs` (the `ac` binary) and
//! `tests/e2e.rs` assemble their agent through [`build_host`] — there is one
//! wiring path, not two that can drift.
//!
//! This is the standing proof that AC works for a host with no app attached: it
//! wires a provider to the built-in tool registry over the runtime loop,
//! contained to a directory. It must never grow app-specific behavior.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ac_provider::{CompletionRequest, Provider, ServerTool, ToolChoice};
use ac_runtime::{AgentConfig, Session, StepHook};
use ac_skills::{LoadSkillTool, SkillLayer, SkillsResolver};
use ac_tool::{NetworkMode, PathPolicy, SandboxPolicy, SubtreePolicy, ToolCtx, ToolRegistry};
use ac_types::{ContentPart, Role};

/// Build the OS-sandbox policy for the generic host: writes contained to the
/// workspace, reads to the workspace, the mandatory secret set denied, and
/// network per the flag. Writes are deliberately workspace-only in v1 (not the
/// whole system temp dir — that would let any command scribble across
/// `$TMPDIR`); a host that needs a scratch dir adds it to `write_roots`
/// explicitly.
fn build_sandbox_policy(workspace: &Path, network: bool) -> SandboxPolicy {
    let mut policy = SandboxPolicy::workspace(workspace);
    policy.network = if network {
        NetworkMode::On
    } else {
        NetworkMode::Off
    };
    // A CLI degrades rather than refusing all shell on a kernel that can't fully
    // enforce (e.g. Linux without an active Landlock LSM): seccomp + rlimits
    // still apply, and the shell result's `sandbox.mode` surfaces `degraded`.
    // macOS always enforces (Seatbelt), so this never downgrades there.
    policy.fail_closed = false;
    policy
}

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
    /// A directory whose immediate subdirectories are skills (each holding a
    /// SKILL.md). When set, `load_skill` joins the registry and the skill
    /// catalog is appended to the system prompt; candidates that fail
    /// validation are reported on stderr, never dropped silently.
    pub skills_root: Option<PathBuf>,
    /// A skill the model must load before doing anything else. Validated at
    /// build time; enforced by a step hook that forces `load_skill` until the
    /// conversation shows *this* skill was loaded successfully. Requires
    /// `skills_root`.
    pub require_skill: Option<String>,
    /// Install an OS sandbox for the `shell` tool (kernel-enforced filesystem
    /// containment + resource caps). Off by default at the library level so
    /// tests are unaffected; the `ac` binary turns it on. Writes are contained
    /// to the workspace (plus the system temp dir); the mandatory secret set is
    /// denied.
    pub sandbox: bool,
    /// When sandboxed, allow the command network access. Off means a real
    /// kernel guarantee of no egress (the strong exfil gate); on keeps
    /// network-using commands (git, package managers) working.
    pub sandbox_network: bool,
}

/// Forces `load_skill` as the tool choice until the request's own message
/// history shows the required skill was actually loaded. Forcing pins the
/// *tool*, not its arguments — the model can still name the wrong skill or
/// hit an error — so satisfaction demands a `load_skill` call whose input
/// named this skill AND whose result was not an error. Stateless by design:
/// the verdict is re-derived from the history every step, so it holds across
/// turns and after a session resume — there is no flag to drift out of sync.
struct RequireSkillHook {
    skill: String,
}

impl StepHook for RequireSkillHook {
    fn prepare(&self, _iteration: usize, request: &mut CompletionRequest) {
        let messages = &request.messages;
        let loaded = messages.iter().any(|m| {
            m.role == Role::Assistant
                && m.content.iter().any(|p| {
                    let ContentPart::ToolUse(tu) = p else {
                        return false;
                    };
                    tu.name == "load_skill"
                        && tu.input.get("name").and_then(|v| v.as_str())
                            == Some(self.skill.as_str())
                        && messages.iter().any(|rm| {
                            rm.content.iter().any(|rp| {
                                matches!(rp, ContentPart::ToolResult(tr)
                                    if tr.tool_use_id == tu.id && !tr.is_error)
                            })
                        })
                })
        });
        if !loaded {
            request.tool_choice = ToolChoice::Force("load_skill".to_string());
        }
    }
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
    let canonical = policy.root();
    let mut tool_ctx = ToolCtx::new(Arc::new(policy));
    if options.sandbox {
        let sandbox_policy = build_sandbox_policy(&canonical, options.sandbox_network);
        tool_ctx = tool_ctx.with_sandbox(Arc::new(ac_sandbox::OsSandbox::new(sandbox_policy)));
    }
    let ctx = Arc::new(tool_ctx);
    let mut registry = generic_registry();

    let mut system = SYSTEM_PROMPT.to_string();
    let mut resolver: Option<Arc<SkillsResolver>> = None;
    if let Some(root) = &options.skills_root {
        let skills = Arc::new(SkillsResolver::new(vec![SkillLayer {
            name: "host".to_string(),
            root: root.clone(),
        }]));
        for skipped in skills.list().skipped {
            eprintln!(
                "warning: skill skipped at {}: {}",
                skipped.dir.display(),
                skipped.reason
            );
        }
        if let Some(catalog) = skills.catalog_markdown() {
            system.push_str("\n\n");
            system.push_str(&catalog);
        }
        registry.register(LoadSkillTool::new(skills.clone()));
        resolver = Some(skills);
    }
    let registry = Arc::new(registry);

    let mut server_tools = Vec::new();
    if options.web_search {
        let web_search = ServerTool::WebSearch {
            max_results: Some(5),
        };
        // Honor the capability handshake: a provider that can't run web search
        // would silently ignore the request, so tell the user instead.
        if provider.supports_server_tool(&web_search) {
            server_tools.push(web_search);
        } else {
            eprintln!(
                "warning: provider '{}' does not support web search; --web-search ignored",
                provider.name()
            );
        }
    }

    let config = AgentConfig {
        model,
        system: Some(system),
        max_iterations: MAX_ITERATIONS,
        server_tools,
        ..Default::default()
    };

    let mut session = Session::new(provider, registry, ctx.clone(), config);

    if let Some(name) = &options.require_skill {
        let resolver = resolver.as_ref().ok_or_else(|| {
            anyhow::anyhow!("required skill {name:?} needs a skills root, but none is configured")
        })?;
        if resolver.resolve(name).is_none() {
            let available: Vec<String> =
                resolver.list().skills.into_iter().map(|s| s.name).collect();
            anyhow::bail!(
                "required skill {name:?} was not found in the skills root (available: {})",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            );
        }
        session.set_hook(Arc::new(RequireSkillHook {
            skill: name.clone(),
        }));
    }

    Ok(GenericHost { session, ctx })
}
