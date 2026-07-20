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

use ac_tool::{Capability, CommandSpec, SandboxMode, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use serde::Deserialize;

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
            // fall back to an unsandboxed spawn behind the caller's back.
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
