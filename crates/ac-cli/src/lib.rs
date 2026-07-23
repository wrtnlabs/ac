//! The generic host, factored into a library so the *shipped* wiring is exactly
//! what the offline e2e tests exercise. Both `src/main.rs` (the `ac` binary) and
//! `tests/e2e.rs` assemble their agent through [`build_host`] — there is one
//! wiring path, not two that can drift.
//!
//! This is the standing proof that AC works for a host with no app attached: it
//! wires a provider to the built-in tool registry over the runtime loop,
//! contained to a directory. It must never grow app-specific behavior.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ac_context::{Cadence, FragmentClass, ReactiveSection};
use ac_provider::{Provider, ServerTool};
use ac_runtime::{AgentConfig, ReferenceSpawner, Session};
use ac_skills::{
    Skill, SkillLayer, SkillsResolver, build_skill_injections, catalog_markdown,
    extract_skill_mentions, select_skills_for_mentions,
};
use ac_tool::{
    AgentDefinition, Effort, GrantedReadPolicy, NetworkMode, PathPolicy, ReadGrants,
    ReadOnlyPolicy, SandboxPolicy, SpawnRequest, SubtreePolicy, ToolCtx, ToolRegistry, ToolScope,
    as_dyn,
};
use ac_tools::{EditFile, Fetch, Glob, Grep, ListFiles, ReadFile, Shell, Task, WriteFile};
use tokio_util::sync::CancellationToken;

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

/// The sub-agent definitions the generic host ships when delegation is enabled
/// ([docs/ac-subagents.md]): a read-only researcher and a full-toolset worker.
/// These are host content — the kit ships only the [`AgentDefinition`] shape.
pub fn subagent_definitions() -> Vec<AgentDefinition> {
    vec![
        AgentDefinition::new(
            "explore",
            "Read-only researcher: reads, lists, searches files and fetches URLs, but cannot \
             write or run shell. Delegate investigation to it so it spends its own context, not \
             yours, and returns a synthesis.",
        )
        .with_prompt(
            "You are a read-only research sub-agent. Investigate exactly what you were asked \
             using only read, list, glob, grep, and fetch — you cannot write files or run shell. \
             Return a concise synthesis of what you found.",
        )
        .with_tools(ToolScope::Allow(vec![
            "read_file".into(),
            "list_files".into(),
            "glob".into(),
            "grep".into(),
            "fetch".into(),
        ]))
        .with_effort(Effort::Low)
        .read_only(),
        AgentDefinition::new(
            "general",
            "General worker with the full file and shell toolset. Delegate a self-contained \
             sub-task and it carries it out end to end and reports the result.",
        )
        .with_prompt(SYSTEM_PROMPT)
        .with_tools(ToolScope::All),
    ]
}

/// A child's tool surface, built from its definition: the built-ins the
/// definition admits, and never the delegation tool (which is not a built-in —
/// the recursion guard). A read-only definition narrows the *policy* below; its
/// `ToolScope` already lists only read tools.
fn child_registry(def: &AgentDefinition) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    if def.tools.admits("read_file") {
        r.register(ReadFile);
    }
    if def.tools.admits("write_file") {
        r.register(WriteFile);
    }
    if def.tools.admits("edit_file") {
        r.register(EditFile);
    }
    if def.tools.admits("list_files") {
        r.register(ListFiles);
    }
    if def.tools.admits("glob") {
        r.register(Glob);
    }
    if def.tools.admits("grep") {
        r.register(Grep);
    }
    if def.tools.admits("shell") {
        r.register(Shell);
    }
    if def.tools.admits("fetch") {
        r.register(Fetch);
    }
    r
}

/// The sub-agent catalog appended to the parent's system prompt, so the model
/// knows whom it can delegate to via the `task` tool.
fn agents_catalog(defs: &[AgentDefinition]) -> String {
    let mut s = String::from(
        "## Sub-agents\n\nYou may delegate a scoped, self-contained sub-task to a sub-agent with \
         the `task` tool (pass `agent` and a `prompt`). The sub-agent runs in its own context and \
         returns only its result. Available agents:\n",
    );
    for d in defs {
        s.push_str(&format!("- `{}` — {}\n", d.name, d.description));
    }
    s
}

/// The delegation policy the parent agent is told to follow ([docs/ac-ultra.md]
/// §4). Flipping it emits a superseding standing-instruction fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationMode {
    /// Delegate only when the user asks (the baseline).
    OnRequest,
    /// Delegate proactively whenever parallel work helps (the "ultra" policy).
    Proactive,
}

