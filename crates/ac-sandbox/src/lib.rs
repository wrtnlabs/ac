//! `ac-sandbox` — kernel-enforced OS containment for the commands the `shell`
//! tool runs, implementing the [`SandboxLauncher`] seam from `ac-tool`.
//!
//! Doctrine and the full design are in `docs/ac-sandbox.md`. The one rule: only
//! *actual* (kernel-enforced) mechanisms ship. On macOS that is Apple Seatbelt
//! via `sandbox-exec`; on Linux it is `landlock` (filesystem) + `seccompiler`
//! (syscall) + `setrlimit` (resources), all self-applied in the child before
//! `exec` — no bubblewrap, no user namespace, no setuid helper. On any other
//! platform (native Windows) there is no honest kernel path: the launcher runs
//! the command unsandboxed and reports [`SandboxMode::Off`], or, if the policy
//! is fail-closed, refuses.
//!
//! v1 covers filesystem containment, syscall restriction, resource caps, and
//! binary network on/off. Domain-level egress filtering is a deferred phase
//! (`docs/ac-sandbox.md` v2).

use ac_tool::{CommandSpec, Prepared, SandboxError, SandboxLauncher, SandboxMode, SandboxPolicy};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(unix)]
mod rlimit;

/// The host-facing launcher. Construct with a [`SandboxPolicy`]; it dispatches
/// to the platform mechanism at `prepare` time.
pub struct OsSandbox {
    policy: SandboxPolicy,
}

impl OsSandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// Validate the spec's cwd against the policy's write roots. The
    /// in-process `PathPolicy` already contains where a tool may act; this is
    /// the sandbox's own independent check (the two layers must both hold).
    fn check_cwd(&self, spec: &CommandSpec) -> Result<(), SandboxError> {
        if self.policy.write_roots.is_empty()
            || self
                .policy
                .write_roots
                .iter()
                .any(|root| spec.cwd.starts_with(root))
        {
            Ok(())
        } else {
            Err(SandboxError::Invalid(format!(
                "working directory {} is outside every write root",
                spec.cwd.display()
            )))
        }
    }
}

impl SandboxLauncher for OsSandbox {
    fn prepare(&self, spec: &CommandSpec) -> Result<Prepared, SandboxError> {
        self.check_cwd(spec)?;
        #[cfg(target_os = "macos")]
        {
            macos::prepare(&self.policy, spec)
        }
        #[cfg(target_os = "linux")]
        {
            linux::prepare(&self.policy, spec)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            prepare_unsandboxed(&self.policy, spec)
        }
    }

    fn mode(&self) -> SandboxMode {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            SandboxMode::Strict
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            SandboxMode::Off
        }
    }

    fn permits_cwd(&self, cwd: &std::path::Path) -> bool {
        self.policy.write_roots.is_empty()
            || self.policy.write_roots.iter().any(|r| cwd.starts_with(r))
    }
}

/// An explicit no-op launcher: always runs the command unsandboxed and reports
/// [`SandboxMode::Off`]. For a host that deliberately wants no OS containment
/// but still routes through the seam so the envelope is uniform. (Not
/// installing any launcher has the same runtime effect; this makes the choice
/// explicit and greppable.)
pub struct OffSandbox;

impl SandboxLauncher for OffSandbox {
    fn prepare(&self, spec: &CommandSpec) -> Result<Prepared, SandboxError> {
        Ok(Prepared {
            command: build_bare_command(spec),
            mode: SandboxMode::Off,
        })
    }

    fn mode(&self) -> SandboxMode {
        SandboxMode::Off
    }
}

/// Build the plain (unsandboxed) command from a spec — the common starting
/// point every backend wraps or augments.
#[allow(dead_code)] // used by OffSandbox always; by the fallback only off macOS/Linux
fn build_bare_command(spec: &CommandSpec) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);
    cmd
}

/// The fallback used on platforms with no kernel mechanism. Fails closed when
/// the policy demands it; otherwise runs unsandboxed with a surfaced `Off`
/// mode. Only reachable off macOS/Linux (e.g. native Windows).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn prepare_unsandboxed(
    policy: &SandboxPolicy,
    spec: &CommandSpec,
) -> Result<Prepared, SandboxError> {
    if policy.fail_closed {
        return Err(SandboxError::NotEnforceable(
            "no OS sandbox mechanism on this platform (native Windows); \
             run under WSL2 for the Linux path, or set the policy to allow \
             degraded/off to run unsandboxed"
                .to_string(),
        ));
    }
    Ok(Prepared {
        command: build_bare_command(spec),
        mode: SandboxMode::Off,
    })
}
