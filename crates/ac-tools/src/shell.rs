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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
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

/// Per-stream capture cap (~32 KiB); the envelope's tails stop here. Output
/// beyond it is flagged AND spilled in full to a log file (see [`SpillSink`]).
const STREAM_CAP: usize = 32 * 1024;
/// Full-transcript spill cap (both streams combined); the spill file itself
/// notes truncation when it is hit.
const SPILL_CAP: u64 = 8 * 1024 * 1024;

/// Where shell spill files go. A host installs one in `ctx.extensions` to
/// choose the directory; absent, a subdirectory of the OS temp dir is used.
/// The directory is created lazily, only when a spill actually happens.
pub struct ShellSpillDir(pub PathBuf);

/// Lazily-created spill file shared by both stream drains. Until the first
/// overflow byte the transcript is only buffered in memory (bounded: at most
/// both in-memory heads plus one read chunk, since overflow triggers
/// activation); on first overflow the file is created and the buffer flushed,
/// so the file always carries the FULL transcript from byte 0.
///
/// Format: both streams interleaved in arrival order, as a merged terminal
/// shows them — chunk-granular, so ordering across the two streams is
/// best-effort. Chosen over sectioning because it needs no second buffering
/// pass and keeps the file readable top-to-bottom as the command ran.
///
/// Spill failures are never tool errors: the in-memory tails still ride the
/// result; the sink just goes dead (`failed`) so memory stays bounded.
struct SpillSink {
    dir: PathBuf,
    state: Mutex<SpillState>,
}

#[derive(Default)]
struct SpillState {
    prelude: Vec<u8>,
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    written: u64,
    capped: bool,
    failed: bool,
}