/// A reactive section that injects the current delegation-mode standing
/// instruction ([docs/ac-ultra.md] §4). The prose here is host content; the kit
/// owns the reactive driver and the fragment recognition. The shared handle lets
/// the host flip the mode mid-session — the next turn boundary emits the change.
pub struct DelegationModeSection {
    class: FragmentClass,
    mode: Arc<Mutex<DelegationMode>>,
}

impl DelegationModeSection {
    pub fn new(mode: Arc<Mutex<DelegationMode>>) -> Self {
        Self {
            // Markers improbable in organic text; the reactive driver registers
            // this class so the fragment is recognized (stripped on compaction,
            // filtered from user input).
            class: FragmentClass::new(
                "delegation-mode",
                ac_types::Role::User,
                "[[delegation-mode]]",
                "[[/delegation-mode]]",
                Some(Cadence::Reactive),
                4096,
            ),
            mode,
        }
    }
}

impl ReactiveSection for DelegationModeSection {
    fn class(&self) -> &FragmentClass {
        &self.class
    }
    fn body(&self) -> Option<String> {
        Some(
            match *self.mode.lock().expect("delegation-mode lock poisoned") {
                DelegationMode::OnRequest => {
                    "Delegation policy: delegate to a sub-agent only when the user explicitly asks."
                        .to_string()
                }
                DelegationMode::Proactive => {
                    "Delegation policy: delegate PROACTIVELY. When a task \
                     has independent parts that parallel sub-agents could do faster or better, use \
                     the `task` tool to fan them out concurrently in one step, then synthesize \
                     their results. Do not do serially what you can delegate in parallel; spend \
                     sub-agents' context on research and self-contained sub-tasks rather than your \
                     own."
                        .to_string()
                }
            },
        )
    }
}

/// The host's skills wiring, codex-style: a resolver over the skills root
/// plus any skills selected up front (the `--skill` flag — the structured
/// equivalent of a `$mention`). There is no skill tool; the catalog sits in
/// the system prompt and [`compose_turn_input`] injects selected bodies into
/// the turn's input text.
pub struct HostSkills {
    pub resolver: Arc<SkillsResolver>,
    pub selected: Vec<Skill>,
}

/// An assembled generic agent: the session plus the shared run context, whose
/// cancel token the binary wires to Ctrl-C.
pub struct GenericHost {
    pub session: Session,
    pub ctx: Arc<ToolCtx>,
    pub skills: Option<HostSkills>,
    /// The delegation-mode flip handle, present under `--ultra`: a host may set
    /// it mid-session and the next turn emits the change ([docs/ac-ultra.md] §5).
    pub delegation_mode: Option<Arc<Mutex<DelegationMode>>>,
}

/// Options for [`build_host`] beyond the required provider/dir/model.
#[derive(Debug, Clone, Default)]
pub struct HostOptions {
    /// Request the provider's server-side web search each turn. The host opts
    /// in; whether it actually runs depends on the provider (a provider that
    /// can't do it ignores the request). Web search is NOT a built-in tool.
    pub web_search: bool,
    /// A directory scanned (recursively, bounded depth) for skills — each a
    /// directory holding a SKILL.md. When set, the skill catalog is appended
    /// to the system prompt, the root becomes readable (in-process policy and
    /// OS sandbox), and `$name` mentions in the prompt inject skill bodies
    /// into the turn input. Candidates that fail validation are reported on
    /// stderr, never dropped silently.
    pub skills_root: Option<PathBuf>,
    /// A skill to select up front, by name — the structured equivalent of
    /// writing `$name` in the prompt: its body is injected into the first
    /// turn's input. Validated at build time; requires `skills_root`.
    pub skill: Option<String>,
    /// Install an OS sandbox for the `shell` tool (kernel-enforced filesystem
    /// containment + resource caps). Off by default at the library level so
    /// tests are unaffected; the `ac` binary turns it on.
    pub sandbox: bool,
    /// When sandboxed, allow the command network access. Off means a real
    /// kernel guarantee of no egress (the strong exfil gate); on keeps
    /// network-using commands (git, package managers) working.
    pub sandbox_network: bool,
    /// Enable sub-agent delegation ([docs/ac-subagents.md]): register the `task`
    /// tool, install a [`ReferenceSpawner`] that runs children (the `explore`
    /// and `general` definitions) contained to the same workspace, and append
    /// the agent catalog to the system prompt. Off by default.
    pub subagents: bool,
    /// Default reasoning-effort tier for the session ([docs/ac-ultra.md] §3),
    /// applied to every request and to children that neither the spawn request
    /// nor the agent definition overrides. `None` uses the provider's default.
    pub effort: Option<Effort>,
    /// "Ultra" ([docs/ac-ultra.md] §5) — the host composition: force sub-agents
    /// on, default the effort to `Max`, and inject the *proactive* delegation
    /// mode. This is a host switch, not a kit primitive; the kit never maps
    /// effort to a delegation policy.
    pub ultra: bool,
}

