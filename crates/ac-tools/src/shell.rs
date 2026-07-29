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

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ac_approvals::{ApprovalConfig, RoleContainment, Verdict};
use ac_tool::{Capability, CommandSpec, PathPolicy, SandboxMode, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

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
/// beyond it is flagged and spilled in full to a transcript.
const STREAM_CAP: usize = 32 * 1024;
/// Full-transcript spill cap (both streams combined); the spill file itself
/// notes truncation when it is hit.
const SPILL_CAP: u64 = 8 * 1024 * 1024;

/// Where shell spill files go. A host installs one in `ctx.extensions` to
/// choose the directory; absent, a subdirectory of the OS temp dir is used.
/// The directory is created lazily, only when a spill actually happens.
pub struct ShellSpillDir(pub PathBuf);

/// Transcripts routinely carry secrets. Force 0700 on their directory and
/// 0600/create-new on each file.
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
/// Default hard wall-clock timeout for a command.
pub const DEFAULT_SHELL_TIMEOUT_MS: u64 = 120_000;
/// Smallest model-selected timeout accepted by the stock tool.
pub const MIN_SHELL_TIMEOUT_MS: u64 = 1_000;
/// Largest model-selected timeout accepted by the stock tool.
pub const MAX_SHELL_TIMEOUT_MS: u64 = 600_000;
/// Default grace period used to reap a child after exit or cancellation.
pub const DEFAULT_SHELL_KILL_GRACE: Duration = Duration::from_secs(5);

/// Program and prefix arguments used to execute one model-authored command.
///
/// The final command string is appended after `args_before_command`. The stock
/// value is `sh -c`; hosts that need the user's configured login shell can use
/// [`from_user_shell`](Self::from_user_shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellInvocation {
    pub program: String,
    pub args_before_command: Vec<String>,
}

impl ShellInvocation {
    pub fn new(
        program: impl Into<String>,
        args_before_command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args_before_command: args_before_command.into_iter().map(Into::into).collect(),
        }
    }

    /// Read `$SHELL`, falling back to `fallback`. Bash and zsh run as login
    /// shells so user-installed command paths are available; other shells use
    /// their ordinary `-c` form.
    pub fn from_user_shell(fallback: impl Into<String>) -> Self {
        let program = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| fallback.into());
        let name = Path::new(&program)
            .file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let args = if name == "bash" || name == "zsh" {
            vec!["-l", "-c"]
        } else {
            vec!["-c"]
        };
        Self::new(program, args)
    }

    fn args(&self, command: String) -> Vec<String> {
        let mut args = self.args_before_command.clone();
        args.push(command);
        args
    }
}

impl Default for ShellInvocation {
    fn default() -> Self {
        Self::new("sh", ["-c"])
    }
}

/// Per-run behavior for the stock [`Shell`] tool.
///
/// Install this in [`ToolCtx::extensions`] to select a command interpreter,
/// timeout/cleanup policy, transcript policy, or a sandbox-compatible cwd.
/// All fields are host-neutral; application-specific paths and names stay in
/// the host.
#[derive(Clone)]
pub struct ShellConfig {
    pub invocation: ShellInvocation,
    pub default_timeout: Duration,
    pub kill_grace: Duration,
    /// `None` keeps the stock overflow-only transcript behavior.
    pub capture: Option<ShellCaptureOptions>,
    /// Optional additional restriction over paths the active policy permits
    /// reading. The resolved cwd must be contained by at least one root.
    pub cwd_roots: Option<Vec<PathBuf>>,
    /// Cwd used only while the sandbox launcher derives its policy when the
    /// requested read-only cwd is not itself an admissible launcher cwd.
    pub sandbox_cwd_fallback: Option<PathBuf>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            invocation: ShellInvocation::default(),
            default_timeout: Duration::from_millis(DEFAULT_SHELL_TIMEOUT_MS),
            kill_grace: DEFAULT_SHELL_KILL_GRACE,
            capture: None,
            cwd_roots: None,
            sandbox_cwd_fallback: None,
        }
    }
}

/// Host-provided environment values for one shell invocation.
///
/// Providers may read mutable run binding state. Returning an error refuses
/// the command before sandbox preparation or process spawn.
pub trait ShellEnvironmentProvider: Send + Sync {
    fn environment(&self) -> Result<Vec<(OsString, OsString)>, String>;
}

