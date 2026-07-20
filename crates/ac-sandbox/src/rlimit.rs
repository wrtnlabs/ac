//! Resource caps applied to the sandboxed child via `setrlimit` in a
//! `pre_exec` hook. The limit values are read from the policy in the parent
//! (plain integers); the closure that runs in the forked child before `exec`
//! does nothing but issue `setrlimit` syscalls, which are async-signal-safe.
//!
//! Caps inherit across the child's `exec`, so wrapping `sandbox-exec` (macOS)
//! still bounds the shell it launches.

use std::io;

use ac_tool::{ResourceLimits, SandboxError};

/// One (resource, value) pair to set. `resource` is the platform `RLIMIT_*`
/// constant (already the right integer type per OS via the `libc` crate).
struct Limit {
    resource: LimitId,
    value: u64,
}

// libc's setrlimit takes a different resource type per platform; carry the
// constant through a thin newtype so the closure stays platform-clean.
#[cfg(target_os = "linux")]
type LimitId = u32;
#[cfg(not(target_os = "linux"))]
type LimitId = libc::c_int;

/// Install the requested resource limits as a `pre_exec` hook on `cmd`.
///
/// Returns `Err` if a requested limit cannot be honestly enforced on this
/// platform (e.g. `max_address_space` on macOS, which lacks an effective
/// `RLIMIT_AS`) — the caller decides whether that is fatal (fail-closed) or a
/// reason to report `Degraded`.
pub fn install(
    cmd: &mut tokio::process::Command,
    limits: &ResourceLimits,
) -> Result<(), SandboxError> {
    let mut to_set: Vec<Limit> = Vec::new();

    if let Some(n) = limits.max_processes {
        to_set.push(Limit {
            resource: libc::RLIMIT_NPROC as LimitId,
            value: n,
        });
    }
    if let Some(n) = limits.max_cpu_seconds {
        to_set.push(Limit {
            resource: libc::RLIMIT_CPU as LimitId,
            value: n,
        });
    }
    if let Some(n) = limits.max_file_size {
        to_set.push(Limit {
            resource: libc::RLIMIT_FSIZE as LimitId,
            value: n,
        });
    }
    if let Some(n) = limits.max_address_space {
        // macOS has no effective RLIMIT_AS (it aliases the ignored RLIMIT_RSS),
        // so setting it would be a lie. Refuse rather than pretend.
        #[cfg(target_os = "macos")]
        {
            let _ = n;
            return Err(SandboxError::NotEnforceable(
                "max_address_space (RLIMIT_AS) is not enforced on macOS".to_string(),
            ));
        }
        #[cfg(not(target_os = "macos"))]
        {
            to_set.push(Limit {
                resource: libc::RLIMIT_AS as LimitId,
                value: n,
            });
        }
    }

    if to_set.is_empty() {
        return Ok(());
    }

    // SAFETY: the closure runs in the child between fork and exec. It only
    // calls setrlimit (async-signal-safe) over values captured by move; it
    // allocates nothing and touches no shared state.
    unsafe {
        cmd.pre_exec(move || {
            for lim in &to_set {
                let rl = libc::rlimit {
                    rlim_cur: lim.value as libc::rlim_t,
                    rlim_max: lim.value as libc::rlim_t,
                };
                if libc::setrlimit(lim.resource, &rl) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}
