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
//! `deny_path` that overlaps a granted root, and recursive component write
//! denies, cannot be subtracted by Landlock. Such a policy is reported
//! `Degraded` (or refused when fail-closed) rather than falsely `Strict`; the
//! default secret set normally lives outside granted roots and remains denied
//! by omission.
//!
//! Network model: when the policy is network-off, a seccomp filter EPERMs
//! `socket()` for every family (no TCP, UDP, or DNS) and `io_uring_*` (which
//! could otherwise create sockets without `socket()`); `ptrace`/`process_vm_*`
//! are denied regardless (anti-escape). When network is on, `socket()` is
//! allowed and only the anti-escape denials remain.

use std::ffi::OsStr;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use ac_tool::{CommandSpec, NetworkMode, Prepared, SandboxError, SandboxMode, SandboxPolicy};
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr,
};
use rustix::fs::{Mode, OFlags, open, openat};

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
    // Decide the achievable mode up front from a parent-side probe that checks
    // ACTUAL enforcement, not mere syscall availability — on a kernel where
    // Landlock is compiled but not in the active LSM list, `landlock_create_
    // ruleset` succeeds yet `restrict_self` enforces nothing. Reporting Strict
    // there would be the fail-open the whole design forbids.
    let landlock_supported = landlock_enforces();
    let policy_gap = unenforceable_policy_gap(policy);
    if (!landlock_supported || policy_gap.is_some()) && policy.fail_closed {
        let detail = match (landlock_supported, policy_gap.as_deref()) {
            (false, Some(gap)) => format!(
                "landlock does not enforce on this kernel and cannot enforce this policy: {gap}"
            ),
            (false, None) => {
                "landlock does not enforce on this kernel (LSM inactive or absent)".to_string()
            }
            (true, Some(gap)) => gap.to_string(),
            (true, None) => unreachable!(),
        };
        return Err(SandboxError::NotEnforceable(format!(
            "{detail}; Linux Landlock is allow-only, so enable degraded mode \
                 or use a backend that supports subtractive path rules"
        )));
    }

    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);

    // Resource caps first (their pre_exec runs before the containment closure;
    // setrlimit needs neither landlock nor seccomp).
    let mut degraded = !landlock_supported || policy_gap.is_some();
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

/// Best mode knowable before preparing a command.
pub(crate) fn mode(policy: &SandboxPolicy) -> SandboxMode {
    if !landlock_enforces() || unenforceable_policy_gap(policy).is_some() {
        SandboxMode::Degraded
    } else {
        SandboxMode::Strict
    }
}

/// Return the part of the requested path policy Landlock cannot express.
///
/// Landlock rules are additive allows. Once a tree is granted, neither an
/// absolute descendant nor a component name at arbitrary depth can be denied.
fn unenforceable_policy_gap(policy: &SandboxPolicy) -> Option<String> {
    if !policy.write_deny_rules.is_empty() {
        return Some(format!(
            "{} recursive component write deny rule(s) cannot be subtracted \
             from writable roots",
            policy.write_deny_rules.len()
        ));
    }

    let grants = policy.read_roots.iter().chain(policy.write_roots.iter());
    for deny in &policy.deny_paths {
        for grant in grants.clone() {
            if deny.starts_with(grant) || grant.starts_with(deny) {
                return Some(format!(
                    "absolute deny {} overlaps granted root {}",
                    deny.display(),
                    grant.display()
                ));
            }
        }
    }
    None
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

/// Whether Landlock *actually enforces* on this kernel — not merely whether the
/// syscalls exist. Probed once per process (cached) by applying a real ruleset
/// on a throwaway thread and confirming it denies a forbidden action. This is
/// the honest detector: a "present but inactive LSM" (Landlock compiled in but
/// not on the boot `lsm=` list, e.g. Docker Desktop's LinuxKit) accepts the
/// syscalls but enforces nothing, and only an actual violation attempt reveals
/// it.
fn landlock_enforces() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(probe_landlock)
}

fn probe_landlock() -> bool {
    // `restrict_self` binds the *calling thread* (and its future children), so
    // run the probe on a scratch thread — the main thread and the real command
    // children stay unrestricted.
    std::thread::spawn(|| {
        let abi = ABI::V1;
        // Handle all FS access but grant only READ on "/": no write anywhere.
        let ruleset = match Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(abi))
            .and_then(|r| r.create())
        {
            Ok(r) => r,
            Err(_) => return false,
        };
        let ruleset = match PathFd::new("/") {
            Ok(fd) => match ruleset.add_rule(PathBeneath::new(fd, AccessFs::from_read(abi))) {
                Ok(r) => r,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        if ruleset.restrict_self().is_err() {
            return false;
        }
        // Creating a file requires a write grant we did not give. If it
        // succeeds, Landlock is not actually enforcing.
        let probe = std::env::temp_dir().join(format!("ac-ll-probe-{}", std::process::id()));
        match std::fs::File::create(&probe) {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                false
            }
            Err(_) => true,
        }
    })
    .join()
    .unwrap_or(false)
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
        if let Ok(fd) = open_root_capability(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_only))
                .map_err(|e| e.to_string())?;
        }
    }
    for path in write_paths {
        if let Ok(fd) = open_root_capability(path) {
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

/// Open a policy root component-by-component without following symlinks.
///
/// `PathFd::new(path)` follows a replacement symlink. Policy roots are
/// canonical identities captured by the host, so doing that at command launch
/// could grant a post-capture symlink target. Descriptor-relative traversal
/// with `O_NOFOLLOW` instead turns replacement into a missing grant (safe
/// capability reduction), never new authority.
fn open_root_capability(path: &Path) -> io::Result<OwnedFd> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("sandbox root is not absolute: {}", path.display()),
        ));
    }
    let dir_flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let leaf_flags = OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut fd = open(Path::new("/"), dir_flags, Mode::empty()).map_err(io::Error::from)?;
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(Ok(name)),
            Component::RootDir => None,
            _ => Some(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("sandbox root is not normalized: {}", path.display()),
            ))),
        })
        .collect::<io::Result<Vec<&OsStr>>>()?;
    for (index, component) in components.iter().enumerate() {
        let flags = if index + 1 == components.len() {
            leaf_flags
        } else {
            dir_flags
        };
        fd = openat(&fd, *component, flags, Mode::empty()).map_err(io::Error::from)?;
    }
    Ok(fd)
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