/// SIGKILL the child's whole process group (it is a group leader — see
/// `process_group(0)` in `run`), sweeping any processes it forked. A negative
/// pid targets the group; `ESRCH` when the group is already gone is harmless.
#[cfg(unix)]
fn kill_process_group(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), signal);
        }
    }
}
#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>, _signal: i32) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKillReason {
    Timeout,
    Cancelled,
}

/// Transcript/capture policy for [`execute_shell`].
#[derive(Debug, Clone)]
pub struct ShellCaptureOptions {
    pub tail_bytes: usize,
    pub tail_lines: usize,
    pub transcript_dir: PathBuf,
    /// Create the transcript before spawn even when output stays under the
    /// in-memory cap. Product hosts commonly use this for durable diagnostics.
    pub always_transcript: bool,
    /// Maximum transcript bytes across stdout+stderr. `None` is unlimited.
    pub transcript_cap: Option<u64>,
}

impl ShellCaptureOptions {
    pub fn overflow_only(transcript_dir: PathBuf) -> Self {
        Self {
            tail_bytes: STREAM_CAP,
            tail_lines: usize::MAX,
            transcript_dir,
            always_transcript: false,
            transcript_cap: Some(SPILL_CAP),
        }
    }
}

/// Host-configured command request over AC's shared shell executor.
///
/// The stock `shell` tool and app-specific schema/copy adapters both use this
/// type, keeping sandbox preparation, approval classification, capture,
/// timeout/cancel, process-group cleanup, and reaping in one implementation.
pub struct ShellExecRequest {
    pub program: String,
    pub args: Vec<String>,
    /// Original model-authored command line used by `ac-approvals`.
    pub command_for_approval: String,
    pub cwd: PathBuf,
    /// Cwd handed to the sandbox launcher when the real cwd is read-only. The
    /// prepared process is switched back to `cwd` after policy derivation.
    pub sandbox_cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
    pub timeout: Duration,
    pub kill_grace: Duration,
    pub capture: ShellCaptureOptions,
}

#[derive(Debug)]
pub struct ShellExecResult {
    pub exit_code: Option<i32>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub truncated: bool,
    pub duration: Duration,
    pub sandbox_mode: SandboxMode,
    pub output_path: Option<PathBuf>,
    pub killed: Option<ShellKillReason>,
}

struct Transcript {
    dir: PathBuf,
    cap: Option<u64>,
    state: Mutex<TranscriptState>,
}

#[derive(Default)]
struct TranscriptState {
    prelude: Vec<u8>,
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    written: u64,
    capped: bool,
    failed: bool,
}

impl Transcript {
    fn new(dir: PathBuf, cap: Option<u64>) -> Self {
        Self {
            dir,
            cap,
            state: Mutex::new(TranscriptState::default()),
        }
    }

    fn activate(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().expect("transcript lock poisoned");
        if state.file.is_some() {
            return Ok(());
        }
        if state.failed {
            return Err(std::io::Error::other("transcript is unavailable"));
        }
        let created = create_private_dir(&self.dir).and_then(|_| {
            let path = self.dir.join(format!("{}.log", uuid::Uuid::new_v4()));
            create_private_file(&path).map(|file| (file, path))
        });
        let (file, path) = match created {
            Ok(created) => created,
            Err(error) => {
                state.failed = true;
                state.prelude.clear();
                return Err(error);
            }
        };
        state.file = Some(file);
        state.path = Some(path);
        let prelude = std::mem::take(&mut state.prelude);
        self.append_locked(&mut state, &prelude);
        Ok(())
    }

    fn push(&self, bytes: &[u8]) {
        let mut state = self.state.lock().expect("transcript lock poisoned");
        if state.failed {
            return;
        }
        if state.file.is_some() {
            self.append_locked(&mut state, bytes);
        } else {
            state.prelude.extend_from_slice(bytes);
        }
    }

    fn append_locked(&self, state: &mut TranscriptState, bytes: &[u8]) {
        use std::io::Write;
        if state.capped {
            return;
        }
        let take = self
            .cap
            .map(|cap| bytes.len().min(cap.saturating_sub(state.written) as usize))
            .unwrap_or(bytes.len());
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
            let _ = file.write_all(b"\n[transcript truncated: output exceeds the cap]\n");
        }
    }

    fn finish(&self) -> Option<PathBuf> {
        use std::io::Write;
        let mut state = self.state.lock().expect("transcript lock poisoned");
        if let Some(file) = state.file.as_mut() {
            let _ = file.flush();
        }
        state.path.clone()
    }
}

