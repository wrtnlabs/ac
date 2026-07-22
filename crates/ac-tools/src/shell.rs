//! The `shell` tool: run a command via `sh -c` inside the workspace.
//!
//! Two layers of containment compose. The cwd is always resolved through the
//! host [`PathPolicy`], so a command cannot be launched from outside the
//! permitted root. Beyond that, if the host installed a [`SandboxLauncher`] on
//! the [`ToolCtx`], the command is wrapped into a kernel-contained one and the
//! achieved [`SandboxMode`] rides the result envelope; the launcher fails
//! closed, so a policy it cannot enforce refuses the command rather than
//! running it weakly. If NO launcher is installed the command runs
//! unsandboxed — it can reach anything the host process can — and the envelope
//! says so (`sandbox.mode == "off"`). A host that needs isolation installs a
//! launcher (see the `ac-sandbox` crate).

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use ac_approvals::{ApprovalConfig, RoleContainment, Verdict};
use ac_tool::{Capability, CommandSpec, PathPolicy, SandboxMode, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use serde::Deserialize;

/// Adapts the tool context's [`PathPolicy`] into the [`RoleContainment`] the
/// approval engine delegates path-role checks to: a role token is *readable* iff
/// a read resolve succeeds, *writable* iff a write resolve succeeds. The region
/// verdict is what matters — a relative token resolves against the policy root,
/// which is the same write region the command runs in, so a false "not
/// contained" only ever over-asks (raises to `prompt`), never under-asks.
struct PolicyContainment<'a>(&'a dyn PathPolicy);

impl RoleContainment for PolicyContainment<'_> {
    fn readable(&self, path: &str) -> bool {
        self.0.resolve_read(Path::new(path)).is_ok()
    }
    fn writable(&self, path: &str) -> bool {
        self.0.resolve_write(Path::new(path)).is_ok()
    }
}

/// Per-stream capture cap (~32 KiB); output beyond it is dropped and flagged.
const STREAM_CAP: usize = 32 * 1024;
/// Hard wall-clock timeout for a command.
const TIMEOUT: Duration = Duration::from_secs(120);
/// Grace period to reap the child and collect output after it exits or is
/// killed; bounds the drain so a backgrounded grandchild holding a pipe open
/// cannot hang the tool past its advertised cap.
const GRACE: Duration = Duration::from_secs(5);

/// SIGKILL the child's whole process group (it is a group leader — see
/// `process_group(0)` in `run`), sweeping any processes it forked. A negative
/// pid targets the group; `ESRCH` when the group is already gone is harmless.
#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}
#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}

/// Run a shell command with `sh -c` inside the workspace.
///
/// The working directory defaults to the workspace root and must resolve inside
/// it. Output is capped per stream and the command is killed after 120 seconds
/// or on cancellation. NOTE: there is no OS sandbox in this phase — containment
/// is the working directory only.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ShellInput {
    /// The command line, executed as `sh -c "<command>"`.
    pub command: String,
    /// Working directory, relative to the workspace root (or absolute inside
    /// it). Defaults to the workspace root.
    pub cwd: Option<String>,
}

/// Executes shell commands (cwd-contained; no OS sandbox yet).
pub struct Shell;

