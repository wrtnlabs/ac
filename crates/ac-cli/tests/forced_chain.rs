//! The forced-chain proof: a host tool rebinds containment mid-run via
//! [`SwapPolicy`], a step hook pins the forced tool (bind a working
//! subdirectory before anything else), and the policy floor holds even when
//! the scripted model defies the forced tool choice. This is the composition
//! the combinators exist for; hermetic — MockProvider, temp dirs, no network.
//! (Skills are injected text, not a tool — so the chain here is the host's
//! own tool; the skills injection proof lives in `skills_full.rs`.)

use std::path::PathBuf;
use std::sync::Arc;

use ac_provider::ToolChoice;
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentConfig, AgentEvent, ForcedChainHook, Session};
use ac_tool::{
    Capability, PathPolicy, ReadOnlyPolicy, SplitPolicy, SubtreePolicy, SwapPolicy, Tool, ToolCtx,
    ToolOutput, ToolRegistry,
};
use ac_tools::{ReadFile, WriteFile};
use ac_types::{ContentPart, Message, Role, StopReason, ToolResult, ToolUse};
use futures::future::BoxFuture;
use serde_json::json;

/// A host tool that commits to a working subdirectory at runtime: validates
/// the name as a single path segment, creates it, then swaps the shared
/// policy from read-only-over-the-workspace to "read the whole workspace,
/// write only the chosen subdirectory".
struct BindWorkdir {
    swap: Arc<SwapPolicy>,
    workspace: PathBuf,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct BindWorkdirInput {
    /// Name of the working subdirectory — a single path segment.
    dir: String,
}

impl Tool for BindWorkdir {
    type Input = BindWorkdirInput;

    fn name(&self) -> &'static str {
        "bind_workdir"
    }