impl SpillSink {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            state: Mutex::new(SpillState::default()),
        }
    }

    fn push(&self, bytes: &[u8]) {
        let mut state = self.state.lock().expect("spill lock poisoned");
        if state.failed {
            return;
        }
        if state.file.is_some() {
            Self::append(&mut state, bytes);
        } else {
            state.prelude.extend_from_slice(bytes);
        }
    }

    /// First overflow byte seen: create the file and flush everything buffered.
    fn activate(&self) {
        let mut state = self.state.lock().expect("spill lock poisoned");
        if state.file.is_some() || state.failed {
            return;
        }
        let created = Self::create_private_dir(&self.dir).and_then(|_| {
            let path = self.dir.join(format!("{}.log", uuid::Uuid::new_v4()));
            Self::create_private_file(&path).map(|f| (f, path))
        });
        match created {
            Ok((file, path)) => {
                state.file = Some(file);
                state.path = Some(path);
                let prelude = std::mem::take(&mut state.prelude);
                Self::append(&mut state, &prelude);
            }
            Err(_) => {
                state.failed = true;
                state.prelude = Vec::new();
            }
        }
    }

    /// Spill transcripts carry whatever the command printed — routinely
    /// secrets — and the default dir lives under the world-writable tmp root,
    /// so on unix the directory is forced to 0700 (tightening a pre-existing
    /// one; failing if it belongs to someone else) and each file is 0600,
    /// opened with `create_new` so a pre-planted name can never be followed.
    fn create_private_dir(dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(path)
    }

    fn append(state: &mut SpillState, bytes: &[u8]) {
        use std::io::Write;
        if state.capped {
            return;
        }
        let take = bytes.len().min((SPILL_CAP - state.written) as usize);
        let Some(file) = state.file.as_mut() else {
            return;
        };
        if file.write_all(&bytes[..take]).is_err() {
            state.failed = true;
            return;
        }
        state.written += take as u64;
        if take < bytes.len() {
            state.capped = true;
            let _ = file.write_all(b"\n[spill truncated: transcript exceeds the spill cap]\n");
        }
    }

    /// The spill file's path, iff a spill happened (flushed).
    fn finish(&self) -> Option<PathBuf> {
        use std::io::Write;
        let mut state = self.state.lock().expect("spill lock poisoned");
        if let Some(file) = state.file.as_mut() {
            let _ = file.flush();
        }
        state.path.clone()
    }
}
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
         per stream); when output overflows the cap, the full transcript (both \
         streams, in arrival order) is spilled to a log file and the result \
         carries its path as 'output_path' — read it for the rest. The command \
         and anything it forks are killed after 120s, on cancel, or when the \
         call returns (no lingering background processes). When the host has \
         installed an OS sandbox the command is kernel-contained and the \
         result reports 'sandbox.mode'; otherwise it runs with the host's own \
         privileges ('sandbox.mode':'off')."
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

            let spill_dir = ctx
                .extensions
                .get::<ShellSpillDir>()
                .map(|d| d.0.clone())
                .unwrap_or_else(|| std::env::temp_dir().join("ac-shell-spill"));
            let spill = Arc::new(SpillSink::new(spill_dir));

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let out_task = {
                let spill = spill.clone();
                tokio::spawn(async move { drain(stdout, spill).await })
            };
            let err_task = {
                let spill = spill.clone();
                tokio::spawn(async move { drain(stderr, spill).await })
            };

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
            if let Some(path) = spill.finish() {
                result["output_path"] = serde_json::Value::String(path.display().to_string());
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

/// Read a child pipe to EOF, keeping the first [`STREAM_CAP`] bytes in memory
/// as the envelope's tail while teeing EVERY byte into the shared spill sink;
/// the sink is activated (file created, buffer flushed) on this stream's first
/// overflow byte. Returns the captured text and whether output overflowed.
async fn drain<R>(reader: Option<R>, spill: Arc<SpillSink>) -> (String, bool)
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
                spill.push(&chunk[..n]);
                if buf.len() < STREAM_CAP {
                    let take = (STREAM_CAP - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n && !truncated {
                        truncated = true;
                        spill.activate();
                    }
                } else if !truncated {
                    truncated = true;
                    spill.activate();
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
    async fn over_cap_output_spills_the_full_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let spill_dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions
            .insert(ShellSpillDir(spill_dir.path().to_path_buf()));

        // ~1.3 MB of stdout — far past the 32 KiB in-memory head.
        let out = run("seq 1 200000", ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["truncated"], true);

        // The spill landed in the host-injected directory...
        let path = std::path::PathBuf::from(v["output_path"].as_str().expect("output_path"));
        assert!(path.starts_with(spill_dir.path()), "{}", path.display());
        // ...carries bytes beyond the in-memory head...
        let spilled = std::fs::read(&path).unwrap();
        assert!(spilled.len() > STREAM_CAP, "only {} bytes", spilled.len());
        // ...and is the transcript from byte 0: its head equals the tail head.
        let head = v["stdout_tail"].as_str().unwrap().as_bytes();
        assert_eq!(&spilled[..head.len()], head);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spill_files_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let spill_root = tempfile::tempdir().unwrap();
        let spill_dir = spill_root.path().join("spill");
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions.insert(ShellSpillDir(spill_dir.clone()));

        let out = run("seq 1 200000", ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let path = std::path::PathBuf::from(v["output_path"].as_str().expect("output_path"));

        // Transcripts can carry secrets: the dir is 0700 and the file 0600,
        // even under a permissive umask.
        let dir_mode = std::fs::metadata(&spill_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "spill dir mode {dir_mode:o}");
        assert_eq!(file_mode, 0o600, "spill file mode {file_mode:o}");
    }

    #[tokio::test]
    async fn under_cap_output_does_not_spill() {
        let dir = tempfile::tempdir().unwrap();
        let spill_dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions
            .insert(ShellSpillDir(spill_dir.path().to_path_buf()));

        let out = run("echo hi", ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert!(v.get("output_path").is_none());
        assert!(v.get("truncated").is_none());
        assert_eq!(std::fs::read_dir(spill_dir.path()).unwrap().count(), 0);
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