impl Tool for Shell {
    type Input = ShellInput;

    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> String {
        "Run a command via 'sh -c' inside the workspace. cwd defaults to the \
         workspace root and must resolve inside it. Output is capped (~32 KiB \
         per stream); the command and anything it forks are killed after 120s, \
         on cancel, or when the call returns (no lingering background \
         processes). When the host has installed an OS sandbox the command is \
         kernel-contained and the result reports 'sandbox.mode'; otherwise it \
         runs with the host's own privileges ('sandbox.mode':'off')."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let cwd = input.cwd.unwrap_or_else(|| ".".to_string());
            let resolved = match ctx.policy.resolve_write(Path::new(&cwd)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };

            // Build the command through the OS-sandbox seam when a launcher is
            // installed; otherwise run it unsandboxed and mark the envelope. A
            // launcher that cannot enforce its policy fails closed — we never
            // fall back to an unsandboxed spawn behind the caller's back. Built
            // here (before classification) but NOT spawned, so the achieved
            // sandbox mode can inform the approval verdict while a `forbidden`
            // still spawns nothing (I1).
            let (mut command, sandbox_mode) = match &ctx.sandbox {
                Some(launcher) => {
                    let spec =
                        CommandSpec::new("sh", ["-c", input.command.as_str()], resolved.clone());
                    match launcher.prepare(&spec) {
                        Ok(prepared) => (prepared.command, prepared.mode),
                        Err(e) => {
                            return ToolOutput::error(format!(
                                "sandbox refused to run the command: {e}"
                            ));
                        }
                    }
                }
                None => {
                    let mut c = tokio::process::Command::new("sh");
                    c.arg("-c").arg(&input.command).current_dir(&resolved);
                    (c, SandboxMode::Off)
                }
            };

            // Pre-flight intent classification (ac-approvals). When the host has
            // installed an ApprovalConfig, classify the command line before the
            // built command is spawned (I1): a `forbidden` verdict refuses here,
            // as data the model reads (R3). No interactive approval channel is
            // wired yet, so `prompt` resolves to `forbidden` (ac-approvals §3) — a
            // host that wires a channel is where interactive prompting lands.
            // The unknown default `U` is honored only under STRICT kernel
            // containment; where the achieved mode is degraded or off, `U` is
            // clamped up to at least `prompt`, so a host that set `U = safe`
            // cannot silently allow unknown commands on an unsandboxed host (§2).
            // Classification composes with — never replaces — the path-policy and
            // sandbox layers (I5). Absent a config, the command runs unclassified.
            if let Some(cfg) = ctx.extensions.get::<ApprovalConfig>() {
                let unknown = if matches!(sandbox_mode, SandboxMode::Strict) {
                    cfg.unknown
                } else {
                    cfg.unknown.join(Verdict::Prompt)
                };
                let containment = PolicyContainment(ctx.policy.as_ref());
                let class =
                    ac_approvals::classify(&input.command, &cfg.policy, &containment, unknown);
                if ac_approvals::without_channel(class.verdict) == Verdict::Forbidden {
                    let mut msg = String::from("command refused by approval policy");
                    let reasons = class.refusal_reasons();
                    if !reasons.is_empty() {
                        msg.push_str(": ");
                        msg.push_str(&reasons.join("; "));
                    }
                    return ToolOutput::error(msg);
                }
            }
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // Own process group so we can kill the command AND anything it forks.
            #[cfg(unix)]
            command.process_group(0);

            let mut child = match command.spawn() {
                Ok(c) => c,
                Err(e) => return ToolOutput::error(format!("failed to spawn command: {e}")),
            };
            let pid = child.id();

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let out_task = tokio::spawn(async move { drain(stdout).await });
            let err_task = tokio::spawn(async move { drain(stderr).await });

            let mut killed: Option<&str> = None;
            let mut exit_code: Option<i32> = None;

            tokio::select! {
                status = child.wait() => {
                    exit_code = status.ok().and_then(|s| s.code());
                }
                _ = tokio::time::sleep(TIMEOUT) => {
                    killed = Some("timeout");
                }
                _ = ctx.cancel.cancelled() => {
                    killed = Some("cancelled");
                }
            }

            // Whether the command exited or timed out, sweep its process group so
            // no forked/backgrounded child survives the call or keeps a pipe open
            // past the drain grace. Then reap the leader (best-effort, bounded).
            let _ = child.start_kill();
            kill_process_group(pid);
            let _ = tokio::time::timeout(GRACE, child.wait()).await;

            // Killing the group closes the pipes, so the drains finish promptly;
            // still bound them so a pathological case can't hang the tool.
            let (stdout_tail, out_trunc) = match tokio::time::timeout(GRACE, out_task).await {
                Ok(Ok(v)) => v,
                _ => (String::new(), true),
            };
            let (stderr_tail, err_trunc) = match tokio::time::timeout(GRACE, err_task).await {
                Ok(Ok(v)) => v,
                _ => (String::new(), true),
            };
            let truncated = out_trunc || err_trunc;

            let mut result = serde_json::json!({
                "exit_code": exit_code,
                "stdout_tail": stdout_tail,
                "stderr_tail": stderr_tail,
                "sandbox": { "mode": sandbox_mode.as_str() },
            });
            if truncated {
                result["truncated"] = serde_json::Value::Bool(true);
            }
            if let Some(reason) = killed {
                result["killed"] = serde_json::Value::String(reason.to_string());
            }

            let body = serde_json::to_string(&result)
                .unwrap_or_else(|_| "{\"error\":\"failed to encode result\"}".to_string());

            if killed.is_some() {
                ToolOutput::error(body)
            } else {
                ToolOutput::ok(body)
            }
        })
    }
}