/// Compose the actual turn input from the user's prompt: extract `$name`
/// mentions, add the up-front selection, and append each selected skill's
/// SKILL.md as a `<skill>` block (codex's injection shape — the skill enters
/// the turn as text, not through a tool). Warnings (unreadable skill files)
/// go to stderr; the turn proceeds with whatever loaded.
pub fn compose_turn_input(host: &GenericHost, prompt: &str) -> String {
    let Some(skills) = &host.skills else {
        return prompt.to_string();
    };
    let listing = skills.resolver.list();
    let mentions = extract_skill_mentions(prompt);
    let mut selected = skills.selected.clone();
    for skill in select_skills_for_mentions(&listing.skills, &mentions) {
        if !selected.iter().any(|s| s.skill_md == skill.skill_md) {
            selected.push(skill);
        }
    }
    let (injections, warnings) = build_skill_injections(&selected);
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    if injections.is_empty() {
        return prompt.to_string();
    }
    let mut input = prompt.to_string();
    for injection in injections {
        input.push_str("\n\n");
        input.push_str(&injection.render());
    }
    input
}

/// Assemble the generic host over a chosen provider and sandbox directory. The
/// single wiring path — binary and tests both call it.
pub fn build_host(
    provider: Arc<dyn Provider>,
    dir: &Path,
    model: String,
    options: HostOptions,
) -> anyhow::Result<GenericHost> {
    let subtree = SubtreePolicy::new(dir)
        .map_err(|e| anyhow::anyhow!("cannot use directory {}: {e}", dir.display()))?;
    let canonical = subtree.root();

    // "Ultra" ([docs/ac-ultra.md] §5) is a host composition: it forces sub-agents
    // on, defaults effort to Max (an explicit `--effort` still wins), and injects
    // the proactive delegation mode (installed after the session is built). The
    // kit is untouched — this is all host policy.
    let subagents = options.subagents || options.ultra;
    let effort = options
        .effort
        .or_else(|| options.ultra.then_some(Effort::Max));

    // Resolve skills before containment is assembled: the catalog advertises
    // each skill's CANONICAL SKILL.md path, so the read grants below must
    // cover every listed skill's canonical directory — the skills root plus
    // any skill that is a symlink pointing outside it. Skill use never
    // changes policy at runtime, and writes never widen.
    let mut system = SYSTEM_PROMPT.to_string();
    let mut skills: Option<HostSkills> = None;
    let mut skill_read_dirs: Vec<PathBuf> = Vec::new();
    if let Some(root) = &options.skills_root {
        let resolver = Arc::new(SkillsResolver::new(vec![SkillLayer {
            name: "host".to_string(),
            root: root.clone(),
        }]));
        let listing = resolver.list();
        for skipped in &listing.skipped {
            eprintln!(
                "warning: skill skipped at {}: {}",
                skipped.dir.display(),
                skipped.reason
            );
        }
        if let Some(catalog) = catalog_markdown(&listing) {
            system.push_str("\n\n");
            system.push_str(&catalog);
        }
        match root.canonicalize() {
            Ok(canonical_root) => {
                for skill in &listing.skills {
                    if !skill.dir.starts_with(&canonical_root)
                        && !skill_read_dirs.contains(&skill.dir)
                    {
                        skill_read_dirs.push(skill.dir.clone());
                    }
                }
                skill_read_dirs.push(canonical_root);
            }
            Err(e) => eprintln!(
                "warning: skills root {} is not readable: {e}",
                root.display()
            ),
        }
        let mut selected = Vec::new();
        if let Some(name) = &options.skill {
            let matches: Vec<&ac_skills::Skill> =
                listing.skills.iter().filter(|s| s.name == *name).collect();
            match matches.as_slice() {
                [] => {
                    let available: Vec<String> =
                        listing.skills.iter().map(|s| s.name.clone()).collect();
                    anyhow::bail!(
                        "skill {name:?} was not found in the skills root (available: {})",
                        if available.is_empty() {
                            "none".to_string()
                        } else {
                            available.join(", ")
                        }
                    );
                }
                [skill] => selected.push((*skill).clone()),
                many => {
                    // Same rule as a plain $mention: an ambiguous name is
                    // refused, never guessed.
                    let paths: Vec<String> = many
                        .iter()
                        .map(|s| s.skill_md.display().to_string())
                        .collect();
                    anyhow::bail!(
                        "skill name {name:?} is ambiguous ({} skills carry it: {})",
                        many.len(),
                        paths.join(", ")
                    );
                }
            }
        }
        skills = Some(HostSkills { resolver, selected });
    } else if let Some(name) = &options.skill {
        anyhow::bail!("skill {name:?} needs a skills root, but none is configured");
    }

    let policy: Arc<dyn PathPolicy> = if options.skills_root.is_some() {
        let grants = Arc::new(ReadGrants::new());
        for dir in &skill_read_dirs {
            if let Err(e) = grants.grant(dir) {
                eprintln!(
                    "warning: skill directory {} is not readable: {e}",
                    dir.display()
                );
            }
        }
        Arc::new(GrantedReadPolicy::new(Arc::new(subtree), grants))
    } else {
        Arc::new(subtree)
    };

    let mut tool_ctx = ToolCtx::new(policy);
    if options.sandbox {
        let mut sandbox_policy = build_sandbox_policy(&canonical, options.sandbox_network);
        for dir in &skill_read_dirs {
            sandbox_policy = sandbox_policy.read_also(dir.clone());
        }
        tool_ctx = tool_ctx.with_sandbox(Arc::new(ac_sandbox::OsSandbox::new(sandbox_policy)));
    }

    // Sub-agent delegation: install a reference spawner that runs children
    // contained to the same workspace, and advertise the agents in the prompt.
    // The child assembler builds each child with NO spawner (the recursion
    // guard) and a cancellation token derived from the parent's.
    if subagents {
        let defs = subagent_definitions();
        system.push_str("\n\n");
        system.push_str(&agents_catalog(&defs));

        let child_provider = provider.clone();
        let child_dir = canonical.clone();
        let base_model = model.clone();
        let base_effort = effort;
        let assemble = move |req: &SpawnRequest, cancel: CancellationToken| -> Option<Session> {
            let def = defs.iter().find(|d| d.name == req.agent)?;
            let base = SubtreePolicy::new(&child_dir).ok()?;
            let policy: Arc<dyn PathPolicy> = if def.read_only {
                Arc::new(ReadOnlyPolicy::new(Arc::new(base)))
            } else {
                Arc::new(base)
            };
            // No `.with_spawner(...)` — the omission is the recursion guard.
            let child_ctx = ToolCtx::new(policy).with_cancel(cancel);
            let child_config = AgentConfig {
                model: req
                    .model
                    .clone()
                    .or_else(|| def.model.clone())
                    .unwrap_or_else(|| base_model.clone()),
                system: def.prompt.clone(),
                max_iterations: MAX_ITERATIONS,
                // Effort precedence: spawn override → definition default → parent.
                effort: req.effort.or(def.effort).or(base_effort),
                ..Default::default()
            };
            Some(Session::new(
                child_provider.clone(),
                Arc::new(child_registry(def)),
                Arc::new(child_ctx),
                child_config,
            ))
        };
        tool_ctx = tool_ctx.with_spawner(as_dyn(ReferenceSpawner::new(assemble)));
    }

    let ctx = Arc::new(tool_ctx);
    let registry = Arc::new({
        let mut r = generic_registry();
        if subagents {
            r.register(Task);
        }
        r
    });

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
        effort,
        ..Default::default()
    };

    let mut session = Session::new(provider, registry, ctx.clone(), config);

    // Under ultra, inject the proactive delegation mode as a reactive section and
    // hand the flip handle back so the host can change it mid-session (§4/§5).
    let delegation_mode = if options.ultra {
        let handle = Arc::new(Mutex::new(DelegationMode::Proactive));
        session.add_reactive_section(Arc::new(DelegationModeSection::new(handle.clone())));
        Some(handle)
    } else {
        None
    };

    Ok(GenericHost {
        session,
        ctx,
        skills,
        delegation_mode,
    })
}
