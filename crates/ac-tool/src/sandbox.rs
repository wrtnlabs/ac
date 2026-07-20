//! The OS-sandbox seam: the kit expresses *intent* (a [`SandboxPolicy`]) and a
//! host-supplied [`SandboxLauncher`] turns a command into a kernel-contained
//! one. The kit never spawns an unsandboxed child behind a caller's back — a
//! tool that runs external processes asks the launcher to `prepare` its command
//! and honors the [`SandboxMode`] it gets back.
//!
//! Doctrine (see `docs/ac-sandbox.md`): only *actual* (kernel-enforced)
//! mechanisms ship. Where a platform has no honest kernel path (native
//! Windows), the launcher reports [`SandboxMode::Off`] loudly rather than
//! emitting an advisory approximation. A launcher MUST fail closed: if a
//! strict policy cannot be enforced it returns [`SandboxError`], never a
//! weaker command silently.
//!
//! This module is pure types + a trait. The mechanisms (Seatbelt profiles on
//! macOS, `landlock`+`seccompiler` on Linux) live in the `ac-sandbox` crate,
//! which implements [`SandboxLauncher`]; the runtime and the built-in tools
//! depend only on this seam.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Whether the sandboxed process may reach the network at all. v1 is binary:
/// `Off` is a real kernel guarantee (no reachable socket — no TCP, UDP, or
/// DNS), `On` places no network restriction. Domain-level filtering is a
/// deferred phase (`docs/ac-sandbox.md` v2) precisely because a hostname
/// allowlist is only honest as a full kernel-block-then-proxy architecture,
/// not a string match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    Off,
    On,
}

/// Per-process resource caps applied to the sandboxed child. `None` leaves a
/// limit at the host default. These are the fork-bomb / runaway-cost defense
/// the reference sandboxes all omit; a kit that runs arbitrary shell should
/// not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Max number of processes/threads for the child's real user
    /// (`RLIMIT_NPROC`) — the fork-bomb cap.
    pub max_processes: Option<u64>,
    /// Max address space in bytes (`RLIMIT_AS`).
    pub max_address_space: Option<u64>,
    /// Max CPU seconds (`RLIMIT_CPU`) — a wall-clock timeout already exists in
    /// the shell tool; this caps CPU burn independently.
    pub max_cpu_seconds: Option<u64>,
    /// Max size of a single file the child may create (`RLIMIT_FSIZE`).
    pub max_file_size: Option<u64>,
}

/// The platform-neutral sandbox intent. A host builds one; the launcher
/// translates it into the platform mechanism. All roots should be canonical
/// absolute paths (the launcher does not re-resolve them — resolving at
/// enforcement time is a TOCTOU hole; canonicalize once when building the
/// policy).
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Directories the child may read. Reads outside these are denied.
    pub read_roots: Vec<PathBuf>,
    /// Directories the child may write. Writes outside these are denied.
    pub write_roots: Vec<PathBuf>,
    /// Paths denied regardless of `read_roots`/`write_roots` — SSH keys, cloud
    /// creds, shell rc files, `.git/hooks`, `.git/config`, and the like.
    /// Independent of the allow-set by design (defense against config
    /// tampering that would re-grant the child a way out).
    pub deny_paths: Vec<PathBuf>,
    pub network: NetworkMode,
    pub limits: ResourceLimits,
    /// When true (the default posture), any inability to enforce the requested
    /// containment is a hard [`SandboxError`] — the command is not spawned.
    /// When false, the launcher enforces as much as it can and reports the
    /// achieved [`SandboxMode`] (`Degraded`/`Off`) instead of failing.
    pub fail_closed: bool,
}

impl SandboxPolicy {
    /// A policy that contains reads and writes to a single workspace subtree
    /// with the network off and the standard secret deny-set — the sensible
    /// default for a workspace agent. `root` should already be canonical.
    pub fn workspace(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            read_roots: vec![root.clone()],
            write_roots: vec![root],
            deny_paths: default_deny_paths(),
            network: NetworkMode::Off,
            limits: ResourceLimits::default(),
            fail_closed: true,
        }
    }

    /// Widen reads to an additional root (e.g. a parent workspace) without
    /// granting writes there.
    pub fn read_also(mut self, root: impl Into<PathBuf>) -> Self {
        self.read_roots.push(root.into());
        self
    }

    /// Allow network egress (v1: unrestricted; there is no honest middle
    /// ground until the v2 proxy lands).
    pub fn with_network(mut self, network: NetworkMode) -> Self {
        self.network = network;
        self
    }

    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn allow_degraded(mut self) -> Self {
        self.fail_closed = false;
        self
    }
}