/// Read a child pipe to EOF, keeping only the first [`STREAM_CAP`] bytes while
/// still draining the rest so the child never blocks on a full pipe. Returns
/// the captured text and whether output was dropped.
async fn drain<R>(reader: Option<R>) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else {
        return (String::new(), false);
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < STREAM_CAP {
                    let take = (STREAM_CAP - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_approvals::{Matcher, Policy, ProgramRules, Rule};
    use ac_tool::SubtreePolicy;

    fn run(cmd: &str, ctx: Arc<ToolCtx>) -> impl std::future::Future<Output = ToolOutput> {
        Arc::new(Shell).run(
            ShellInput {
                command: cmd.to_string(),
                cwd: None,
            },
            ctx,
        )
    }

    #[tokio::test]
    async fn a_forbidden_command_is_refused_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        // `echo` is safe (Rest → Safe); everything else is unknown → prompt →
        // (no channel wired) forbidden.
        let policy = Policy::load([ProgramRules::new(
            "echo",
            [Rule::new([Matcher::Rest], Verdict::Safe)],
        )])
        .unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions.insert(ApprovalConfig::new(policy));

        // The safe command runs: a JSON envelope carrying an exit code.
        let out = run("echo hi", ctx.clone()).await;
        assert!(!out.is_error, "echo should be allowed: {}", out.content);
        assert!(out.content.contains("\"exit_code\""));

        // The unknown command is refused as data — not a JSON envelope, so it
        // never spawned (I1).
        let out = run("rm -rf x", ctx).await;
        assert!(out.is_error);
        assert!(
            out.content
                .starts_with("command refused by approval policy")
        );
        assert!(!out.content.contains("\"exit_code\""));
    }

    #[tokio::test]
    async fn a_role_escape_forbids_an_otherwise_safe_command() {
        let dir = tempfile::tempdir().unwrap();
        // `cat <path>` is safe when the path is read-contained.
        let policy = Policy::load([ProgramRules::new(
            "cat",
            [Rule::new([Matcher::ReadPath], Verdict::Safe)],
        )])
        .unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions.insert(ApprovalConfig::new(policy));

        // An in-tree read is allowed; an absolute path escaping the root raises
        // the match to prompt → (no channel) forbidden.
        let out = run("cat /etc/passwd", ctx).await;
        assert!(out.is_error);
        assert!(
            out.content
                .starts_with("command refused by approval policy")
        );
    }

    #[tokio::test]
    async fn without_a_config_commands_run_unclassified() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        let out = run("echo hi", ctx).await;
        assert!(!out.is_error);
        assert!(out.content.contains("\"exit_code\""));
    }

    #[tokio::test]
    async fn u_safe_is_clamped_to_prompt_without_strict_containment() {
        let dir = tempfile::tempdir().unwrap();
        // The host set U = safe, but installs no sandbox launcher, so the
        // achieved mode is `off`. An unknown command must NOT silently allow: the
        // shell clamps U up to prompt → (no channel) forbidden (§2).
        let policy = Policy::load([ProgramRules::new(
            "echo",
            [Rule::new([Matcher::Rest], Verdict::Safe)],
        )])
        .unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions
            .insert(ApprovalConfig::new(policy).with_unknown(Verdict::Safe));

        let out = run("rm -rf x", ctx.clone()).await;
        assert!(
            out.is_error,
            "U=safe must not allow an unknown command off-sandbox"
        );
        assert!(
            out.content
                .starts_with("command refused by approval policy")
        );
        // The explicitly-safe command still runs.
        let out = run("echo hi", ctx).await;
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn a_refusal_cites_the_offending_command_not_a_safe_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy::load([ProgramRules::new(
            "echo",
            [Rule::new([Matcher::Rest], Verdict::Safe).justified("echo is safe")],
        )])
        .unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions.insert(ApprovalConfig::new(policy));

        // `rm` is the reason; the message must name it and NOT parrot "echo is
        // safe" (the allowed sibling segment).
        let out = run("echo hi && rm x", ctx).await;
        assert!(out.is_error);
        assert!(
            out.content.contains("rm"),
            "should cite rm: {}",
            out.content
        );
        assert!(
            !out.content.contains("echo is safe"),
            "must not cite the safe sibling: {}",
            out.content
        );
    }
}