#[derive(Default)]
struct TailCapture {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped: bool,
}

impl TailCapture {
    fn push(&mut self, chunk: &[u8], cap: usize) {
        self.bytes += chunk.len();
        self.chunks.push_back(chunk.to_vec());
        let slack = cap.saturating_mul(4).max(cap);
        while self.bytes > slack && self.chunks.len() > 1 {
            let removed = self.chunks.pop_front().expect("len > 1");
            self.bytes -= removed.len();
            self.dropped = true;
        }
    }

    fn concat(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes);
        for chunk in &self.chunks {
            out.extend_from_slice(chunk);
        }
        out
    }
}

/// Return the last `max_bytes` and then last `max_lines` of a stream.
pub fn tail_output(buf: &[u8], max_bytes: usize, max_lines: usize) -> (String, bool) {
    let byte_cut = buf.len() > max_bytes;
    let trimmed = if byte_cut {
        &buf[buf.len() - max_bytes..]
    } else {
        buf
    };
    let text = String::from_utf8_lossy(trimmed);
    let lines: Vec<&str> = text.split('\n').collect();
    if !byte_cut && lines.len() <= max_lines {
        return (text.into_owned(), false);
    }
    let start = lines.len().saturating_sub(max_lines);
    (lines[start..].join("\n"), true)
}

async fn drain_tail<R>(
    reader: Option<R>,
    capture: Arc<Mutex<TailCapture>>,
    transcript: Arc<Transcript>,
    cap: usize,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut reader) = reader else { return };
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                transcript.push(&chunk[..n]);
                let overflowed = {
                    let mut capture = capture.lock().expect("capture lock poisoned");
                    capture.push(&chunk[..n], cap);
                    capture.dropped || capture.bytes > cap
                };
                if overflowed {
                    // Lazy transcript failures are non-fatal; the bounded tail
                    // remains useful. Eager activation was checked pre-spawn.
                    let _ = transcript.activate();
                }
            }
        }
    }
}