/// The mandatory secret deny-set, relative to the host `$HOME` when it is
/// known. These are denied even if they fall inside an allowed read root.
/// Absolute, existence-agnostic — a launcher applies the ones that make sense
/// for its mechanism.
pub fn default_deny_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        for rel in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/gcloud",
            ".docker/config.json",
            ".netrc",
            ".git-credentials",
            ".npmrc",
            ".pypirc",
            ".kube",
        ] {
            out.push(home.join(rel));
        }
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// What a launcher achieved for one command. Rides a tool's result envelope so
/// a host UI can surface the isolation level (a banner on anything but
/// `Strict`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Every requested containment is kernel-enforced.
    Strict,
    /// A real but partial sandbox — e.g. an older kernel that enforces only
    /// part of the requested filesystem policy, or resource caps the host
    /// could not set. Surfaced, never silent.
    Degraded,
    /// No OS containment (native Windows, or a host that disabled it). The
    /// child runs with the host process's own privileges. Surfaced on every
    /// call.
    Off,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::Strict => "strict",
            SandboxMode::Degraded => "degraded",
            SandboxMode::Off => "off",
        }
    }
}

/// The command a tool wants to run, described so the launcher can wrap it. The
/// launcher owns argv-rewriting (macOS wraps with `sandbox-exec`) and any
/// in-process `pre_exec` enforcement (Linux); the caller keeps ownership of
/// stdio, process-group, spawning, and killing.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// The program to exec (e.g. `sh`).
    pub program: OsString,
    /// Its arguments (e.g. `["-c", "<command>"]`).
    pub args: Vec<OsString>,
    /// Canonical working directory — must lie inside a write root.
    pub cwd: PathBuf,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
        }
    }
}

/// A command the launcher has made ready to spawn. The `command` already has
/// program, args, cwd, any argv wrapping, and any `pre_exec` enforcement
/// applied; the caller adds stdio/process-group and spawns it. `mode` is the
/// isolation actually achieved for this command.
pub struct Prepared {
    pub command: tokio::process::Command,
    pub mode: SandboxMode,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The requested containment could not be enforced and the policy is
    /// fail-closed. The command was not prepared.
    #[error("sandbox could not enforce the requested policy: {0}")]
    NotEnforceable(String),
    /// The command spec is invalid for sandboxing (e.g. a cwd outside every
    /// write root).
    #[error("invalid sandbox command: {0}")]
    Invalid(String),
    /// A mechanism-level failure while building the sandbox (profile
    /// generation, filter compilation, etc.).
    #[error("sandbox setup failed: {0}")]
    Setup(String),
}

/// The host seam. A host installs a launcher on the [`ToolCtx`](crate::ToolCtx);
/// tools that run external processes call [`prepare`](SandboxLauncher::prepare)
/// and spawn the result. Implementations live in `ac-sandbox`.
pub trait SandboxLauncher: Send + Sync {
    /// Wrap `spec` into a kernel-contained command per the launcher's policy.
    /// MUST fail closed when the policy is fail-closed and full enforcement is
    /// impossible.
    fn prepare(&self, spec: &CommandSpec) -> Result<Prepared, SandboxError>;

    /// The best isolation this launcher can achieve, known before any command
    /// runs — for a host to surface a startup banner. The per-command
    /// [`Prepared::mode`] is authoritative for an individual run (it may
    /// degrade at enforcement time).
    fn mode(&self) -> SandboxMode;

    /// Whether `cwd` lies inside a write root of this launcher's policy — a
    /// cheap pre-check a caller can use before building a full spec. Default:
    /// assume yes (a launcher with no filesystem policy).
    fn permits_cwd(&self, _cwd: &Path) -> bool {
        true
    }
}
