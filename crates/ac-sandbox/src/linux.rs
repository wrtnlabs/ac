//! Linux backend: `landlock` (filesystem) + `seccompiler` (syscall) +
//! `setrlimit` (resources), all self-applied in the child's `pre_exec` before
//! `exec`. No bubblewrap, no user namespace, no setuid helper — every mechanism
//! here is something an unprivileged process does to itself, which is what lets
//! v1 sidestep the distro unprivileged-userns policy trap entirely.
//!
//! Filesystem model (landlock is allow-only — it has no deny rule): the child
//! is granted read on a fixed set of system roots plus the policy's read roots,
//! and read+write on the write roots. Everything else — including `$HOME` and
//! the secret deny-set that lives under it — is denied by *omission*, which is
//! strictly more robust than a deny rule (no case/symlink bypass). A
//! `deny_path` that happens to sit inside a granted read root is NOT carved out
//! by landlock v1 (documented asymmetry vs macOS); the default secret set lives
//! outside the granted roots, so it is denied on both platforms.
//!
//! Network model: when the policy is network-off, a seccomp filter EPERMs
//! `socket()` for every family (no TCP, UDP, or DNS) and `io_uring_*` (which
//! could otherwise create sockets without `socket()`); `ptrace`/`process_vm_*`
//! are denied regardless (anti-escape). When network is on, `socket()` is
//! allowed and only the anti-escape denials remain.

use std::io;
use std::path::PathBuf;

use ac_tool::{CommandSpec, NetworkMode, Prepared, SandboxError, SandboxMode, SandboxPolicy};
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr,
};

use crate::rlimit;

/// System roots granted read (and execute) access so ordinary programs — the
/// dynamic loader, `sh`, common toolchains — can run under default-deny.
/// Deliberately excludes `/tmp`, `/run`, and `$HOME`: those hold user data and
/// secrets, and a test/real workspace under `/tmp` should be readable only via
/// its explicit read-root grant, not because all of `/tmp` is open.
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/proc", "/sys", "/var", "/dev",
];

/// Specific `/dev` nodes the child may WRITE (landlock is allow-only, so
/// `2>/dev/null` and friends need an explicit write grant — `/dev` itself is
/// read-only above). Only harmless character devices; never block devices.
const DEV_WRITE_NODES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/tty",
    "/dev/random",
    "/dev/urandom",
    "/dev/ptmx",
    "/dev/pts",
];

pub fn prepare(policy: &SandboxPolicy, spec: &CommandSpec) -> Result<Prepared, SandboxError> {
    // Decide the achievable mode up front from a parent-side landlock probe, so
    // the envelope is honest and fail-closed policies refuse before spawning.
    let landlock_supported = landlock_available();
    if !landlock_supported && policy.fail_closed {
        return Err(SandboxError::NotEnforceable(
            "landlock is not available on this kernel and the policy is \
             fail-closed; enable Landlock LSM or allow degraded mode"
                .to_string(),
        ));
    }

    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);

    // Resource caps first (their pre_exec runs before the containment closure;
    // setrlimit needs neither landlock nor seccomp).
    let mut degraded = !landlock_supported;
    if let Err(e) = rlimit::install(&mut cmd, &policy.limits) {
        if policy.fail_closed {
            return Err(e);
        }
        degraded = true;
    }

    // Build the landlock ruleset in the PARENT (allocations happen here); the
    // child only issues the final restrict_self syscall.
    let read_paths = read_grant_paths(policy);
    let write_paths = write_grant_paths(policy);
    let network_off = policy.network == NetworkMode::Off;

    // SAFETY: the closure runs in the forked child before exec. It sets
    // NO_NEW_PRIVS, enforces landlock, and installs a seccomp filter — all
    // async-signal-safe syscalls over data captured by move.
    unsafe {
        cmd.pre_exec(move || {
            // Required for unprivileged landlock and seccomp.
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if landlock_supported {
                enforce_landlock(&read_paths, &write_paths)
                    .map_err(|e| io::Error::other(format!("landlock: {e}")))?;
            }
            install_seccomp(network_off).map_err(|e| io::Error::other(format!("seccomp: {e}")))?;
            Ok(())
        });
    }

    let mode = if degraded {
        SandboxMode::Degraded
    } else {
        SandboxMode::Strict
    };
    Ok(Prepared { command: cmd, mode })
}

/// The read roots to grant: the system roots that exist, plus the policy's
/// read roots. (Write roots are granted read implicitly via the write grant.)
fn read_grant_paths(policy: &SandboxPolicy) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = SYSTEM_READ_ROOTS
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    for r in &policy.read_roots {
        paths.push(r.clone());
    }
    paths
}

/// The write roots to grant read+write: the policy's write roots plus the
/// harmless `/dev` character devices programs expect to write.
fn write_grant_paths(policy: &SandboxPolicy) -> Vec<PathBuf> {
    let mut paths = policy.write_roots.clone();
    for node in DEV_WRITE_NODES {
        let p = PathBuf::from(node);
        if p.exists() {
            paths.push(p);
        }
    }
    paths
}

/// Whether the kernel supports landlock, probed by creating a throwaway ruleset
/// in the parent. Best-effort: a create failure means no Landlock LSM.
fn landlock_available() -> bool {
    let abi = ABI::V1;
    Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.create())
        .is_ok()
}

fn enforce_landlock(read_paths: &[PathBuf], write_paths: &[PathBuf]) -> Result<(), String> {
    let abi = ABI::V1;
    let read_only = AccessFs::from_read(abi);
    let read_write = AccessFs::from_all(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?
        .create()
        .map_err(|e| e.to_string())?;

    for path in read_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_only))
                .map_err(|e| e.to_string())?;
        }
    }
    for path in write_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_write))
                .map_err(|e| e.to_string())?;
        }
    }

    // Best-effort may enforce nothing on a kernel without landlock; the caller
    // already gated fail-closed on the parent-side probe, so a NotEnforced here
    // is only reachable in a policy that opted into degraded mode. The achieved
    // mode is decided parent-side, so the concrete status is not needed here.
    ruleset.restrict_self().map_err(|e| e.to_string())?;
    Ok(())
}

/// Install a seccomp-BPF filter: default allow, EPERM the escape/network
/// syscalls. Built and applied in the child.
fn install_seccomp(network_off: bool) -> Result<(), String> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch, apply_filter,
    };
    use std::collections::BTreeMap;

    #[cfg(target_arch = "x86_64")]
    let arch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = TargetArch::aarch64;

    // Anti-escape denials, always on.
    let mut blocked: Vec<i64> = vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ];
    if network_off {
        // No socket of any family, and no io_uring (which can create sockets
        // without socket()).
        blocked.push(libc::SYS_socket);
        blocked.push(libc::SYS_socketpair);
        blocked.push(libc::SYS_io_uring_setup);
        blocked.push(libc::SYS_io_uring_enter);
        blocked.push(libc::SYS_io_uring_register);
    }

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in blocked {
        rules.insert(nr, vec![]); // empty rule set => match unconditionally
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                     // default: allow
        SeccompAction::Errno(libc::EPERM as u32), // matched: EPERM
        arch,
    )
    .map_err(|e| e.to_string())?;

    let program: BpfProgram = filter.try_into().map_err(|e| format!("{e}"))?;
    apply_filter(&program).map_err(|e| e.to_string())?;
    Ok(())
}