/// Execute a command through AC's generic shell mechanisms.
pub async fn execute_shell(
    ctx: Arc<ToolCtx>,
    request: ShellExecRequest,
) -> Result<ShellExecResult, String> {
    let started = Instant::now();
    let (mut command, sandbox_mode) = match &ctx.sandbox {
        Some(launcher) => {
            let spec = CommandSpec::new(
                &request.program,
                request.args.iter().map(String::as_str),
                request
                    .sandbox_cwd
                    .clone()
                    .unwrap_or_else(|| request.cwd.clone()),
            );
            let prepared = launcher
                .prepare(&spec)
                .map_err(|error| format!("sandbox refused to run the command: {error}"))?;
            (prepared.command, prepared.mode)
        }
        None => {
            let mut command = tokio::process::Command::new(&request.program);
            command.args(&request.args);
            (command, SandboxMode::Off)
        }
    };

    if let Some(cfg) = ctx.extensions.get::<ApprovalConfig>() {
        let unknown = if matches!(sandbox_mode, SandboxMode::Strict) {
            cfg.unknown
        } else {
            cfg.unknown.join(Verdict::Prompt)
        };
        let containment = PolicyContainment(ctx.policy.as_ref());
        let class = ac_approvals::classify(
            &request.command_for_approval,
            &cfg.policy,
            &containment,
            unknown,
        );
        if ac_approvals::without_channel(class.verdict) == Verdict::Forbidden {
            let mut message = String::from("command refused by approval policy");
            let reasons = class.refusal_reasons();
            if !reasons.is_empty() {
                message.push_str(": ");
                message.push_str(&reasons.join("; "));
            }
            return Err(message);
        }
    }

    command
        .current_dir(&request.cwd)
        .envs(request.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);

    let transcript = Arc::new(Transcript::new(
        request.capture.transcript_dir.clone(),
        request.capture.transcript_cap,
    ));
    if request.capture.always_transcript {
        transcript
            .activate()
            .map_err(|error| format!("failed to open output log: {error}"))?;
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to spawn command: {error}"))?;
    let pid = child.id();
    let stdout_cap = Arc::new(Mutex::new(TailCapture::default()));
    let stderr_cap = Arc::new(Mutex::new(TailCapture::default()));
    let out_task = tokio::spawn(drain_tail(
        child.stdout.take(),
        stdout_cap.clone(),
        transcript.clone(),
        request.capture.tail_bytes,
    ));
    let err_task = tokio::spawn(drain_tail(
        child.stderr.take(),
        stderr_cap.clone(),
        transcript.clone(),
        request.capture.tail_bytes,
    ));

    let mut killed = None;
    let mut exit_code = None;
    tokio::select! {
        status = child.wait() => {
            exit_code = status.ok().and_then(|status| status.code());
        }
        _ = tokio::time::sleep(request.timeout) => {
            killed = Some(ShellKillReason::Timeout);
        }
        _ = ctx.cancel.cancelled() => {
            killed = Some(ShellKillReason::Cancelled);
        }
    }

    if killed.is_some() {
        kill_process_group(pid, libc::SIGTERM);
        match tokio::time::timeout(request.kill_grace, child.wait()).await {
            Ok(status) => exit_code = status.ok().and_then(|status| status.code()),
            Err(_) => {
                kill_process_group(pid, libc::SIGKILL);
                if let Ok(status) = tokio::time::timeout(request.kill_grace, child.wait()).await {
                    exit_code = status.ok().and_then(|status| status.code());
                }
            }
        }
    }

    // Always sweep the process group, even after a successful leader exit.
    // Otherwise `sh -c 'job &'` can leak a background grandchild and keep its
    // pipes alive beyond the tool call.
    kill_process_group(pid, libc::SIGKILL);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(request.kill_grace, child.wait()).await;

    let _ = tokio::time::timeout(request.kill_grace, async {
        let _ = out_task.await;
        let _ = err_task.await;
    })
    .await;

    let (stdout_bytes, stdout_dropped) = {
        let capture = stdout_cap.lock().expect("capture lock poisoned");
        (capture.concat(), capture.dropped)
    };
    let (stderr_bytes, stderr_dropped) = {
        let capture = stderr_cap.lock().expect("capture lock poisoned");
        (capture.concat(), capture.dropped)
    };
    let (stdout_tail, stdout_cut) = tail_output(
        &stdout_bytes,
        request.capture.tail_bytes,
        request.capture.tail_lines,
    );
    let (stderr_tail, stderr_cut) = tail_output(
        &stderr_bytes,
        request.capture.tail_bytes,
        request.capture.tail_lines,
    );

    Ok(ShellExecResult {
        exit_code,
        stdout_tail,
        stderr_tail,
        truncated: stdout_dropped || stderr_dropped || stdout_cut || stderr_cut,
        duration: started.elapsed(),
        sandbox_mode,
        output_path: transcript.finish(),
        killed,
    })
}

/// Run a shell command inside the active host-authorized root.
///
/// The host may configure invocation, environment, capture, and sandbox cwd
/// through [`ShellConfig`] and [`ShellEnvironmentProvider`]. Execution itself
/// always passes through [`execute_shell`].
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellInput {
    /// The command line passed to the configured shell.
    #[schemars(length(min = 1))]
    pub command: String,
    /// Working directory, relative to the active root or an absolute path the
    /// host policy permits reading. Defaults to the active root.
    pub cwd: Option<String>,
    /// Short human-readable description for host UI.
    #[schemars(length(min = 1))]
    pub description: Option<String>,
    /// Timeout in milliseconds.
    #[schemars(range(min = 1_000, max = 600_000))]
    pub timeout_ms: Option<u64>,
}

/// Executes shell commands through AC's shared sandboxed process executor.
#[derive(Default)]
pub struct Shell {
    description_override: Option<String>,
}

impl Shell {
    pub fn with_description(description: impl Into<String>) -> Self {
        Self {
            description_override: Some(description.into()),
        }
    }
}

#[derive(Serialize)]
struct ShellResult {
    exit_code: Option<i32>,
    stdout_tail: String,
    stderr_tail: String,
    truncated: bool,
    duration_ms: u64,
    sandbox: ShellSandboxResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    killed: Option<&'static str>,
}

#[derive(Serialize)]
struct ShellSandboxResult {
    mode: &'static str,
    platform: &'static str,
}

fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        _ => "linux",
    }
}