    fn description(&self) -> String {
        "Commit to a working subdirectory of the workspace. Must be called \
         before any write is permitted."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let dir = input.dir;
            if dir.is_empty() || dir == "." || dir == ".." || dir.contains(['/', '\\']) {
                return ToolOutput::error(format!(
                    "dir must be a single path segment (no separators, no '..'), got {dir:?}"
                ));
            }
            let target = self.workspace.join(&dir);
            if let Err(e) = std::fs::create_dir_all(&target) {
                return ToolOutput::error(format!("cannot create {dir:?}: {e}"));
            }
            let read = match SubtreePolicy::new(&self.workspace) {
                Ok(p) => Arc::new(p),
                Err(e) => return ToolOutput::error(format!("cannot bind workspace: {e}")),
            };
            let write = match SubtreePolicy::new(&target) {
                Ok(p) => Arc::new(p),
                Err(e) => return ToolOutput::error(format!("cannot bind {dir:?}: {e}")),
            };
            // SubtreePolicy canonicalized the target; a pre-existing symlink at
            // <workspace>/<dir> would have resolved to wherever it points. The
            // bind must not follow it out of the workspace — this check is part
            // of the reference composition, not just test hygiene.
            if !write.root().starts_with(read.root()) {
                return ToolOutput::error(format!(
                    "{dir:?} resolves outside the workspace (symlink?) — refusing to bind"
                ));
            }
            self.swap.swap(Arc::new(SplitPolicy { read, write }));
            ToolOutput::ok(format!("working directory bound to {dir}"))
        })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn forced_chain_binds_then_frees_and_the_policy_floor_holds() {
    let workspace_tmp = tempfile::tempdir().unwrap();
    let workspace = workspace_tmp.path().canonicalize().unwrap();
    std::fs::write(workspace.join("sibling.txt"), "sibling ground truth").unwrap();

    // A pre-planted symlink pointing out of the workspace — the classic trap
    // for anything that canonicalizes-then-trusts. Binding to it must refuse.
    let outside_tmp = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside_tmp.path(), workspace.join("evil")).unwrap();

    // Containment starts as read-only over the workspace; only bind_workdir
    // can widen it, and only to one subdirectory.
    let initial = Arc::new(SubtreePolicy::new(&workspace).unwrap());
    let swap = Arc::new(SwapPolicy::new(Arc::new(ReadOnlyPolicy::new(initial))));
    let ctx = Arc::new(ToolCtx::new(swap.clone() as Arc<dyn PathPolicy>));

    let mut registry = ToolRegistry::new();
    registry.register(BindWorkdir {
        swap: swap.clone(),
        workspace: workspace.clone(),
    });
    registry.register(ReadFile);
    registry.register(WriteFile);

    let escape_path = workspace.join("escape.txt");
    let provider = MockProvider::new(vec![
        // Turn 0: the model DEFIES the forced bind_workdir and tries to write
        // at the workspace root. The read-only floor must refuse it — policy
        // containment cannot depend on model compliance.
        vec![
            tool_use(
                "call-defy",
                "write_file",
                json!({ "path": "defiant.txt", "content": "nope" }),
            ),
            stop_tool_use(),
        ],
        // Turn 1: comply with the forced tool, but aim it at the planted
        // symlink — the bind must refuse to follow it out of the workspace.
        vec![
            tool_use("call-evil", "bind_workdir", json!({ "dir": "evil" })),
            stop_tool_use(),
        ],
        // Turn 2: comply — bind a real working subdirectory.
        vec![
            tool_use("call-bind", "bind_workdir", json!({ "dir": "proj" })),
            stop_tool_use(),
        ],
        // Turn 3: free step — a contained write (lands in proj/), an escape
        // attempt at the workspace root (must be refused), and a read of a
        // workspace-root sibling (must succeed: reads stay widened).
        vec![
            tool_use(
                "call-write",
                "write_file",
                json!({ "path": "out.txt", "content": "hello contained world" }),
            ),
            tool_use(
                "call-escape",
                "write_file",
                json!({ "path": escape_path.display().to_string(), "content": "pwned" }),
            ),
            tool_use(
                "call-sibling",
                "read_file",
                json!({ "path": "../sibling.txt" }),
            ),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);

    let config = AgentConfig {
        model: "mock/model".to_string(),
        system: Some("generic test host".to_string()),
        ..Default::default()
    };
    // The kit's stateless forced chain: it forces `bind_workdir` until the
    // history shows a SUCCESSFUL bind — derived from the request's messages, not
    // a flag — so it is resume- and fork-correct by construction ([ac-hooks.md]
    // §3). No bound flag anywhere.
    let mut session = Session::new(Arc::new(provider.clone()), Arc::new(registry), ctx, config);
    session.add_step_hook(Arc::new(ForcedChainHook::new("bind_workdir")));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let driver = tokio::spawn(async move { session.run_turn("do the work".to_string(), tx).await });
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    let result = driver.await.expect("join").expect("turn ok");
    assert_eq!(result, StopReason::EndTurn);

    // --- the hook drove the exact forced sequence ---
    let choices: Vec<ToolChoice> = provider
        .requests()
        .iter()
        .map(|r| r.tool_choice.clone())
        .collect();
    assert_eq!(
        choices,
        vec![
            ToolChoice::Force("bind_workdir".to_string()),
            ToolChoice::Force("bind_workdir".to_string()),
            ToolChoice::Force("bind_workdir".to_string()),
            ToolChoice::Auto,
            ToolChoice::Auto,
        ],
        "forced chain must be bind, bind (after defiance), bind (after the \
         symlink refusal), then free"
    );

    // --- ground truth on disk ---
    let out = workspace.join("proj").join("out.txt");
    assert_eq!(
        std::fs::read_to_string(&out).expect("proj/out.txt must exist"),
        "hello contained world"
    );
    assert!(
        !escape_path.exists(),
        "the workspace-root write must have been contained"
    );
    assert!(
        !workspace.join("defiant.txt").exists(),
        "the pre-bind write must have been refused by the read-only floor"
    );

    // --- per-call results the model saw ---
    let result_of = |id: &str| {
        events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult {
                    id: rid,
                    output,
                    is_error,
                    ..
                } if rid == id => Some((output.clone(), *is_error)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a tool result for {id}"))
    };

    let (defy_out, defy_err) = result_of("call-defy");
    assert!(defy_err, "pre-bind write must be an error result");
    assert!(
        defy_out.contains("writes are not permitted yet"),
        "the refusal must carry the read-only reason: {defy_out}"
    );

    let (evil_out, evil_err) = result_of("call-evil");
    assert!(
        evil_err,
        "binding through the symlink must be an error result"
    );
    assert!(
        evil_out.contains("outside the workspace"),
        "the refusal must carry the symlink reason: {evil_out}"
    );

    let (_, bind_err) = result_of("call-bind");
    assert!(!bind_err, "bind_workdir must succeed");

    let (escape_out, escape_err) = result_of("call-escape");
    assert!(escape_err, "workspace-root write must be an error result");
    assert!(
        escape_out.contains("escapes the permitted root"),
        "the refusal must carry the containment reason: {escape_out}"
    );

    let (sibling_out, sibling_err) = result_of("call-sibling");
    assert!(
        !sibling_err,
        "reading a workspace-root sibling must stay permitted after the bind"
    );
    assert!(
        sibling_out.contains("sibling ground truth"),
        "the sibling read must carry the file's content: {sibling_out}"
    );
}

/// The payoff of stateless derivation ([ac-hooks.md] §3, I5): a session resumed
/// from a history that already shows a successful bind does NOT re-force the
/// chain. A flag-based hook would reset to "unbound" on resume and wrongly force
/// `bind_workdir` again; the E(L)-derived hook reads the bind out of history and
/// stays free.
#[tokio::test]
async fn a_resumed_session_does_not_re_force_a_completed_chain() {
    let workspace = tempfile::tempdir().unwrap();

    // A prior session's history: bind_workdir was called and SUCCEEDED.
    let history = vec![
        Message::text(Role::User, "do the work".to_string()),
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse(ToolUse {
                id: "c-bind".to_string(),
                name: "bind_workdir".to_string(),
                input: json!({ "dir": "proj" }),
            })],
            cache: false,
        },
        Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ToolResult {
                tool_use_id: "c-bind".to_string(),
                content: "working directory bound to proj".to_string(),
                is_error: false,
            })],
            cache: false,
        },
        Message::text(Role::Assistant, "earlier work".to_string()),
    ];

    let ctx = Arc::new(ToolCtx::new(
        Arc::new(SubtreePolicy::new(workspace.path()).unwrap()) as Arc<dyn PathPolicy>,
    ));
    let provider = MockProvider::new(vec![vec![text("done"), stop_end()]]);
    let config = AgentConfig {
        model: "mock/model".to_string(),
        ..Default::default()
    };
    let mut session = Session::resume(
        Arc::new(provider.clone()),
        Arc::new(ToolRegistry::new()),
        ctx,
        config,
        history,
    );
    session.add_step_hook(Arc::new(ForcedChainHook::new("bind_workdir")));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let driver = tokio::spawn(async move { session.run_turn("continue".to_string(), tx).await });
    while rx.recv().await.is_some() {}
    driver.await.expect("join").expect("turn ok");

    let choices: Vec<ToolChoice> = provider
        .requests()
        .iter()
        .map(|r| r.tool_choice.clone())
        .collect();
    assert_eq!(
        choices,
        vec![ToolChoice::Auto],
        "a resumed session whose history shows the bind must not re-force it"
    );
}