impl Tool for Shell {
    type Input = ShellInput;

    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> String {
        self.description_override.clone().unwrap_or_else(|| {
            "Run a command inside the active host-authorized root. Use cwd for \
             another host-authorized directory, description for concise UI \
             context, and timeout_ms for commands that need a custom deadline. \
             Output is returned as bounded stdout/stderr tails; a transcript \
             path is included when configured or when output overflows. The \
             command and its process group are terminated on timeout, cancel, \
             or return. The result reports the achieved sandbox mode."
                .into()
        })
    }

    fn capability(&self) -> Capability {
        Capability::Guarded
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            if input.command.is_empty() {
                return ToolOutput::error("shell: `command` must be a non-empty string.");
            }
            if input.description.as_deref() == Some("") {
                return ToolOutput::error("shell: `description` must be a non-empty string.");
            }

            let config = ctx
                .extensions
                .get::<ShellConfig>()
                .map(|config| (*config).clone())
                .unwrap_or_default();
            let default_timeout_ms = match u64::try_from(config.default_timeout.as_millis()) {
                Ok(timeout) if (MIN_SHELL_TIMEOUT_MS..=MAX_SHELL_TIMEOUT_MS).contains(&timeout) => {
                    timeout
                }
                _ => {
                    return ToolOutput::error(format!(
                        "shell: invalid host configuration: default timeout must be between {MIN_SHELL_TIMEOUT_MS} and {MAX_SHELL_TIMEOUT_MS} milliseconds"
                    ));
                }
            };
            let timeout_ms = input.timeout_ms.unwrap_or(default_timeout_ms);
            if !(MIN_SHELL_TIMEOUT_MS..=MAX_SHELL_TIMEOUT_MS).contains(&timeout_ms) {
                return ToolOutput::error(format!(
                    "shell: `timeout_ms` must be an integer between {MIN_SHELL_TIMEOUT_MS} and {MAX_SHELL_TIMEOUT_MS}."
                ));
            }

            let env = match ctx.extensions.get::<Arc<dyn ShellEnvironmentProvider>>() {
                Some(provider) => match provider.environment() {
                    Ok(environment) => environment,
                    Err(error) => return ToolOutput::error(error),
                },
                None => Vec::new(),
            };

            let cwd = input.cwd.unwrap_or_else(|| ".".to_string());
            let resolved = match ctx.policy.resolve_read(Path::new(&cwd)) {
                Ok(path) => path,
                Err(error) => return ToolOutput::error(error.to_string()),
            };
            if let Some(roots) = &config.cwd_roots
                && !roots.iter().any(|root| resolved.starts_with(root))
            {
                return ToolOutput::error(format!(
                    "shell: cwd resolves outside the configured roots ({})",
                    resolved.display()
                ));
            }

            let capture = config.capture.unwrap_or_else(|| {
                let spill_dir = ctx
                    .extensions
                    .get::<ShellSpillDir>()
                    .map(|dir| dir.0.clone())
                    .unwrap_or_else(|| std::env::temp_dir().join("ac-shell-spill"));
                ShellCaptureOptions::overflow_only(spill_dir)
            });
            let sandbox_cwd = ctx.sandbox.as_ref().and_then(|launcher| {
                (!launcher.permits_cwd(&resolved))
                    .then(|| config.sandbox_cwd_fallback.clone())
                    .flatten()
            });
            let args = config.invocation.args(input.command.clone());
            let execution = match execute_shell(
                ctx,
                ShellExecRequest {
                    program: config.invocation.program,
                    args,
                    command_for_approval: input.command,
                    cwd: resolved,
                    sandbox_cwd,
                    env,
                    timeout: Duration::from_millis(timeout_ms),
                    kill_grace: config.kill_grace,
                    capture,
                },
            )
            .await
            {
                Ok(execution) => execution,
                Err(error) => {
                    let error = error
                        .strip_prefix("failed to spawn command:")
                        .map(|detail| format!("shell: spawn failed:{detail}"))
                        .or_else(|| {
                            error
                                .strip_prefix("failed to open output log:")
                                .map(|detail| format!("shell: failed to open output log:{detail}"))
                        })
                        .unwrap_or(error);
                    return ToolOutput::error(error);
                }
            };

            let result = ShellResult {
                exit_code: execution.exit_code,
                stdout_tail: execution.stdout_tail,
                stderr_tail: execution.stderr_tail,
                truncated: execution.truncated,
                duration_ms: execution.duration.as_millis() as u64,
                sandbox: ShellSandboxResult {
                    mode: execution.sandbox_mode.as_str(),
                    platform: platform_name(),
                },
                output_path: execution.output_path.map(|path| path.display().to_string()),
                killed: execution.killed.map(|reason| match reason {
                    ShellKillReason::Timeout => "timeout",
                    ShellKillReason::Cancelled => "aborted",
                }),
            };
            match serde_json::to_string(&result) {
                Ok(body) => ToolOutput::ok(body),
                Err(error) => ToolOutput::error(format!("shell: cannot serialize result: {error}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_approvals::{Matcher, Policy, ProgramRules, Rule};
    use ac_tool::SubtreePolicy;

    fn run(cmd: &str, ctx: Arc<ToolCtx>) -> impl std::future::Future<Output = ToolOutput> {
        Arc::new(Shell::default()).run(
            ShellInput {
                command: cmd.to_string(),
                cwd: None,
                description: None,
                timeout_ms: None,
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
        // ...and carries the returned tail at its end while preserving the
        // transcript from byte zero.
        let tail = v["stdout_tail"].as_str().unwrap().as_bytes();
        assert!(spilled.ends_with(tail));
        assert!(spilled.starts_with(b"1\n2\n3\n"));
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
        assert_eq!(v["truncated"], false);
        assert_eq!(std::fs::read_dir(spill_dir.path()).unwrap().count(), 0);
    }

    struct TestEnvironment;

    impl ShellEnvironmentProvider for TestEnvironment {
        fn environment(&self) -> Result<Vec<(OsString, OsString)>, String> {
            Ok(vec![(
                OsString::from("AC_SHELL_TEST_VALUE"),
                OsString::from("configured"),
            )])
        }
    }

    #[tokio::test]
    async fn host_configuration_drives_invocation_environment_capture_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        ctx.extensions.insert(ShellConfig {
            invocation: ShellInvocation::new("sh", ["-c"]),
            default_timeout: Duration::from_secs(2),
            kill_grace: Duration::from_millis(100),
            capture: Some(ShellCaptureOptions {
                tail_bytes: 1024,
                tail_lines: 10,
                transcript_dir: transcript_dir.path().to_path_buf(),
                always_transcript: true,
                transcript_cap: Some(4096),
            }),
            cwd_roots: None,
            sandbox_cwd_fallback: None,
        });
        let environment: Arc<dyn ShellEnvironmentProvider> = Arc::new(TestEnvironment);
        ctx.extensions.insert(environment);

        let out = Arc::new(Shell::default())
            .run(
                ShellInput {
                    command: "printf '%s' \"$AC_SHELL_TEST_VALUE\"".to_string(),
                    cwd: None,
                    description: Some("read configured environment".to_string()),
                    timeout_ms: Some(1_000),
                },
                ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let value: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(value["stdout_tail"], "configured");
        assert_eq!(value["truncated"], false);
        assert!(value["duration_ms"].is_u64());
        assert_eq!(value["sandbox"]["mode"], "off");
        assert_eq!(value["sandbox"]["platform"], platform_name());
        let output_path = value["output_path"].as_str().expect("eager transcript");
        assert!(Path::new(output_path).starts_with(transcript_dir.path()));
    }

    #[tokio::test]
    async fn model_timeout_returns_a_normal_killed_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        let out = Arc::new(Shell::default())
            .run(
                ShellInput {
                    command: "sleep 5".to_string(),
                    cwd: None,
                    description: Some("wait past deadline".to_string()),
                    timeout_ms: Some(MIN_SHELL_TIMEOUT_MS),
                },
                ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let value: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(value["killed"], "timeout");
        assert_eq!(value["exit_code"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn cancellation_returns_a_normal_aborted_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(dir.path()).unwrap(),
        )));
        let cancel = ctx.cancel.clone();
        let task = tokio::spawn({
            let ctx = ctx.clone();
            async move { run("sleep 5", ctx).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        let out = task.await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        let value: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(value["killed"], "aborted");
        assert_eq!(value["exit_code"], serde_json::Value::Null);
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
